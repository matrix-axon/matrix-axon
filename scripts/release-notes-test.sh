#!/usr/bin/env bash
# Tests for scripts/release-notes.sh that need neither network nor gh.
set -euo pipefail

script=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)/release-notes.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail=0

check() {
	local name=$1 want=$2 got=$3
	if [ "$want" = "$got" ]; then
		echo "ok   $name"
	else
		echo "FAIL $name" >&2
		printf '  want: %q\n  got:  %q\n' "$want" "$got" >&2
		fail=1
	fi
}

# previous-tag: semver order, not lexical; prereleases and build tags ignored.
git init -q "$work/repo"
(
	cd "$work/repo"
	git -c user.name=t -c user.email=t@t commit -q --allow-empty -m init
	for t in v0.0.9 v0.0.16 v0.1.0 v0.1.1 v0.1.2-rc1 beta-1; do git tag "$t"; done
)
# The script finds tags through its own checkout, so copy it into the repo.
mkdir "$work/repo/scripts"
cp "$script" "$work/repo/scripts/release-notes.sh"
prev() { "$work/repo/scripts/release-notes.sh" previous-tag "$1"; }
check "previous of v0.1.2 skips the rc" "v0.1.1" "$(prev v0.1.2)"
check "previous of v0.1.1" "v0.1.0" "$(prev v0.1.1)"
check "previous of v0.0.16 is not lexical" "v0.0.9" "$(prev v0.0.16)"
check "previous of an existing tag in the middle" "v0.0.16" "$(prev v0.1.0)"
check "first release has none" "" "$(prev v0.0.1)"
if "$script" previous-tag beta-1 2>/dev/null; then got=accepted; else got=rejected; fi
check "rejects a non-release tag" rejected "$got"

# stamp / edited / strip
printf '## Features\n* Add spaces by @a in #540\n' >"$work/notes"
"$script" stamp <"$work/notes" >"$work/stamped"
if printf '\n \n' | "$script" stamp >/dev/null 2>&1; then got=stamped; else got=refused; fi
check "stamp refuses empty notes" refused "$got"
if "$script" edited "$work/stamped"; then got=edited; else got=untouched; fi
check "fresh stamp is untouched" untouched "$got"
{ sed 's/$/\r/' "$work/stamped"; printf '\r\n\r\n'; } >"$work/crlf"
if "$script" edited "$work/crlf"; then got=edited; else got=untouched; fi
check "CRLF and trailing blank lines are not an edit" untouched "$got"
sed 's/spaces/rooms/' "$work/stamped" >"$work/changed"
if "$script" edited "$work/changed"; then got=edited; else got=untouched; fi
check "changed text is an edit" edited "$got"
if "$script" edited "$work/notes"; then got=edited; else got=untouched; fi
check "no marker counts as edited" edited "$got"
check "strip removes the marker" "$(cat "$work/notes")" "$("$script" strip <"$work/stamped")"

# changelog: created, prepended, idempotent, headings demoted.
export CHANGELOG="$work/CHANGELOG.md"
"$script" changelog v0.1.1 "$work/stamped" 2026-10-01 >/dev/null
printf '## Bug Fixes\n* Fix a thing \\n by @b in #541\n' >"$work/notes2"
"$script" changelog v0.1.2 "$work/notes2" 2026-10-02 >/dev/null
check "newest section first" "## v0.1.2 - 2026-10-02" "$(grep '^## ' "$CHANGELOG" | head -n 1)"
check "older section kept" "## v0.1.1 - 2026-10-01" "$(grep '^## ' "$CHANGELOG" | tail -n 1)"
check "headings demoted" "### Features" "$(grep '^### Features' "$CHANGELOG")"
check "backslashes survive" "1" "$(grep -c 'Fix a thing \\n by' "$CHANGELOG")"
check "marker not in changelog" "0" "$(grep -c 'release-notes-sha256' "$CHANGELOG" || true)"
before=$(cat "$CHANGELOG")
"$script" changelog v0.1.2 "$work/notes2" 2026-10-03 >/dev/null
check "second run is a no-op" "$before" "$(cat "$CHANGELOG")"

exit "$fail"
