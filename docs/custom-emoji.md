# Custom emoji discovery

Axon exposes the current room's emoji packs and the account's personal pack through:

```text
GET /v1/accounts/{account_id}/rooms/{room_id}/emoji-packs
```

This is a read of the sync cache, requiring an active account and positive joined membership in the addressed room.
It includes stable `m.room.image_pack` state, legacy `im.ponies.room_emotes` state, and the legacy `im.ponies.user_emotes` personal account-data pack.
Personal packs come first; room packs follow in deterministic event-type/state-key order.
A stable room tuple supersedes the legacy tuple with the same state key, even when the stable tuple has been cleared or redacted.
Pack identity is its source type, room ID, and state key; shortcode collisions across packs are preserved.
Missing pack display names or avatars remain null; a client may fall back to the current room's name/avatar or a personal-pack label.
Subscribed packs from other rooms and canonical-space inheritance are not included in this version.
Creating or editing packs is also outside this API.

An example response is:

```json
{
  "data": {
    "packs": [
      {
        "room_id": null,
        "event_type": "im.ponies.user_emotes",
        "state_key": null,
        "event_id": null,
        "display_name": "Personal emoji",
        "avatar_url": null,
        "images": [
          {
            "shortcode": "allthethings",
            "url": "mxc://example.org/custom-image",
            "body": "All the things"
          }
        ],
        "partial": false
      }
    ],
    "partial": false
  }
}
```

Only emoticon-capable images are included.
Per-image `usage`, when present, overrides the pack's usage; absent or empty usage permits both emoticons and stickers.
Malformed image entries are skipped without losing valid siblings.
Shortcodes and descriptions are untrusted plain text and must never be interpreted as HTML.
Images and pack avatars must be structurally valid MXC URIs of at most 2,048 UTF-8 bytes.
Names are bounded to 100 UTF-8 bytes before trimming, with C0/C1 controls rejected, matching reaction display-name validation.
Display names and descriptions are capped at 256 and 1,024 bytes respectively.

Each source is capped at 128 KiB in PostgreSQL before transfer and JSON decoding.
At most 64 room packs and one personal pack are exposed, with at most 512 entries examined per pack and 1,000 images returned overall.
The top-level or per-pack `partial` flag reports skipped invalid/oversized content or truncation.
An empty result does not prove absence upstream while initial sync is incomplete.
Consumers should re-read when opening the picker and after reconnecting.

The SDK UI room list does not request image-pack state and does not expose configuration to add required state.
Axon therefore runs one additional state-only Simplified Sliding Sync connection per active account run, requesting both room-pack types with all state keys in batches of 100 rooms and no timeline events.
The ordinary raw-state ingestion path persists updates, using its existing timestamp guards and redaction/departure handling.
The account run owns, cancels, and joins this worker before restart, preventing duplicate workers and retained SDK clients.
Each long poll has a 90-second outer deadline and failures retry with bounded backoff without interrupting message sync.
The connection starts fresh after restart, so existing packs hydrate without a manual re-sync or historical-event backfill.
Personal pack updates use the existing generic account-data sync path.
Structured sync diagnostics are available through the normal tracing configuration under `axon_sync::image_packs`; no separate developer-mode switch is required.

Picker images can be fetched through the existing authenticated original and thumbnail media routes before they have ever been used as reactions.
Current account-scoped packs authorize these previews; removed or redacted room packs, departed rooms, invalid entries, and another account's packs do not.
Preview authorization examines at most 66 matching pack candidates and fails closed if no parsed image/avatar matches within that bound.
Attachment lookups still take precedence, preserving encrypted attachment descriptors.
Pack MIME declarations are not trusted: only PNG, JPEG, GIF, and WebP signatures receive inline image MIME types; other bytes remain `application/octet-stream` with `nosniff`.
Authorization precedes conditional ETag handling for originals and thumbnails.
Media storage continues to use the existing per-object limits and globally bounded cache.

To send a selection, use the existing reaction endpoint with its unchanged MXC key and optional display name:

```json
{
  "key": "mxc://example.org/custom-image",
  "shortcode": "allthethings"
}
```

The optional `shortcode` requires a valid MXC key and is written as top-level metadata in the outgoing `m.reaction` content.
The key alone determines reaction grouping and withdrawal.
Requests without `shortcode` retain their existing behavior, so independently distributed clients can upgrade separately.
