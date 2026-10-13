#!/usr/bin/env bash
# Run a `cargo test` command and fail unless at least <min> tests passed.
#
# Usage: scripts/expect-tests.sh <min> cargo test ...
#
# `cargo test <filter>` exits 0 when the filter matches nothing, so a job that
# exists to run three named tests stays green after one is renamed, having run
# none. This turns that into a failure.
set -euo pipefail

if [ "$#" -lt 2 ]; then
	echo "usage: $0 <minimum passed> <command...>" >&2
	exit 2
fi
min=$1
shift

log=$(mktemp)
trap 'rm -f "$log"' EXIT

"$@" 2>&1 | tee "$log"

# One "test result: ok. N passed; ..." line per test binary.
passed=$(awk '/^test result: ok\./ { n += $4 } END { print n + 0 }' "$log")
if [ "$passed" -lt "$min" ]; then
	echo "FAIL: $passed tests passed, expected at least $min: $*" >&2
	echo "A filter that matches nothing is not a pass. Was a test renamed?" >&2
	exit 1
fi
echo "ok: $passed tests passed (at least $min expected)"
