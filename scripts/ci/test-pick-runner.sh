#!/usr/bin/env bash
# Tests for pick-runner.sh, against a fake `gh` that serves canned API responses.
#
#   scripts/ci/test-pick-runner.sh
#
# Needs bash 4+ and jq, which every Linux runner and the hosted macOS images have.
set -uo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
pick="$here/pick-runner.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

fail() {
  echo "FAIL: $1" >&2
  failures=$((failures + 1))
}

# The fake gh answers `gh api <path>` from $FIXTURES/<path with / ? = & made _>.
# A matching .status file holds an HTTP status to fail with instead.
mkdir -p "$work/bin"
cat >"$work/bin/gh" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == api ]] || exit 99
name=$(printf '%s' "$2" | tr '/?=&' '____')
if [[ -f "$FIXTURES/$name.status" ]]; then
  echo "gh: Something failed (HTTP $(cat "$FIXTURES/$name.status"))" >&2
  exit 1
fi
[[ -f "$FIXTURES/$name" ]] || { echo "gh: unexpected request $2" >&2; exit 98; }
cat "$FIXTURES/$name"
EOF
chmod +x "$work/bin/gh"

# Runs a mode with a clean environment plus the given VAR=value pairs. Leaves the
# step outputs in $work/out, the stdout in $work/log, and the exit status in $rc.
run() {
  local mode=$1
  shift
  : >"$work/out"
  : >"$work/summary"
  env -i PATH="$work/bin:$PATH" HOME="$HOME" FIXTURES="$work/fx" \
    GITHUB_OUTPUT="$work/out" GITHUB_STEP_SUMMARY="$work/summary" "$@" \
    bash "$pick" "$mode" >"$work/log" 2>&1
  rc=$?
}

output() { sed -n "s/^$1=//p" "$work/out"; }

expect() { # name key value
  local got
  got=$(output "$2")
  [[ "$got" == "$3" ]] || fail "$1: $2 is '$got', expected '$3' (log: $(tr '\n' ' ' <"$work/log"))"
}

expect_warning() { # name yes|no
  if grep -q '^::warning' "$work/log"; then
    [[ $2 == yes ]] || fail "$1: unexpected warning: $(grep '^::warning' "$work/log")"
  else
    [[ $2 == no ]] || fail "$1: expected a warning"
  fi
}

expect_each_output_once() { # name
  local key
  for key in linux macos; do
    [[ $(grep -c "^$key=" "$work/out") == 1 ]] || fail "$1: $key written $(grep -c "^$key=" "$work/out") times"
  done
  ((rc == 0)) || fail "$1: exited $rc"
}

# --- precheck -------------------------------------------------------------------
trusted=(REPOSITORY=o/r HAS_KEY=true CLIENT_ID=Iv1.abc ACTOR=someone TRIGGERING_ACTOR=someone)

run precheck "${trusted[@]}" EVENT_NAME=push
expect "push" proceed true
expect_warning "push" no

for ev in schedule workflow_dispatch; do
  run precheck "${trusted[@]}" EVENT_NAME=$ev
  expect "$ev" proceed true
done

run precheck "${trusted[@]}" EVENT_NAME=pull_request HEAD_REPOSITORY=o/r
expect "same-repo PR" proceed true

run precheck "${trusted[@]}" EVENT_NAME=pull_request HEAD_REPOSITORY=fork/r
expect "fork PR" proceed false
expect_warning "fork PR" no

run precheck "${trusted[@]}" EVENT_NAME=pull_request
expect "PR from a deleted fork" proceed false

run precheck "${trusted[@]}" EVENT_NAME=pull_request_target HEAD_REPOSITORY=o/r
expect "pull_request_target" proceed false

run precheck REPOSITORY=o/r EVENT_NAME=pull_request HEAD_REPOSITORY=o/r ACTOR='dependabot[bot]' HAS_KEY=false
expect "Dependabot PR" proceed false
expect_warning "Dependabot PR" no

run precheck "${trusted[@]}" EVENT_NAME=push TRIGGERING_ACTOR='dependabot[bot]'
expect "Dependabot as triggering actor" proceed false

run precheck "${trusted[@]}" EVENT_NAME=push KILL_SWITCH=off
expect "kill switch" proceed false
expect_warning "kill switch" no

run precheck REPOSITORY=o/r EVENT_NAME=push HAS_KEY=false CLIENT_ID=Iv1.abc
expect "missing key" proceed false
expect_warning "missing key" yes

run precheck REPOSITORY=o/r EVENT_NAME=push HAS_KEY=true
expect "missing client id" proceed false
expect_warning "missing client id" yes

# --- decide ---------------------------------------------------------------------
fx="$work/fx"
mkdir -p "$fx"
runners() { printf '%s' "$1" >"$fx/orgs_o_actions_runners_per_page_100"; }
runs() { # status, ids...
  local status=$1
  shift
  jq -cn --args '{workflow_runs: [$ARGS.positional[] | {id: tonumber}]}' "$@" \
    >"$fx/repos_o_r_actions_runs_status_${status}_per_page_100"
}
jobs() { printf '%s' "$2" >"$fx/repos_o_r_actions_runs_$1_jobs_filter_latest_per_page_100"; }
reset_fx() {
  rm -rf "$fx"
  mkdir -p "$fx"
  runs queued
  runs in_progress
}

linux_idle='{"runners":[{"status":"online","busy":false,"labels":[{"name":"self-hosted"},{"name":"axon-linux"}]}]}'
decide_ok=(PROCEED=true TOKEN_OUTCOME=success GH_TOKEN=t ORG=o REPOSITORY=o/r RUN_ID=1
  LINUX_FALLBACK=ubuntu-latest MACOS_FALLBACK=macos-26)
self_linux='["self-hosted","axon-linux"]'

reset_fx
runners "$linux_idle"
run decide "${decide_ok[@]}"
expect "idle linux runner" linux "$self_linux"
expect "idle linux runner" macos '"macos-26"'
expect_each_output_once "idle linux runner"
expect_warning "idle linux runner" no

run decide "${decide_ok[@]}" LINUX_FALLBACK=ubuntu-24.04 PROCEED=false PRECHECK_REASON=x
expect "precheck said no" linux '"ubuntu-24.04"'
expect_each_output_once "precheck said no"

run decide "${decide_ok[@]}" TOKEN_OUTCOME=failure GH_TOKEN=
expect "token failed" linux '"ubuntu-latest"'
expect_warning "token failed" yes
expect_each_output_once "token failed"

reset_fx
runners '{"runners":[{"status":"online","busy":true,"labels":[{"name":"axon-linux"}]}]}'
run decide "${decide_ok[@]}"
expect "busy runner" linux '"ubuntu-latest"'

reset_fx
runners '{"runners":[{"status":"offline","busy":false,"labels":[{"name":"axon-linux"}]}]}'
run decide "${decide_ok[@]}"
expect "offline runner" linux '"ubuntu-latest"'

reset_fx
runners '{"runners":[{"status":"online","busy":false,"labels":[{"name":"self-hosted"}]}]}'
run decide "${decide_ok[@]}"
expect "runner without the pool label" linux '"ubuntu-latest"'

# One idle runner, and a job in another run already waiting for it.
reset_fx
runners "$linux_idle"
runs queued 7
jobs 7 '{"jobs":[{"status":"queued","labels":["self-hosted","axon-linux"]}]}'
run decide "${decide_ok[@]}"
expect "job already waiting" linux '"ubuntu-latest"'
expect_warning "job already waiting" no

# A waiting job for the other pool, a running job and a finished one do not count.
reset_fx
runners "$linux_idle"
runs in_progress 8
jobs 8 '{"jobs":[{"status":"queued","labels":["axon-macos"]},{"status":"in_progress","labels":["axon-linux"]},{"status":"completed","labels":["axon-linux"]}]}'
run decide "${decide_ok[@]}"
expect "other jobs" linux "$self_linux"

# This run's own jobs are not counted.
reset_fx
runners "$linux_idle"
runs in_progress 1
run decide "${decide_ok[@]}"
expect "own run skipped" linux "$self_linux"

# Two idle instances and one waiting job: one instance is still free.
reset_fx
runners '{"runners":[{"status":"online","busy":false,"labels":[{"name":"axon-linux"}]},{"status":"online","busy":false,"labels":[{"name":"axon-linux"}]}]}'
runs queued 7
jobs 7 '{"jobs":[{"status":"queued","labels":["self-hosted","axon-linux"]}]}'
run decide "${decide_ok[@]}"
expect "two idle, one waiting" linux "$self_linux"

reset_fx
runners '{"runners":[{"status":"online","busy":false,"labels":[{"name":"axon-macos"}]}]}'
run decide "${decide_ok[@]}"
expect "idle mac" macos '["self-hosted","axon-macos"]'
expect "idle mac" linux '"ubuntu-latest"'

for status in 401 403; do
  reset_fx
  echo $status >"$fx/orgs_o_actions_runners_per_page_100.status"
  run decide "${decide_ok[@]}"
  expect "runners API $status" linux '"ubuntu-latest"'
  expect_warning "runners API $status" yes
  expect_each_output_once "runners API $status"
done

reset_fx
echo 502 >"$fx/orgs_o_actions_runners_per_page_100.status"
run decide "${decide_ok[@]}"
expect "runners API 502" linux '"ubuntu-latest"'
expect_warning "runners API 502" no

reset_fx
runners "$linux_idle"
runs queued 7
echo 500 >"$fx/repos_o_r_actions_runs_7_jobs_filter_latest_per_page_100.status"
run decide "${decide_ok[@]}"
expect "jobs API error" linux '"ubuntu-latest"'
expect_each_output_once "jobs API error"

reset_fx
runners 'not json'
run decide "${decide_ok[@]}"
expect "malformed runners response" linux '"ubuntu-latest"'
expect_each_output_once "malformed runners response"

run bogus
((rc == 2)) || fail "unknown mode: exit $rc, expected 2"

if ((failures > 0)); then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "test-pick-runner: all passed"
