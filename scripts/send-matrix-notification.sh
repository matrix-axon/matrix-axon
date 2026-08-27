#!/usr/bin/env bash
# Send a prepared Markdown message to a Matrix room without exposing credentials.
#
# Transport only: the caller builds NOTIFY_MESSAGE, this posts it. The receiving
# end is the Maubot webhook plugin (https://github.com/jkhsjdhjs/maubot-webhook),
# which picks the room and renders the Markdown, so neither the room ID nor a
# homeserver access token needs to exist in CI.
#
# Configure the webhook in the Maubot dashboard as below, and set the NOTIFY_URL
# secret to the Maubot-supplied URL for it.
#
#   path: /send
#   method: POST
#   room: !room-identifier-here
#   message: |
#       {{ json.body }}
#   message_format: markdown
#   message_type: m.notice
#   auth_type: Basic
#   auth_token: NOTIFY_TOKEN from GH secret, as <username>:<password>
#   force_json: false
#   ignore_empty_messages: true
#
# The plugin's Jinja environment has autoescape on, so raw HTML in the message
# is escaped for us -- but Markdown is not. Callers MUST run any untrusted text
# (issue and pull-request titles, bodies and authors) through `escape_untrusted`
# from scripts/lib/markdown-escape.jq before interpolating it, or an attacker
# can forge CI-looking notices: links that go somewhere else, or headings
# matching the ones the smoke notifications use.
#
# This script cannot enforce that, because escaping has to happen per field
# before the message is composed and all it receives is the finished string.
# What it can do is make the rule cheap to follow and hard to get subtly wrong:
# the helpers live in one module rather than being pasted per caller, and
# scripts/check-notify-escaping.sh pins what they must catch.
#
# Do NOT HTML-escape on this side: the plugin already does, and pre-escaping
# shows up in the room as literal `&amp;`.
#
# Unlike the Matrix send API this replaces, the webhook takes no transaction ID,
# so it cannot deduplicate: re-running a workflow posts the message again. The
# run attempt is in the message so duplicates are at least distinguishable.

set -euo pipefail

: "${NOTIFY_URL:?NOTIFY_URL is empty}"
: "${NOTIFY_TOKEN:?NOTIFY_TOKEN is empty}"
: "${NOTIFY_MESSAGE:?NOTIFY_MESSAGE is empty}"

# The plugin requires basic-auth credentials as <username>:<password> and would
# otherwise answer an opaque 401. Fail with a clear reason instead.
case "$NOTIFY_TOKEN" in
  *:*) ;;
  *)
    echo "NOTIFY_TOKEN must be <username>:<password> for HTTP basic auth" >&2
    exit 1
    ;;
esac

# NOTIFY_URL is opaque to this script; refuse to send basic-auth credentials in
# the clear if the secret is ever set to an http:// endpoint.
case "$NOTIFY_URL" in
  https://*) ;;
  *)
    echo "NOTIFY_URL must be an https:// endpoint" >&2
    exit 1
    ;;
esac

message="$(jq -n --arg body "$NOTIFY_MESSAGE" '{body: $body}')"

# --fail, not --fail-with-body: the plugin's 500 body echoes the message and the
# room ID, and the room ID is no longer a registered secret, so Actions would not
# mask it out of a public log. Report the status code instead.
#
# The exit status is captured rather than left to `set -e`: curl exits 22 on an
# HTTP error, which would kill the script at this assignment and skip the line
# below -- losing the status code in exactly the case worth reporting. curl
# still writes %{http_code} on a failed request, so it is available either way.
rc=0
status="$(curl --fail --silent --show-error --max-time 15 \
  --proto '=https' \
  -H "Content-Type: application/json" \
  --user "$NOTIFY_TOKEN" \
  --output /dev/null \
  --write-out '%{http_code}' \
  "$NOTIFY_URL" \
  --data "$message")" || rc=$?

if [ "$rc" -ne 0 ]; then
  echo "Notification webhook failed: curl exit $rc, HTTP ${status:-none}" >&2
  exit "$rc"
fi

echo "Notification webhook returned HTTP $status"
