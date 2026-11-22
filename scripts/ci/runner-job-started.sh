#!/usr/bin/env bash
# Job-started hook for the self-hosted runners (ADR 0141, decision 7).
#
# This copy is the reviewed source. Each runner runs a root-owned copy installed
# outside the repository (docs/self-hosted-runners.md), named by
# ACTIONS_RUNNER_HOOK_JOB_STARTED in the runner's root-owned `.env`. The runner
# app runs it before any step of every job and fails the job if it exits non-zero.
# A fork can edit `runs-on`, but not this file, which is why this script, and not
# pick-runner.yml, is the boundary.
#
# It allows only jobs from ALLOWED_REPOSITORY whose event is push, schedule,
# workflow_dispatch, or a pull_request whose head is that same repository, and
# never a job Dependabot started: the same rule as check 2 in
# scripts/ci/pick-runner.sh. Anything it cannot read or does not recognise is
# refused.
#
#   runner-job-started.sh            # as the hook; the runner passes no arguments
#   runner-job-started.sh owner/repo # tests only: allow another repository
#
# The runner gives the hook its own process environment (the root-owned `.env`)
# plus the github and runner contexts as GITHUB_* and RUNNER_* variables. A
# workflow's `env:` does not reach it: in actions/runner, JobHookProvider builds
# the hook's handler with an empty environment, and only StepsRunner merges the
# workflow's env into steps. The allowed repository is still an argument, not an
# environment variable, so nothing a job controls can change it.

set -uo pipefail

ALLOWED_REPOSITORY=${1:-matrix-axon/matrix-axon}

refuse() {
  echo "::error title=Self-hosted runner::refusing this job: $1. This runner takes only trusted jobs from $ALLOWED_REPOSITORY (ADR 0141)."
  exit 1
}

command -v jq >/dev/null 2>&1 || refuse "jq is not installed on this runner, so the event cannot be checked"
[[ -n "${GITHUB_EVENT_PATH:-}" && -r "${GITHUB_EVENT_PATH}" ]] || refuse "the event payload (GITHUB_EVENT_PATH) is not readable"
[[ -n "${GITHUB_EVENT_NAME:-}" ]] || refuse "GITHUB_EVENT_NAME is not set"

field() { jq -r "$1 // empty" "$GITHUB_EVENT_PATH" 2>/dev/null; }

repo=$(field '.repository.full_name')
[[ "$repo" == "$ALLOWED_REPOSITORY" ]] || refuse "repository '${repo:-unknown}'"
# The environment's name for the repository must agree with the payload's.
[[ "${GITHUB_REPOSITORY:-$repo}" == "$ALLOWED_REPOSITORY" ]] || refuse "repository '${GITHUB_REPOSITORY}'"

for actor in "$(field '.sender.login')" "${GITHUB_ACTOR:-}" "${GITHUB_TRIGGERING_ACTOR:-}"; do
  [[ "$actor" == "dependabot[bot]" ]] && refuse "Dependabot jobs run on hosted runners"
done

case "$GITHUB_EVENT_NAME" in
  push | schedule | workflow_dispatch) ;;
  pull_request)
    head=$(field '.pull_request.head.repo.full_name')
    [[ "$head" == "$ALLOWED_REPOSITORY" ]] || refuse "pull request from '${head:-a deleted repository}'"
    ;;
  *) refuse "event '$GITHUB_EVENT_NAME'" ;;
esac

echo "Self-hosted runner: allowed $GITHUB_EVENT_NAME job from $repo."
exit 0
