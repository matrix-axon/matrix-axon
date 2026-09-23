#!/usr/bin/env bash
# Fill Formula/axon-server.rb from the GitHub Release zips and push it to
# matrix-axon/homebrew-tap.
#
#   TAG=v0.1.0 TAP_TOKEN=... packaging/homebrew/publish-tap.sh
#
# Dry run (no network, no token): hash zips already on disk and commit into
# a local checkout.
#
#   TAG=v0.1.0 packaging/homebrew/publish-tap.sh \
#     --dry-run --zip-dir DIR --tap-dir DIR
set -euo pipefail

dry_run=0
zip_dir=
tap_dir=
tap_repo=${TAP_REPO:-matrix-axon/homebrew-tap}

while [ $# -gt 0 ]; do
	case "$1" in
	--dry-run)
		dry_run=1
		shift
		;;
	--zip-dir)
		zip_dir=${2:-}
		shift 2
		;;
	--tap-dir)
		tap_dir=${2:-}
		shift 2
		;;
	-h | --help)
		sed -n '2,12p' "$0" >&2
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		exit 2
		;;
	esac
done

if [ -z "${TAG:-}" ]; then
	echo "TAG is required (the release tag, for example v0.1.0)" >&2
	exit 2
fi

if ! printf '%s' "$tap_repo" | grep -Eq '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$'; then
	echo "refusing TAP_REPO: $tap_repo" >&2
	exit 1
fi

if [ "$dry_run" -eq 0 ] && [ -z "${TAP_TOKEN:-}" ]; then
	echo "TAP_TOKEN is required to push ${tap_repo}" >&2
	echo "In GitHub Actions the workflow copies secrets.HOMEBREW_TAP_TOKEN into TAP_TOKEN." >&2
	echo "Fine-grained PAT, contents write on that repository only." >&2
	exit 1
fi

# Strip a trailing newline so it is not part of the password.
if [ -n "${TAP_TOKEN:-}" ]; then
	TAP_TOKEN=${TAP_TOKEN//$'\r'/}
	TAP_TOKEN=${TAP_TOKEN//$'\n'/}
fi

# git_github talks to GitHub with TAP_TOKEN only, as the HTTP basic password.
# GitHub's git endpoint rejects Authorization: Bearer for the OAuth tokens
# `gh auth token` prints (gho_…): the clone then dies with
# "could not read Username" because prompts are disabled.
# The empty credential.helper clears ~/.gitconfig first. Leaving
# `gh auth setup-git` in place and also sending TAP_TOKEN makes GitHub
# answer "invalid credentials".
git_github() {
	if [ -n "${TAP_TOKEN:-}" ]; then
		# git's shell expands TAP_TOKEN. This script must not.
		# shellcheck disable=SC2016
		helper='!f() { echo username=x-access-token; echo "password=$TAP_TOKEN"; }; f'
		GIT_TERMINAL_PROMPT=0 \
			GIT_CONFIG_COUNT=2 \
			GIT_CONFIG_KEY_0=credential.helper \
			GIT_CONFIG_VALUE_0='' \
			GIT_CONFIG_KEY_1=credential.helper \
			GIT_CONFIG_VALUE_1="$helper" \
			git "$@"
	else
		GIT_TERMINAL_PROMPT=0 git -c credential.helper= "$@"
	fi
}

root=$(CDPATH="" cd -- "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

sha256_file() {
	file=$1
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum -- "$file" | awk '{print $1}'
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 -- "$file" | awk '{print $1}'
	else
		echo "need sha256sum or shasum" >&2
		exit 1
	fi
}

download_asset() {
	name=$1
	dest=$2
	attempt=1
	while [ "$attempt" -le 6 ]; do
		if gh release download "$TAG" --repo matrix-axon/matrix-axon \
			--pattern "$name" --dir "$dest" --clobber &&
			[ -s "$dest/$name" ]; then
			return 0
		fi
		echo "waiting for release asset $name (attempt $attempt)" >&2
		sleep $((attempt * 5))
		attempt=$((attempt + 1))
	done
	echo "release $TAG is missing $name" >&2
	return 1
}

if [ -n "$zip_dir" ]; then
	zips=$zip_dir
else
	if ! command -v gh >/dev/null 2>&1; then
		echo "gh is required to download release assets" >&2
		exit 1
	fi
	zips=$work/zips
	mkdir -p "$zips"
	download_asset axon-server-macos-silicon.zip "$zips"
	download_asset axon-server-macos-intel.zip "$zips"
	download_asset axon-server-linux.zip "$zips"
fi

for name in axon-server-macos-silicon.zip axon-server-macos-intel.zip axon-server-linux.zip; do
	if [ ! -s "$zips/$name" ]; then
		echo "missing zip: $zips/$name" >&2
		exit 1
	fi
done

rendered=$work/axon-server.rb
"$root/packaging/homebrew/render-formula.sh" \
	--tag "$TAG" \
	--sha-macos-silicon "$(sha256_file "$zips/axon-server-macos-silicon.zip")" \
	--sha-macos-intel "$(sha256_file "$zips/axon-server-macos-intel.zip")" \
	--sha-linux-x86_64 "$(sha256_file "$zips/axon-server-linux.zip")" \
	--out "$rendered"

version=$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$rendered")
if [ -z "$version" ]; then
	echo "rendered formula has no version" >&2
	exit 1
fi

if [ -z "$tap_dir" ]; then
	tap_dir=$work/tap
	if ! git_github clone --depth 1 "https://github.com/${tap_repo}.git" "$tap_dir"; then
		echo "Could not clone https://github.com/${tap_repo}." >&2
		echo "Create that public repository once, then re-run the tag workflow." >&2
		exit 1
	fi
fi

if [ ! -d "$tap_dir/.git" ]; then
	echo "tap directory is not a git checkout: $tap_dir" >&2
	exit 1
fi

git -C "$tap_dir" config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git -C "$tap_dir" config user.name "github-actions[bot]"

mkdir -p "$tap_dir/Formula"
cp "$rendered" "$tap_dir/Formula/axon-server.rb"
cp "$root/packaging/homebrew/tap-README.md" "$tap_dir/README.md"
git -C "$tap_dir" add Formula/axon-server.rb README.md

if git -C "$tap_dir" diff --cached --quiet; then
	echo "tap already matches $TAG"
	exit 0
fi

git -C "$tap_dir" commit -m "axon-server ${version}"

if [ "$dry_run" -eq 1 ]; then
	echo "dry run committed axon-server ${version} in $tap_dir"
	exit 0
fi

branch=main
if git -C "$tap_dir" show-ref --verify --quiet refs/remotes/origin/HEAD; then
	branch=$(git -C "$tap_dir" symbolic-ref --short refs/remotes/origin/HEAD)
	branch=${branch#origin/}
fi

git_github -C "$tap_dir" push origin "HEAD:${branch}"
echo "pushed axon-server ${version} to ${tap_repo} (${branch})"
