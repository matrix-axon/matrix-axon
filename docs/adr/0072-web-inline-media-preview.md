# ADR 0072 — Web client inline media preview

## Context

ADR 0064 split inbound media two ways: an `m.image`/`m.sticker` renders inline
through `MediaImage`, and everything else — `m.file`, `m.audio`, `m.video` —
renders as a download card (`MediaAttachment`). That ADR's rule was explicit:

> No native `<video>`/`<audio>` element — those buffer the whole object into
> memory — and no bytes are fetched until the user clicks Download.

ADR 0065 then gave the client the write half, and it uploads anything: a
voice memo, a screen recording, a PDF. So the client can now put files into a
room that it cannot itself open. A voice message is a filename and a Download
button; a shared PDF has to leave the app to be read. That asymmetry is the
problem this ADR closes.

The constraint that shaped ADR 0064's choice is real and unchanged: `/v1/media`
is bearer-guarded, a browser cannot put an `Authorization` header on
`<video src>`, and so every object is `fetch()`ed with the token and handed to
the DOM as a `blob:` URL — which means the whole object lands in memory before
anything can play. What was over-broad was applying that to *mounting*: the
memory cost is only incurred if the player mounts unasked.

## Decision

### Preview on demand, not on scroll

`MediaAttachment` gains a `Play` / `Preview` button beside `Download`. Nothing
is fetched by scrolling past a card — ADR 0064's actual invariant — but a click
mounts `MediaPreview` inline below the card, which loads the object and renders
a player. Collapsing unmounts it and drops the reference.

The bytes come through the existing `useMediaBlob` → `MediaService.acquire()`
path, so the object URL is refcounted, LRU-governed, and released on unmount
exactly like an image. `fetchBlobUrl` is deliberately *not* used: it is uncached
and caller-revoked, which suits the transient Download anchor and not a player
that can be re-opened.

### Video and PDF take the lightbox; audio and text stay inline

Not everything previewable wants the same surface. A PDF page in a timeline
column is unreadable and a video shown small is a video you squint at — and both
are things you look at once and then leave. Those open in `Lightbox`, the
full-viewport shell images already use (ADR 0064), which on a phone *is* the
full screen once the overlay's padding is dropped at the narrow breakpoint. No
Fullscreen API call is involved: the overlay already covers the viewport, and
iOS promotes a playing `<video>` on its own.

An audio player is a control strip, not a view; covering the timeline to show
one would be theft. Text reads fine in place. Both stay inline, where the
expand button remains a true disclosure toggle that reads `Hide` when open.

The lightbox is dismissed three ways — Escape (capture-phase, so it beats the
timeline's own handlers), the ✕, and a backdrop click — which is the shared
modal contract, not something new. Because it is dismissed from inside, the
card's button is not a toggle for those kinds: it never reads `Hide`, and
carries `aria-haspopup="dialog"` instead of `aria-expanded`.

This split is why `Lightbox` is now a presentation shell taking `children`,
with loading left to the caller (`LightboxImage` for the image case): an image,
a video and a PDF want different elements and different failure text.

### The card for a lightbox kind is a tile, and the tile is a placeholder

Video and PDF cards are tiles — an aspect-correct surface with a play/document
glyph, duration and size, the whole thing clickable — rather than a row of text
with a verb beside it. The thing you want is the media, so the media is the
affordance.

What the tile is *not* is a rendered frame, and that is a constraint rather than
a choice. There is no poster to fetch: our upload path attaches no
`info.thumbnail_*`, and because the media is encrypted the homeserver cannot
generate one either — it cannot decrypt the bytes. The only way to a real first
frame is to download the whole object and draw it to a canvas, which is exactly
the scroll-past cost this ADR exists to avoid. So the tile is an honest
placeholder: dark stage for video, paper for a document. When a sender *did*
attach a thumbnail (Element does for video), it is used as the poster, fetched
lazily like any other thumbnail.

The real fix is upstream of here — generate a poster at send time and attach
`info.thumbnail_file`, which serves every client rather than this one. That is
the upload silo, and a follow-up.

### `info.mimetype` cannot be relied on

The obvious design — classify on `content.info.mimetype` — does not survive
contact with real events. A phone video sent through our own web client arrives
as:

```json
{ "msgtype": "m.file",
  "filename": "IMG_9306.mov",
  "info": { "mimetype": "application/octet-stream", "size": 2360866 } }
```

No `m.video`, no media type. `uploadKind()` (`media-service.ts`) maps anything
non-image to `kind: file`, and the browser gave no `File.type` for `.mov`, so
the only signal that this is a video at all is the filename. A voice memo is the
same shape, with the name in `body` rather than `filename`.

So `previewPlan()` (`src/media/preview-kind.ts`) resolves in three tiers:

1. **A declared, specific `info.mimetype` is authoritative** — including a
   declaration we refuse, which refuses the whole preview rather than falling
   through. Otherwise an `m.video` labelled `text/html` would still open a
   player over those bytes.
2. **A generic declaration** (`application/octet-stream` and friends) carries no
   information and falls through to the filename extension, against a closed
   `extension → (kind, MIME)` table.
3. **The msgtype** covers an extensionless filename on an `m.audio`/`m.video`.

### Per-kind size ceilings

Blob-backed playback has no `Range` streaming, so a large video is a large
allocation. Binary kinds are capped at `MAX_UPLOAD_BYTES` — the most our own
clients can put in a room — rather than something smaller: a lower bar would
have made a phone video, the single most likely thing to want to play inline,
the one thing that could not be. Text is capped at 512 KB because a `<pre>` of a
megabyte is unreadable, not for memory. Media with no `info.size` is allowed
through; refusing on a missing field would drop previews for every sender that
omits it, and a failed fetch already renders as an error.

### The client re-types the bytes; the server does not change

Two separate reasons the served `Content-Type` is unusable:

- `is_inline_safe()` (`crates/axon-api/src/routes/media.rs`) downgrades anything
  that is not image/audio/video to `application/octet-stream`, so a `blob:` of a
  PDF renders nothing.
- A sender-declared `application/octet-stream` is passed through unchanged — and
  a `<video>`/`<audio>` element will not play a blob typed that way either.

Rather than widen the server's allowlist — which would weaken a defence that
protects *every* client, present and future — the web client re-types the blob
locally via a new `MediaRequestOptions.contentType`, from `previewPlan()`'s
resolved type. The re-typed fetch gets its own cache key so it cannot collide
with the raw one.

The safety property is not that the value is trusted but that **every reachable
value is inert**: `previewPlan` can only ever emit a type from the extension
table or `application/pdf`/`text/plain`, and nothing maps to `text/html` or
`image/svg+xml`. A `.html` attachment has no entry, so it has no preview and no
re-type. A test enumerates this.

### PDFs are drawn with pdf.js, not embedded in an `<iframe>`

Two implementations were tried and abandoned before this one, both defeated by
the same class of problem — a framed PDF is the browser's document, not ours:

1. **`<iframe sandbox="">`** rendered a viewer with blank pages. A sandbox gives
   the frame an opaque origin, and a `blob:` URL is readable only by the origin
   that minted it. Restoring `allow-same-origin` (and, in some browsers,
   `allow-scripts`) would hand back exactly the privileges the sandbox existed
   to remove, so the attribute bought nothing.
2. **An unsandboxed `<iframe>`** works on desktop and is unusable on a phone:
   iOS Safari renders only the *first page*, with no scrolling and no page
   controls. A WebKit restriction on framed PDFs; no sizing or CSS reaches it.
   A one-page-only viewer for a multi-page document is not a viewer.

So `PdfViewer` renders pages to `<canvas>` with `pdfjs-dist`, loaded through a
dynamic `import()`. Pages rasterise lazily as they scroll into view (reusing the
media layer's `observeVisible`) and each reserves its aspect ratio first, so the
scrollbar is honest and lazy rendering cannot shift the document under a
reader's finger. Both the inline and lightbox presentations use it — one code
path, one set of behaviors.

The cost is real and paid only on use: a ~425 KB viewer chunk (127 KB gzipped)
and a ~1.25 MB worker, fetched the first time someone opens a PDF and never
during ordinary use of the app. This reverses the "no pdf.js" call taken
earlier in this ADR's life, and it should: that call was made about *card
thumbnails*, where the objection was having to download a whole document to
draw a postage stamp. Here the document is already downloaded and the user has
asked to read it.

The forced `application/pdf` blob type keeps doing the security work it did for
the frame: pdf.js parses the bytes as a document, so an attachment whose bytes
are really HTML fails to parse rather than rendering as markup in our origin.

Text takes a different route entirely: `MediaService.fetchText()` decodes the
bytes to a string that Preact escapes into a `<pre>`. No object URL, no MIME
type, and nothing that could be interpreted as markup regardless of what the
sender declared.

### Deferred: service-worker streaming

A service worker that injects the bearer token into `/v1/media/*` requests would
let `<video src="/v1/media/…">` stream with real `Range` seeking — the server
already honours `Range`, `ETag`, and `Accept-Ranges` — and would retire the size
ceilings. It also brings SW lifecycle, token synchronisation, and Tauri-shell
questions that do not belong in this change. Filed as a follow-up.

### Deferred: posters at send time

Generating a thumbnail during upload (ADR 0065's path) and attaching it as
`info.thumbnail_file` gives every client a poster instead of each one having to
download the object to invent one. That is the upload silo, and it only helps
messages sent after it lands — existing media stays placeholder-only.

A PDF *card thumbnail* (first page rendered onto the tile) is now cheap to build
— pdf.js is already here — but is still declined: it would download the whole
document merely to scroll past the card, which is the cost this ADR exists to
avoid. The distinction is consent, not capability.

## Consequences

- A previewed object is fully resident in memory for as long as the preview is
  open, bounded by the ceilings above and by the existing LRU once collapsed.
- Opening a PDF pulls a large viewer chunk and worker. First open on a slow
  connection shows the skeleton for noticeably longer than a video does.
- PDF rendering is ours now, which means PDFs pdf.js cannot parse fail in the
  app rather than in the browser's viewer. The failure path says so and points
  at Download.
- `Lightbox`'s signature changed (`accountId`/`mxcUrl`/`alt` → `label` +
  `children`). `MediaImage` is its only other caller and moves to
  `LightboxImage`.
- `previewPlan()`'s tables are a security-relevant surface. Adding an entry
  means asking whether that type can carry active content — the extension table
  supplies a `Content-Type` the browser will honour.
- A container the browser cannot decode (`.mov` outside Safari is the common
  case) reaches a player and fails there, so the element's `onError` reports it
  as unplayable-here rather than as a load failure, and points at Download.

## Alternatives rejected

- **Widening `is_inline_safe()` to allow `application/pdf`.** Cheaper, but it
  relaxes a server-side defence for every client in order to serve one, and the
  client-side re-type achieves the same rendering with the blast radius confined
  to the caller.
- **Auto-expanding audio.** Matches Element, but re-introduces exactly the
  unasked-for buffering ADR 0064 refused.
- **Inline video and PDF everywhere, with a separate "expand" affordance.** Two
  ways to view the same thing, the first of which is too small to be the one you
  want. The lightbox is the view; the card is the handle.
- **The Fullscreen API on mobile.** Redundant — a fixed, inset-0 overlay is
  already the whole screen — and it fails in ways a `<div>` does not (user-
  gesture requirements, iOS restricting it to `<video>`).
