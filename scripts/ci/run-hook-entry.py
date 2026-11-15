#!/usr/bin/env python3
"""Run one pre-push hook entry from the repo root, with no shell.

CI steps call this instead of copying the command out of
`.pre-commit-config.yaml`. The entry stays the one command. A step that
retypes it, or turns the check off, fails `scripts/check-hook-commands.py`.

This file is stdlib only. The migrations and OpenAPI jobs do not install
PyYAML, and the runner must still start there.

Usage:
    python3 scripts/ci/run-hook-entry.py HOOK_ID [--pull-request-base]

`--pull-request-base` reads `BASE_REF` and `BASE_SHA` and appends the base
commit as the entry's last argument. It prefers `origin/$BASE_REF` when that
commit exists, and otherwise uses `BASE_SHA`. On a pull_request event the job
runs in the base repo, so `origin` is the canonical repo. `base.sha` is the
fallback for the moment the PR was opened, which goes stale as main advances.
"""

from __future__ import annotations

import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

HOOK_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")
# Shell syntax the runner refuses to interpret. An entry is an argv, the same
# way pre-commit runs it. `=` belongs to `env VAR=value` and is allowed.
_SHELL = set(";|&<>`$()\\")
_ID_LINE = re.compile(r"^(\s*)- id:\s*(\S+)\s*$")
_ENTRY_LINE = re.compile(r"^(\s*)entry:\s*(.*)$")
_FOLDED = {">", "|", ">-", "|-", ">+", "|+", ""}


def repo_root() -> Path:
    """The checkout that contains this script and the hook config."""
    here = Path(__file__).resolve().parent
    for candidate in (here, *here.parents):
        config = candidate / ".pre-commit-config.yaml"
        script = candidate / "scripts" / "ci" / "run-hook-entry.py"
        if config.is_file() and script.is_file():
            return candidate
    raise SystemExit("run-hook-entry: could not find the repo root from " + str(here))


def _strip_yaml_comment(value: str) -> str:
    in_single = False
    in_double = False
    for index, char in enumerate(value):
        if char == "'" and not in_double:
            in_single = not in_single
        elif char == '"' and not in_single:
            in_double = not in_double
        elif char == "#" and not in_single and not in_double:
            return value[:index].rstrip()
    return value.strip()


def _unquote(value: str) -> str | None:
    """A plain YAML scalar, or None when it is a block or an escape sequence."""
    value = _strip_yaml_comment(value).strip()
    if value in _FOLDED:
        return None
    if len(value) >= 2 and value[0] == value[-1] and value[0] in {'"', "'"}:
        inner = value[1:-1]
        if value[0] == "'" and "''" in value:
            return None
        if value[0] == '"' and "\\" in inner:
            return None
        return inner
    return value


def scan_entries(text: str) -> dict[str, str | None]:
    """Map each hook id to its single-line `entry`, without PyYAML.

    None means the id was seen and its entry is not a single plain line.
    An id with no entry line is absent. The checker compares this to PyYAML.
    """
    found: dict[str, str | None] = {}
    current_id: str | None = None
    current_indent = 0
    for raw in text.splitlines():
        id_match = _ID_LINE.match(raw)
        if id_match:
            current_indent = len(id_match.group(1))
            current_id = id_match.group(2)
            continue
        if current_id is None:
            continue
        if raw.strip() and not raw.lstrip().startswith("#"):
            indent = len(raw) - len(raw.lstrip(" "))
            if indent <= current_indent:
                current_id = None
                continue
        entry_match = _ENTRY_LINE.match(raw)
        if entry_match is None or current_id in found:
            continue
        if len(entry_match.group(1)) <= current_indent:
            continue
        found[current_id] = _unquote(entry_match.group(2))
    return found


def argv_from_entry(entry: str) -> list[str]:
    """Split an entry into an argv. Refuse anything a shell would interpret."""
    if "\n" in entry or any(char in _SHELL for char in entry):
        raise ValueError(f"hook entry is not a single command: {entry!r}")
    argv = shlex.split(entry)
    if not argv:
        raise ValueError("hook entry is empty")
    return argv


def resolve_pull_request_base(base_ref: str, base_sha: str, rev_parse) -> str:
    """Prefer origin/<base_ref> when that commit exists, else base_sha.

    `rev_parse(ref)` is true when `git rev-parse --verify ref^{commit}` would
    succeed. The returned value is the ref the hook script receives, without
    the `^{commit}` suffix.
    """
    if base_ref and rev_parse(f"origin/{base_ref}"):
        return f"origin/{base_ref}"
    return base_sha


def _git_commit_exists(ref: str) -> bool:
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )
    return result.returncode == 0


def _git_short(ref: str) -> str:
    result = subprocess.run(
        ["git", "rev-parse", "--short", ref],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        return "?"
    return result.stdout.strip()


def _parse_args(argv: list[str]) -> tuple[str, bool]:
    if len(argv) == 2 and HOOK_ID.fullmatch(argv[1]):
        return argv[1], False
    if len(argv) == 3 and HOOK_ID.fullmatch(argv[1]) and argv[2] == "--pull-request-base":
        return argv[1], True
    raise SystemExit(
        "usage: python3 scripts/ci/run-hook-entry.py HOOK_ID [--pull-request-base]"
    )


def main(argv: list[str] | None = None) -> int:
    hook_id, with_base = _parse_args(sys.argv if argv is None else argv)
    root = repo_root()
    os.chdir(root)
    scanned = scan_entries((root / ".pre-commit-config.yaml").read_text(encoding="utf-8"))
    if hook_id not in scanned:
        print(f"run-hook-entry: no hook id {hook_id!r} in .pre-commit-config.yaml", file=sys.stderr)
        return 1
    entry = scanned[hook_id]
    if entry is None:
        print(f"run-hook-entry: {hook_id} entry is not a single plain line", file=sys.stderr)
        return 1
    try:
        command = argv_from_entry(entry)
    except ValueError as exc:
        print(f"run-hook-entry: {exc}", file=sys.stderr)
        return 1
    if with_base:
        base = resolve_pull_request_base(
            os.environ.get("BASE_REF", ""),
            os.environ.get("BASE_SHA", ""),
            _git_commit_exists,
        )
        shown = base if base else "HEAD"
        print(f"base: {base} ({_git_short(shown)})", flush=True)
        command.append(base)
    try:
        os.execvp(command[0], command)
    except OSError as exc:
        print(f"run-hook-entry: could not run {command[0]}: {exc}", file=sys.stderr)
        return 127
    return 127


if __name__ == "__main__":
    raise SystemExit(main())
