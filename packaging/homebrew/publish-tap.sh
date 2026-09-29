#!/usr/bin/env bash
# Fill Formula/axon-server.rb and Formula/axon-tui.rb from the GitHub
# Release zips, and Casks/axon.rb from the universal disk image, and push
# them to matrix-axon/homebrew-tap.
# The formulas publish even when the disk image is missing. The cask is
# added by a later run of the same tag once the image is attached.
#
#   TAG=v0.1.0 TAP_TOKEN=... packaging/homebrew/publish-tap.sh
#
# Only stable vX.Y.Z tags publish; beta-*/alpha-* tags exit 0 untouched.
# A tag older than a formula or the cask already in the tap exits 0
# untouched too, so re-running an old tag's workflow or a backport tag
# cannot downgrade.
#
# Dry run (no network, no token): hash zips already on disk and commit into
# a local checkout. Both directories are required. The checkout's git
# config is not changed.
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
		zip_dir=${2:?missing value for $1}
		shift 2
		;;
	--tap-dir)
		tap_dir=${2:?missing value for $1}
		shift 2
		;;
	-h | --help)
		sed -n '2,16p' "$0" >&2
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

# Same pattern as render-formula.sh. cross-build.yml already skips
# non-v tags; this covers a manual run and a v tag like v1.0.0-rc1.
if ! printf '%s' "$TAG" | grep -Eq '^v[0-9]+(\.[0-9]+)+$'; then
	echo "skipping $TAG: only stable vX.Y.Z tags are published to the tap"
	exit 0
fi

if [ "$dry_run" -eq 1 ] && { [ -z "$zip_dir" ] || [ -z "$tap_dir" ]; }; then
	echo "--dry-run needs --zip-dir and --tap-dir: it downloads nothing and clones nothing" >&2
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

assets="axon-server-macos-silicon.zip axon-server-macos-intel.zip axon-server-linux.zip axon-tui-macos-silicon.zip axon-tui-macos-intel.zip axon-tui-linux.zip"

# One download of every zip per attempt, so the retry backoff is paid
# once rather than once per asset.
download_assets() {
	dest=$1
	patterns=()
	for name in $assets; do
		patterns+=(--pattern "$name")
	done
	attempt=1
	while [ "$attempt" -le 6 ]; do
		if gh release download "$TAG" --repo matrix-axon/matrix-axon \
			"${patterns[@]}" --dir "$dest" --clobber; then
			missing=
			for name in $assets; do
				[ -s "$dest/$name" ] || missing="$missing $name"
			done
			if [ -z "$missing" ]; then
				return 0
			fi
		fi
		echo "waiting for release assets (attempt $attempt)" >&2
		sleep $((attempt * 5))
		attempt=$((attempt + 1))
	done
	echo "release $TAG is missing one of: $assets" >&2
	return 1
}

# desktop-build.yml attaches the disk image on its own and usually finishes
# after the zip release job. Poll for up to 20 minutes. If it never
# appears, the caller publishes the formulas without the cask.
download_dmg() {
	dest=$1
	name=$2
	attempt=1
	while [ "$attempt" -le 40 ]; do
		if gh release download "$TAG" --repo matrix-axon/matrix-axon \
			--pattern "$name" --dir "$dest" --clobber && [ -s "$dest/$name" ]; then
			return 0
		fi
		echo "waiting for $name (attempt $attempt)" >&2
		sleep 30
		attempt=$((attempt + 1))
	done
	echo "release $TAG is missing $name (desktop-build.yml attaches it)" >&2
	return 1
}

# True when dotted-numeric version $1 is older than $2. Equal versions are
# not older: republishing the same tag must reach the "already matches" path.
version_lt() {
	[ "$1" = "$2" ] && return 1
	[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" = "$1" ]
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
	download_assets "$zips"
fi

for name in $assets; do
	if [ ! -s "$zips/$name" ]; then
		echo "missing zip: $zips/$name" >&2
		exit 1
	fi
done

render_formula() {
	formula=$1
	"$root/packaging/homebrew/render-formula.sh" \
		--tag "$TAG" \
		--template "$root/packaging/homebrew/${formula}.rb.tmpl" \
		--sha-macos-silicon "$(sha256_file "$zips/${formula}-macos-silicon.zip")" \
		--sha-macos-intel "$(sha256_file "$zips/${formula}-macos-intel.zip")" \
		--sha-linux-x86_64 "$(sha256_file "$zips/${formula}-linux.zip")" \
		--out "$work/${formula}.rb"
}

render_formula axon-server
render_formula axon-tui

version=$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$work/axon-server.rb")
tui_version=$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$work/axon-tui.rb")
if [ -z "$version" ] || [ "$version" != "$tui_version" ]; then
	echo "rendered formulas disagree on version ('${version}' vs '${tui_version}')" >&2
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

# A non-numeric current version is replaced, not compared. Any of the
# formulas or the cask being newer skips the whole commit, so one tag
# cannot move them apart.
for spec in Formula/axon-server.rb Formula/axon-tui.rb Casks/axon.rb; do
	current=
	if [ -f "$tap_dir/$spec" ]; then
		current=$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$tap_dir/$spec")
	fi
	if printf '%s' "$current" | grep -Eq '^[0-9]+(\.[0-9]+)+$' &&
		version_lt "$version" "$current"; then
		echo "skipping $TAG: the tap already has ${spec} ${current}, which is newer than ${version}"
		exit 0
	fi
done

# After the downgrade guard, so an older tag does not sit on the poll.
# A missing image must not hold the formulas back: desktop-build.yml can
# fail, or the asset can be named differently. Republishing this same tag
# once the image exists adds the cask, because an equal formula version
# is not newer and the guard above does not skip it.
dmg="Axon_${version}_universal.dmg"
publish_cask=0
if [ -s "$zips/$dmg" ]; then
	publish_cask=1
elif [ -n "$zip_dir" ]; then
	echo "disk image $zips/$dmg is missing; publishing the formulas without the cask" >&2
else
	if download_dmg "$zips" "$dmg"; then
		publish_cask=1
	else
		echo "disk image $dmg is missing; publishing the formulas without the cask" >&2
	fi
fi

if [ "$publish_cask" -eq 1 ]; then
	cask=$work/axon.rb
	"$root/packaging/homebrew/render-cask.sh" \
		--tag "$TAG" \
		--sha "$(sha256_file "$zips/$dmg")" \
		--out "$cask"
	cask_version=$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$cask")
	if [ "$version" != "$cask_version" ]; then
		echo "rendered cask version '${cask_version}' does not match formula '${version}'" >&2
		exit 1
	fi
fi

mkdir -p "$tap_dir/Formula"
cp "$work/axon-server.rb" "$tap_dir/Formula/axon-server.rb"
cp "$work/axon-tui.rb" "$tap_dir/Formula/axon-tui.rb"
cp "$root/packaging/homebrew/tap-README.md" "$tap_dir/README.md"
git -C "$tap_dir" add Formula/axon-server.rb Formula/axon-tui.rb README.md
subject="axon-server and axon-tui ${version}"
if [ "$publish_cask" -eq 1 ]; then
	mkdir -p "$tap_dir/Casks"
	cp "$cask" "$tap_dir/Casks/axon.rb"
	git -C "$tap_dir" add Casks/axon.rb
	subject="axon-server, axon-tui, and axon ${version}"
fi

if git -C "$tap_dir" diff --cached --quiet; then
	echo "tap already matches $TAG"
	exit 0
fi

# -c, not git config: a --tap-dir checkout keeps its own identity.
git -C "$tap_dir" \
	-c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
	-c user.name="github-actions[bot]" \
	commit -m "$subject"

if [ "$dry_run" -eq 1 ]; then
	echo "dry run committed ${subject} in $tap_dir"
	exit 0
fi

branch=main
if git -C "$tap_dir" show-ref --verify --quiet refs/remotes/origin/HEAD; then
	branch=$(git -C "$tap_dir" symbolic-ref --short refs/remotes/origin/HEAD)
	branch=${branch#origin/}
fi

git_github -C "$tap_dir" push origin "HEAD:${branch}"
echo "pushed ${subject} to ${tap_repo} (${branch})"
