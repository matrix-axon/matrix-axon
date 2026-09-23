#!/usr/bin/env bash
# Render packaging/homebrew/axon-server.rb.tmpl into a Homebrew formula.
# Checksums are the sha256 of the GitHub Release zips, lowercase hex.
#
# Usage:
#   render-formula.sh --tag v0.1.0 \
#     --sha-macos-silicon HEX --sha-macos-intel HEX --sha-linux-x86_64 HEX \
#     --out PATH
set -euo pipefail

usage() {
	echo "usage: $0 --tag TAG --sha-macos-silicon HEX --sha-macos-intel HEX --sha-linux-x86_64 HEX --out PATH" >&2
}

tag=
sha_silicon=
sha_intel=
sha_linux=
out=

while [ $# -gt 0 ]; do
	case "$1" in
	--tag)
		tag=${2:-}
		shift 2
		;;
	--sha-macos-silicon)
		sha_silicon=${2:-}
		shift 2
		;;
	--sha-macos-intel)
		sha_intel=${2:-}
		shift 2
		;;
	--sha-linux-x86_64)
		sha_linux=${2:-}
		shift 2
		;;
	--out)
		out=${2:-}
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

if [ -z "$tag" ] || [ -z "$sha_silicon" ] || [ -z "$sha_intel" ] || [ -z "$sha_linux" ] || [ -z "$out" ]; then
	usage
	exit 2
fi

# Same tag shapes as cross-build.yml / package.yml, and nothing a formula
# string or a release URL could treat as syntax.
if ! printf '%s' "$tag" | grep -Eq '^(v[0-9][0-9A-Za-z._-]*|beta-[0-9A-Za-z._-]+|alpha-[0-9A-Za-z._-]+)$'; then
	echo "refusing tag: $tag" >&2
	exit 1
fi

normalize_sha() {
	printf '%s' "$1" | tr 'A-F' 'a-f'
}

sha_silicon=$(normalize_sha "$sha_silicon")
sha_intel=$(normalize_sha "$sha_intel")
sha_linux=$(normalize_sha "$sha_linux")

for sha in "$sha_silicon" "$sha_intel" "$sha_linux"; do
	if ! printf '%s' "$sha" | grep -Eq '^[0-9a-f]{64}$'; then
		echo "sha256 must be 64 hex characters" >&2
		exit 1
	fi
done

version=$tag
case $version in
v*) version=${version#v} ;;
esac

root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
template=$root/packaging/homebrew/axon-server.rb.tmpl

for token in VERSION TAG SHA_MACOS_SILICON SHA_MACOS_INTEL SHA_LINUX_X86_64; do
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
	-e "s|@@SHA_MACOS_SILICON@@|${sha_silicon}|g" \
	-e "s|@@SHA_MACOS_INTEL@@|${sha_intel}|g" \
	-e "s|@@SHA_LINUX_X86_64@@|${sha_linux}|g" \
	"$template" >"$tmp"

if grep -q '@@' "$tmp"; then
	echo "rendered formula still contains a placeholder" >&2
	exit 1
fi

mkdir -p "$(dirname "$out")"
mv "$tmp" "$out"
trap - EXIT
