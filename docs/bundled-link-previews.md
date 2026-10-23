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
The vendored SDK correction in `crates/third-party/encryption-state-monotonic.patch` prevents an older negative state lookup from erasing a known encryption event.

## Budgets and errors

- At most eight previews, with UTF-16 ceilings of 4096 code units for URL, 300 for title, 1000 for description and 120 for site name.
- At most 32 KiB of canonical JSON UTF-8 preview metadata, including resolved image descriptors.
  Preflight reserves 2048 bytes per image for bounded SDK media/encryption metadata; actual content is checked before dispatch too.
- At most 40 KiB of serialized preview-aware content, counting body, formatting, relations and previews together.
  Caption sends reserve 16 KiB within this budget for filename and media metadata.
  This conservative content budget leaves room below Matrix's 65,536-byte event cap for Megolm/base64 expansion and federation envelope/signatures.
  It counts both protocol spellings, so compatibility metadata can tighten the effective preview allowance further.

All limits apply together; no card or field is silently removed or truncated.
Preflight overflow rejects before any upload claim or event dispatch.
`413` codes are `event_too_large` for the content budget or an explicit homeserver size rejection, `preview_too_large` for the preview aggregate budget, and `payload_too_large` for thumbnail bytes.
`400` reports invalid metadata/image/type and `404` reports an unavailable or wrong-account upload.
Preserve the draft for editing on definite validation failure.

An owned task completes or releases its acquired uploads even if the HTTP caller disconnects.
Four preview-send slots have a five-second acquisition deadline; decoder slots share the upload pool, and SDK work has the configured upstream-upload deadline.
Failures release acquired uploads; successful sends consume them.
A timeout, disconnect or general upstream error does not prove that the event was not sent.
These APIs have no caller idempotency key: never automatically replay an uncertain send, even if an upload ID becomes available again.
Uploaded Matrix objects can remain orphaned after failure; local staged bytes retain their existing expiry and boot-reconciliation rules.
Logs identify account/room/event/upload IDs and counts, never shared URLs, titles, image bytes or encrypted-file keys.

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
