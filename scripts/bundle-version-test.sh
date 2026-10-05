#!/usr/bin/env bash
# Check scripts/bundle-version.sh against a scratch package.json.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)
script=$root/scripts/bundle-version.sh
version=$("$root/scripts/release-version.sh" print)

scratch=$(mktemp -d "${TMPDIR:-/tmp}/bundle-version-test.XXXXXX")
trap 'rm -rf "$scratch"' EXIT
export BUNDLE_PACKAGE_JSON=$scratch/package.json

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

printf '{\n  "name": "x",\n  "version": "0.0.0",\n  "scripts": {\n    "a": "b"\n  }\n}\n' >"$BUNDLE_PACKAGE_JSON"
fail "$script" check

pass "$script" sync
pass "$script" check
# Only the version moves; the rest of the file is as it was.
if [ "$(node -e 'const p=require(process.argv[1]);process.stdout.write(p.name+p.scripts.a+p.version)' "$BUNDLE_PACKAGE_JSON")" != "xb$version" ]; then
	echo "sync changed something besides the version" >&2
	exit 1
fi

# Idempotent: a second sync leaves the bytes alone.
before=$(cksum <"$BUNDLE_PACKAGE_JSON")
pass "$script" sync
[ "$before" = "$(cksum <"$BUNDLE_PACKAGE_JSON")" ] || { echo "sync is not idempotent" >&2; exit 1; }

fail "$script"
fail "$script" bogus

echo "bundle version test ok ($version)"
