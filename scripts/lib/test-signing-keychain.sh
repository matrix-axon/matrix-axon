#!/usr/bin/env bash
# Tests for signing-keychain.sh.
#
#   scripts/lib/test-signing-keychain.sh
#
# macOS only: it needs `security`. The unlock tests use a throwaway keychain with
# an empty password that is created and deleted here and never added to the
# search list, so the user's real keychains are only ever read.
set -euo pipefail

if ! command -v security >/dev/null 2>&1; then
  echo "skipped: no \`security\` here (this is a macOS tool)"
  exit 0
fi

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
# The shell the library is exercised under: TEST_BASH=/bin/bash runs it under
# macOS's own bash 3.2, which is what `#!/usr/bin/env bash` finds on a Mac
# without Homebrew's.
test_bash=${TEST_BASH:-bash}
work=$(mktemp -d)
keychain="$work/throwaway.keychain-db"
cleanup() {
  security delete-keychain "$keychain" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
failures=0

fail() {
  echo "FAIL: $1" >&2
  shift
  for line in "$@"; do echo "      $line" >&2; done
  failures=$((failures + 1))
}

# in_lib <env assignments...> -- <shell snippet>: run the snippet in a fresh bash
# with the library sourced and a clean environment. Prints stdout then stderr
# then "rc=N", so a test can look at all three.
in_lib() {
  local envs=()
  while [ "$1" != "--" ]; do envs+=("$1"); shift; done
  shift
  local snippet=$1 out err rc=0
  out=$(env -i PATH="$PATH" HOME="${HOME_FOR_TEST:-$HOME}" ${envs[@]+"${envs[@]}"} "$test_bash" --noprofile --norc -c '. "$1"; '"$snippet" _ "$here/signing-keychain.sh" 2>"$work/err") || rc=$?
  err=$(cat "$work/err")
  printf 'OUT<%s>\nERR<%s>\nrc=%s\n' "$out" "$err" "$rc"
}

contains() { case $1 in *"$2"*) return 0 ;; *) return 1 ;; esac; }

# --- canon_path --------------------------------------------------------------

mkdir -p "$work/real"
ln -s "$work/real" "$work/link"
want="$(cd "$work/real" && pwd -P)/k.keychain-db"
got=$(env -i PATH="$PATH" "$test_bash" --noprofile --norc -c '. "$1"; canon_path "$2"' _ "$here/signing-keychain.sh" "$work/link/k.keychain-db")
[ "$got" = "$want" ] || fail "canon_path resolves a symlinked directory" "want: $want" "got:  $got"

got=$(env -i PATH="$PATH" "$test_bash" --noprofile --norc -c '. "$1"; canon_path "$2" || echo FAILED' _ "$here/signing-keychain.sh" "$work/no/such/dir/k")
[ "$got" = "FAILED" ] || fail "canon_path of a missing directory prints nothing and fails" "got: $got"

# --- search-list membership, against the real list ---------------------------

first=$(env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; signing_keychain_search_list | head -1' _ "$here/signing-keychain.sh")
if [ -n "$first" ] && [ -f "$first" ]; then
  dir=$(dirname "$first"); base=$(basename "$first")
  ln -sfn "$dir" "$work/dirlink"
  for form in "absolute|$first" "relative|./$base" "via symlink|$work/dirlink/$base"; do
    label=${form%%|*}; path=${form#*|}
    if [ "$label" = relative ]; then
      ok=$(cd "$dir" && env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; keychain_in_search_list "$2" && echo yes || echo no' _ "$here/signing-keychain.sh" "$path")
    else
      ok=$(env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; keychain_in_search_list "$2" && echo yes || echo no' _ "$here/signing-keychain.sh" "$path")
    fi
    [ "$ok" = yes ] || fail "a listed keychain is found by its $label path" "path: $path"
  done
else
  echo "note: no keychain in the search list to test membership against" >&2
fi

ok=$(env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; keychain_in_search_list "$2" && echo yes || echo no' _ "$here/signing-keychain.sh" "$work/not-listed.keychain-db")
[ "$ok" = no ] || fail "an unlisted keychain is reported as not listed"

# The repair command must keep everything already listed, in order, after the new one.
fix=$(env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; signing_keychain_fix_command "$2"' _ "$here/signing-keychain.sh" "/new/one.keychain-db")
case $fix in "security list-keychains -d user -s \"/new/one.keychain-db\""*) ;; *) fail "the repair command starts with the new keychain" "got: $fix" ;; esac
while IFS= read -r listed; do
  [ -z "$listed" ] && continue
  contains "$fix" "\"$listed\"" || fail "the repair command keeps $listed" "got: $fix"
done <<EOF
$(env -i PATH="$PATH" HOME="$HOME" "$test_bash" --noprofile --norc -c '. "$1"; signing_keychain_search_list' _ "$here/signing-keychain.sh")
EOF

# --- unlock_signing_keychain: which variable is read, and what it says -------

out=$(in_lib -- 'unlock_signing_keychain')
contains "$out" "OUT<>" && contains "$out" "ERR<>" && contains "$out" "rc=0" || fail "nothing set is a silent no-op" "$out"

mkdir -p "$work/home/Library/Keychains"
out=$(HOME_FOR_TEST="$work/home" in_lib AXON_SIGNING_KEYCHAIN=/nonexistent/x.keychain-db -- 'unlock_signing_keychain')
contains "$out" "AXON_SIGNING_KEYCHAIN is set but /nonexistent/x.keychain-db does not exist" && contains "$out" "rc=1" || fail "the new name is read and named in the error" "$out"
contains "$out" "old name" && fail "the new name alone prints no deprecation note" "$out"

out=$(HOME_FOR_TEST="$work/home" in_lib AXON_IOS_KEYCHAIN=/nonexistent/old.keychain-db -- 'unlock_signing_keychain')
contains "$out" "AXON_IOS_KEYCHAIN is the old name" && contains "$out" "AXON_IOS_KEYCHAIN is set but /nonexistent/old.keychain-db" && contains "$out" "rc=1" || fail "the old name still works, with a note" "$out"

out=$(HOME_FOR_TEST="$work/home" in_lib AXON_SIGNING_KEYCHAIN=/nonexistent/new.keychain-db AXON_IOS_KEYCHAIN=/nonexistent/old.keychain-db -- 'unlock_signing_keychain')
contains "$out" "/nonexistent/new.keychain-db" && ! contains "$out" "/nonexistent/old.keychain-db" && ! contains "$out" "old name" || fail "the new name wins when both are set" "$out"

out=$(HOME_FOR_TEST="$work/home" in_lib AXON_SIGNING_KEYCHAIN=barename -- 'unlock_signing_keychain')
contains "$out" "$work/home/Library/Keychains/barename.keychain-db does not exist" || fail "a bare name is looked up under ~/Library/Keychains with the suffix" "$out"
out=$(HOME_FOR_TEST="$work/home" in_lib AXON_SIGNING_KEYCHAIN=barename.keychain-db -- 'unlock_signing_keychain')
contains "$out" "$work/home/Library/Keychains/barename.keychain-db does not exist" || fail "a bare name that already has the suffix does not get it twice" "$out"

# --- against a real, throwaway keychain --------------------------------------

security create-keychain -p "" "$keychain" >/dev/null
security lock-keychain "$keychain" >/dev/null 2>&1 || true

out=$(in_lib AXON_SIGNING_KEYCHAIN="$keychain" -- 'unlock_signing_keychain')
contains "$out" "is unlocked but not in the keychain search list" && contains "$out" "security list-keychains -d user -s" && contains "$out" "rc=1" || fail "unlocked-but-unlisted is refused with the repair command" "$out"

out=$(in_lib AXON_SIGNING_KEYCHAIN="$keychain" AXON_SIGNING_KEYCHAIN_PASSWORD=wrong -- 'unlock_signing_keychain')
contains "$out" "could not unlock" && contains "$out" "AXON_SIGNING_KEYCHAIN_PASSWORD" && contains "$out" "rc=1" || fail "a wrong password is reported by the new variable's name" "$out"

# The old password variable is honoured when the new one is not set at all...
out=$(in_lib AXON_SIGNING_KEYCHAIN="$keychain" AXON_IOS_KEYCHAIN_PASSWORD=wrong -- 'unlock_signing_keychain')
contains "$out" "could not unlock" || fail "the old password variable is the fallback" "$out"
# ...and an explicitly empty new one is an answer, not an absence.
out=$(in_lib AXON_SIGNING_KEYCHAIN="$keychain" AXON_SIGNING_KEYCHAIN_PASSWORD= AXON_IOS_KEYCHAIN_PASSWORD=wrong -- 'unlock_signing_keychain')
contains "$out" "could not unlock" && fail "an empty new password overrides a stale old one" "$out"

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
