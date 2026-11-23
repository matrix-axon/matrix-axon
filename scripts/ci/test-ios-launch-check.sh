#!/usr/bin/env bash
#
# Tests for the parts of ios-launch-check.sh that can be tried without building the app:
# that evidence-gathering cannot end the script, and that it cleans up only the
# simulator it made. They run the script's own functions, cut out of it, under its own
# strict mode, with `xcrun` and `log` stubbed so that nothing touches a real simulator.
#
#   scripts/ci/test-ios-launch-check.sh
#   TEST_BASH=/bin/bash scripts/ci/test-ios-launch-check.sh     # macOS's bash 3.2
set -euo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
script="$here/ios-launch-check.sh"
test_bash=${TEST_BASH:-bash}
work=$(mktemp -d "${TMPDIR:-/tmp}/test-ios-launch-check.XXXXXX")
failures=0
trap 'chmod -R u+rwx "$work" 2>/dev/null || true; rm -rf "$work"' EXIT

fail() {
  echo "FAIL: $1" >&2
  shift
  for line in "$@"; do echo "      $line" >&2; done
  failures=$((failures + 1))
}
pass() { echo "ok:   $1"; }

# The functions under test, as they are in the script.
functions="$work/functions.sh"
for name in with_timeout remove_created_sim keep_simulator_diagnostics; do
  sed -n "/^$name() {/,/^}/p" "$script" >>"$functions"
done
grep -q '^keep_simulator_diagnostics() {' "$functions" || { echo "could not cut the functions out of $script" >&2; exit 2; }

# Stubs: xcrun records what it was asked and, when told to, fails; log always fails.
stubs="$work/stubs"
mkdir -p "$stubs"
cat >"$stubs/xcrun" <<'STUB'
#!/bin/sh
echo "$*" >>"$STUB_CALLS"
[ -z "${STUB_XCRUN_FAILS:-}" ] || exit 1
echo "simctl output for: $*"
STUB
printf '#!/bin/sh\nexit 1\n' >"$stubs/log"
chmod +x "$stubs/xcrun" "$stubs/log"

# run_case <name> <env assignments...> -- <shell code>: the functions, then the code, under
# set -euo pipefail; prints "survived" if the shell got to the end of it.
run_case() {
  local out
  out=$(env -i PATH="$stubs:$PATH" HOME="$case_home" STUB_CALLS="$work/calls" "${@:2}" \
    "$test_bash" -c "set -euo pipefail; . '$functions'; $CASE_CODE; echo survived" 2>/dev/null) || true
  printf '%s' "$out"
}

# --- remove_created_sim -------------------------------------------------------------
: >"$work/calls"
case_home="$work/home"; mkdir -p "$case_home"
CASE_CODE='created_sim=""; remove_created_sim'
out=$(run_case x)
if [ "$out" = survived ] && [ ! -s "$work/calls" ]; then
  pass "remove_created_sim does nothing when no simulator was made"
else
  fail "remove_created_sim touched a simulator when none was made" "calls: $(cat "$work/calls")"
fi

: >"$work/calls"
CASE_CODE='created_sim="MADE-BY-US"; remove_created_sim'
out=$(run_case x)
expected=$(printf 'simctl shutdown MADE-BY-US\nsimctl delete MADE-BY-US')
if [ "$out" = survived ] && [ "$(cat "$work/calls")" = "$expected" ]; then
  pass "remove_created_sim shuts down and deletes exactly the simulator it made"
else
  fail "remove_created_sim did the wrong thing" "calls: $(cat "$work/calls")"
fi

# --- keep_simulator_diagnostics -----------------------------------------------------
# The variables the function reads, as the script has them by then.
setup='facts="some facts"; udid="U"; bootstatus_log="'$work'/no-such-bootstatus.log"; started_marker="'$work'/marker"; touch "$started_marker"'

# 1. No DiagnosticReports folder at all (the review's case: find fails under pipefail).
: >"$work/calls"; case_home="$work/empty-home"; mkdir -p "$case_home"
CASE_CODE="$setup; diagnostics='$work/out1'; keep_simulator_diagnostics"
out=$(run_case x)
if [ "$out" = survived ] && [ -f "$work/out1/app-facts.txt" ]; then
  pass "a missing DiagnosticReports folder does not end the script, and the facts are still kept"
else
  fail "a missing DiagnosticReports folder ended the script or lost the facts" "output: [$out]"
fi

# 2. A destination that cannot be written.
: >"$work/calls"; mkdir -p "$work/locked"; chmod 555 "$work/locked"
CASE_CODE="$setup; diagnostics='$work/locked/inside'; keep_simulator_diagnostics"
out=$(run_case x)
chmod 755 "$work/locked"
if [ "$out" = survived ]; then
  pass "an unwritable destination does not end the script"
else
  fail "an unwritable destination ended the script" "output: [$out]"
fi

# 3. Every command failing at once.
: >"$work/calls"
CASE_CODE="$setup; diagnostics='$work/out3'; keep_simulator_diagnostics"
out=$(run_case x STUB_XCRUN_FAILS=1)
if [ "$out" = survived ]; then
  pass "failing simctl and log calls do not end the script"
else
  fail "failing simctl or log calls ended the script" "output: [$out]"
fi

# 4. Nothing to do without a destination.
CASE_CODE="$setup; diagnostics=''; keep_simulator_diagnostics"
out=$(run_case x)
if [ "$out" = survived ] && [ ! -e "$work/out4" ]; then
  pass "with no destination it does nothing"
else
  fail "with no destination it did something" "output: [$out]"
fi

# 5. The working case still keeps what it should.
: >"$work/calls"
CASE_CODE="$setup; diagnostics='$work/out5'; keep_simulator_diagnostics"
out=$(run_case x)
if [ "$out" = survived ] && [ -s "$work/out5/app-facts.txt" ] && [ -s "$work/out5/simulators.txt" ]; then
  pass "the device and runtime lists and the facts are kept"
else
  fail "the working case kept too little" "output: [$out]" "files: $(ls "$work/out5" 2>&1 | tr '\n' ' ')"
fi

# --- what the script does not do ----------------------------------------------------
if grep -q 'simctl uninstall' "$script"; then
  fail "the script uninstalls the app from a simulator, which removes its data from a simulator somebody may be using"
else
  pass "the script never uninstalls anything"
fi
if grep -q 'sharing-simulator-device.mjs" pick "\$target_ios" --new' "$script"; then
  pass "the script asks for a new simulator, never an installed one"
else
  fail "the script does not ask the picker for a new simulator (--new)"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures failed" >&2
  exit 1
fi
echo "all passed"
