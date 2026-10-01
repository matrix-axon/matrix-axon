#!/usr/bin/env bash
# Tests for asc.sh.
#
#   scripts/lib/test-asc.sh
#   TEST_BASH=/bin/bash scripts/lib/test-asc.sh     # under macOS's bash 3.2
#
# The App Store Connect lookup itself is tested by test_asc_next_build_number.py.
# This covers the shell around it: where the credentials come from, and how the
# number it prints is validated and passed on. The helper is replaced by a stub
# in a throwaway repository root, so nothing here touches the network.
# `A && B || fail` is meant: fail when the conjunction does not hold. SC2015 warns that
# `fail` could also run if B itself fails, which for these `contains` checks is the same event.
# shellcheck disable=SC2015
set -euo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
test_bash=${TEST_BASH:-bash}
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

# run_asc <env assignments...> -- <snippet>: fresh shell, `set -euo pipefail` as
# the real scripts have, asc.sh sourced. Prints OUT<>, ERR<>, rc=.
run_asc() {
  local envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  local snippet=$1 out rc=0
  out=$(env -i PATH="$PATH" HOME="$work" ${envs[@]+"${envs[@]}"} "$test_bash" --noprofile --norc -c 'set -euo pipefail; . "$1"; '"$snippet" _ "$here/asc.sh" 2>"$work/err") || rc=$?
  printf 'OUT<%s>\nERR<%s>\nrc=%s\n' "$out" "$(cat "$work/err")" "$rc"
}

# --- asc_require_credentials -------------------------------------------------

repo="$work/repo"
mkdir -p "$repo"

rm -f "$repo/.env"
out=$(run_asc ASC_KEY_ID=ENVKEY ASC_ISSUER_ID=ENVISS -- "asc_require_credentials '$repo'; echo \"\$ASC_KEY_ID \$ASC_ISSUER_ID\"")
contains "$out" "OUT<ENVKEY ENVISS>" && contains "$out" "ERR<>" && contains "$out" "rc=0" || fail "credentials in the environment need no .env and print nothing" "$out"

printf 'ASC_KEY_ID=FILEKEY\nASC_ISSUER_ID=FILEISS\nDATABASE_URL=postgres://secret\n' >"$repo/.env"
out=$(run_asc -- "asc_require_credentials '$repo'; echo \"\$ASC_KEY_ID \$ASC_ISSUER_ID\"; echo \"db=\${DATABASE_URL-unset}\"")
contains "$out" "FILEKEY FILEISS" && contains "$out" "rc=0" || fail "credentials are taken from .env" "$out"
contains "$out" "db=unset" || fail "nothing else in .env is exported" "$out"
contains "$out" "ASC_KEY_ID taken from $repo/.env" && contains "$out" "ASC_ISSUER_ID taken from $repo/.env" || fail "it says which names came from the file" "$out"

# The values must be used, but never printed by the library itself.
out=$(run_asc -- "asc_require_credentials '$repo' >\"$work/stdout\" 2>\"$work/stderr\"; cat \"$work/stdout\" \"$work/stderr\"")
if contains "$out" "FILEKEY" || contains "$out" "FILEISS"; then
  fail "the values are not printed" "$out"
fi

out=$(run_asc ASC_KEY_ID=ENVKEY -- "asc_require_credentials '$repo'; echo \"\$ASC_KEY_ID \$ASC_ISSUER_ID\"")
contains "$out" "ENVKEY FILEISS" && contains "$out" "ASC_ISSUER_ID taken from" && ! contains "$out" "ASC_KEY_ID taken from" || fail "the environment beats .env per variable" "$out"

rm -f "$repo/.env"
out=$(run_asc -- "asc_require_credentials '$repo'; echo survived")
contains "$out" "rc=1" && ! contains "$out" "survived" && contains "$out" "in the environment or in .env at the repository root" || fail "missing credentials stop the script and name both places" "$out"

printf 'ASC_KEY_ID=ONLYKEY\n' >"$repo/.env"
out=$(run_asc -- "asc_require_credentials '$repo'; echo survived")
contains "$out" "rc=1" && contains "$out" "ASC_ISSUER_ID" && ! contains "$out" "survived" || fail "one missing credential is still an error" "$out"

# --- asc_next_build_number ---------------------------------------------------

fake="$work/fake"
mkdir -p "$fake/scripts/lib" "$fake/conf"
printf '{"identifier":"org.example.app"}\n' >"$fake/conf/tauri.conf.json"
stub() { printf '%s\n' "$1" >"$fake/scripts/lib/asc-next-build-number.py"; }

# The stub plays an app with iOS builds up to 41 and Mac builds up to 2.
stub 'import sys
bundle, platform = sys.argv[1], sys.argv[2]
print({"ios": "42", "macos": "3"}[platform] if bundle == "org.example.app" else "WRONG BUNDLE " + bundle)'
out=$(run_asc -- "n=\$(asc_next_build_number '$fake' '$fake/conf/tauri.conf.json' ios); echo \"got=\$n\"")
contains "$out" "got=42" && contains "$out" "rc=0" || fail "the bundle ID from tauri.conf.json and the platform are passed to the helper and its number returned" "$out"
contains "$out" "asking App Store Connect for the next ios build number (org.example.app)" || fail "it says what it is doing, and for which platform, on stderr" "$out"
out=$(run_asc -- "n=\$(asc_next_build_number '$fake' '$fake/conf/tauri.conf.json' macos); echo \"got=\$n\"")
contains "$out" "got=3" || fail "the same app on another platform gets that platform's number, not the iOS one" "$out"

# stdout carries the number and nothing else, so $(...) captures just it.
out=$(env -i PATH="$PATH" HOME="$work" "$test_bash" --noprofile --norc -c 'set -euo pipefail; . "$1"; asc_next_build_number "$2" "$3" macos 2>/dev/null' _ "$here/asc.sh" "$fake" "$fake/conf/tauri.conf.json")
[ "$out" = "3" ] || fail "stdout is the number alone" "got: <$out>"

for good in 1 7 1.2 1.2.3; do
  stub "print('$good')"
  out=$(run_asc -- "asc_next_build_number '$fake' '$fake/conf/tauri.conf.json' ios 2>/dev/null")
  contains "$out" "OUT<$good>" || fail "$good is accepted as a build number" "$out"
done
for bad in abc "1.2.3.4" "1 2" '1"}' "-1" ""; do
  stub "print('$bad')"
  out=$(run_asc -- "asc_next_build_number '$fake' '$fake/conf/tauri.conf.json' ios; echo survived")
  contains "$out" "which is not a build number" && contains "$out" "rc=1" && ! contains "$out" "survived" || fail "'$bad' from the helper is refused, not passed to --config" "$out"
done

out=$(run_asc -- "asc_next_build_number '$fake' '$fake/conf/tauri.conf.json'; echo survived")
contains "$out" "needs a platform" && contains "$out" "rc=1" && ! contains "$out" "survived" || fail "a missing platform is an error, not a guess" "$out"

stub 'import sys; sys.exit(1)'
out=$(run_asc -- "asc_next_build_number '$fake' '$fake/conf/tauri.conf.json' ios; echo survived")
contains "$out" "could not work out the next build number; pass --build-number <n> instead" && contains "$out" "rc=1" && ! contains "$out" "survived" || fail "a failing helper is an error that names the way out" "$out"

out=$(run_asc -- "asc_next_build_number '$fake' '$fake/conf/missing.json' ios; echo survived")
contains "$out" "could not read the bundle identifier" && contains "$out" "rc=1" || fail "an unreadable tauri.conf.json is an error" "$out"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
