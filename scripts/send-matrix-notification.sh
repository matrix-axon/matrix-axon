#!/usr/bin/env bash
# Send a prepared Markdown-formatted Matrix message without exposing credentials.
# This script uses the Maubot webhook plugin https://github.com/jkhsjdhjs/maubot-webhook
# webhook should be configured as shown below in Maubot dashboard. 
# set NOTIFY_URL GH secret to the Maubot-supplied path for the webhook
#
# path: /send
# method: POST
# room: !room-identifier-here
# message: |
#     {{ json.body }}
# message_format: markdown
# message_type: m.notice
# auth_type: Basic
# auth_token: NOTIFY_TOKEN from GH secret
# force_json: false
# ignore_empty_messages: false

set -euo pipefail

: "${NOTIFY_URL:?NOTIFY_URL is empty}"
: "${NOTIFY_TOKEN:?NOTIFY_TOKEN is empty}"
: "${MATRIX_FORMATTED_BODY:?MATRIX_FORMATTED_BODY is empty}"

echo "Sending message to Matrix room..."

message="$(jq -n --arg body "$MATRIX_FORMATTED_BODY" '{body: $body}')"

curl --fail-with-body --silent --show-error --max-time 15 \
  -X POST \
  -H "Content-Type: application/json" \
  -u "${NOTIFY_TOKEN}" \
  "${NOTIFY_URL}" \
  --data "${message}"
