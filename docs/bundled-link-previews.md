# Sending bundled link previews

The server accepts optional `url_previews` on text and media-caption sends, following [ADR 0125](https://github.com/matrix-axon/matrix-axon/blob/a09f547ce18b333c87fcfbc6a1ce1b437f16669c/docs/adr/0125-ios-first-incoming-sharing.md).
Native share intake, image-card rendering and preview generation remain separate stages in [the approved plan](https://github.com/matrix-axon/matrix-axon/blob/a09f547ce18b333c87fcfbc6a1ce1b437f16669c/docs/mobile-incoming-sharing.md).
This API never fetches the linked webpage or an external preview image.

## Request shape

Send `POST /v1/accounts/{account_id}/rooms/{room_id}/send` with:

```json
{
  "body": "Read https://example.org/article",
  "url_previews": [
    {
      "matched_url": "https://example.org/article",
      "title": "Article title",
      "description": "A supplied description",
      "site_name": "Example"
    }
  ]
}
```

For an image, first use the existing account-scoped media upload endpoint with `kind=image`, then include its `image_upload_id` in the preview object.
PNG, JPEG and WebP are accepted after bounded decoding, with at most 2 MiB and 4,000,000 pixels; declared type must match the file signature.
The preview-aware send uploads the thumbnail as persistent Matrix media and includes it in the parent message, without sending a separate image event.
Encrypted rooms use SDK-encrypted image bytes and `matrix:image:encrypted`; unencrypted rooms use an `og:image` MXC reference.
The authenticated media proxy resolves and decrypts these references from retained event content.
Messages include both the MSC4095 proposal and deployed Beeper spellings, with the documented `matched_url` and `beeper:image:encryption` compatibility renames.
The proxy recognizes either spelling; a present standard field remains authoritative, including an empty array.

For `POST .../send-media`, provide the ordinary attachment `upload_id`, its `caption`, and the same optional `url_previews` array.
All upload IDs within one request must be distinct and belong to the path account.
Matched HTTP(S) URLs must occur as complete bare links in the body/caption, contain no username/password, and identify distinct destinations.
Only the documented metadata fields are accepted; external image URLs, arbitrary OG keys and arbitrary event content are rejected.
Omitting `url_previews` or setting it to null retains the original send path; `[]` explicitly sends `m.url_previews: []`.

Encryption decisions preserve positive SDK evidence and fetch current authenticated room state before permitting plaintext thumbnail uploads.
A failed state lookup aborts the send; a room that becomes encrypted during uploads is rejected before event dispatch.
Metadata-only sends make one fresh lookup when encryption is not already known; only sends that uploaded plaintext media need the second check.
Media replies use the same SDK reply builder as ordinary attachments, preserving intentional mentions, self-mention suppression and thread behavior, including explicit opt-out.
The vendored SDK correction in `crates/third-party/encryption-state-monotonic.patch` prevents an older negative state lookup from erasing a known encryption event.

## Budgets and errors

- At most eight previews, with UTF-16 ceilings of 4096 code units for URL, 300 for title, 1000 for description and 120 for site name.
- At most 32 KiB of canonical JSON UTF-8 preview metadata, including resolved image descriptors.
  Preflight reserves 1280 bytes per image for bounded SDK media/encryption metadata; actual content is checked before dispatch too.
- At most 40 KiB of serialized preview-aware content, counting body, formatting, relations and previews together.
  Caption sends reserve 12 KiB within this budget for filename and media metadata.
  This conservative content budget leaves room below Matrix's 65,536-byte event cap for Megolm/base64 expansion and federation envelope/signatures.
  It counts both protocol spellings, so compatibility metadata can tighten the effective preview allowance further.

All limits apply together; no card or field is silently removed or truncated.
Preflight overflow rejects before any upload claim or event dispatch.
`413` codes are `event_too_large` for the content budget or an explicit homeserver size rejection, `preview_too_large` for the preview aggregate budget, and `payload_too_large` for thumbnail bytes.
`400` reports invalid metadata/image/type and `404` reports an unavailable or wrong-account upload.
Preserve the draft for editing on definite validation failure.

An owned task completes or releases its acquired uploads even if the HTTP caller disconnects.
Each account has four upload-bearing preview-send slots with a five-second acquisition deadline; a slow homeserver cannot consume another account's capacity.
Metadata-only text sends, including explicit opt-out, bypass these upload slots.
Decoder slots share the upload pool, and SDK work has the configured upstream-upload deadline for the complete send.
At most two preview uploads run concurrently per send, preserving card order and consuming their owned bytes without copying them.
SDK media-source descriptors are capped at 1024 serialized bytes; actual content is checked before dispatch.
The smaller proven reserves allow eight short image cards on a caption send, while all count, field and aggregate ceilings still apply together.
Failures release acquired uploads; successful sends consume them.
A timeout, disconnect or general upstream error does not prove that the event was not sent.
These APIs have no caller idempotency key: never automatically replay an uncertain send, even if an upload ID becomes available again.
Uploaded Matrix objects can remain orphaned after failure; local staged bytes retain their existing expiry and boot-reconciliation rules.
Logs identify account/room/event/upload IDs, counts, failure status/code and cleanup failure category, never shared URLs, titles, image bytes or encrypted-file keys.

## Upgrade behavior

The bundled-preview migration builds two ordinary GIN indexes on `events` inside the normal migration transaction.
These builds can block writes for their duration on large event stores; plan an appropriate maintenance window.
[Issue 843](https://github.com/matrix-axon/matrix-axon/issues/843) tracks a safe concurrent/prebuild procedure, including interrupted builds and coordination with the migration ledger.
Do not simply precreate these index names: the current migration uses plain `CREATE INDEX` and would reject an existing name.

## Verification and contract fixtures

The portable [contract fixtures](../crates/axon-api/tests/fixtures/link-preview-contract.json) run through the production DTO and preflight in API unit tests.
They cover omitted/empty metadata, image references, unsafe/unmatched URLs, unknown fields, UTF-16 boundaries, escaping, aggregate overflow and combined formatting limits.
The OpenAPI golden test checks schema synchronization; future native/web adapters should run these fixtures against their wire projections.

```sh
cargo test -p axon-core link_preview
cargo test -p axon-sync --lib preview_encryption_refreshes
cargo test -p axon-api --lib bundled_previews
cargo test -p axon-api --test openapi
# Against an isolated local test database:
cargo test -p axon-api --test mutations -- --ignored --test-threads=1
cargo test -p axon-store --test events get_event_by_mxc_url -- --ignored --test-threads=1
cargo test -p axon-server preview_claim -- --ignored --test-threads=1
```

For real-service verification, send text and caption previews into encrypted and unencrypted test rooms, wait for their sync/REST/WS echo, and fetch each resulting preview MXC through the authenticated proxy.
Proxy plaintext must match the original thumbnail, and encrypted-room preview metadata must omit `og:image` and carry `matrix:image:encrypted`.
No extra image message should appear in the timeline.

## Optional manual interoperability check before publication

Run the branch's server against a test homeserver and use dedicated test rooms, one encrypted and one unencrypted.
Use the API request above to supply preview metadata; the current Axon composer does not yet expose the new send fields.
Open both rooms in Axon and a second Matrix client.

1. Send a text link with title/description and a staged thumbnail, then send a file with the same preview in its caption.
   Confirm the fallback text and attachment appear once, and that the thumbnail is attached to its parent rather than appearing as another message.
   If the second client renders bundled preview images, confirm its card loads the thumbnail in both rooms.
   Axon's own preview-image UI is a later stage, so its existing text card is expected here.
2. Reply to a message authored by the second account using `send-media`, with `reply_to` set and either a preview array or `url_previews: []`.
   Confirm the reply targets the right message and the second account receives the expected mention, subject to its notification settings.
   Also reply to your own message and check that it does not self-mention.
3. Edit the request to contain an unrelated `matched_url` or an excessive field.
   Confirm the API rejects it and no message appears; retain the staged image for an explicit corrected attempt.

The automated tests cover encryption descriptors, raw ciphertext, authenticated proxy decryption, bounds, account isolation and cancellation.
This manual pass checks client interoperability and notification presentation.
No iPhone simulator or physical-device test is needed for this server-only stage.

### Copy-and-paste curl requests

Save the following block as a local Bash script and run it with `curl` and `jq` installed.
Set the server URL, account ID and one of your dedicated test room IDs, and choose an existing PNG image no larger than 2 MiB and 4,000,000 pixels.
Run the sequence once per room, changing `ROOM_ID` between the encrypted and unencrypted test rooms.
The token is read without echo and passed to curl through standard input rather than a command-line argument.
Do not enable shell tracing while working with credentials.

```bash
#!/usr/bin/env bash
set -euo pipefail
AXON_URL='http://127.0.0.1:8080'
ACCOUNT_ID='<Axon account UUID>'
ROOM_ID='<Matrix test room ID>'
IMAGE_PATH='/absolute/path/to/test-preview.png'
read -r -s -p 'Axon bearer token: ' AXON_TOKEN </dev/tty
printf '\n'
AXON_URL=${AXON_URL%/}
ROOM_PATH=$(jq -nr --arg room "$ROOM_ID" '$room | @uri')
ROOM_API="$AXON_URL/v1/accounts/$ACCOUNT_ID/rooms/$ROOM_PATH"
LINK='https://example.org/axon-preview-check'

axon_curl() {
  printf 'header = "Authorization: Bearer %s"\n' "$AXON_TOKEN" |
    curl --config - --silent --show-error --fail-with-body \
      --connect-timeout 5 --max-time 90 "$@"
}

stage_preview() {
  axon_curl -X POST -H 'Content-Type: image/png' \
    --data-binary "@$IMAGE_PATH" \
    "$AXON_URL/v1/accounts/$ACCOUNT_ID/media/uploads?kind=image&filename=preview.png" |
    jq -er '.data.upload_id'
}

stage_attachment() {
  axon_curl -X POST -H 'Content-Type: text/plain' \
    --data-binary 'Ordinary Axon interoperability test attachment.' \
    "$AXON_URL/v1/accounts/$ACCOUNT_ID/media/uploads?kind=file&filename=interop-check.txt" |
    jq -er '.data.upload_id'
}

# Text message with supplied metadata and an image.
PREVIEW_ID=$(stage_preview)
JSON=$(jq -n --arg url "$LINK" --arg image "$PREVIEW_ID" '{
  body: ("[Axon preview check] text " + $url),
  url_previews: [{matched_url: $url, title: "Axon preview check",
    description: "Supplied metadata; no webpage fetch", site_name: "Example",
    image_upload_id: $image}]
}')
TEXT_EVENT_ID=$(axon_curl -X POST -H 'Content-Type: application/json' \
  --data-binary "$JSON" "$ROOM_API/send" | jq -er '.data.event_id')
printf 'Text event: %s\n' "$TEXT_EVENT_ID"

# A file with its own caption preview. Every send needs fresh upload IDs.
PREVIEW_ID=$(stage_preview)
ATTACHMENT_ID=$(stage_attachment)
JSON=$(jq -n --arg url "$LINK" --arg image "$PREVIEW_ID" \
  --arg upload "$ATTACHMENT_ID" '{
  upload_id: $upload, caption: ("[Axon preview check] caption " + $url),
  url_previews: [{matched_url: $url, title: "Caption preview", image_upload_id: $image}]
}')
CAPTION_EVENT_ID=$(axon_curl -X POST -H 'Content-Type: application/json' \
  --data-binary "$JSON" "$ROOM_API/send-media" | jq -er '.data.event_id')
printf 'Caption event: %s\n' "$CAPTION_EVENT_ID"

# Default: reply to yourself. Replace REPLY_TO with a second account's
# message ID in this room to check mentions/notifications for another author.
REPLY_TO="$TEXT_EVENT_ID"
ATTACHMENT_ID=$(stage_attachment)
JSON=$(jq -n --arg upload "$ATTACHMENT_ID" --arg reply "$REPLY_TO" '{
  upload_id: $upload, caption: "[Axon preview check] media reply, cards disabled",
  reply_to: $reply, url_previews: []
}')
REPLY_EVENT_ID=$(axon_curl -X POST -H 'Content-Type: application/json' \
  --data-binary "$JSON" "$ROOM_API/send-media" | jq -er '.data.event_id')
printf 'Reply event: %s\n' "$REPLY_EVENT_ID"

# Inspect the synced event after a short wait. Never print an entire encrypted
# file descriptor: it includes decryption keys. Show only safe summary fields.
EVENT_PATH=$(jq -nr --arg event "$CAPTION_EVENT_ID" '$event | @uri')
axon_curl "$AXON_URL/v1/accounts/$ACCOUNT_ID/events/$EVENT_PATH" |
  jq '.data | {event_id, msgtype: .content.msgtype,
    preview_count: (.content["m.url_previews"] | length),
    encrypted_thumbnail: (.content["m.url_previews"][0] | has("matrix:image:encrypted"))}'

unset AXON_TOKEN
```

Expected: one text message, one file/caption message, and one media reply in each room, with no separate thumbnail message.
An encrypted-room preview has `matrix:image:encrypted`; an unencrypted-room preview has `og:image`.
If an event has not reached sync yet, repeat only the final GET, not the send.
A timeout or lost response after a POST is outcome-unknown: inspect the timeline before making another attempt.
