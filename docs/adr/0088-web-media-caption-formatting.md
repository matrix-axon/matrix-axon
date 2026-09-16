# ADR 0088 — Media captions: formatting, edits, and display

## Status

Accepted.
Phase 1 (server) is implemented alongside this revision; phases 2 (TUI) and 3 (web) follow as separate PRs.

This revision supersedes the July 2026 draft, which covered only formatted captions in the web client.
Part of that draft has since shipped: the web client renders captions through `MediaCaption`, which also converts an Axon caption's markdown at display time.
The file name is kept so existing links, including issue #237's, still resolve.

## Context

### What a caption is

Media captions were added to the Matrix spec in v1.10, from MSC2530.
An `m.image`, `m.file`, `m.audio`, or `m.video` event can carry a `filename` as well as a `body`.
If `filename` is present and differs from `body`, then `body` is a caption; otherwise `body` is the filename.
`format` and `formatted_body` are used only for a caption.

So there are not two candidate caption fields.
`filename` is always the file's name, and `body` is the caption only when a distinct `filename` exists.
An uncaptioned event, including every pre-v1.10 event, has no `filename` and carries the name in `body`.
The spec requires clients to render the caption alongside the media and to prefer its formatted form.

Axon already reads captions this way: `parseMedia` in the web client and `parsed_media` in the TUI.
Ruma's `caption()` and `filename()` accessors implement the same rule.

### What was wrong

**Formatted captions were dropped on send (#237).**
The web composer renders a caption to HTML and then discards it.
`SendMediaRequest` had no `format` / `formatted_body`, and the gateway always built the caption with `TextMessageEventContent::plain`.
Other clients therefore saw Axon captions as literal markdown.

**Caption edits never applied (#401).**
`SdkGateway::edit` fetched the original only to check its author, then always sent `msgtype: m.text` in both `m.new_content` and the fallback.
The store applies an edit only when `m.new_content.msgtype` matches the original's (`TIMELINE_SELECT`), so an `m.text` edit of an image was ignored and the old caption stayed.
Emote and notice edits were broken the same way.

A correct media edit has two traps a naive fix falls into:

- An uncaptioned image has no `filename`. Replacing only `body` makes the new caption read as the file's name, and no caption shows.
- `m.new_content` replaces the _whole_ content. A replacement built by copying the original keeps any old `formatted_body`, which renderers prefer, so a plain edit would still display the old formatted caption.

**The TUI loses the media label for a formatted caption.**
`message_body_lines` renders `formatted_body` whenever it is present, whatever the msgtype.
For a media event with a formatted caption, the caption HTML replaces the `[image: filename]` label entirely.
This already happens with captions from other clients, and #237 would make it the norm for Axon captions.
The TUI also orders a media row as label, caption, thumbnail, where the web client puts the caption under the image.

**The web client hides the filename, and the room list shows it.**
The filename reaches only `alt` and `aria-label`.
Element Web and gomuks show it as a tooltip on the image.
Conversely, the room-list preview (`isPreviewEvent`) accepts any `m.room.message` with a non-empty `body`, so an uncaptioned image previews as its filename.

### Production evidence

A survey of the three production instances on 2026-09-16 (event shapes only, no message text) found:

- **Axon's broken edits were rare.** 15 `m.text` replacements of images, on 4 images, all from Axon. One of those images was uncaptioned and edited four times, so the first trap above has already happened.
- **Other clients keep the msgtype.** Edits sent through matrix-rust-sdk (the Element X shape: `m.mentions`, a full-media fallback with a `* ` body, `filename` kept) match what this ADR adopts. One added a caption to an uncaptioned image and set `filename` correctly.
- **Bridges vary.** A double-puppeting iMessage bridge sent 33 same-msgtype media edits, 20 of which omit `filename`, which by the spec removes the caption. A Discord bridge turned one image into `m.text`, and bots or bridges sent 9 notice or emote edits as `m.text`; the store ignores all 10.

## Decision

### Phase 1 — server (API silo)

**Formatted captions on send.**
`SendMediaRequest` gains optional `format` / `formatted_body`, validated by the same `formatted()` helper as text sends.
Formatting without a `caption` is a `400`, and all validation happens before the staged upload is claimed, so a malformed request does not consume it.
`MessageSender::send_media` takes `formatted: Option<Formatted>`.
The gateway builds the caption with `TextMessageEventContent::html` when formatting is present, and `media_message_type` carries it, which also covers the thread-member path that bypasses `send_attachment`.

**Type-preserving edits.**
`SdkGateway::edit` still fetches the original and enforces authorship, then:

- `editable_message_content` accepts only an original `m.room.message`. A redacted message, an undecryptable one, or any other event type is a `400`, instead of a replacement that would be ignored or would change what the original is.
- `replacement_content` keeps the original msgtype. For `m.text`, `m.notice`, and `m.emote`, `body` (and formatting) become the new text. For the four media msgtypes, the media, `info`, and filename are kept and only the caption changes. Any other msgtype, such as `m.location`, is a `400`.
- `set_media_caption` applies MSC2530: pin the filename first (`filename`, else `body`); an empty caption, or one equal to the filename, removes the caption (`body` becomes the filename, `filename` is unset, formatting dropped); otherwise the caption becomes `body`, the filename moves to `filename`, and the formatting is replaced, never inherited.
- Ruma's `make_replacement` produces the envelope, which is the matrix-rust-sdk shape: `m.new_content` holds the content as-is, and the fallback is the same content with a `* ` body prefix. For a plain text edit, the empty `* ` HTML fallback Ruma adds is removed, since an edit-unaware client that prefers HTML would render only an asterisk.

`EditRequest` is unchanged in shape; its documentation now says that `body` is the new caption for a media message and that an empty `body` removes it.

The store's same-msgtype rule is unchanged.

### Phase 2 — TUI

- A media row with a thumbnail renders as header (plus any reply or thread context line), thumbnail, then the caption beneath it, indented to the thumbnail's left edge: the formatted caption if present, otherwise the plain caption, otherwise the filename, dimmed so it does not read as a caption. No `[image: …]` label is drawn above the thumbnail; the filename under the reserved rows identifies an image that is still loading, and a failed decode already paints `[image unavailable: …]` into those rows.
- `message_body_lines` no longer lets a media event's `formatted_body` replace its label. HTML is used only for the caption text itself.
- Rows without thumbnails (files, audio, video) keep the label with the caption after it. Reply snippets, search results, and the media preview popup are unchanged.
- This phase should land before phase 3 starts sending formatted captions.

### Phase 3 — web

- **Formatted captions.** The composer keeps the caption's HTML for media; the local echo (`pushMediaEcho`), `completeMediaSend`, and the failed-send retry path all carry `format` / `formatted_body`, so a caption does not flash as raw markdown before the server confirms. `MediaCaption` keeps its display-time markdown conversion for captions already in history.
- **Caption editing.** Edit mode for a media event starts from the caption (empty when there is none), not from `event.body`, which for an uncaptioned image is the filename. An empty submit is allowed in edit mode for media and removes the caption. Formatted caption edits need no other client change.
- **Filename tooltip.** Single images and gallery cells show the filename on hover (fine pointers only) and on keyboard focus. The tooltip is rendered through `BodyPortal` with `position: fixed`, because `.media-image` and `.gallery-cell` clip overflow and timeline rows use containment. It is placed above the media (below when there is no room) with its horizontal position clamped to the viewport; `max-width: min(22rem, calc(100vw - 2rem))` plus `overflow-wrap: anywhere` wraps names without spaces, and it is clamped to three lines. It is `aria-hidden`, since the filename is already in the accessible name. Touch devices have no hover, so the lightbox also shows the filename.
- **Room-list preview.** One `previewText` helper serves both the fetched and live previews: text-like msgtypes preview their `body`, captioned media previews its caption, and uncaptioned media is skipped in favour of the most recent text. A live uncaptioned image still counts as activity but leaves the preview alone. When no text is found in the scan window, the preview is the media kind (for example "Image"), never the filename. Replacement events are skipped. Previews are not persisted, so no cache version changes.

## Consequences

- Axon's caption edits apply in Axon and match what Element X sends, so other clients apply them too. A new edit of an image carrying a broken edit supersedes it everywhere.
- Emote and notice edits sent from Axon now apply.
- Editing a location, a verification request, a custom msgtype, a redacted message, or an undecryptable message now fails with a `400` instead of silently sending an ignored `m.text` replacement.
- Media round-trips through Ruma's types, so non-standard keys on the original (for example `com.beeper.linkpreviews`) are not carried into the replacement, and JWK `key_ops` may be reordered. matrix-rust-sdk behaves the same way.
- Captions sent before this change stay as they are: formatting cannot be added to history, and `MediaCaption`'s display-time conversion remains for them.
- The iMessage bridge's `filename`-less media edits will, correctly by the spec, show as uncaptioned. That is the bridge's behaviour and Axon does not work around it.

## Alternatives rejected

- **Relax the store's same-msgtype rule** so Axon's `m.text` edits of images would apply. The spec allows a replacement to change msgtype, but applying an `m.text` replacement to an image replaces the image with a line of text. The gateway was what was wrong.
- **Call matrix-sdk's `Room::make_edit_event`** with `EditedContent::MediaCaption`. It encodes exactly these rules, but it fetches the target through the event cache, which Axon does not enable, so every edit would fetch the original twice. Its `update_media_caption` helper is crate-private in 0.18, so `replacement_content` mirrors it on Ruma's public types instead.
- **Build the replacement by copying the original JSON** and swapping `body`. This walks into both traps above unless each field is handled by hand, which is what the typed path already does.
- **An `m.text` fallback for media edits.** Proposed during design, but matrix-rust-sdk keeps the full media fallback with a `* ` body, and diverging from the dominant client buys nothing.
- **A native `title` tooltip on the web.** It never appears on touch devices and gives no control over width or wrapping for long filenames.

## Open questions

- **Text-like msgtype changes.** The store could let an edit switch between `m.text`, `m.notice`, and `m.emote` (which would apply the 9 ignored bot and bridge edits) while still refusing to turn media into text. That is a store change, not part of these phases.
