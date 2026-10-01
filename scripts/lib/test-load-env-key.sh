#!/usr/bin/env bash
# Tests for load-env-key.sh.
#
#   scripts/lib/test-load-env-key.sh
#
# Each case writes a throwaway .env, runs load_env_key in a fresh bash, and
# compares what it exported. Nothing here reads a real .env.
# `A && B || fail` is meant: fail when the conjunction does not hold. SC2015 warns that
# `fail` could also run if B itself fails, which for these `contains` checks is the same event.
# shellcheck disable=SC2015
set -euo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# check <name> <expected> <env-file contents> [VAR=value ...to preset]
# Prints the value NAME ends up with, or the word UNSET.
check() {
  local label=$1 expected=$2 contents=$3
  shift 3
  printf '%b' "$contents" >"$work/.env"
  local got
  got=$(env -i PATH="$PATH" HOME="$work" "$@" bash --noprofile --norc -c '
    set -euo pipefail
    . "$1"
    rc=0; load_env_key "$2" "$3" || rc=$?
    echo "${!2-UNSET}|$rc"
  ' _ "$here/load-env-key.sh" ASC_KEY_ID "$work/.env")
  if [ "$got" != "$expected" ]; then
    echo "FAIL: $label" >&2
    echo "      want: $expected" >&2
    echo "      got:  $got" >&2
    failures=$((failures + 1))
  fi
}

# Last field is the return code: 10 = taken from the file, 0 = nothing to do.
check "plain value"                      'ABC123|10'   'ASC_KEY_ID=ABC123\n'
check "export prefix"                    'ABC123|10'   'export ASC_KEY_ID=ABC123\n'
check "double quotes are removed"        'ABC 123|10'  'ASC_KEY_ID="ABC 123"\n'
check "single quotes are removed"        'ABC123|10'   "ASC_KEY_ID='ABC123'\n"
check "mismatched quotes are not quotes" "\"ABC'|10"   "ASC_KEY_ID=\"ABC'\n"
check "last assignment wins"             'second|10'   'ASC_KEY_ID=first\nASC_KEY_ID=second\n'
check "CRLF line endings"                'ABC123|10'   'ASC_KEY_ID=ABC123\r\n'
check "no trailing newline"              'ABC123|10'   'ASC_KEY_ID=ABC123'
check "surrounding whitespace"           'ABC123|10'   '  ASC_KEY_ID=ABC123   \n'
check "inline comment on a bare value"   'ABC123|10'   'ASC_KEY_ID=ABC123 # the key\n'
check "# inside quotes is kept"          'AB # C|10'   'ASC_KEY_ID="AB # C"\n'
check "empty value is not exported"      'UNSET|0'     'ASC_KEY_ID=\n'
check "empty quotes are not exported"    'UNSET|0'     'ASC_KEY_ID=""\n'
check "commented-out line is ignored"    'UNSET|0'     '# ASC_KEY_ID=ABC123\n'
check "a longer name is not matched"     'UNSET|0'     'XASC_KEY_ID=nope\nASC_KEY_ID_OLD=nope\n'
check "another key is not loaded"        'UNSET|0'     'DATABASE_URL=postgres://x\n'
check "missing file is fine"             'UNSET|0'     ''
rm -f "$work/.env"
got=$(env -i PATH="$PATH" HOME="$work" bash --noprofile --norc -c '. "$1"; rc=0; load_env_key ASC_KEY_ID "$2" || rc=$?; echo "${ASC_KEY_ID-UNSET}|$rc"' _ "$here/load-env-key.sh" "$work/does-not-exist")
[ "$got" = 'UNSET|0' ] || { echo "FAIL: nonexistent file: $got" >&2; failures=$((failures + 1)); }

# The environment wins, and the file is not consulted for it.
check "environment beats the file"       'fromenv|0'   'ASC_KEY_ID=fromfile\n' ASC_KEY_ID=fromenv
check "an empty variable does not"       'fromfile|10' 'ASC_KEY_ID=fromfile\n' ASC_KEY_ID=

# A value is data. Command substitution, backticks and $VAR must come through as
# the literal text, and must not run.
canary="$work/canary"
check "command substitution is literal"  "\$(touch $canary)|10" "ASC_KEY_ID=\$(touch $canary)\n"
check "backticks are literal"            "\`touch $canary\`|10"  "ASC_KEY_ID=\`touch $canary\`\n"
check "\$VAR is not expanded"            '$HOME/x|10'  'ASC_KEY_ID=$HOME/x\n'
if [ -e "$canary" ]; then
  echo "FAIL: a value in the file was executed" >&2
  failures=$((failures + 1))
fi

# Under `set -e` a caller that does not guard the return code dies on the 10.
printf 'ASC_KEY_ID=ABC123\n' >"$work/.env"
if env -i PATH="$PATH" bash --noprofile --norc -c 'set -e; . "$1"; load_env_key ASC_KEY_ID "$2"; echo survived' _ "$here/load-env-key.sh" "$work/.env" >/dev/null 2>&1; then
  echo "FAIL: expected an unguarded call to end a set -e caller (the header says it does)" >&2
  failures=$((failures + 1))
fi

# And bash 3.2, which is what a Mac without Homebrew bash runs this under.
if [ -x /bin/bash ] && [ "$(/bin/bash -c 'echo ${BASH_VERSINFO[0]}')" = 3 ]; then
  got=$(env -i PATH="$PATH" HOME="$work" /bin/bash -c '. "$1"; rc=0; load_env_key ASC_KEY_ID "$2" || rc=$?; echo "${ASC_KEY_ID-UNSET}|$rc"' _ "$here/load-env-key.sh" "$work/.env")
  [ "$got" = 'ABC123|10' ] || { echo "FAIL: under /bin/bash 3.2: $got" >&2; failures=$((failures + 1)); }
  echo "also ran under /bin/bash 3.2"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
