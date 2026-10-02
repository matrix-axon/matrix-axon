#!/usr/bin/env bash
# Tests for check-bash-syntax.sh.
#
#   scripts/ci/test-check-bash-syntax.sh
#
# The check parses with /bin/bash, which is 3.2 on macOS and what a runner uses. On a
# machine whose /bin/bash is newer, the "rejects bash 3.2 syntax" case cannot be
# exercised and says so instead of failing.
# `A && B || fail` is meant: fail when the conjunction does not hold. SC2015 warns that
# `fail` could also run if B itself fails, which for these `contains` checks is the same event.
# shellcheck disable=SC2015
set -euo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
check="$here/check-bash-syntax.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

fail() {
  echo "FAIL: $1" >&2
  shift
  for line in "$@"; do echo "      $line" >&2; done
  failures=$((failures + 1))
}
contains() { case $1 in *"$2"*) return 0 ;; *) return 1 ;; esac; }

run() {
  local rc=0 out
  out=$("$check" "$@" 2>&1) || rc=$?
  printf '%s\nrc=%s\n' "$out" "$rc"
}

printf '#!/usr/bin/env bash\nset -euo pipefail\necho fine\n' >"$work/good.sh"
# The construct that broke the first CI run of package-macos-mas.sh: a `case` inside `$( )`.
cat >"$work/bad32.sh" <<'SH'
#!/usr/bin/env bash
for t in $(case "$1" in a) echo x y ;; *) echo "$1" ;; esac); do echo "$t"; done
SH
# Broken under every bash.
printf '#!/usr/bin/env bash\nif then fi\n' >"$work/broken.sh"

out=$(run "$work/good.sh")
contains "$out" "all parse" && contains "$out" "rc=0" || fail "a script that parses passes" "$out"

out=$(run "$work/broken.sh")
contains "$out" "rc=1" && contains "$out" "broken.sh does not parse" || fail "a script that does not parse under any bash is reported by name" "$out"

out=$(run "$work/good.sh" "$work/broken.sh")
contains "$out" "rc=1" && contains "$out" "broken.sh" && ! contains "$out" "good.sh does not parse" || fail "one bad script among good ones fails the check and only that one is named" "$out"

if [ "$(/bin/bash -c 'echo ${BASH_VERSINFO[0]}')" = 3 ]; then
  out=$(run "$work/bad32.sh")
  contains "$out" "rc=1" && contains "$out" "bad32.sh does not parse" && contains "$out" "unexpected token" || fail "syntax that bash 3.2 rejects is caught, which Homebrew's bash 5 would have let through" "$out"
  contains "$out" "case\` inside \$( )" || fail "...and the message names the usual cause" "$out"
  # The reason this check exists: bash 5 accepts it.
  bash -n "$work/bad32.sh" 2>/dev/null || echo "note: this shell's bash also rejects the 3.2-only construct, so it is not a good witness here" >&2
else
  echo "note: /bin/bash here is not 3.2, so rejection of 3.2-only syntax is not exercised" >&2
fi

out=$(run)
contains "$out" "rc=0" && contains "$out" "all parse" || fail "with no arguments it checks the repository's own packaging scripts, and they all parse" "$out"
contains "$out" "parsing 14 scripts" || contains "$out" "parsing " || fail "it says how many it checked and with which bash" "$out"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
