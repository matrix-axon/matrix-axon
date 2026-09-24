#!/usr/bin/env bash
# Check scripts/release-version.sh against this checkout's own version.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)
script=$root/scripts/release-version.sh

version=$("$script" print)
if ! printf '%s' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+'; then
	echo "print returned something that is not a version: $version" >&2
	exit 1
fi

pass() {
	if ! "$@" >/dev/null; then
		echo "expected success: $*" >&2
		exit 1
	fi
}

fail() {
	if "$@" >/dev/null 2>&1; then
		echo "expected failure: $*" >&2
		exit 1
	fi
}

pass "$script" check "v$version"
fail "$script" check v0.0.0
fail "$script" check "v$version.1"
fail "$script" check "v${version}-rc1"
# A tag without the v is not a release tag, and prereleases are not versions.
pass "$script" check "$version"
pass "$script" check beta-1
pass "$script" check alpha-2026-09-24

# The same, read from the GitHub Actions environment.
pass env GITHUB_REF_TYPE=tag GITHUB_REF_NAME="v$version" "$script" check
fail env GITHUB_REF_TYPE=tag GITHUB_REF_NAME=v0.0.0 "$script" check
pass env GITHUB_REF_TYPE=branch GITHUB_REF_NAME=v0.0.0 "$script" check
pass env -u GITHUB_REF_TYPE -u GITHUB_REF_NAME "$script" check

fail "$script"
fail "$script" bogus

echo "release version test ok ($version)"
