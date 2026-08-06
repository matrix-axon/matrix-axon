# ADR 0088 — Render Matrix formatting in image/media captions

## Status

Draft — plan only, not yet implemented. Filed while batching small web
fixes on `web/polish-round-2`; this is not one of them because it crosses
the API/store silo and the web silo (one silo per PR).

## Context

A media caption typed with Matrix formatting (e.g. `**bold**`, a link, a
mention) sends and displays as literal markdown/plain text instead of
rendered rich text — unlike an ordinary text message, where the same input
renders correctly. Investigating why turned up a gap that runs the entire
stack, not a renderer bug:

1. **Composer** (`clients/web/src/components/use-message-composer.ts:150-151`)
   already runs the caption through `formatComposerBody`, which returns both
   `body` and `formatted_body` (`formatMessageBody`, used identically for
   plain-text sends). For media it keeps only `.body` and drops
   `.formatted_body` on the floor. The plain-text path two lines down
   (`use-message-composer.ts:174-178`) passes `formattedBody` through — media
   is the outlier.
2. **`timeline.ts`** never had anywhere to put it if it had been kept:
   `pushMediaEcho` (`~1095-1132`) builds the local echo's `content` with only
   `msgtype`/`body`/`filename`/`info` — no `format`/`formatted_body`, unlike
   `buildLocalEcho`'s `formattedParts(body, options.formattedBody)` for text
   (`~975-978`). `completeMediaSend` (`~1135-1166`) POSTs only
   `{ upload_id, caption, reply_to, thread_root }` — no formatted variant.
3. **API contract**: `SendMediaRequest` (`crates/axon-api/src/dto.rs:540`) has
   only `caption?: string`. There is no field to carry HTML even if the
   client sent one.
4. **Server route/sender**: `MessageSender::send_media`
   (`crates/axon-api/src/sender.rs:69-76`) takes `caption: Option<&str>`
   only — contrast `send_message`'s `formatted: Option<Formatted<'_>>`
   (`sender.rs:56-63`), which already exists for exactly this purpose on text
   messages.
5. **Gateway/SDK**: `axon-sync/src/gateway.rs:738` builds the caption with
   `TextMessageEventContent::plain(caption)`, always. The same Ruma type has
   an `::html(body, formatted_body)` constructor already in use for text
   sends elsewhere in this file — the SDK-level support exists, Axon simply
   never calls it for media.
6. **Client rendering**: even with a formatted caption in hand,
   `MediaGalleryRow.tsx:290-300` interpolates the caption as a plain string.
   It needs to render through `FormattedBody` (`components/FormattedBody.tsx`)
   the way `EventBody` does for message bodies, which means it needs the
   *content* (for `format`/`formatted_body`), not just the extracted
   `caption` string that `ParsedMedia` currently exposes
   (`clients/web/src/media/parse-media.ts:23,82-85`).

So this is genuinely new plumbing, symmetric to what text messages already
have, not a quick client-side patch.

## Decision

Two stacked PRs, split by silo, in this order:

### Phase 1 — API/store silo

- Add `format?: string` / `formatted_body?: string` to `SendMediaRequest`
  (`dto.rs`), following the same doc-comment convention as the text
  `SendMessageRequest` (`format` must be `org.matrix.custom.html` paired with
  `formatted_body`; Axon carries it verbatim).
- Extend `MessageSender::send_media` to take `formatted: Option<Formatted<'_>>`
  (reuse the existing `Formatted` type from `send_message`, don't invent a
  second one).
- In `axon-sync/src/gateway.rs`, build the caption with
  `TextMessageEventContent::html(caption, formatted_body)` when formatting is
  present, `::plain(caption)` otherwise — same branch shape `send_message`
  already has for the text case.
- Thread the new field through `routes/messages.rs::send_media` and update
  the TUI's sender trait implementation (it implements the same trait; a
  `None` default keeps it behaviorally unchanged, no TUI feature work
  required in this PR).
- Regenerate `clients/web/src/api/schema.d.ts` from the updated OpenAPI spec.
- Server-side tests: a captioned media send with `format`/`formatted_body`
  round-trips through sync as `content.formatted_body` on the `m.image`
  event, same as it does for `m.text`.

### Phase 2 — Web silo (stacked on phase 1)

- `use-message-composer.ts`: keep `formatComposerBody(body).formatted_body`
  for the media path instead of discarding it; pass both to
  `sendMediaBatch`.
- `timeline.ts`:
  - `MediaSendOptions` gains `formattedCaption?: string`.
  - `pushMediaEcho` sets `format`/`formatted_body` on the echo's `content`
    via the existing `formattedParts` helper, so the echo renders correctly
    before the round trip completes (matching text-message echo behavior —
    without this the caption would flash as raw markdown, then re-render
    once confirmed).
  - `completeMediaSend` includes `format`/`formatted_body` in the
    `send-media` POST body.
  - The failed-echo retry path (`~1285-1288`, the "invert the echo's
    `caption ?? filename` body rule" comment) needs the same treatment for
    formatted text, or retries will silently drop formatting.
- `parse-media.ts`: `ParsedMedia` needs a way to get at `content` for
  formatting — either expose `format`/`formattedCaption` fields alongside
  `caption`, or (simpler, less duplication) have callers pass the event's
  raw `content` through to `FormattedBody` directly, the way `EventBody`
  does for message bodies.
- `MediaGalleryRow.tsx`: render each caption via `<FormattedBody accountId
  body={caption} content={event.content} />` instead of interpolating the
  string, matching how `EventBody` renders ordinary message bodies.
- Update `MediaGalleryRow.test.tsx` with a captioned-media case asserting
  the caption's HTML rendered, not the raw markdown.

## Consequences

- Two PRs, not one — the API contract change has to land and be deployed
  (or at least merged to the branch the web PR builds against) before the
  web half has anything real to send.
- Local-echo parity matters: skipping the echo-side `formattedParts` call in
  phase 2 would produce a visible unformatted → formatted flash on every
  captioned send, which is worse than the current always-unformatted
  behavior in one respect (inconsistency) even though it fixes the steady
  state.
- No migration concerns: existing captions with no `format`/`formatted_body`
  render exactly as they do today (`FormattedBody` already handles the
  absent-formatting case for message bodies).
- The TUI is out of scope here (one silo per PR) and keeps rendering
  captions as plain text; its sender-trait implementation just needs to
  compile against the widened trait signature.
