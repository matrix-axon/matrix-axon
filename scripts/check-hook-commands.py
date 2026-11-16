#!/usr/bin/env python3
"""Fail when a CI gate step does not run its pre-push hook entry.

ADR 0092 makes `.pre-commit-config.yaml` the list of what must pass before a
push. The expensive CI steps stay separate checks, with the dependency caches
the workflows set up, but they do not keep a second copy of the command
(#200). Each one is:

    python3 scripts/ci/run-hook-entry.py HOOK_ID

That runner reads the hook `entry` and execs it from the repo root. This
script fails when a gate step's `run` is not that invocation, and when the
step is turned off.

## What "turned off" means

For every gate step (pre-commit, the runner, and the pinned format check):

- `continue-on-error` is absent or boolean false. The string "false" is not
  the boolean, so it does not count.
- `shell` is absent or `bash`. A shell inherited from the job or the
  workflow `defaults.run` must be bash too. The job's value wins when both
  are set.
- `env` is absent, unless the hook takes `--pull-request-base`, in which case
  the step has exactly `BASE_REF` and `BASE_SHA` from the pull_request event.
- `if` is absent, except the three rust steps in `lint-and-clippy.yml`, which
  may skip when `rust-changed` says the PR did not touch Rust.
- The job's own `if` is absent, except `cross-build.yml`'s `lint-fmt-test`,
  which honors the workflow's `checks` input.

A `working-directory` on the job or the workflow would hide
`scripts/ci/run-hook-entry.py`. A step in such a job sets `working-directory`
to `${{ github.workspace }}`. The runner then chdirs to the repo root, so
`pnpm --dir` and `--manifest-path` in the entry stay correct. The pinned
`pnpm format:check` step sets no directory of its own and must inherit
`clients/web`.

## Every hook is classified

A new hook fails this script until it is one of:

- **via pre-commit** — `lint-and-clippy.yml` runs
  `pre-commit run <id> --all-files --hook-stage pre-push`
- **via the runner** — the named steps exec the entry, with
  `--pull-request-base` for the two checks that need the PR's base ref
- **local only** — no CI twin, with a reason written down here

A watched job's other `run:` steps are either pinned (`fixed`) or explicitly
exempt. A new `cargo` or `pnpm` line in one of those jobs fails until it is
named.

The hook's `files` regex is generated from the paths this script reads. It
has to equal that regex, not merely match it.

Usage: scripts/check-hook-commands.py
"""

from __future__ import annotations

import importlib.util
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import yaml

LINT = ".github/workflows/lint-and-clippy.yml"
LINT_JOB = "lint-and-test"
DISPATCH = ".github/workflows/lint-and-test.yml"
CROSS = ".github/workflows/cross-build.yml"
CROSS_JOB = "lint-fmt-test"
WEB = ".github/workflows/web-lint-and-test.yml"
WEB_JOB = "web-lint-and-test"
DESKTOP = ".github/workflows/desktop-build.yml"
DESKTOP_JOB = "checks"
MIGRATIONS = ".github/workflows/migrations-immutable.yml"
OPENAPI = ".github/workflows/openapi-compat.yml"

# Paths this script reads besides the workflows named by the step tables.
# `scripts/check-hook-filters.py` unions `hook-commands` onto these.
HOOK_FILTER_PATHS = (
    ".pre-commit-config.yaml",
    "scripts/check-hook-commands.py",
    "scripts/ci/run-hook-entry.py",
    "scripts/ci/test-check-hook-commands.py",
)

# Regex specials, minus `-`. Python 3.14's `re.escape` started escaping
# hyphens and 3.12's does not, and this string is compared to the hook on
# whichever interpreter CI happens to run. A hyphen is literal in an
# alternation, so leaving it alone matches on both.
_REGEX_SPECIAL = set(r"\.^$*+?{}[]()|")

RUNNER_ARGV = "python3 scripts/ci/run-hook-entry.py"
WORKSPACE = "${{ github.workspace }}"
RUST_STEP_IF = "steps.rust-changed.outputs.changed == 'true'"
CROSS_JOB_IF = "${{ github.event.inputs.checks != 'false' }}"
BASE_ENV = {
    "BASE_REF": "${{ github.event.pull_request.base.ref }}",
    "BASE_SHA": "${{ github.event.pull_request.base.sha }}",
}

def _load_runner():
    path = Path(__file__).resolve().parent / "ci" / "run-hook-entry.py"
    spec = importlib.util.spec_from_file_location("run_hook_entry", path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules["run_hook_entry"] = module
    spec.loader.exec_module(module)
    return module


runner = _load_runner()


@dataclass(frozen=True)
class Step:
    """One `run:` step, addressed by workflow, job id, and step name."""

    file: str
    job: str
    name: str


@dataclass(frozen=True)
class Pinned:
    """A step whose command is frozen, and which is not a hook entry.

    `directory` is the working directory the step must inherit from the job
    or the workflow. None means the repo root.
    """

    command: str
    reason: str
    directory: str | None = None


@dataclass(frozen=True)
class Run:
    """Steps that exec one hook entry. `pull_request_base` appends the PR base."""

    steps: tuple[Step, ...]
    pull_request_base: bool = False


def _rust(name: str) -> tuple[Step, ...]:
    return (
        Step(LINT, LINT_JOB, name),
        Step(DISPATCH, "lint-and-test", name),
        Step(CROSS, CROSS_JOB, name),
    )


def runner_command(hook_id: str, *, pull_request_base: bool = False) -> str:
    command = f"{RUNNER_ARGV} {hook_id}"
    if pull_request_base:
        command += " --pull-request-base"
    return command


VIA_PRE_COMMIT: dict[str, Step] = {
    "actionlint": Step(LINT, LINT_JOB, "actionlint"),
    "markdown-no-liquid": Step(LINT, LINT_JOB, "check-liquid"),
    "adr-numbers": Step(LINT, LINT_JOB, "check-duplicate-adrs"),
    "hook-filters": Step(LINT, LINT_JOB, "check-hook-filters"),
    "hook-commands": Step(LINT, LINT_JOB, "check-hook-commands"),
    "bundle-version": Step(LINT, LINT_JOB, "check-bundle-version"),
}

RUNNER: dict[str, Run] = {
    "rustfmt": Run(_rust("Check formatting")),
    "rust-clippy": Run(_rust("Clippy")),
    "cargo-test": Run(_rust("Test")),
    "web-install": Run((Step(WEB, WEB_JOB, "Install dependencies"),)),
    "web-peers": Run((Step(WEB, WEB_JOB, "Check peer dependencies"),)),
    "web-api-schema": Run(
        (
            Step(
                WEB,
                WEB_JOB,
                "Check generated API client is in sync with openapi/openapi.json",
            ),
        )
    ),
    "web-lint": Run((Step(WEB, WEB_JOB, "Lint"),)),
    "web-test": Run((Step(WEB, WEB_JOB, "Test"),)),
    "web-build": Run((Step(WEB, WEB_JOB, "Build"),)),
    "shell-fmt": Run((Step(DESKTOP, DESKTOP_JOB, "rustfmt"),)),
    "shell-clippy": Run((Step(DESKTOP, DESKTOP_JOB, "clippy"),)),
    "shell-test": Run((Step(DESKTOP, DESKTOP_JOB, "test"),)),
    "migrations-immutable": Run(
        (
            Step(
                MIGRATIONS,
                "migrations-immutable",
                "Migrations are forward-only",
            ),
        ),
        pull_request_base=True,
    ),
    "openapi-compat": Run(
        (
            Step(
                OPENAPI,
                "openapi-compat",
                "OpenAPI spec has no unacknowledged breaking change",
            ),
        ),
        pull_request_base=True,
    ),
}

LOCAL_ONLY: dict[str, str] = {
    "notify-escaping": (
        "The hook tests scripts/lib/markdown-escape.jq. notify-opened.yml calls "
        "that library; it does not run this script."
    ),
    "prettier": (
        "The hook vendors its own prettier and checks the whole repo. The web "
        "job's format step is the package's prettier, scoped to clients/web, and "
        "is pinned below so a format failure stays its own check."
    ),
    "icons-regenerated": (
        "No workflow runs scripts/check-icons-regenerated.py. The coupling check "
        "is the pre-push hook."
    ),
    "smoke-gate": (
        "The entry is scripts/smoke-gate-hook.sh, a no-op unless RUN_SMOKE is "
        "set. smoke.yml calls scripts/smoke-gate.sh with a lane, which is a "
        "different command."
    ),
}

FIXED: dict[Step, Pinned] = {
    Step(WEB, WEB_JOB, "Check formatting"): Pinned(
        "pnpm format:check",
        "the web package's prettier, not the repo prettier hook",
        directory="clients/web",
    ),
}

EXEMPT: dict[Step, str] = {
    Step(LINT, LINT_JOB, "Install pre-commit"): "installs the runner the pre-commit steps use",
    Step(LINT, LINT_JOB, "test changed-by-hook"): "unit test of the path filter, not a hook entry",
    Step(LINT, LINT_JOB, "rust-changed"): "decides whether the rust steps run; not a hook entry",
    Step(DESKTOP, DESKTOP_JOB, "Install Linux webview dependencies"): (
        "apt packages the shell links against"
    ),
    Step(DESKTOP, DESKTOP_JOB, "Install web dependencies"): (
        "frozen-lockfile install so the shell crate can compile; not the web-install gate"
    ),
    Step(DESKTOP, DESKTOP_JOB, "Build the frontend the crate embeds"): (
        "the shell embeds clients/web/dist at compile time; not the web-build gate"
    ),
}

# Only these rust steps may carry an `if`. The same hooks run unconditionally
# in the dispatch and cross-build copies; copying this condition there would
# skip them, because those jobs have no `rust-changed` step.
STEP_IF = {
    Step(LINT, LINT_JOB, "Check formatting"): RUST_STEP_IF,
    Step(LINT, LINT_JOB, "Clippy"): RUST_STEP_IF,
    Step(LINT, LINT_JOB, "Test"): RUST_STEP_IF,
}

JOB_IF = {
    (CROSS, CROSS_JOB): CROSS_JOB_IF,
}


@dataclass(frozen=True)
class Spec:
    """Which hooks and steps this check knows about. Tests pass a smaller one."""

    via_pre_commit: dict[str, Step]
    runner: dict[str, Run]
    local_only: dict[str, str]
    fixed: dict[Step, Pinned]
    exempt: dict[Step, str]


SPEC = Spec(VIA_PRE_COMMIT, RUNNER, LOCAL_ONLY, FIXED, EXEMPT)


def pre_commit_command(hook_id: str) -> str:
    return f"pre-commit run {hook_id} --all-files --hook-stage pre-push"


def _escape_path(path: str) -> str:
    return "".join(f"\\{char}" if char in _REGEX_SPECIAL else char for char in path)


def watched_paths(spec: Spec = SPEC) -> tuple[str, ...]:
    """Every file the command check reads, in the order the files regex uses."""
    paths = {step.file for step in _named_steps(spec)} | set(HOOK_FILTER_PATHS)
    return tuple(sorted(paths))


def expected_files_regex(spec: Spec = SPEC) -> str:
    body = "|".join(_escape_path(path) for path in watched_paths(spec))
    return f"^({body})$"


def _named_steps(spec: Spec) -> list[Step]:
    steps: list[Step] = []
    steps.extend(spec.via_pre_commit.values())
    for run in spec.runner.values():
        steps.extend(run.steps)
    steps.extend(spec.fixed)
    steps.extend(spec.exempt)
    return steps


def _class_overlaps(spec: Spec) -> list[str]:
    classes = {
        "via pre-commit": set(spec.via_pre_commit),
        "runner": set(spec.runner),
        "local only": set(spec.local_only),
    }
    problems = []
    names = list(classes)
    for index, left in enumerate(names):
        for right in names[index + 1 :]:
            both = classes[left] & classes[right]
            if both:
                problems.append(f"hooks classified as both {left} and {right}: {sorted(both)}")
    for hook_id, reason in spec.local_only.items():
        if not reason.strip():
            problems.append(f"{hook_id}: local-only classification has an empty reason")
    for step, reason in spec.exempt.items():
        if not reason.strip():
            problems.append(f"{step.file} step {step.name!r}: exemption has an empty reason")
    for step, pinned in spec.fixed.items():
        if not pinned.command.strip() or not pinned.reason.strip():
            problems.append(f"{step.file} step {step.name!r}: pinned command needs text and a reason")
    for hook_id, run in spec.runner.items():
        if not run.steps:
            problems.append(f"{hook_id}: runner classification names no steps")
    seen: dict[Step, str] = {}
    labelled = [
        *[(step, "via pre-commit") for step in spec.via_pre_commit.values()],
        *[(step, "runner") for run in spec.runner.values() for step in run.steps],
        *[(step, "fixed") for step in spec.fixed],
        *[(step, "exempt") for step in spec.exempt],
    ]
    for step, label in labelled:
        previous = seen.get(step)
        if previous:
            problems.append(
                f"{step.file} job {step.job} step {step.name!r} is both {previous} and {label}"
            )
        else:
            seen[step] = label
    return problems


def _load_yaml(path: Path) -> tuple[dict | None, str | None]:
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        return None, f"{path}: {exc}"
    try:
        loaded = yaml.safe_load(text)
    except yaml.YAMLError as exc:
        return None, f"{path}: {exc}"
    if not isinstance(loaded, dict):
        return None, f"{path}: expected a mapping"
    return loaded, None


def _hook_rows(config: dict) -> tuple[dict[str, dict], list[str]]:
    problems = []
    hooks: dict[str, dict] = {}
    for repo in config.get("repos") or []:
        if not isinstance(repo, dict):
            continue
        for hook in repo.get("hooks") or []:
            if not isinstance(hook, dict):
                continue
            hook_id = hook.get("id")
            if not hook_id:
                problems.append("a hook has no id")
                continue
            if hook_id in hooks:
                problems.append(f"hook id {hook_id} is defined twice")
                continue
            hooks[str(hook_id)] = hook
    return hooks, problems


def _plain_entry(hook: dict) -> tuple[str | None, bool]:
    """(entry, broken). broken is true when an entry exists but is not one line."""
    if "entry" not in hook:
        return None, False
    entry = hook.get("entry")
    if not isinstance(entry, str) or "\n" in entry:
        return None, True
    return entry, False


def _entry_agreement(text: str, hooks: dict[str, dict]) -> list[str]:
    """The line scanner and PyYAML must see the same single-line entry."""
    scanned = runner.scan_entries(text)
    problems = []
    for hook_id, hook in hooks.items():
        plain, broken = _plain_entry(hook)
        scanned_entry = scanned.get(hook_id, _MISSING)
        if broken or scanned_entry is None:
            problems.append(f"{hook_id}: hook entry must be one plain line so CI can exec it")
            continue
        if scanned_entry is _MISSING:
            if plain is not None:
                problems.append(
                    f"{hook_id}: entry scanner missed {plain!r} (the line must be a plain scalar)"
                )
            continue
        if plain != scanned_entry:
            problems.append(
                f"{hook_id}: entry scanner read {scanned_entry!r} but YAML has {plain!r}"
            )
    return problems


class _Missing:
    pass


_MISSING = _Missing()


def _find_step(doc: dict, step: Step) -> tuple[dict | None, dict | None, str | None]:
    jobs = doc.get("jobs")
    if not isinstance(jobs, dict) or step.job not in jobs:
        return None, None, f"{step.file}: no job {step.job!r}"
    job = jobs[step.job]
    if not isinstance(job, dict):
        return None, None, f"{step.file}: job {step.job!r} is not a mapping"
    found = [
        item
        for item in job.get("steps") or []
        if isinstance(item, dict) and item.get("name") == step.name
    ]
    if not found:
        return job, None, f"{step.file} job {step.job}: no step named {step.name!r}"
    if len(found) > 1:
        return job, None, f"{step.file} job {step.job}: more than one step named {step.name!r}"
    return job, found[0], None


def _collapsed(text: str) -> str:
    return " ".join(text.split())


def _run_text(step: Step, item: dict) -> tuple[str | None, str | None]:
    run = item.get("run")
    if not isinstance(run, str) or not run.strip():
        return None, f"{step.file} step {step.name!r} has no run:"
    return run, None


def _mapping(value: object) -> dict | None:
    if isinstance(value, dict):
        return value
    return None


def _run_defaults(container: dict) -> dict:
    defaults = _mapping(container.get("defaults")) or {}
    return _mapping(defaults.get("run")) or {}


def _inherited(doc: dict, job: dict, key: str):
    """Job `defaults.run` wins. Otherwise the workflow's value, else None."""
    job_run = _run_defaults(job)
    if key in job_run:
        return job_run.get(key)
    workflow_run = _run_defaults(doc)
    if key in workflow_run:
        return workflow_run.get(key)
    return None


def _where(step: Step) -> str:
    return f"{step.file} job {step.job} step {step.name!r}"


def _check_disabled(
    doc: dict, job: dict, item: dict, step: Step, *, expect_env: dict | None
) -> list[str]:
    """Finding 1: a gate step cannot be switched off around its run line."""
    problems = []
    where = _where(step)
    continued = item.get("continue-on-error", None)
    if continued not in (None, False):
        problems.append(
            f"{where} sets continue-on-error: {continued!r} "
            "(only boolean false leaves the step a gate)"
        )
    shell = item.get("shell", None)
    if shell not in (None, "bash"):
        problems.append(f"{where} sets shell: {shell!r}; the gate runs under bash")
    else:
        inherited = _inherited(doc, job, "shell")
        if shell is None and inherited not in (None, "bash"):
            problems.append(
                f"{where} inherits shell {inherited!r}; the gate runs under bash"
            )
    actual_if = item.get("if", None)
    allowed_if = STEP_IF.get(step)
    if allowed_if is None:
        if actual_if is not None:
            problems.append(f"{where} sets if: {actual_if!r}, which can skip the gate")
    elif actual_if != allowed_if:
        problems.append(f"{where} if: must be {allowed_if!r}, found {actual_if!r}")
    actual_env = item.get("env", None)
    if expect_env is None:
        if actual_env not in (None, {}):
            problems.append(f"{where} sets env {actual_env!r}, which would change the hook command")
    elif actual_env != expect_env:
        problems.append(f"{where} env must be {expect_env!r}, found {actual_env!r}")
    return problems


def _check_launch_directory(
    doc: dict, job: dict, item: dict, step: Step, *, directory: str | None
) -> list[str]:
    """`directory` is what a pinned step must inherit. None means the repo root."""
    where = _where(step)
    own = item.get("working-directory", None)
    own_text = None if own is None else str(own)
    inherited = _inherited(doc, job, "working-directory")
    inherited_text = None if inherited is None else str(inherited)
    if directory is not None:
        if own_text is not None:
            return [
                f"{where} sets working-directory {own_text!r}; "
                f"this step is pinned to {directory!r}"
            ]
        if inherited_text != directory:
            return [
                f"{where} must inherit working-directory {directory!r}, found {inherited_text!r}"
            ]
        return []
    if inherited_text is not None:
        if own_text != WORKSPACE:
            return [
                f"{where} must set working-directory to {WORKSPACE!r} "
                f"because the job runs in {inherited_text!r}; found {own_text!r}"
            ]
        return []
    if own_text is not None:
        return [
            f"{where} sets working-directory {own_text!r}; the runner starts at the repo root"
        ]
    return []


def _check_job_gate(relative: str, job_id: str, job: dict) -> list[str]:
    problems = []
    where = f"{relative} job {job_id}"
    continued = job.get("continue-on-error", None)
    if continued not in (None, False):
        problems.append(f"{where} sets continue-on-error: {continued!r}")
    actual_if = job.get("if", None)
    allowed_if = JOB_IF.get((relative, job_id))
    if allowed_if is None:
        if actual_if is not None:
            problems.append(f"{where} sets if: {actual_if!r}, which can skip the gate")
    elif actual_if != allowed_if:
        problems.append(f"{where} if: must be {allowed_if!r}, found {actual_if!r}")
    return problems


def _command_problem(where: str, expected: str, run: str) -> str:
    return f"{where} must be `{expected}`, found `{_collapsed(run)}`"


def check(root: Path, spec: Spec = SPEC, enforce_files_filter: bool | None = None) -> list[str]:
    """Problems in `root`. Empty means every gate step runs its hook entry."""
    if enforce_files_filter is None:
        enforce_files_filter = spec is SPEC
    problems = _class_overlaps(spec)

    config_path = root / ".pre-commit-config.yaml"
    try:
        config_text = config_path.read_text(encoding="utf-8")
    except OSError as exc:
        return [*problems, f"{config_path}: {exc}"]
    config, error = _load_yaml(config_path)
    if config is None:
        return [*problems, error or f"{config_path}: unreadable"]
    hooks, hook_problems = _hook_rows(config)
    problems.extend(hook_problems)
    problems.extend(_entry_agreement(config_text, hooks))

    classified = set(spec.via_pre_commit) | set(spec.runner) | set(spec.local_only)
    missing = set(hooks) - classified
    if missing:
        problems.append(f"hooks with no CI classification: {sorted(missing)}")
    unknown = classified - set(hooks)
    if unknown:
        problems.append(f"classification names hooks that do not exist: {sorted(unknown)}")

    workflows: dict[str, dict | None] = {}

    def workflow(relative: str) -> dict | None:
        if relative in workflows:
            return workflows[relative]
        loaded, load_error = _load_yaml(root / relative)
        if loaded is None:
            problems.append(load_error or f"{relative}: unreadable")
        workflows[relative] = loaded
        return loaded

    def resolve(step: Step) -> tuple[dict | None, dict | None]:
        doc = workflow(step.file)
        if doc is None:
            return None, None
        job, item, find_error = _find_step(doc, step)
        if find_error:
            problems.append(find_error)
            return job, None
        return job, item

    entries: dict[str, str] = {}
    for hook_id, hook in hooks.items():
        plain, broken = _plain_entry(hook)
        if plain is not None and not broken:
            entries[hook_id] = plain

    for hook_id, run in spec.runner.items():
        entry = entries.get(hook_id)
        if hook_id not in hooks:
            continue
        if entry is None:
            problems.append(f"{hook_id}: CI runs this hook but it has no single-line entry")
            continue
        try:
            runner.argv_from_entry(entry)
        except ValueError as exc:
            problems.append(f"{hook_id}: {exc}")

    checked_jobs: set[tuple[str, str]] = set()
    for step in _named_steps(spec):
        key = (step.file, step.job)
        if key in checked_jobs:
            continue
        checked_jobs.add(key)
        doc = workflow(step.file)
        if doc is None:
            continue
        job = (doc.get("jobs") or {}).get(step.job)
        if isinstance(job, dict):
            problems.extend(_check_job_gate(step.file, step.job, job))

    for hook_id, step in spec.via_pre_commit.items():
        doc = workflow(step.file)
        job, item = resolve(step)
        if item is None or job is None or doc is None:
            continue
        where = _where(step)
        problems.extend(_check_disabled(doc, job, item, step, expect_env=None))
        problems.extend(_check_launch_directory(doc, job, item, step, directory=None))
        run, run_error = _run_text(step, item)
        if run_error or run is None:
            if run_error:
                problems.append(run_error)
            continue
        expected = pre_commit_command(hook_id)
        if _collapsed(run) != expected:
            problems.append(_command_problem(where, expected, run))

    for hook_id, executed in spec.runner.items():
        expected = runner_command(hook_id, pull_request_base=executed.pull_request_base)
        expect_env = BASE_ENV if executed.pull_request_base else None
        for step in executed.steps:
            doc = workflow(step.file)
            job, item = resolve(step)
            if item is None or job is None or doc is None:
                continue
            where = _where(step)
            problems.extend(_check_disabled(doc, job, item, step, expect_env=expect_env))
            problems.extend(_check_launch_directory(doc, job, item, step, directory=None))
            run, run_error = _run_text(step, item)
            if run_error or run is None:
                if run_error:
                    problems.append(run_error)
                continue
            if _collapsed(run) != expected:
                problems.append(_command_problem(where, expected, run))

    for step, pinned in spec.fixed.items():
        doc = workflow(step.file)
        job, item = resolve(step)
        if item is None or job is None or doc is None:
            continue
        where = _where(step)
        problems.extend(_check_disabled(doc, job, item, step, expect_env=None))
        problems.extend(
            _check_launch_directory(doc, job, item, step, directory=pinned.directory)
        )
        run, run_error = _run_text(step, item)
        if run_error or run is None:
            if run_error:
                problems.append(run_error)
            continue
        actual = _collapsed(run)
        if actual != pinned.command:
            problems.append(
                f"{where} is pinned to `{pinned.command}` ({pinned.reason}), found `{actual}`"
            )
            continue
        for hook_id, entry in entries.items():
            if actual == entry:
                problems.append(
                    f"{where} is the {hook_id} hook entry; run it through "
                    f"{RUNNER_ARGV} instead of pinning it ({pinned.reason})"
                )
        for hook_id, executed in spec.runner.items():
            command = runner_command(hook_id, pull_request_base=executed.pull_request_base)
            if actual == command:
                problems.append(
                    f"{where} runs the {hook_id} hook through the runner; "
                    "classify it there instead of pinning it"
                )

    known = set(_named_steps(spec))
    watched: dict[tuple[str, str], None] = {}
    for step in known:
        watched.setdefault((step.file, step.job))
    for relative, job_id in watched:
        doc = workflow(relative)
        if doc is None:
            continue
        job = (doc.get("jobs") or {}).get(job_id)
        if not isinstance(job, dict):
            continue
        for item in job.get("steps") or []:
            if not isinstance(item, dict) or "run" not in item:
                continue
            name = item.get("name")
            if not isinstance(name, str) or not name:
                problems.append(
                    f"{relative} job {job_id}: a run step has no name, so it cannot be classified"
                )
                continue
            step = Step(relative, job_id, name)
            if step in known:
                continue
            run = item.get("run")
            preview = _collapsed(run) if isinstance(run, str) else "(not a string)"
            extra = ""
            if isinstance(run, str):
                for hook_id, entry in entries.items():
                    if preview == entry:
                        extra = f" It is the {hook_id} hook entry; run it through {RUNNER_ARGV}."
                    command = runner_command(hook_id)
                    based = runner_command(hook_id, pull_request_base=True)
                    if preview in {command, based}:
                        extra = f" It runs the {hook_id} hook; add it to the runner table."
            problems.append(
                f"{relative} job {job_id} step {name!r} is not classified ({preview}).{extra} "
                "Add it to the runner, pre-commit, fixed, or exempt table "
                "in scripts/check-hook-commands.py."
            )

    if enforce_files_filter and "hook-commands" in hooks:
        pattern = hooks["hook-commands"].get("files")
        expected = expected_files_regex(spec)
        if not isinstance(pattern, str):
            problems.append("hook-commands has no files filter, so it would run on every push")
        elif pattern != expected:
            problems.append(
                "hook-commands files filter is not the path list this check reads.\n"
                f"  found:    {pattern}\n"
                f"  expected: {expected}"
            )

    return problems


def repo_root() -> Path:
    here = Path(__file__).resolve().parent
    for candidate in (here, here.parent, Path.cwd()):
        config = candidate / ".pre-commit-config.yaml"
        script = candidate / "scripts" / "check-hook-commands.py"
        if config.is_file() and script.is_file():
            return candidate
    return here


def _run_unit_tests(root: Path) -> int:
    test = root / "scripts" / "ci" / "test-check-hook-commands.py"
    completed = subprocess.run([sys.executable, str(test)], check=False)
    return completed.returncode


def main(argv: list[str] | None = None) -> int:
    args = list(sys.argv[1:] if argv is None else argv[1:])
    if args not in ([], ["--under-test"]):
        print("usage: python3 scripts/check-hook-commands.py [--under-test]", file=sys.stderr)
        return 2
    root = repo_root()
    # The hook is the CI run of these tests. The lint workflow does not invoke
    # them with the system interpreter, which has no PyYAML (finding 7).
    # `--under-test` is the only recursion guard. An inherited environment
    # variable must not skip the suite.
    if args != ["--under-test"]:
        status = _run_unit_tests(root)
        if status != 0:
            return status
    found = check(root)
    if found:
        for problem in found:
            print(problem, file=sys.stderr)
            print(file=sys.stderr)
        print(f"{len(found)} problem(s).", file=sys.stderr)
        return 1
    via_runner = [hook for hook, run in RUNNER.items() if not run.pull_request_base]
    with_base = [hook for hook, run in RUNNER.items() if run.pull_request_base]
    hooks = len(VIA_PRE_COMMIT) + len(RUNNER) + len(LOCAL_ONLY)
    print(
        "hook commands ok "
        f"({hooks} hooks: {len(VIA_PRE_COMMIT)} via pre-commit, "
        f"{len(via_runner)} via the runner, "
        f"{len(with_base)} via the runner with a base ref, "
        f"{len(LOCAL_ONLY)} local-only)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
