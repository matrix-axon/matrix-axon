#!/usr/bin/env bash
# Send a prepared plain-text/HTML Matrix message without exposing credentials.
set -euo pipefail

: "${NOTIFY_HOMESERVER:?NOTIFY_HOMESERVER is empty}"
: "${NOTIFY_ROOM:?NOTIFY_ROOM is empty}"
: "${NOTIFY_TOKEN:?NOTIFY_TOKEN is empty}"
: "${MATRIX_BODY:?MATRIX_BODY is empty}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID is empty}"
: "${GITHUB_RUN_ATTEMPT:?GITHUB_RUN_ATTEMPT is empty}"

homeserver="${NOTIFY_HOMESERVER%/}"
room="$(jq -rn --arg room "$NOTIFY_ROOM" '$room | @uri')"

if [ -n "${MATRIX_FORMATTED_BODY:-}" ]; then
  message="$(jq -n \
    --arg body "$MATRIX_BODY" \
    --arg formatted_body "$MATRIX_FORMATTED_BODY" \
    '{
      msgtype: "m.text",
      format: "org.matrix.custom.html",
      body: $body,
      formatted_body: $formatted_body
    }')"
else
  message="$(jq -n --arg body "$MATRIX_BODY" '{msgtype: "m.text", body: $body}')"
fi

curl --fail-with-body --silent --show-error --max-time 15 \
  -X PUT \
  "$homeserver/_matrix/client/v3/rooms/$room/send/m.room.message/${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}" \
  -H "Authorization: Bearer $NOTIFY_TOKEN" \
  -H "Content-Type: application/json" \
  --data "$message"
