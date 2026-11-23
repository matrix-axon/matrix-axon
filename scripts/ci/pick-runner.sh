#!/usr/bin/env bash
# Chooses each pool's `runs-on` for .github/workflows/pick-runner.yml (ADR 0141,
# decision 1). Two modes, one per step of that workflow:
#
#   pick-runner.sh precheck   checks 1-3: kill switch, trusted event, App configured
#   pick-runner.sh decide     check 4: an idle runner nobody is already waiting for
#
# Both modes read their inputs from the environment (listed under each mode) and
# write step outputs to $GITHUB_OUTPUT and a table to $GITHUB_STEP_SUMMARY.
#
# Neither mode ever exits non-zero, and `decide` writes every pool's output on
# every path, including an unexpected error: the trap below writes whatever the
# outputs hold when the script exits, and they hold the hosted fallback until a
# positive answer replaces them. A failed picker would skip every job that
# `needs` it, which is worse than a slow run.

set -uo pipefail

# Pool name -> runner label. A pool here that a caller does not use costs nothing:
# one runners listing and one jobs scan serve every pool.
POOLS=(linux:axon-linux macos:axon-macos)

out() { printf '%s=%s\n' "$1" "$2" >>"${GITHUB_OUTPUT:-/dev/null}"; }
# shellcheck disable=SC2329 # called from write_outputs, which the EXIT trap runs
summary() { printf '%s\n' "$@" >>"${GITHUB_STEP_SUMMARY:-/dev/null}"; }
warn() { printf '::warning title=Runner picker::%s\n' "$1"; }

# --- precheck -----------------------------------------------------------------
# Env: KILL_SWITCH (vars.SELF_HOSTED_RUNNERS), EVENT_NAME, REPOSITORY,
# HEAD_REPOSITORY (the pull request's head repo, empty for other events), ACTOR,
# TRIGGERING_ACTOR, HAS_KEY ("true" when the private-key secret is non-empty),
# CLIENT_ID (vars.RUNNER_PICKER_CLIENT_ID).
# Outputs: proceed (true|false), reason.
precheck() {
  local proceed=false reason
  if [[ "${KILL_SWITCH:-}" == off ]]; then
    reason="the SELF_HOSTED_RUNNERS variable is off"
  elif ! trusted_event; then
    reason=$untrusted_reason
  elif [[ "${HAS_KEY:-}" != true || -z "${CLIENT_ID:-}" ]]; then
    # Every event that gets here is one GitHub gives secrets to (fork PRs and
    # Dependabot stopped above), so a missing secret or variable is a
    # misconfiguration, not the normal case. Say so on the run's summary.
    reason="the runner picker App is not configured (RUNNER_PICKER_PRIVATE_KEY secret or RUNNER_PICKER_CLIENT_ID variable missing)"
    warn "$reason; using hosted runners"
  else
    proceed=true
    reason="checks 1-3 passed"
  fi
  out proceed "$proceed"
  out reason "$reason"
  echo "precheck: proceed=$proceed ($reason)"
}

untrusted_reason=""
trusted_event() {
  local actor
  for actor in "${ACTOR:-}" "${TRIGGERING_ACTOR:-}"; do
    if [[ "$actor" == "dependabot[bot]" ]]; then
      untrusted_reason="Dependabot runs never use self-hosted runners"
      return 1
    fi
  done
  case "${EVENT_NAME:-}" in
    push | schedule | workflow_dispatch) return 0 ;;
    pull_request)
      if [[ -n "${HEAD_REPOSITORY:-}" && "${HEAD_REPOSITORY}" == "${REPOSITORY:-}" ]]; then
        return 0
      fi
      untrusted_reason="pull request from another repository (${HEAD_REPOSITORY:-unknown})"
      return 1
      ;;
    *)
      untrusted_reason="event '${EVENT_NAME:-}' never uses self-hosted runners"
      return 1
      ;;
  esac
}

# --- decide -------------------------------------------------------------------
# Env: PROCEED and PRECHECK_REASON (precheck's outputs), TOKEN_OUTCOME (the token
# step's outcome: success, failure or skipped), GH_TOKEN (the App token), ORG,
# REPOSITORY, RUN_ID (this run, whose own jobs are not counted), LINUX_FALLBACK,
# MACOS_FALLBACK (hosted images).
# Outputs: one per pool, as JSON for fromJSON().
declare -A choice reason
# shellcheck disable=SC2329 # run by the EXIT trap in decide
write_outputs() {
  local entry pool
  summary "| Pool | runs-on | Why |" "| --- | --- | --- |"
  for entry in "${POOLS[@]}"; do
    pool=${entry%%:*}
    out "$pool" "${choice[$pool]}"
    summary "| $pool | \`${choice[$pool]}\` | ${reason[$pool]} |"
    echo "$pool: ${choice[$pool]} (${reason[$pool]})"
  done
}

fallback_json() {
  local var default
  var="$(printf '%s' "$1" | tr '[:lower:]' '[:upper:]')_FALLBACK"
  # An unset or empty fallback is the caller's mistake; the pool's usual hosted
  # image is still a better answer than an empty runs-on, which fails the job.
  case $1 in
    macos) default=macos-latest ;;
    *) default=ubuntu-latest ;;
  esac
  jq -cn --arg v "${!var:-$default}" '$v'
}

set_all() {
  local entry
  for entry in "${POOLS[@]}"; do reason[${entry%%:*}]=$1; done
}

# Runs `gh api` with a time limit, leaving the response in $api_body. On failure
# it leaves the HTTP status (or a short error) in $api_status and returns 1.
# Globals, not stdout: a caller's $(api ...) would run it in a subshell, and the
# status would never reach api_failed.
api_status=""
api_body=""
api() {
  local err rc
  err=$(mktemp)
  api_body=$(timeout 20 gh api "$@" 2>"$err")
  rc=$?
  if ((rc != 0)); then
    api_status=$(grep -oE 'HTTP [0-9]{3}' "$err" | head -n1)
    [[ -n "$api_status" ]] || api_status="error (exit $rc: $(head -c 200 "$err" | tr '\n' ' '))"
    rm -f "$err"
    return 1
  fi
  rm -f "$err"
}

api_failed() {
  local what=$1
  if [[ "$api_status" == "HTTP 401" || "$api_status" == "HTTP 403" ]]; then
    warn "the runners API rejected the App token ($api_status) while reading $what; using hosted runners"
    set_all "the runners API rejected the token ($api_status)"
  else
    set_all "could not read $what ($api_status)"
  fi
}

decide() {
  local entry pool label
  for entry in "${POOLS[@]}"; do
    pool=${entry%%:*}
    choice[$pool]=$(fallback_json "$pool")
    reason[$pool]="unexpected error"
  done
  trap write_outputs EXIT

  if [[ "${PROCEED:-}" != true ]]; then
    set_all "${PRECHECK_REASON:-precheck did not run}"
    return
  fi
  if [[ "${TOKEN_OUTCOME:-}" != success || -z "${GH_TOKEN:-}" ]]; then
    warn "could not mint the runner picker App token; using hosted runners"
    set_all "could not mint the App token"
    return
  fi

  local runners runs jobs=() run_id status
  api "orgs/$ORG/actions/runners?per_page=100" || {
    api_failed "the org's runners"
    return
  }
  runners=$api_body

  # Jobs already waiting for a label, across this repository's unfinished runs.
  # Only jobs that are queued count: an in-progress job holds a runner, which the
  # busy flag already shows.
  for status in queued in_progress; do
    api "repos/$REPOSITORY/actions/runs?status=$status&per_page=100" || {
      api_failed "$status runs"
      return
    }
    runs=$api_body
    while IFS= read -r run_id; do
      [[ -z "$run_id" || "$run_id" == "${RUN_ID:-}" ]] && continue
      api "repos/$REPOSITORY/actions/runs/$run_id/jobs?filter=latest&per_page=100" || {
        api_failed "the jobs of run $run_id"
        return
      }
      jobs+=("$api_body")
    done < <(jq -r '.workflow_runs[].id' <<<"$runs")
  done

  local idle waiting
  for entry in "${POOLS[@]}"; do
    pool=${entry%%:*}
    label=${entry#*:}
    idle=$(jq --arg l "$label" \
      '[.runners[] | select(.status == "online" and (.busy | not) and any(.labels[]; .name == $l))] | length' \
      <<<"$runners") || { reason[$pool]="unexpected runners response"; continue; }
    waiting=0
    if ((${#jobs[@]} > 0)); then
      waiting=$(printf '%s\n' "${jobs[@]}" | jq -s --arg l "$label" \
        '[.[].jobs[] | select(.status == "queued" and any(.labels[]; . == $l))] | length') ||
        { reason[$pool]="unexpected jobs response"; continue; }
    fi
    if ((idle > waiting)); then
      choice[$pool]=$(jq -cn --arg l "$label" '["self-hosted", $l]')
      reason[$pool]="$idle idle runner(s) labelled $label, $waiting job(s) already waiting"
    else
      reason[$pool]="no free runner labelled $label ($idle idle, $waiting job(s) already waiting)"
    fi
  done
}

case "${1:-}" in
  precheck) precheck ;;
  decide) decide ;;
  *)
    echo "usage: $0 precheck|decide" >&2
    exit 2
    ;;
esac
exit 0
