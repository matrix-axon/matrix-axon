#!/usr/bin/env bash
# Build the smoke-run status message and hand it to send-matrix-notification.sh.
#
# The run's identity comes from the default GitHub Actions environment, so a call
# site only has to pass what Actions does not already provide: which lane ran,
# which job is reporting, and how it ended. That keeps the message template in
# one place instead of once per notifying job.

set -euo pipefail

: "${JOB_LABEL:?JOB_LABEL is empty}"
: "${STATUS:?STATUS is empty}"

# Not required: a job that dies before it resolves its lane still needs to be
# able to report that, and that is the case where the alert matters most.
SMOKE_LANE="${SMOKE_LANE:-unresolved}"
: "${GITHUB_SERVER_URL:?GITHUB_SERVER_URL is empty}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is empty}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID is empty}"
: "${GITHUB_RUN_ATTEMPT:?GITHUB_RUN_ATTEMPT is empty}"
: "${GITHUB_REF_NAME:?GITHUB_REF_NAME is empty}"
: "${GITHUB_EVENT_NAME:?GITHUB_EVENT_NAME is empty}"
: "${GITHUB_SHA:?GITHUB_SHA is empty}"
: "${GITHUB_ACTOR:?GITHUB_ACTOR is empty}"

run_url="$GITHUB_SERVER_URL/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID"

# Everything here is GitHub-controlled rather than attacker-controlled, and
# every field is single-line, so the inline escape is enough -- there is no
# untrusted block structure to worry about. A branch name can still hold
# Markdown punctuation, though, so it does get escaped. escape_md is shared
# with the untrusted path in scripts/lib/markdown-escape.jq; see there for why
# the two levels are separate. The run URL is GitHub-generated and goes in a
# link target unescaped.
NOTIFY_MESSAGE="$(jq -rn -L "$(dirname "$0")/lib" \
  --arg job "$JOB_LABEL" \
  --arg status "$STATUS" \
  --arg branch "$GITHUB_REF_NAME" \
  --arg trigger "$GITHUB_EVENT_NAME" \
  --arg lane "$SMOKE_LANE" \
  --arg commit "$GITHUB_SHA" \
  --arg actor "$GITHUB_ACTOR" \
  --arg attempt "$GITHUB_RUN_ATTEMPT" \
  --arg run_url "$run_url" \
  '
  include "markdown-escape";
  "# Smoke Test Run\n"
  + "## " + ($job | escape_md) + ": " + ($status | escape_md)
  + "  \n**Branch:** " + ($branch | escape_md)
  + "  \n**Trigger:** " + ($trigger | escape_md)
  + "  \n**Lane:** " + ($lane | escape_md)
  + "  \n**Commit:** " + ($commit | escape_md)
  + "  \n**Actor:** " + ($actor | escape_md)
  + "  \n**Attempt:** " + ($attempt | escape_md)
  + "  \n**Run:** [" + $run_url + "](" + $run_url + ")"
  ')"
export NOTIFY_MESSAGE

exec "$(dirname "$0")/send-matrix-notification.sh"
