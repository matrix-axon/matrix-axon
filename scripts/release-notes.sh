#!/usr/bin/env bash
# Release notes and CHANGELOG.md for a release (ADR 0108). The notes come
# from GitHub's generate-notes API, grouped by PR label through
# .github/release.yml; .github/workflows/release-notes.yml drives this.
#
#   scripts/release-notes.sh previous-tag TAG
#       Print the highest vX.Y.Z tag below TAG, or nothing for the first.
#       TAG need not exist yet.
#
#   scripts/release-notes.sh generate TAG [PREVIOUS]
#       Print the generated notes for TAG, covering the PRs merged since
#       PREVIOUS (default: previous-tag). Needs gh, GH_TOKEN and
#       GITHUB_REPOSITORY. TARGET (default main) is the commit TAG will be
#       cut from while TAG does not exist.
#
#   scripts/release-notes.sh stamp
#       Append a marker recording a hash of stdin, so `edited` can tell
#       later whether a person changed the notes. Fails on empty input.
#
#   scripts/release-notes.sh edited FILE
#       Exit 0 if FILE's notes differ from what `stamp` recorded, or carry
#       no marker; exit 1 if untouched.
#
#   scripts/release-notes.sh strip
#       Remove the marker from stdin.
#
#   scripts/release-notes.sh changelog TAG NOTES_FILE [DATE]
#       Add a "## TAG - DATE" section holding NOTES_FILE at the top of
#       CHANGELOG.md (CHANGELOG overrides the path). Does nothing if TAG
#       already has one.
set -euo pipefail

root=$(CDPATH="" cd -- "$(dirname "$0")/.." && pwd)
marker='<!-- release-notes-sha256:'
release_tag_re='^v[0-9]+\.[0-9]+\.[0-9]+$'

usage() {
	sed -n '2,30p' "$0" >&2
}

# Hash the notes without the marker, line endings or trailing blank lines,
# which editing on github.com can change without anyone meaning to.
body_hash() {
	printf '%s' "$( (tr -d '\r' | grep -v -F "$marker") || true)" | sha256sum | cut -d' ' -f1
}

strip_marker() {
	local text
	text=$( (tr -d '\r' | grep -v -F "$marker") || true)
	printf '%s\n' "$text"
}

need_tag() {
	if ! [[ ${1:-} =~ $release_tag_re ]]; then
		echo "error: '${1:-}' is not a vX.Y.Z release tag" >&2
		exit 2
	fi
}

previous_tag() {
	local tag=$1
	{
		git -C "$root" tag --list 'v*' | grep -E "$release_tag_re" || true
		echo "$tag"
	} | sort -Vu | awk -v t="$tag" '$0 == t { print prev; exit } { prev = $0 }'
}

case "${1:-}" in
previous-tag)
	need_tag "${2:-}"
	previous_tag "$2"
	;;
generate)
	need_tag "${2:-}"
	tag=$2
	previous=${3:-$(previous_tag "$tag")}
	args=(-f "tag_name=$tag" -f "target_commitish=${TARGET:-main}")
	if [ -n "$previous" ]; then
		args+=(-f "previous_tag_name=$previous")
	fi
	gh api -X POST "repos/${GITHUB_REPOSITORY:?}/releases/generate-notes" \
		"${args[@]}" --jq .body
	;;
stamp)
	text=$(strip_marker)
	if ! printf '%s' "$text" | grep -q '[^[:space:]]'; then
		echo "error: refusing to stamp empty notes" >&2
		exit 1
	fi
	hash=$(printf '%s' "$text" | body_hash)
	printf '%s\n\n%s %s -->\n' "$text" "$marker" "$hash"
	;;
edited)
	file=${2:?usage: edited FILE}
	recorded=$( (grep -F "$marker" "$file" | tail -n 1 | sed -E 's/.*sha256: *([0-9a-f]+).*/\1/') || true)
	if [ -z "$recorded" ] || [ "$recorded" != "$(body_hash <"$file")" ]; then
		exit 0
	fi
	exit 1
	;;
strip)
	strip_marker
	;;
changelog)
	need_tag "${2:-}"
	tag=$2
	notes=${3:?usage: changelog TAG NOTES_FILE [DATE]}
	date=${4:-$(date -u +%Y-%m-%d)}
	file=${CHANGELOG:-$root/CHANGELOG.md}
	if [ -f "$file" ] && grep -q -E "^## $tag( |\$)" "$file"; then
		echo "$file already has a section for $tag"
		exit 0
	fi
	if [ ! -f "$file" ]; then
		printf '# Changelog\n\nNotable changes to each release, newest first.\nThe same notes are on the [GitHub Releases](../../releases) page.\n' >"$file"
	fi
	tmp=$(mktemp)
	trap 'rm -f "$tmp"' EXIT
	# The notes' own headings sit one level below the release heading.
	section=$(strip_marker <"$notes" | sed -E 's/^(#+) /\1# /')
	# ENVIRON, not awk -v, which would interpret backslashes in the notes.
	HEAD="## $tag - $date" SECTION="$section" awk '
		!done && /^## / { print ENVIRON["HEAD"]; print ""; print ENVIRON["SECTION"]; print ""; done = 1 }
		{ print }
		END { if (!done) { print ""; print ENVIRON["HEAD"]; print ""; print ENVIRON["SECTION"] } }
	' "$file" >"$tmp"
	cat "$tmp" >"$file"
	;;
*)
	usage
	exit 2
	;;
esac
