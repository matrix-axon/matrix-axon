#!/usr/bin/env bash
# Assert that an axon-server binary carries the web bundle it was built with
# (ADR 0116). Release pipelines run this on every server binary they ship.
#
# Usage: scripts/assert-web-embedded.sh <axon-server binary> <dist directory>
#
# The build already fails when AXON_WEB_DIST names a directory with no bundle.
# This is the check on the artifact itself: the embedded table's keys are the
# URL paths, so every content-hashed file in dist/assets must appear in the
# binary by name. A binary built without the feature, or against some other
# bundle, has none of them.
#
# On a tag it also refuses a bundle whose version.json is not a clean commit
# (`<hash>-dirty` or `unknown`): the web client compares that string to decide
# whether a newer build exists (ADR 0087), and a release must not ship one that
# names no commit.
set -euo pipefail

if [ "$#" -ne 2 ]; then
	echo "usage: $0 <axon-server binary> <dist directory>" >&2
	exit 2
fi
bin=$1
dist=$2

[ -f "$bin" ] || { echo "no such binary: $bin" >&2; exit 1; }
[ -f "$dist/index.html" ] || { echo "no bundle at $dist (no index.html)" >&2; exit 1; }

count=0
missing=0
for file in "$dist"/assets/*; do
	[ -f "$file" ] || continue
	count=$((count + 1))
	key="/assets/$(basename "$file")"
	if ! grep -aqF -- "$key" "$bin"; then
		echo "not in the binary: $key" >&2
		missing=$((missing + 1))
	fi
done

if [ "$count" -eq 0 ]; then
	echo "$dist/assets is empty; that is not a built web client" >&2
	exit 1
fi
if [ "$missing" -ne 0 ]; then
	echo "$bin does not carry the bundle at $dist ($missing of $count assets missing)." >&2
	echo "Build it with AXON_WEB_DIST=$dist and --features embed-web." >&2
	exit 1
fi

version=$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' "$dist/version.json")
if [ "${GITHUB_REF_TYPE:-}" = "tag" ]; then
	case $version in
	"" | unknown* | *-dirty)
		echo "the bundle's version is '$version', not a clean commit; refusing it on a tag" >&2
		exit 1
		;;
	esac
fi

echo "ok: $bin carries all $count assets of web build $version"
