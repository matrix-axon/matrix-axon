# Custom emoji discovery

Axon exposes the current room's emoji packs, joined canonical ancestor spaces' packs, and the account's personal pack through:

```text
GET /v1/accounts/{account_id}/rooms/{room_id}/emoji-packs
```

This is a read of the sync cache, requiring an active account and positive joined membership in the addressed room.
It includes stable `m.room.image_pack` state, legacy `im.ponies.room_emotes` state, and the legacy `im.ponies.user_emotes` personal account-data pack.
Personal packs come first, followed by current-room packs and then canonical ancestors from nearest to furthest.
Within each room, packs follow deterministic event-type/state-key order.
A stable room tuple supersedes the legacy tuple with the same state key, even when the stable tuple has been cleared or redacted.
Pack identity is its source type, room ID, and state key; shortcode collisions across packs are preserved.
Missing pack display names or avatars remain null; a client may fall back to the source room's name/avatar or a personal-pack label.
Canonical inheritance follows [MSC2545](https://github.com/matrix-org/matrix-spec-proposals/blob/main/proposals/2545-emotes.md#space-image-packs) and the [Matrix parent relationship validation rules](https://spec.matrix.org/latest/client-server-api/#mspaceparent-relationships).
Each inherited room must be a positively joined space with a non-redacted canonical parent claim and a nonempty string `via` list.
The claim must have a valid reciprocal child link, or its sender must have cached joined membership and current permission to send `m.space.child` in the parent.
Unverifiable claims are omitted; discovery does not perform homeserver requests to fill missing membership evidence.
If several locally valid canonical parents exist, the lexicographically lowest room ID wins.
Traversal stops at 16 ancestors or a cycle and marks the response partial if that prevents further traversal.
An ancestor appears once, preserving its own source room ID and pack identities.
All inherited packs share the existing pack and image budgets with personal and current-room packs.
Subscribed packs from other rooms are not included in this version.
Creating or editing packs is also outside this API.

The integration tests cover personal and room packs together, standalone legacy room packs, and stable room packs at different state keys.
They exercise empty and named state keys, multiple packs in one room, identical shortcodes with different images across packs, missing optional metadata, and pack-level versus per-image usage.
Mixed emoji/sticker packs retain independent usage flags; each picker selects its own usage.
PNG and GIF preview fixtures exercise safe MIME detection independently of declared image metadata.
Separate cases cover stable-over-legacy replacement, clears and redaction, account and room isolation, membership departure, and resource limits.

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

By default only emoticon-capable images are included.
Use `?usage=sticker` to select sticker-capable images instead, including sticker-only entries.
Each image exposes independent `emoticon` and `sticker` booleans and optional bounded `info` metadata.
Mixed-use images appear in both modes; the default response stays compatible with existing reaction pickers.
Per-image `usage`, when present, overrides the pack's usage; absent or empty usage permits both emoticons and stickers.
Malformed image entries are skipped without losing valid siblings.
An empty object clears a pack without marking it partial.
Within one pack, names that collide after trimming retain the first usable entry in deterministic source-key order and mark the pack partial.
Shortcodes and descriptions are untrusted plain text and must never be interpreted as HTML.
Images and pack avatars must be structurally valid MXC URIs of at most 2,048 UTF-8 bytes.
Names are bounded to 100 UTF-8 bytes before trimming, with C0/C1 controls rejected, matching reaction display-name validation.
Display names and descriptions are capped at 256 and 1,024 bytes respectively.

Each source is capped at 128 KiB of stored JSONB or rendered JSON in PostgreSQL before transfer and JSON decoding.
The size result is computed once per candidate; checking rendered size as well prevents compressed oversized JSON from bypassing the limit.
At most 66 source candidates are examined from one database snapshot, and at most 64 nonempty room packs and one personal pack are exposed, with at most 512 entries examined per pack and 1,000 images returned overall.
The top-level or per-pack `partial` flag reports skipped invalid/oversized content or truncation, including the source-candidate scan limit.
Cleared packs and packs without images for the requested usage do not consume the returned-pack budget.
Spending the image budget only reports truncation when a later examined source actually has images for that usage.
The last returned pack is marked partial if its images were truncated; subsequent packs are omitted once the overall image budget is exhausted.
Membership authorization, relationship validation, ancestor traversal, and pack discovery use one database statement snapshot.
An empty result does not prove absence upstream while initial sync is incomplete.
Consumers should re-read when opening the picker and after reconnecting.

The SDK UI room list does not request image-pack state and does not expose configuration to add required state.
Axon therefore runs one additional state-only Simplified Sliding Sync connection per active account run, requesting both room-pack types with all state keys in batches of 100 rooms and no timeline events.
The transport uses raw SDK HTTP requests, disables SDK retries, and projects only allowlisted pack and hierarchy state through the shared state persistence helper.
It never submits a response to SDK sync processing, so summary fields, recency, pagination tokens, timelines, and extensions cannot overwrite the primary connection's SDK state.
The connection also requests parent/child links, creation and power-level state, and the local user's membership so inherited packs hydrate after restart.
Since [ADR 0126](adr/0126-room-detail-acquisition-and-coverage.md) it also carries guest-access and server-ACL state for [room metadata](room-metadata.md#sync-coverage-and-remaining-acquisition), as connection `axon-state`.
It does not request whole member lists; one-way claims without cached sender membership remain unverifiable.
The projection retains existing timestamp guards and redaction/departure handling.
Space parent and child clears with `{}` are persisted unchanged; an absent `via` removes the relationship.
The account run owns, cancels, and joins this worker before restart, preventing duplicate workers and retained SDK clients.
Each long poll has a 30-second server wait and 90-second client/outer deadline.
Successful polls are paced at least one second apart; failures retry with exponential backoff from two to 60 seconds without interrupting message sync.
Transient failures retain the sync position; only an unknown-position response resets it.
ADR 0124 records the transport decision.
Each poll is a long-poll to the ADR 0122 homeserver load gate: it waits while the gate's breaker is open, holds no slot while polling, and leaves a held slot when it times out.
The connection starts fresh after restart, so existing packs hydrate without a manual re-sync or historical-event backfill.
Personal pack updates use the existing generic account-data sync path.
Structured sync diagnostics are available through the normal tracing configuration; no separate developer-mode switch is required.
See the targeted DEBUG configuration below.

Picker images can be fetched through the existing authenticated original and thumbnail media routes before they have ever been used as reactions.
Current account-scoped emoji and sticker packs authorize these previews; removed or redacted room packs, departed rooms, invalid entries, and another account's packs do not.
Preview authorization examines matching packs in keyset pages of at most 66 candidates until a usable image/avatar matches or the candidates are exhausted.
A ten-second overall deadline bounds the scan; expiration is an operational error rather than a false 404.
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
Requests without `shortcode` send the MXC annotation key without a display-name field, so independently distributed clients can upgrade separately.
Receiving clients may show a generic custom-reaction label when no display name is available; omitting the name does not change grouping or withdrawal.

## Standalone stickers

`POST /v1/accounts/{account_id}/rooms/{room_id}/send-sticker` sends an existing MXC image as a standard `m.sticker` event, without uploading it again.
The SDK send path handles room encryption; optional `reply_to` and `thread_root` fields use the same relation semantics as text and media sends.
The existing mutation deadline bounds the SDK operation.
A successful send is logged with account, room, and resulting event IDs.

```json
{
  "body": "A mountain",
  "url": "mxc://example.org/mountain",
  "info": { "w": 278, "h": 185, "mimetype": "image/png", "size": 129986 },
  "thread_root": "$optional-thread-root"
}
```

`body` must be nonblank plain text of at most 1,024 UTF-8 bytes without control characters.
`info` is optional at the Axon boundary; the outgoing Matrix event always includes an object, even when source metadata is absent.
Only `w`, `h`, `size`, `mimetype`, and `is_animated` are copied from image-pack metadata.
Dimensions must be between 1 and 16,384; declared size is at most 100 MiB; MIME metadata supports PNG, JPEG, GIF, and WebP.
Malformed pack metadata is omitted and marks its pack partial, while a usable image remains selectable.
Invalid send metadata returns 400 before calling the gateway.
Sticker-only images authorize previews under the same account, membership, redaction, and resource limits as emoji images.
Received stickers use the existing generic timeline ingestion and attachment media authorization paths.

## Troubleshooting discovery and inheritance

Temporarily add these directives to the server's normal log filter:

```text
axon_store::image_packs=debug,axon_api::routes::image_packs=debug,axon_sync::image_packs=debug
```

Open the emoji or sticker picker in the affected room, then find `image pack inheritance stopped` using its `account_id` and `room_id`.
The log records the terminal `stop_room_id`, ancestor count, and aggregate parent-link evidence from the same database snapshot as discovery.
Its `stop_reason` distinguishes:

- `no_parent_link`: the terminal room has no cached `m.space.parent` event.
- `no_canonical_parent`: parent events exist, but none declares boolean `canonical: true`.
- `invalid_parent_link`: canonical claims are redacted or lack a usable nonempty `via` list.
- `parent_not_joined`: usable canonical claims exist, but their parents are not positively joined locally.
- `parent_not_space`: joined canonical parents lack cached creation state identifying them as spaces.
- `unverifiable_parent`: a joined canonical space has neither a usable reciprocal child link nor locally verifiable sender permission.
- `cycle` or `depth_limit`: traversal reached its existing safety bounds, also reported by the response's `partial` flag.

A normal successful chain also ends with a reason, usually `no_parent_link` at the outermost space.
Check `ancestor_count` and `stop_room_id` before interpreting the reason as a problem with the original room.
When multiple claims fail at different stages, the reason identifies the furthest stage reached by any candidate; evidence counts expose the earlier exclusions.
Additional evidence queries run only while the store DEBUG diagnostic is enabled, and do not change discovery results.

`selected image pack` identifies each returned source room/event, selected image count, images filtered by emoji/sticker usage, and partial content.
`image pack discovery completed` records fetched source count, returned pack/image counts, invalid source count, usage-filtered image count, partial status, and total elapsed milliseconds.
These records help distinguish missing inheritance, usage filtering, malformed packs, and response limits.

Sync DEBUG records identify worker startup/shutdown, per-room projection attempts and malformed/ignored event counts, and hydration/update/recovery progress with room/state counts, requested/next room ranges, and request/processing durations.
Unchanged successful polls do not emit progress records.
Retry warnings include the failure category, Matrix HTTP status when available, request duration, retry delay, and whether an unknown position reset the connection.
Projection attempts are not success counts: persistence failures retain the shared state-ingestion warning with account, room, and event type.
Logs do not include pack names, shortcodes, image URLs, event bodies, access tokens, or opaque sync positions.
