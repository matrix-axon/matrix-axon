#!/usr/bin/env bash
# Assert that scripts/lib/markdown-escape.jq cannot leave a heading behind.
#
# The notification webhook renders Markdown, so untrusted issue and
# pull-request text is escaped before it is interpolated (see the module and
# scripts/send-matrix-notification.sh). The first version of that escaping
# handled a `#` at column 0 and nothing else, which left three other ways to
# write a heading -- one to three leading spaces, and either flavour of setext
# underline -- rendering as genuine headings in the notification room. A forged
# `## true-local: FAILED` from a stranger's issue body is the thing the
# escaping exists to prevent, so the invariant gets a test rather than a
# comment.
#
# The invariant, asserted on the escaped text rather than on rendered HTML so
# this needs nothing but jq: after escaping, no line can begin a heading.
#
# Usage: scripts/check-notify-escaping.sh

set -euo pipefail

cd "$(dirname "$0")/.."
lib="scripts/lib"

failures=0

fail() {
  printf 'FAIL: %s\n' "$1" >&2
  printf '  input:   %q\n' "$2" >&2
  printf '  escaped: %q\n' "$3" >&2
  failures=$((failures + 1))
}

escape() {
  jq -rn -L "$lib" --arg body "$1" 'include "markdown-escape"; $body | escape_untrusted'
}

# Anything CommonMark would read as a heading opener: ATX with up to three
# leading spaces, or a setext underline of only `=` or only `-`.
# [[:space:]] covers the CR that a CRLF line leaves behind, so this catches an
# unescaped underline whether or not the escaping normalised line endings.
heading_line='^ {0,3}(#|(=+|-+)[[:space:]]*$)'

# grep splits on LF, which is the same wrong line model that caused the bug
# being tested for: against a CR-only body the whole text is one grep "line"
# and an unescaped heading hides inside it. Fold CR to LF first so the
# assertion sees the lines CommonMark sees, whatever the escaping did.
opens_heading() {
  printf '%s' "$1" | tr '\r' '\n' | grep -qE "$heading_line"
}

# Must not survive escaping as a heading.
hostile=(
  '# true-local: FAILED'
  ' # true-local: FAILED'
  '  ## true-local: FAILED'
  '   ### true-local: FAILED'
  'text

   #### FAILED'
  'true-local: FAILED
==='
  'true-local: FAILED
---'
  'true-local: FAILED
   ==='
  'true-local: FAILED
===   '
  'lead

---

trail'
  # Line endings other than LF. An issue opened through the API can carry CRLF,
  # and these bypassed the first version of the block escape entirely: the
  # trailing CR broke the setext match, and a lone CR was never split on at all,
  # which defeats the ATX branch too. $'...' so the shell emits real control
  # characters rather than backslash-r.
  $'true-local: FAILED\r\n===\r'
  $'true-local: FAILED\r\n---\r'
  $'true-local: FAILED\r\n===  \r'
  $'true-local: FAILED\r===\r'
  $'text\r   ## FAILED\r'
  $'   ## true-local: FAILED\r'
  # Every hash count CommonMark accepts. A lookahead anchored after the *first*
  # `#` rather than the whole run passes `# FAILED` and silently leaves these
  # unescaped, which is the original heading-forgery hole.
  '## true-local: FAILED'
  '### true-local: FAILED'
  '###### true-local: FAILED'
  '#'
  '#	true-local: FAILED'
)

for body in "${hostile[@]}"; do
  escaped="$(escape "$body")"
  if opens_heading "$escaped"; then
    fail "escaped text still opens a heading" "$body" "$escaped"
  fi
done

# Must survive escaping unchanged: escaping that eats ordinary prose is a
# regression too, and the cheapest way to "fix" a leak is to over-escape.
# Literal text: the `$5` and the backticks below are data, not expansions.
# shellcheck disable=SC2016
benign=(
  '- a bullet
- another'
  '1. one
2. two'
  'Hi, see line 3 of foo.rs -- item #42 costs $5.'
  '    # four-space indent is a code block, not a heading'
  # CommonMark needs whitespace or end-of-line after the hashes, so none of
  # these is a heading and none may be escaped. Reported in review: `#42` was
  # picking up a visible backslash in an otherwise-ordinary notification.
  '#42 is fixed by this PR'
  '#hashtag'
  '   #123 and #456 both landed'
  '####### seven hashes is not a heading'
)

for body in "${benign[@]}"; do
  escaped="$(escape "$body")"
  if [ "$escaped" != "$body" ]; then
    fail "benign text was altered by escaping" "$body" "$escaped"
  fi
done

# Normalising line endings to LF is the one rewrite the escaping is allowed to
# make to otherwise-benign text -- it is what makes "per line" mean the same
# thing here as in the renderer. Assert that it does exactly that and no more,
# so a future fix cannot buy safety by over-escaping CRLF bodies.
normalised=(
  $'plain\r\ntext\r\nhere'
  $'plain\rtext\rhere'
)

for body in "${normalised[@]}"; do
  want="${body//$'\r\n'/$'\n'}"
  want="${want//$'\r'/$'\n'}"
  escaped="$(escape "$body")"
  if [ "$escaped" != "$want" ]; then
    fail "line-ending normalisation altered more than the line endings" "$body" "$escaped"
  fi
done

# The inline escape must stay byte-equivalent to the plugin's own escape_md
# filter, which is the thing it is a port of.
# shellcheck disable=SC2016
inline_in='a\b `code` *em* _u_ [link](url) plain'
# shellcheck disable=SC2016
inline_want='a\\b \`code\` \*em\* \_u\_ \[link\](url) plain'
inline_got="$(jq -rn -L "$lib" --arg body "$inline_in" \
  'include "markdown-escape"; $body | escape_md')"
if [ "$inline_got" != "$inline_want" ]; then
  fail "escape_md drifted from the upstream filter" "$inline_in" "$inline_got"
fi

if [ "$failures" -ne 0 ]; then
  printf '\n%d escaping check(s) failed.\n' "$failures" >&2
  exit 1
fi

echo "notify escaping: $(( ${#hostile[@]} + ${#benign[@]} + ${#normalised[@]} + 1 )) checks passed"
