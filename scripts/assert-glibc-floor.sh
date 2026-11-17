#!/usr/bin/env bash
# Assert that Linux binaries need no glibc symbol version newer than a ceiling.
# A release job runs this on the Linux binaries it builds, so a build that
# lands on a newer runner image (ubuntu-latest) fails here, not on a user's
# Debian 12 box with `GLIBC_2.39 not found`. It checks only the files its
# caller passes; each job names what it covers.
#
# Usage: scripts/assert-glibc-floor.sh <max version, e.g. 2.35> <binary>...
#
# Reads the versioned undefined symbols (`objdump -T`), the same list the
# dynamic loader checks, and fails on any GLIBC_x.y above the ceiling.
set -euo pipefail

if [ "$#" -lt 2 ]; then
	echo "usage: $0 <max glibc version> <binary>..." >&2
	exit 2
fi
max=$1
shift

fail=0
for bin in "$@"; do
	[ -f "$bin" ] || { echo "no such binary: $bin" >&2; exit 1; }
	# `|| true`: under pipefail a non-ELF file (objdump fails) or a static/musl
	# binary (grep finds nothing) would abort here, before the message below.
	syms=$({ objdump -T "$bin" 2>/dev/null | grep -o 'GLIBC_[0-9][0-9.]*' | sort -u; } || true)
	[ -n "$syms" ] || { echo "$bin: no GLIBC symbol versions found; wrong file?" >&2; exit 1; }
	highest=$(printf '%s\n' "$syms" | sed 's/^GLIBC_//' | sort -V | tail -n 1)
	too_new=$(printf '%s\n%s\n' "$max" "$highest" | sort -V | tail -n 1)
	if [ "$too_new" != "$max" ]; then
		echo "::error::$bin needs GLIBC_$highest, newer than the GLIBC_$max floor" >&2
		fail=1
	else
		echo "$bin: highest GLIBC_$highest (floor $max)"
	fi
done
exit "$fail"
