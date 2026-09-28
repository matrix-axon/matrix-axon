#!/usr/bin/env bash
# Render packaging/homebrew/axon.rb.tmpl into a Homebrew cask.
# The checksum is the sha256 of the universal disk image, lowercase hex.
#
# Usage:
#   render-cask.sh --tag v0.1.2 --sha HEX --out PATH
set -euo pipefail

usage() {
	echo "usage: $0 --tag TAG --sha HEX --out PATH" >&2
}

tag=
sha=
out=

while [ $# -gt 0 ]; do
	case "$1" in
	--tag)
		tag=${2:?missing value for $1}
		shift 2
		;;
	--sha)
		sha=${2:?missing value for $1}
		shift 2
		;;
	--out)
		out=${2:?missing value for $1}
		shift 2
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		usage
		exit 2
		;;
	esac
done

if [ -z "$tag" ] || [ -z "$sha" ] || [ -z "$out" ]; then
	usage
	exit 2
fi

# Same stable-tag rule as render-formula.sh.
if ! printf '%s' "$tag" | grep -Eq '^v[0-9]+(\.[0-9]+)+$'; then
	echo "refusing tag: $tag (only stable vX.Y.Z tags are published)" >&2
	exit 1
fi

sha=$(printf '%s' "$sha" | tr 'A-F' 'a-f')
if ! printf '%s' "$sha" | grep -Eq '^[0-9a-f]{64}$'; then
	echo "sha256 must be 64 hex characters" >&2
	exit 1
fi

version=${tag#v}
root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
template=$root/packaging/homebrew/axon.rb.tmpl

for token in VERSION TAG SHA_DMG; do
	if ! grep -q "@@${token}@@" "$template"; then
		echo "template is missing @@${token}@@" >&2
		exit 1
	fi
done

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

sed \
	-e "s|@@VERSION@@|${version}|g" \
	-e "s|@@TAG@@|${tag}|g" \
	-e "s|@@SHA_DMG@@|${sha}|g" \
	"$template" >"$tmp"

if grep -q '@@' "$tmp"; then
	echo "rendered cask still contains a placeholder" >&2
	exit 1
fi

mkdir -p "$(dirname "$out")"
mv "$tmp" "$out"
trap - EXIT
