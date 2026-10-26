# Sending bundled link previews

The server accepts optional `url_previews` on text and media-caption sends, following [ADR 0125](https://github.com/matrix-axon/matrix-axon/blob/a09f547ce18b333c87fcfbc6a1ce1b437f16669c/docs/adr/0125-ios-first-incoming-sharing.md).
Native share intake and image-card rendering remain separate stages in [the approved plan](https://github.com/matrix-axon/matrix-axon/blob/a09f547ce18b333c87fcfbc6a1ce1b437f16669c/docs/mobile-incoming-sharing.md).
Sending never fetches the linked webpage or an external preview image.
The explicit [generation helper](#opt-in-homeserver-generation) delegates acquisition to the homeserver.

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

## Opt-in homeserver generation

`POST /v1/accounts/{account_id}/link-previews/generate` accepts `{"url":"https://example.org/page"}` under the existing Axon bearer authentication.
Invoke it only after the user requests or has enabled generation, explaining that it discloses the URL to the selected account's homeserver, including for an encrypted destination room.
Receiving a share, rendering a timeline, and sending a message never invoke generation automatically.
Prefer valid source-supplied metadata when available.
This server capability does not add a client preference or UI.

Axon calls the configured homeserver's capabilities and authenticated Matrix v1 media `preview_url` endpoints, preserving any configured URL path prefix.
An explicit disabled `m.preview_url` capability stops before URL submission; an absent capability or unsupported capabilities endpoint permits the preview request for compatibility.
Homeservers must implement the authenticated media endpoints introduced in Matrix 1.11; Axon does not fall back to unauthenticated media or crawl arbitrary websites.
The homeserver owns DNS resolution, webpage redirects and origin-fetch SSRF protection, including private-address DNS answers and rebinding.
Axon rejects non-HTTP(S), credential-bearing, oversized, local-name and nonpublic literal-IP input before calling the homeserver.
Axon's requests follow no redirects, carry only the selected account's Matrix credentials, and never forward the incoming Axon bearer token.
Only eligible `mxc://` thumbnail references from that response are downloaded, through the same configured homeserver's authenticated media endpoint.
Remote HTTP image references and arbitrary metadata/HTML/image dimensions are ignored.

Successful responses use the usual `data` envelope with these fields:

| Field              | Meaning                                                                                      |
| ------------------ | -------------------------------------------------------------------------------------------- |
| `status`           | `ready`, `unsupported`, `disabled`, `timeout`, `unavailable`, `busy`, or `no_metadata`       |
| `preview`          | A settled `LinkPreviewRequest` ready for one entry of `url_previews`, or null for a fallback |
| `thumbnail_status` | `ready`, `absent`, or `unavailable`; thumbnail failure can still produce a useful text card  |
| `image_expires_at` | Server staging expiry when an image is ready, otherwise null                                 |

Fallback outcomes are HTTP 200 and do not modify or dispatch a draft.
Malformed/nonpublic URL input or a URL that cannot satisfy the settled exact body-link matching rule is HTTP 400 `bad_request`, before any homeserver request; a missing account is HTTP 404 `not_found` and an inactive account is HTTP 403 `forbidden`.
Keep the original link sendable, and offer an explicit retry if desired.
Do not assume every webpage has metadata or a supported raster image.
Text normalization retains only nonempty title/description/site-name strings within the send contract's UTF-16 limits; invalid/oversized optional fields are omitted.
The exact input URL remains the matched URL; sending still validates its membership in the final body/caption.

Generation admits four concurrent operations per account without queuing; excess calls for that account return `busy`.
Another account retains its own slots.
A separate four-slot decoder pool bounds total generation CPU work; decoder saturation preserves a metadata card with an unavailable thumbnail.
Each upstream request has an eight-second deadline, and connection establishment plus all acquisition has a 25-second deadline.
Generation request bodies are capped at 32 KiB before JSON decoding.
Capability/preview responses are capped at 64 KiB, thumbnails at 2 MiB, and raster decoding at four million pixels with bounded allocation.
Existing PNG/JPEG/WebP validation derives MIME type and dimensions from bytes before durable staging.
Staging (including its capacity wait) has a 15-second deadline, honors lower configured upload-byte limits, and uses the ordinary upload expiry and cleanup policies.
A disconnected HTTP caller's owned task finishes staging; abandoned handles expire rather than sending anything.
Generated thumbnails have an independent, persistent per-account ceiling of 160 pending handles and 64 MiB.
The count limit bounds many small thumbnails; the byte limit bounds fewer large thumbnails.
A transaction-scoped account lock makes quota checks and insertion atomic, and expired rows continue to count until cleanup releases their files.
Quota exhaustion retains useful text metadata with `thumbnail_status: unavailable`; it does not remove the original link or silently replace another thumbnail.
Only an explicit false `m.preview_url.enabled` capability produces `disabled`; a URL-specific HTTP 403 produces `unavailable`, and a homeserver HTTP 429 produces transient `busy`.
The capabilities probe is optional: failures (including 403, 429, timeout or malformed JSON) still allow a bounded attempt at `preview_url`.
Unknown-token responses can refresh and retry once through the SDK, which owns rotation, locking, and persistence notifications.
SDK refresh runs in an owned task bounded to 30 seconds, so a shorter preview deadline does not discard an accepted token rotation.
The preview transport retains streaming byte caps and disables redirects; the normal SDK send/media helpers buffer the full response before exposing it.
Its TLS minimum, user agent and system proxy behavior match the active production SDK defaults; Axon exposes no separate per-account proxy or custom TLS configuration.
Logs contain account IDs, request phases, HTTP status and stable outcome categories, without URLs, metadata, credentials or free-form upstream errors.

### Durable thumbnail recovery

A generated thumbnail is a normal account-owned staged upload, consumed with its parent message under the existing encrypted/unencrypted send contract.
Before marking an incoming share durably recoverable, retrieve its image with authenticated `GET /v1/accounts/{account_id}/media/uploads/{upload_id}/preview` and retain a protected local copy.
The endpoint also supports eligible source-supplied staged thumbnails, verifies the same byte/pixel/type limits under a dedicated four-slot recovery pool, returns raster bytes with `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`, and does not consume, claim or extend the upload.
Retrieve before dispatch: a sending, consumed, expired, missing, truncated, size-mismatched or wrong-account handle returns HTTP 404 with `{"error":{"code":"not_found","message":"..."}}`.
The message is explanatory text; adapters must branch on the stable status/code.
A retrieval 404 does not prove that a parent send failed: sending and consumed handles share this response.
Preserve an outcome-unknown send for user resolution, and never use thumbnail recovery to replay its parent automatically.
Oversized bytes return HTTP 413 `payload_too_large`, invalid rasters HTTP 400 `bad_request`, and exhausted retrieval capacity HTTP 503 `service_unavailable`.
All errors use the existing `ErrorResponse` envelope, including HTTP 401 for missing/invalid Axon authentication.

Server upload expiry is independent of the planned 24-hour local-share retention.
When a handle expires, re-upload the retained bytes and replace only the image upload ID; never automatically regenerate the webpage preview.
If local bytes are missing or corrupt, require a visible explicit replacement, image-removal or discard choice.
The native adapter contract examples live in `crates/axon-api/tests/fixtures/preview-generation-contract.json`; the test checks DTO round trips and the OpenAPI outcome enum.

To verify a supporting homeserver, request generation explicitly, retrieve the staged thumbnail, then use the returned `preview` as a `url_previews` entry in the text and media-caption curl requests above, once per room encryption mode.
Read the synced parent events and compare authenticated thumbnail bytes with the retrieved source.
Confirm persistent MXC references for plaintext rooms and encrypted descriptors for encrypted rooms, without an extra thumbnail message.
On a preview-disabled homeserver, verify a nonfatal fallback and send the original plain link.

Using the `axon_curl`, `AXON_URL` and `ACCOUNT_ID` variables from the script above, opt in explicitly:

```bash
# This request discloses LINK to the selected account's homeserver.
LINK='https://matrix.org/'
GENERATION_REQUEST=$(jq -n --arg url "$LINK" '{url:$url}')
GENERATED=$(axon_curl -X POST -H 'Content-Type: application/json' \
  --data-raw "$GENERATION_REQUEST" \
  "$AXON_URL/v1/accounts/$ACCOUNT_ID/link-previews/generate")
printf '%s\n' "$GENERATED" | jq '.data | {status, thumbnail_status, image_expires_at}'
IMAGE_ID=$(printf '%s' "$GENERATED" | jq -r '.data.preview.image_upload_id // empty')
if [ -n "$IMAGE_ID" ]; then
  umask 077
  axon_curl "$AXON_URL/v1/accounts/$ACCOUNT_ID/media/uploads/$IMAGE_ID/preview" \
    --output "${IMAGE_PATH}.generated"
fi
# Only when status is ready, preserve this preview with the original link.
if printf '%s' "$GENERATED" | jq -e '.data.status == "ready"' >/dev/null; then
  SEND_REQUEST=$(printf '%s' "$GENERATED" | jq --arg body "$LINK" \
    '{body:$body,url_previews:[.data.preview]}')
  axon_curl -X POST -H 'Content-Type: application/json' \
    --data-raw "$SEND_REQUEST" "$ROOM_API/send"
fi
```

Run `cargo test -p axon-sync --lib preview_generation` for adversarial upstream boundaries, `cargo test -p axon-api --test preview_generation` for the native contract fixture, and the ignored tests in that API target and `cargo test -p axon-server preview_recovery -- --ignored --test-threads=1` against an isolated PostgreSQL database for router/ownership/expiry checks.

### Stage 3 validation record

Verified initially and again after review on October 25, 2026 against disposable Synapse v1.163.0 with URL previews enabled, using `https://matrix.org/` as the public source.
Explicit generation produced a verified 12,170-byte raster thumbnail in each room encryption mode.
Authenticated staging retrieval was repeatable before dispatch and returned the documented private/no-sniff headers; consumed handles returned HTTP 404 afterward.
Text and file-caption sends passed in plaintext and encrypted rooms, with four authenticated media-proxy downloads matching the retained thumbnail bytes.
Plaintext events carried persistent `og:image` MXC references; encrypted events carried `matrix:image:encrypted` without a plaintext `og:image`.
Caption sends reuploaded retained local bytes without generating the page preview again, and no separate thumbnail message appeared.
After disabling Synapse previews, generation returned `unsupported` (its unavailable endpoint response), and the original plain link still sent successfully.
An explicitly false capability and forbidden preview response are separately covered by the mocked homeserver tests, including stopping before URL submission for a false capability.
The same boundary suite exercises all three acquisition requests under a configured homeserver path prefix, including encoded prefix segments and trailing-slash variants.
IPv4 eligibility tests also cover the exact reserved-subnet boundaries and adjacent accepted addresses.
After these corrections, `cargo fmt --all -- --check`, workspace Clippy with all features/targets and warnings denied, and `cargo test --all` passed (1,408 tests passed; PostgreSQL-only cases remain separately gated).
The focused PostgreSQL media-upload run passed nine tests, server raster/capacity/recovery/quota checks passed six, and the router/OpenAPI/native-contract run passed ten tests.
Actual SDK OAuth fixtures passed for refresh during all three acquisition phases, denied/rejected refresh, missing refresh credentials and ownership after waiter cancellation.
The PostgreSQL encrypted OAuth rotation/persistence test also passed.
The server smoke gate passed all ten scenarios against a fresh real stack.
Generated-schema drift/compatibility checks and web lint/tests/build passed, with 2,645 web tests passing and one pre-existing ESLint warning in unchanged `Composer.tsx`.
A real MAS-backed expired-token → preview generation → restart sequence remains a recommended additional deployment interoperability check; the automated SDK OAuth fixture uses a local authorization-server stub.
This records server/API behavior; native sharing and third-party card rendering are not covered by this stage.
