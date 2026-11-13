#!/usr/bin/env python3
"""Say whether a pull request changes anything a pre-push hook would select.

    changed-by-hook.py <hook-id> [--also REGEX]... [--base REF] [--config FILE]

Prints `changed=true` or `changed=false`, and appends the same line to
$GITHUB_OUTPUT when that is set, so a workflow step can gate on
`steps.<id>.outputs.changed`.

## Why this exists

`lint-and-test` and the smoke `stub` are *required* checks, so they cannot be
skipped with a workflow-level `paths:` filter: a skipped workflow never reports
a status, the required check stays "pending", and the PR can never merge. The
job has to run and report success. What can be skipped is the expensive steps
inside it, and this script is how a step decides.

## One definition of "Rust inputs"

The decision reuses the `files`/`exclude` regexes of a hook in
`.pre-commit-config.yaml` (YAML anchors are resolved by the loader, so
`*rust_files` and the hook that carries it agree), rather than a second copy in
a workflow. The local pre-push gate and CI therefore skip the same pushes, and
`scripts/check-hook-filters.py` already pins what that filter selects. Selection
mirrors pre-commit's: `re.search` for `files`, and for `exclude`.

`--also` adds extra include regexes for inputs the hook does not know about,
such as the smoke lane's own scripts.

## Fail-safe

Skipping a gate by mistake merges unreviewed code; running it by mistake costs
minutes. So every doubt resolves to `changed=true`:

- any event other than a pull request (a push to main and a manual run always
  get the full gate -- the caller passes `--base` only on pull requests);
- no `--base`, or a base that git cannot resolve (a shallow clone without it);
- a failing `git diff`;
- a hook id that is not in the config, or a config that does not parse.

Usage in a workflow: check out with `fetch-depth: 2` and, on a pull request,
pass `--base HEAD^1`. The default checkout of a pull request is the merge commit
of the PR head into the base tip, so its first parent is the base and
`HEAD^1..HEAD` is exactly the PR's own change.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys


def emit(changed: bool, why: str, *, undecided: bool = False) -> int:
    line = f"changed={'true' if changed else 'false'}"
    print(line)
    print(f"changed-by-hook: {why}", file=sys.stderr)
    if undecided and os.environ.get("GITHUB_ACTIONS") == "true":
        # The fail-safe is correct but invisible: the gate runs in full and the
        # saving silently disappears. An annotation on the run makes a broken
        # dependency or a bad ref show up in the PR instead of in a bill.
        print(f"::warning title=changed-by-hook::cannot decide ({why}); ran the full gate")
    out = os.environ.get("GITHUB_OUTPUT")
    if out:
        with open(out, "a", encoding="utf-8") as fh:
            fh.write(line + "\n")
    return 0


def hook_filters(config: str, hook_id: str) -> tuple[str, str]:
    """(files, exclude) for a hook, with pre-commit's defaults."""
    import yaml

    with open(config, encoding="utf-8") as fh:
        doc = yaml.safe_load(fh)
    for repo in doc.get("repos", []):
        for hook in repo.get("hooks", []):
            if hook.get("id") == hook_id:
                return hook.get("files", ""), hook.get("exclude", "^$")
    raise KeyError(f"no hook with id {hook_id!r} in {config}")


def changed_files(base: str) -> list[str]:
    proc = subprocess.run(
        ["git", "diff", "--name-only", "-z", base, "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    )
    return [p for p in proc.stdout.split("\0") if p]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("hook_id")
    ap.add_argument("--also", action="append", default=[], metavar="REGEX")
    ap.add_argument("--base", default="")
    ap.add_argument("--config", default=".pre-commit-config.yaml")
    args = ap.parse_args()

    if not args.base:
        return emit(True, "no --base (not a pull request): running everything")

    try:
        files, exclude = hook_filters(args.config, args.hook_id)
        paths = changed_files(args.base)
        includes = [files, *args.also]
        # Matching is inside the try: a malformed regex is as much a doubt as a
        # missing ref, and must run everything rather than fail the step.
        for path in paths:
            if any(re.search(rx, path) for rx in includes) and not re.search(exclude, path):
                return emit(True, f"{path} is an input")
    except Exception as exc:  # noqa: BLE001 -- every failure must fail safe
        return emit(True, f"cannot decide ({exc}): running everything", undecided=True)
    return emit(False, f"none of {len(paths)} changed file(s) is an input")


if __name__ == "__main__":
    sys.exit(main())
