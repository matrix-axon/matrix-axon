#!/usr/bin/env python3
"""Assert that .pre-commit-config.yaml says what the docs say it says.

Two checks, both about the config drifting from something that claims to
describe it.

## 1. The `files` regexes select what they claim

A hook whose filter matches nothing is worse than no hook: it reports success on
every push. That is what #180 was -- `web-lint`, `web-test` and `web-build`
filtered on `^(...|clients/web/|...)$`, and `clients/web/` inside a `$`-anchored
group matches only the literal string `clients/web/`, never a real path like
`clients/web/src/app.tsx`. Those three passed for months without running once.

Nothing about that is visible in review, and `pre-commit run --all-files` hides
it as well, because that path bypasses the filters entirely. So the filters get
a test.

Selection here mirrors pre-commit's own: `re.search(files, filename)` for the
include and the same for `exclude` (`pre_commit.commands.run.classify_files`).

Scope is the `repo: local` hooks. A remote repo's hooks carry their own filters
upstream, combined with a `types:` selector that a regex-only model cannot
represent -- guessing at those would assert something this script does not
actually check.

## 2. Hook ids named in prose still exist

AGENTS.md and CONTRIBUTING.md deliberately do *not* enumerate the gate -- they
point at `.pre-commit-config.yaml`, because a prose copy of the list is a second
list to keep in sync, which is the failure ADR 0092 exists to end. (An earlier
version of this script instead asserted that both docs listed every hook in
order. That worked, but it was enforcing agreement between three copies rather
than removing two of them.)

What the docs do name is individual ids, in `SKIP=<id>` examples. Those are the
one place prose can go stale invisibly: rename `cargo-test` and every
`SKIP=cargo-test` in the tree silently becomes a no-op that still looks like it
skips something. So every `SKIP=<id>` in the docs and in the config's own header
must name a hook that exists.

Usage: scripts/check-hook-filters.py [<config>]
"""

from __future__ import annotations

import re
import sys

import yaml

# (path, exactly the local hook ids that must select it).
#
# Add a row when you add a hook or widen a filter. A row that goes stale fails
# loudly, which is the point -- the failure mode being tested is silence.
CASES: list[tuple[str, set[str]]] = [
    # The #180 regression: a real web source file must reach every web hook.
    ("clients/web/src/app.tsx", {"prettier-web", "web-lint", "web-test", "web-build"}),
    ("clients/web/src/stores/invites.ts", {"prettier-web", "web-lint", "web-test", "web-build"}),
    ("clients/web/src/app.css", {"prettier-web", "web-lint", "web-test", "web-build"}),
    # A manifest change additionally reinstalls, re-checks peers, and re-checks
    # the generated API client.
    (
        "clients/web/package.json",
        {
            "prettier-web",
            "web-install",
            "web-peers",
            "web-api-schema",
            "web-lint",
            "web-test",
            "web-build",
        },
    ),
    (
        "clients/web/pnpm-workspace.yaml",
        {"prettier-web", "web-install", "web-peers", "web-lint", "web-test", "web-build"},
    ),
    # Generated and vendored files are excluded from prettier but still build.
    ("clients/web/src/api/schema.d.ts", {"web-api-schema", "web-lint", "web-test", "web-build"}),
    ("clients/web/dist/index.html", {"web-lint", "web-test", "web-build"}),
    # The schema contract is consumed by the build, not by lint or unit tests.
    ("openapi/openapi.json", {"web-api-schema", "web-build"}),
    # Rust sources reach every rust hook...
    ("crates/axon-store/src/lib.rs", {"rustfmt", "rust-clippy", "cargo-test"}),
    ("clients/tui/src/app/rooms.rs", {"rustfmt", "rust-clippy", "cargo-test"}),
    ("crates/axon-api/Cargo.toml", {"rustfmt", "rust-clippy", "cargo-test"}),
    ("Cargo.lock", {"rustfmt", "rust-clippy", "cargo-test"}),
    # ...including the fixtures axon-sync `include_str!`s, which the old filter
    # missed entirely.
    ("tests/fixtures/decryption/utd_encrypted_event.json", {"rustfmt", "rust-clippy", "cargo-test"}),
    # ...and docs and binary fixtures under the same trees do not. A full
    # `cargo clippy --all-features --all-targets` is minutes; a README cannot
    # change a compilation unit.
    ("crates/axon-store/README.md", set()),
    ("clients/tui/README.md", set()),
    ("smoke/local-stack/corpus/media/avatar.png", set()),
    # Migrations are BOTH an immutability-checked input and a rust build input:
    # migrations.rs embeds the directory with `include_dir!`, so a .sql edit
    # changes the compiled checksums (ADR 0092). An earlier draft of this file
    # asserted `{"migrations-immutable"}` alone, which is exactly the kind of
    # near-miss these cases exist to pin down.
    (
        "crates/axon-store/migrations/20260101000000_init.sql",
        {"rustfmt", "rust-clippy", "cargo-test"},
    ),
    # .cargo/config.toml can carry rustflags and target overrides, so it is a
    # rust build input even though it holds only aliases today.
    (".cargo/config.toml", {"rustfmt", "rust-clippy", "cargo-test"}),
    ("docs/adr/0092-unified-pre-push-gate.md", {"adr-numbers"}),
    ("scripts/check-hook-filters.py", {"hook-filters"}),
    # Editing the config re-runs everything it could invalidate, including this.
    (
        ".pre-commit-config.yaml",
        {
            "prettier-web",
            "web-lint",
            "web-test",
            "web-build",
            "rustfmt",
            "rust-clippy",
            "cargo-test",
            "hook-filters",
        },
    ),
    # Watched so a `SKIP=<id>` example edited into any of them is checked
    # (see SKIP_EXAMPLE and PROSE_NAMING_HOOKS below).
    ("AGENTS.md", {"hook-filters"}),
    ("CONTRIBUTING.md", {"hook-filters"}),
    ("scripts/setup-hooks.sh", {"hook-filters"}),
    # Nothing local claims a workflow file or the root README; actionlint does
    # claim the former, and it is out of scope here (see the module docstring).
    (".github/workflows/smoke.yml", set()),
    ("README.md", set()),
]

# Hooks that intentionally have no `files` filter, i.e. run on every push.
# Keep this list short and explicit; an unfiltered hook is a cost everyone pays.
#   smoke-gate           returns immediately unless RUN_SMOKE names a lane
#   migrations-immutable sub-second, and a filter would skip the edit-then-
#                        revert-in-one-push case the script exists to catch
ALWAYS_RUN = {"smoke-gate", "migrations-immutable"}

# Files whose prose names hook ids. They are not required to mention any, and
# they must not enumerate the gate -- the config is the list. This only asks
# that whatever they *do* name exists.
PROSE_NAMING_HOOKS = (
    "AGENTS.md",
    "CONTRIBUTING.md",
    ".pre-commit-config.yaml",
    "scripts/setup-hooks.sh",  # prints the same escape hatch after installing
    # The ADR carries three SKIP=cargo-test examples of its own. Omitting it
    # meant a rename could update every watched file, pass green, and leave the
    # one document that explains why that cannot happen holding stale ids.
    "docs/adr/0092-unified-pre-push-gate.md",
)

# `SKIP=cargo-test` matches; the `SKIP=<hook-id>` placeholder does not, because
# `<` is outside the character class.
SKIP_EXAMPLE = re.compile(r"SKIP=([a-z][a-z0-9-]*)")

# Hooks carrying `fail_fast: true`, and the expensive hooks each one exists to
# short-circuit. `fail_fast` only stops hooks that come *after* it, so the
# guarantee is really about file order -- reorder the file and the gate keeps
# passing while silently protecting nothing.
FAIL_FAST_GATES = {
    "rustfmt": ("rust-clippy", "cargo-test"),
    "web-lint": ("web-test", "web-build"),
}


def skip_examples(path: str) -> set[str] | None:
    """Hook ids named in `SKIP=<id>` examples, or None if the file is missing."""
    try:
        with open(path) as fh:
            return set(SKIP_EXAMPLE.findall(fh.read()))
    except FileNotFoundError:
        return None


def selected(hooks: dict[str, dict], path: str) -> set[str]:
    # Deliberately a copy of pre-commit's include/exclude logic rather than an
    # import of it. `pre_commit.commands.run` is a private module with no
    # stability promise, and coupling this check to it would mean a pre-commit
    # bump could break the guard that protects the gate. The contract being
    # mirrored is one documented line -- `re.search` on `files`, then on
    # `exclude` -- so a copy is cheaper to own than the dependency. The
    # `types:` guard in main() is what keeps that copy honest: a hook using a
    # selector this does not model is rejected rather than silently mismodeled.
    out = set()
    for hook_id, hook in hooks.items():
        include = hook.get("files")
        if not include or not re.search(include, path):
            continue
        exclude = hook.get("exclude")
        if exclude and re.search(exclude, path):
            continue
        out.add(hook_id)
    return out


def main() -> int:
    config = sys.argv[1] if len(sys.argv) > 1 else ".pre-commit-config.yaml"
    with open(config) as fh:
        parsed = yaml.safe_load(fh)

    hooks = {
        h["id"]: h
        for repo in parsed["repos"]
        if repo.get("repo") == "local"
        for h in repo["hooks"]
    }
    if not hooks:
        print(f"{config}: no local hooks found -- did the file move?", file=sys.stderr)
        return 2

    problems = []

    # `types:`/`types_or:` would make the regex model above incomplete, so a
    # local hook may not use one without teaching this script about it first.
    typed = {i for i, h in hooks.items() if {"types", "types_or", "exclude_types"} & h.keys()}
    if typed:
        problems.append(
            f"local hooks using a types selector this script does not model: {sorted(typed)}"
        )

    unfiltered = {i for i, h in hooks.items() if not h.get("files")}
    if unfiltered - ALWAYS_RUN:
        problems.append(
            f"local hooks with no files filter: {sorted(unfiltered - ALWAYS_RUN)} "
            f"(only {sorted(ALWAYS_RUN)} may run unfiltered)"
        )

    covered = {hook_id for _, expected in CASES for hook_id in expected}
    untested = set(hooks) - covered - ALWAYS_RUN
    if untested:
        problems.append(f"local hooks with no case in CASES: {sorted(untested)}")

    unknown = covered - set(hooks)
    if unknown:
        problems.append(f"CASES names hooks that do not exist: {sorted(unknown)}")

    # `fail_fast` is a claim about order, so check the order.
    ordered = [h["id"] for repo in parsed["repos"] for h in repo["hooks"]]
    declared = {
        h["id"] for repo in parsed["repos"] for h in repo["hooks"] if h.get("fail_fast")
    }
    if declared != set(FAIL_FAST_GATES):
        problems.append(
            f"hooks with fail_fast: {sorted(declared)} but FAIL_FAST_GATES "
            f"describes {sorted(FAIL_FAST_GATES)} -- keep them in step"
        )
    for gate, gated in FAIL_FAST_GATES.items():
        if gate not in ordered:
            problems.append(f"FAIL_FAST_GATES names a hook that does not exist: {gate}")
            continue
        for other in gated:
            if other not in ordered:
                problems.append(f"{gate} is declared to gate {other}, which does not exist")
            elif ordered.index(other) < ordered.index(gate):
                problems.append(
                    f"{gate} has fail_fast but runs after {other}, so it gates nothing"
                )

    # A `SKIP=<id>` example naming a hook that no longer exists still reads like
    # a working incantation, and silently skips nothing. Remote hooks count.
    all_ids = set(ordered)
    for doc in PROSE_NAMING_HOOKS:
        named = skip_examples(doc)
        if named is None:
            problems.append(f"{doc}: not found -- SKIP= examples unchecked")
            continue
        stale = named - all_ids
        if stale:
            problems.append(f"{doc}: SKIP= names hooks that do not exist: {sorted(stale)}")

    for problem in problems:
        print(problem, file=sys.stderr)

    failures = 0
    for path, expected in CASES:
        got = selected(hooks, path)
        if got != expected:
            failures += 1
            print(f"FAIL {path}", file=sys.stderr)
            print(f"     selected {sorted(got)}", file=sys.stderr)
            print(f"     expected {sorted(expected)}", file=sys.stderr)

    if failures or problems:
        print(
            f"\n{failures} path(s) mis-selected, {len(problems)} structural problem(s).",
            file=sys.stderr,
        )
        return 1

    print(f"hook filters ok ({len(CASES)} paths, {len(hooks)} local hooks)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
