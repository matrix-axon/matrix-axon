#!/usr/bin/env bash
# Run every `#[ignore]`d test that needs Postgres, one fresh database per test
# target.
#
# Usage: DATABASE_ADMIN_URL=postgres://user:pass@host:port/postgres \
#            scripts/postgres-tests.sh [package ...]
#
# The URL's role must be able to CREATE DATABASE (and, for the
# `database_timeouts` tests, CREATE ROLE). Its path component is only used to
# connect; each test target gets its own database named `axon_pgtest_<n>`,
# which is dropped afterwards.
#
# Why a database per target and not one shared database: the oauth and token
# tests are marked "requires empty Postgres" and fail with 409s once an earlier
# binary has bootstrapped the instance. Why one target at a time with
# `--test-threads=1`: the tests share that database's singleton state.
#
# A request that matches no test target (a mistyped package name) is an error,
# not an empty pass.
#
# A failing target does not stop the run, so one nightly shows every broken
# target, not just the first. The script exits 1 if any failed.
set -euo pipefail

if [ -z "${DATABASE_ADMIN_URL:-}" ]; then
	echo "DATABASE_ADMIN_URL is not set" >&2
	exit 2
fi
case "$DATABASE_ADMIN_URL" in
*\?*)
	echo "DATABASE_ADMIN_URL must not carry a query string" >&2
	exit 2
	;;
esac
base=${DATABASE_ADMIN_URL%/*}

# The `DROP DATABASE IF EXISTS` below clears a leftover from a killed run. On a
# fresh runner nothing is left, and Postgres reports that as a NOTICE for every
# target. Raise the threshold so only warnings and errors reach the log.
export PGOPTIONS='-c client_min_messages=warning'

# "<package> <cargo flag...>" per lib and integration-test target of the
# requested packages (all workspace packages by default).
targets=$(cargo metadata --no-deps --format-version 1 | jq -r --args '
	($ARGS.positional) as $want
	| .packages[]
	| select(($want | length) == 0 or (.name as $n | $want | index($n)))
	| .name as $p
	| .targets[]
	| if (.kind | index("lib")) then "\($p) --lib"
	  elif (.kind | index("test")) then "\($p) --test \(.name)"
	  else empty end' "$@")

n=0
failed=()
while read -r pkg flag name; do
	[ -n "$pkg" ] || continue
	n=$((n + 1))
	db="axon_pgtest_$n"
	label="$pkg ${flag#--}${name:+ $name}"
	psql "$DATABASE_ADMIN_URL" -v ON_ERROR_STOP=1 -qc "DROP DATABASE IF EXISTS $db" -c "CREATE DATABASE $db"
	echo "::group::$label"
	# shellcheck disable=SC2086 # $flag/$name are cargo flags, split on purpose
	if DATABASE_URL="$base/$db" cargo test -p "$pkg" $flag $name -- --ignored --test-threads=1 </dev/null; then
		echo "ok: $label"
	else
		failed+=("$label")
	fi
	echo "::endgroup::"
	psql "$DATABASE_ADMIN_URL" -qc "DROP DATABASE IF EXISTS $db" || true
done <<<"$targets"

if [ "$n" -eq 0 ]; then
	echo "no test targets matched: $*" >&2
	exit 2
fi
if [ "${#failed[@]}" -gt 0 ]; then
	echo "FAILED targets (${#failed[@]} of $n):" >&2
	printf '  %s\n' "${failed[@]}" >&2
	exit 1
fi
echo "all $n targets passed"
