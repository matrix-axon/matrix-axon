#!/usr/bin/env bash
#
# Fail if two ADRs claim the same number.
#
# ADRs are referenced by number everywhere -- commit messages, AGENTS.md, code
# comments -- so a collision makes every one of those references ambiguous, and
# it is only ever noticed long after both files have been cited. Renumbering
# after the fact means rewriting the citations too, so this is caught at push
# time and in CI rather than in review.
#
# Two authors branching from the same main and each taking "the next number" is
# the whole failure mode; it has happened here before (ADR 0046/0047).
#
# Portability: this walks the directory with a bash glob rather than `find`.
# `find -printf` is a GNU findutils extension that BSD find -- which is what
# macOS ships -- does not implement, and CONTRIBUTING.md lists macOS as a
# supported platform. A GNU-only pre-push hook would fail there with
# `find: -printf: unknown primary or operator`, which reads as a broken tool
# rather than as the duplicate-ADR diagnosis it is meant to produce, and CI
# (ubuntu-latest) would never surface it.
#
# Usage: scripts/check-adr-numbers.sh [<adr-dir>]
set -euo pipefail

# An unmatched glob must expand to nothing, not to the pattern itself.
shopt -s nullglob

# Both callers (pre-commit and lint-and-clippy.yml) run from the repo root, and
# no `cd` here keeps a relative <adr-dir> argument meaning what the caller typed.
adr_dir="${1:-docs/adr}"

if [ ! -d "$adr_dir" ]; then
  echo "check-adr-numbers: no such directory: $adr_dir" >&2
  exit 2
fi

status=0

names=()
for path in "$adr_dir"/*; do
  [ -f "$path" ] || continue
  names+=("${path##*/}")
done

if [ ${#names[@]} -eq 0 ]; then
  echo "check-adr-numbers: no files in $adr_dir" >&2
  exit 2
fi

# Every file must be NNNN-something. A file that is not gets flagged rather
# than sorted in under its own full name, which is what the inline CI version
# used to do -- an unnumbered file could never collide, so it was invisible.
unnumbered=()
numbers=()
for name in "${names[@]}"; do
  if [[ $name =~ ^([0-9]{4})-.+\.md$ ]]; then
    numbers+=("${BASH_REMATCH[1]}")
  else
    unnumbered+=("$name")
  fi
done

if [ ${#unnumbered[@]} -gt 0 ]; then
  echo "ADR files that are not NNNN-title.md:" >&2
  printf '  %s\n' "${unnumbered[@]}" >&2
  status=1
fi

# Guarded because macOS ships bash 3.2, where expanding an empty array as
# "${numbers[@]}" under `set -u` is an unbound-variable error (fixed upstream
# in 4.4). Reachable whenever every file in the directory is unnumbered.
duplicates=""
if [ ${#numbers[@]} -gt 0 ]; then
  duplicates=$(printf '%s\n' "${numbers[@]}" | sort | uniq -d)
fi

if [ -n "$duplicates" ]; then
  echo "Duplicate ADR numbers detected; renumber before submitting:" >&2
  while read -r number; do
    [ -n "$number" ] || continue
    echo "  $number:" >&2
    for path in "$adr_dir/$number"-*.md; do
      echo "    $path" >&2
    done
  done <<<"$duplicates"
  status=1
fi

if [ "$status" -ne 0 ]; then
  exit "$status"
fi

echo "ADR numbers clear (${#names[@]} files)."
