#!/usr/bin/env bash
# The release version is axon-server's Cargo version: what `axon-server
# --version` prints, what packaging/package.sh stamps on the .deb/.rpm, and
# what release-plz bumps (release-plz.toml, ADR 0106). A GitHub Release is
# named by its tag instead, so the two must agree.
#
#   scripts/release-version.sh print
#       Print the version.
#
#   scripts/release-version.sh check [TAG]
#       Fail unless a v* TAG equals v<version>. TAG defaults to
#       GITHUB_REF_NAME when GITHUB_REF_TYPE is tag; any other ref passes.
#       beta-*/alpha-* tags pass: they name builds, not Cargo versions.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)

usage() {
	sed -n '2,14p' "$0" >&2
}

# cargo metadata, not a grep of Cargo.toml: it resolves version.workspace
# the way the build does, so this is the CARGO_PKG_VERSION the binary gets.
version() {
	cargo metadata --no-deps --format-version 1 --manifest-path "$root/Cargo.toml" |
		jq -er '.packages[] | select(.name == "axon-server") | .version'
}

case "${1:-}" in
print)
	version
	;;
check)
	if [ $# -ge 2 ]; then
		tag=$2
	elif [ "${GITHUB_REF_TYPE:-}" = tag ]; then
		tag=${GITHUB_REF_NAME:?}
	else
		echo "not a tag push; nothing to check"
		exit 0
	fi
	case $tag in
	v*) ;;
	*)
		echo "$tag is not a v* release tag; nothing to check"
		exit 0
		;;
	esac
	want=$(version)
	if [ "$tag" != "v$want" ]; then
		echo "::error::tag $tag does not match the workspace version $want (Cargo.toml)." >&2
		echo "Release through the release-plz PR, which bumps Cargo.toml and tags the merge." >&2
		echo "For a hand-made tag, bump [workspace.package] version in a PR first, then tag its merge as v<version>." >&2
		exit 1
	fi
	echo "tag $tag matches the workspace version"
	;;
-h | --help)
	usage
	;;
*)
	usage
	exit 2
	;;
esac
