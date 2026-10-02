#!/usr/bin/env bash
#
# Fails if any packaging script does not parse under the bash that a macOS runner
# runs it with.
#
#   check-bash-syntax.sh [file...]
#
# With no arguments, the scripts the store-build workflow runs. A runner's
# `#!/usr/bin/env bash` finds macOS's /bin/bash, which is 3.2 and rejects syntax that
# Homebrew's bash 5 accepts. One such construct, a `case` inside `$( )`, sat in
# package-macos-mas.sh until the first CI run found it: every earlier check, and every
# run on a developer's Mac, used bash 5. Parsing is cheap, so this runs at the start of
# each job, before anything that takes minutes.
#
# Parse only (`-n`): it finds syntax errors, not constructs bash 3.2 lacks at run time,
# such as associative arrays.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
# /bin/bash is what a runner uses; elsewhere fall back to whatever bash is here.
sh=/bin/bash
[ -x "$sh" ] || sh=bash

if [ $# -eq 0 ]; then
  set -- "$root/scripts/package-ios.sh" "$root/scripts/package-macos-mas.sh" \
    "$root"/scripts/lib/*.sh "$root"/scripts/ci/*.sh
fi

echo "parsing $# scripts with $("$sh" --version | head -1)"
failed=0
for f in "$@"; do
  if ! out=$("$sh" -n "$f" 2>&1); then
    echo "error: $f does not parse under this bash:" >&2
    printf '%s\n' "$out" | sed 's/^/       /' >&2
    failed=1
  fi
done
if [ "$failed" -ne 0 ]; then
  echo "error: macOS's /bin/bash is 3.2; rewrite the construct above without syntax it lacks (a \`case\` inside \$( ) is the usual one)." >&2
  exit 1
fi
echo "all parse"
