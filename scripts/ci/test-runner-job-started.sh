#!/usr/bin/env bash
# Tests for runner-job-started.sh, the self-hosted runners' job-started hook.
#
#   scripts/ci/test-runner-job-started.sh
#
# Bash 3.2 is enough, as on the macOS runner the hook also runs on.
set -uo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
hook="$here/runner-job-started.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failures=0

# check <allow|refuse> <name> <event-name> <payload JSON> [VAR=value...]
check() {
  local want=$1 name=$2 event=$3 payload=$4 rc
  shift 4
  printf '%s' "$payload" >"$work/event.json"
  env -i PATH="$PATH" GITHUB_EVENT_PATH="$work/event.json" GITHUB_EVENT_NAME="$event" \
    GITHUB_REPOSITORY=o/r GITHUB_ACTOR=someone "$@" \
    bash "$hook" o/r >"$work/log" 2>&1
  rc=$?
  if [[ $want == allow && $rc != 0 ]]; then
    echo "FAIL: $name: refused: $(cat "$work/log")" >&2
    failures=$((failures + 1))
  elif [[ $want == refuse && $rc == 0 ]]; then
    echo "FAIL: $name: allowed" >&2
    failures=$((failures + 1))
  fi
}

base='"repository":{"full_name":"o/r"},"sender":{"login":"someone"}'
pr() { printf '{%s,"pull_request":{"head":{"repo":%s}}}' "$base" "$1"; }

check allow "push" push "{$base}"
check allow "schedule" schedule "{$base}"
check allow "workflow_dispatch" workflow_dispatch "{$base}"
check allow "same-repo PR" pull_request "$(pr '{"full_name":"o/r"}')"

check refuse "fork PR" pull_request "$(pr '{"full_name":"fork/r"}')"
check refuse "PR from a deleted fork" pull_request "$(pr null)"
check refuse "PR without pull_request" pull_request "{$base}"
check refuse "pull_request_target" pull_request_target "$(pr '{"full_name":"o/r"}')"
check refuse "workflow_run" workflow_run "{$base}"
check refuse "issue_comment" issue_comment "{$base}"
check refuse "another repository" push '{"repository":{"full_name":"x/y"},"sender":{"login":"someone"}}'
check refuse "environment names another repository" push "{$base}" GITHUB_REPOSITORY=x/y
check refuse "Dependabot as sender" pull_request \
  '{"repository":{"full_name":"o/r"},"sender":{"login":"dependabot[bot]"},"pull_request":{"head":{"repo":{"full_name":"o/r"}}}}'
check refuse "Dependabot as actor" push "{$base}" GITHUB_ACTOR='dependabot[bot]'
check refuse "Dependabot as triggering actor" push "{$base}" GITHUB_TRIGGERING_ACTOR='dependabot[bot]'
check refuse "malformed payload" push 'not json'
check refuse "empty event name" "" "{$base}"
check refuse "unreadable payload" push "{$base}" GITHUB_EVENT_PATH="$work/missing.json"

# With no argument, the hook allows only this repository, whatever the job's env says.
printf '{%s}' '"repository":{"full_name":"o/r"},"sender":{"login":"someone"}' >"$work/event.json"
if env -i PATH="$PATH" GITHUB_EVENT_PATH="$work/event.json" GITHUB_EVENT_NAME=push \
  GITHUB_REPOSITORY=o/r ALLOWED_REPOSITORY=o/r bash "$hook" >/dev/null 2>&1; then
  echo "FAIL: the default allowed repository can be changed from the environment" >&2
  failures=$((failures + 1))
fi

if ((failures > 0)); then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "test-runner-job-started: all passed"
