# ADR 0142 — Media metadata, thumbnails and posters at send time

**Status:** Proposed. Amends [ADR 0059](0059-media-send.md) (the upload kinds and the `send-media` body) and [ADR 0065](0065-web-media-send.md) (what the web client stages); supersedes nothing.
This ADR is the plan only; implementation waits for its review.
Design issue [1205](https://github.com/matrix-axon/matrix-axon/issues/1205); tracking issue [1213](https://github.com/matrix-axon/matrix-axon/issues/1213).

## Context

### What Axon sends today

Every media event Axon sends carries a size, a mimetype and nothing else.

- **Two kinds only.** `MediaUploadKindDto` and `MediaSendKind` are `Image` and `File`.
  A video or audio file is staged as `file`, and `effective_mime` (`crates/axon-sync/src/gateway.rs`) rewrites its `video/*` or `audio/*` type to `application/octet-stream`.
  It does that on purpose: matrix-sdk picks `m.image`, `m.video` or `m.audio` from the major type, and ADR 0059 shipped only `m.file` for non-images.
  An iPhone video sent from the web client looks like this ([issue 24](https://github.com/matrix-axon/matrix-axon/issues/24)):

  ```json
  {
    "msgtype": "m.file",
    "filename": "IMG_9306.mov",
    "info": { "mimetype": "application/octet-stream", "size": 2360866 }
  }
  ```

- **No dimensions.** `attachment_info` fills only `size`, so an image has no `w` or `h` ([issue 11](https://github.com/matrix-axon/matrix-axon/issues/11)).
- **No thumbnails or posters.** `SendMediaRequest` has no field for one, and the server makes none.

### Why it matters

- **Other clients show a video as a download.** Element renders `m.file` as a file tile, with no player.
- **Axon's own streaming refuses it.** ADR 0139 issues a stream ticket only for an object declared `video/*` or `audio/*`, so a video Axon sent is buffered whole in the page, capped at 100 MiB, and limited to 120 s.
  Axon creates the same event shape [issue 1182](https://github.com/matrix-axon/matrix-axon/issues/1182) works around for other senders.
- **Timelines jump.** Without `w` and `h` a client cannot reserve space before the image loads.
  In issue 11's sample, 5 of 13 image events had no dimensions, and nothing sent through Axon had dimensions or a thumbnail.
- **Encrypted media has no cheap preview.** In an encrypted room the homeserver cannot make a thumbnail, because it cannot decrypt the object.
  A sender-embedded `thumbnail_file` is the only kind that can exist, so without one every client downloads the full image just to show a small preview.
  Videos render as a placeholder tile everywhere ([ADR 0072](0072-web-inline-media-preview.md) § "Deferred: posters at send time").

### What other clients send

From the current sources of Element Web (`apps/web/src/ContentMessages.ts`, `apps/web/src/utils/image-media.ts`), Element X iOS (`MediaUploadingPreprocessor.swift`) and Element X Android (`ThumbnailFactory.kt`):

|                              | Element Web                                                              | Element X iOS                | Element X Android        |
| ---------------------------- | ------------------------------------------------------------------------ | ---------------------------- | ------------------------ |
| Bounding box                 | 800 × 600                                                                | 800 × 600 (longest side 800) | 800 × 600                |
| Image thumbnail format       | JPEG for a JPEG source, otherwise PNG                                    | JPEG                         | the source's format      |
| Video poster format          | JPEG                                                                     | JPEG                         | JPEG                     |
| JPEG quality                 | browser default                                                          | 0.78                         | 78                       |
| Poster frame                 | first decoded frame                                                      | 5 s                          | 0 s (closest sync frame) |
| Skip an image thumbnail when | the original is ≤ 32 KiB, or the thumbnail saves ≤ 64 KiB **and** ≤ 10 % | never                        | never                    |
| Always thumbnail             | AVIF, WebP, SVG sources, and every video                                 |                              |                          |
| Blurhash                     | yes                                                                      | yes                          | yes                      |

The 800 × 600 box is also the largest of Synapse's default thumbnail sizes, so a client that can render any Matrix thumbnail can render one this size.
JPEG and PNG are the two formats every Matrix client decodes.

### What exists already

- **Server-side image decoding with bounded memory.** The `image` crate (0.25, `png`, `jpeg` and `webp` features) is an `axon-server` dependency for link previews.
  `validate_preview_image` (`crates/axon-server/src/uploads.rs`) sniffs the format, checks it against the declared MIME type, caps the pixels and decodes under `Limits::max_alloc`.
  `PreviewGenerationService` (`crates/axon-server/src/preview_generation.rs`) decodes in `spawn_blocking` under a semaphore, and keeps the permit until the decode finishes even if the caller goes away.
- **A validated thumbnail claim.** `StagedUploadService::claim_preview_upload` claims a staged upload for use as a link-preview image: PNG, JPEG or WebP, at most 2 MiB and 4,000,000 pixels, with the type and dimensions taken from the bytes.
- **The server holds the plaintext.** axon-server is the Matrix sender.
  It reads the staged file and encrypts it itself (`upload_media_source_with_encryption`), so it can make a thumbnail for an encrypted room.
  Issue 11 assumed only the sending client could do this; for Axon that is not true.
- **SDK support.** matrix-sdk 0.19.1's `AttachmentConfig::thumbnail(Thumbnail)` uploads the thumbnail, encrypted when the room is, and its `make_media_type` attaches `thumbnail_source` and `thumbnail_info` for `m.image`, `m.video` and `m.file`.
  `BaseImageInfo`, `BaseVideoInfo` and `BaseAudioInfo` carry `width`, `height` and `duration` as appropriate.
- **Two hand-built send paths.** `send_thread_member_attachment` (a thread member that is not a reply) and `send_with_previews` (a caption with bundled link previews) build the event themselves, so the SDK's thumbnail handling does not reach them.
- **Client decoders.** The browser decodes the video codecs the platform has, including HEVC on Apple devices, and HEIC on Safari.
  The web client already lazy-loads pdf.js (`PdfViewer.tsx`), and `downscale-preview.ts` already downscales staged images for the composer.

## Decision

### 1. Four upload kinds, and the event type follows the kind

`MediaUploadKindDto` and `MediaSendKind` gain `video` and `audio`, next to `image` and `file`.

| Kind    | Required type                                                                                  | Event     |
| ------- | ---------------------------------------------------------------------------------------------- | --------- |
| `image` | `image/*`                                                                                      | `m.image` |
| `video` | `video/*`                                                                                      | `m.video` |
| `audio` | `audio/*`                                                                                      | `m.audio` |
| `file`  | anything; `image/*`, `video/*` and `audio/*` are still rewritten to `application/octet-stream` | `m.file`  |

The declared type is kept for `image`, `video` and `audio`, so other clients play the media and ADR 0139 streams it.
A kind that does not match its type is a `400`, as `image` already is.
`file` keeps its rewrite, so an existing client that stages a video as `file` sends exactly what it sends today.
Clients stage `video/*` and `audio/*` files with the new kinds; a client that does not is unaffected.

Voice messages (MSC3245, with a waveform) are not part of this ADR.

### 2. One thumbnail standard for every producer

Whichever side makes a thumbnail, it follows the same rules.

- **Size.** Fit inside 800 × 600, keeping the aspect ratio, and never upscale.
- **Format.** JPEG at quality 78.
  PNG only when the source has an alpha channel with at least one pixel that is not fully opaque, since JPEG would paint the transparent area black.
  No WebP or AVIF thumbnails: JPEG and PNG are the only formats every Matrix client decodes, and at 800 × 600 the size gap is small.
- **Orientation.** Apply the EXIF orientation before scaling.
  The thumbnail is upright and has no EXIF data, so a thumbnail never leaks the original's metadata.
- **Image thumbnails are skipped when they don't help**, using Element Web's rule.
  No thumbnail when the original is at most 32 KiB, or when the thumbnail saves at most 64 KiB _and_ at most 10 % of the original.
  A WebP original always gets one, because some clients cannot render WebP.
- **Video posters are always sent**, whatever their size: there is no other way to show a frame without downloading the video.
- **Poster frame.** The frame at 1 s, or at half the duration for a video shorter than 2 s.
  Frame 0 is often black or a fade-in.
  1 s is a short seek, where Element X iOS's 5 s lands past the end of short clips.
- **Metadata.** `thumbnail_info` carries `w`, `h`, `mimetype` and `size` of the thumbnail.
  Every thumbnail is at most 2 MiB, the existing preview-image cap; at 800 × 600 a JPEG is far below it.

### 3. Each thumbnail is made by the side that can decode the media

| Media                                                | Who makes the thumbnail                 | Why                                                                                                                                                                                                                                                                                                 |
| ---------------------------------------------------- | --------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| PNG, JPEG, WebP, GIF (first frame)                   | **axon-server**                         | The server holds the plaintext, already decodes these formats with bounded memory, and one implementation covers every client: web, TUI, CLI and the share extensions ([819](https://github.com/matrix-axon/matrix-axon/issues/819), [820](https://github.com/matrix-axon/matrix-axon/issues/820)). |
| Video                                                | **the client**                          | The browser and the OS have the codecs. The server would need ffmpeg.                                                                                                                                                                                                                               |
| PDF (first page)                                     | **the client**                          | The web client already ships pdf.js. The server would need a PDF renderer.                                                                                                                                                                                                                          |
| HEIC, AVIF and other images the server cannot decode | **the client**, when it can decode them | ADR 0101 left the HEIC decoder open: every practical one is libheif with libde265, which are LGPL-3.0 and would be the first copyleft component Axon ships. Safari decodes it natively.                                                                                                             |

The server validates every thumbnail a client supplies (section 4) and never passes client bytes through unchecked.

A client that makes no thumbnail still gets one for any image the server can decode.
The TUI makes none of its own: it has no video decoder, and the server covers its images.

### 4. `send-media` carries client-supplied metadata and a thumbnail

`SendMediaRequest` gains two optional fields:

```jsonc
{
  "upload_id": "…",
  "info": { "width": 1920, "height": 1080, "duration_ms": 31250 }, // each field optional
  "thumbnail_upload_id": "…",
}
```

- **`info`.** `width` and `height` must come together, and each must be positive and fit a Matrix integer.
  `duration_ms` must be positive and fit a Matrix integer.
  `duration_ms` is accepted for `video` and `audio`, `width` and `height` for `image` and `video`; anything else is a `400`.
- **`thumbnail_upload_id`.** A second staged upload, staged with kind `image`.
  It is claimed with `claim_preview_upload`, so it must be PNG, JPEG or WebP, at most 2 MiB and 4,000,000 pixels, with a type matching its bytes.
  Its `thumbnail_info` dimensions and type come from the verified bytes, not from the client.
  It is accepted for `image`, `video` and `file`, and refused for `audio`, which has no thumbnail in the Matrix spec.
  It must differ from `upload_id`.
- **Validation before anything is sent.** A malformed `info`, an invalid thumbnail or a wrong kind is a `400`, and neither upload is consumed.
- **The thumbnail upload shares the send's fate.** It is consumed when the send succeeds and released when it fails, as `image_upload_id` already is on the link-preview path.
- **Precedence.**
  - When the server can read an image's header, the dimensions it reads win; a client's `width` and `height` are used only for formats the server cannot read.
  - A supplied thumbnail is used as it is, and the server generates none.
  - `duration_ms` and a video's dimensions always come from the client.
- **The link-preview path takes the same fields.** A media send with `url_previews` goes through `bundled_previews::send`, which accepts and applies `info` and `thumbnail_upload_id` the same way.

### 5. Server-side image thumbnails

When an `image` send arrives without a thumbnail, the server derives what it can.

- **Dimensions, always.** Read from the header and the EXIF orientation (`ImageDecoder::dimensions` and `ImageDecoder::orientation`) with no full decode, and swapped for orientations that rotate by 90°.
  This also covers images too large to thumbnail and sends where the thumbnail pool is busy.
- **A thumbnail, when section 2 says one helps.** For PNG, JPEG, WebP and GIF; this adds the `image` crate's `gif` feature.
  - A dedicated pool of **one** decode permit, separate from link previews' four.
    One permit bounds the worst case to a single full-size decode.
  - **A 50 MP cap**, which covers 48 MP phone sensors.
    A larger image gets dimensions and no thumbnail.
    `Limits::max_alloc` is set to 256 MiB, which fits a 50 MP decode with RGBA pixels.
  - **A 5 s budget** for waiting for the permit and for the decode.
    The decode runs in `spawn_blocking` and keeps its permit until it finishes, even after the budget expires, as `PreviewGenerationService` does.
    When the budget expires, the image is sent without a thumbnail.
  - **A failure never fails the send.** A decode error, a busy pool or an expired budget sends the image without a thumbnail and logs the reason as an `error_kind`.
- **Placement.** A consumer-owned port in `axon-api` (`SendThumbnailer`, called by the `send-media` route), with its adapter in `axon-server` next to `validate_preview_image` (ADR 0021).
  `axon-api` and `axon-sync` take no `image` dependency.
- **A kill switch.** `media.send_thumbnails` (default `true`) turns generation off.
  Dimensions are still read.
  It is for operators on small machines, and is documented with the other `media.*` settings.

The thumbnail adds no work for any other kind: a `file`, `video` or `audio` send is unchanged unless the client supplied a thumbnail.

### 6. The gateway attaches the metadata on every send path

`MediaAttachment` gains the optional width, height, duration and a verified thumbnail (bytes, type, width, height).

- `attachment_info` builds `BaseImageInfo`, `BaseVideoInfo`, `BaseAudioInfo` or `BaseFileInfo` from them.
- The SDK path passes the thumbnail to `AttachmentConfig::thumbnail`.
- The two hand-built paths (`send_thread_member_attachment`, `send_with_previews`) upload the thumbnail with the room's encryption state, as they already upload the main object.
  They set `thumbnail_source` and `thumbnail_info` themselves.
  A test pins each path, so the three paths cannot drift apart.
- An edit of a media caption already carries the original content forward, so it keeps the thumbnail without any change.

### 7. Client-side posters and thumbnails (web)

The web client generates, stages and attaches what the server cannot.

- **Video.** Load the staged file into an off-DOM `<video>` from an object URL, wait for `loadedmetadata`, seek to the section 2 frame, and draw it to a canvas once it has `seeked`.
  Report `duration_ms` (rounded up, as Element does), and `width` and `height` from `videoWidth` and `videoHeight`.
  Give up after 10 s, and on a codec the browser cannot decode (for example HEVC on some Chromium builds), and send without a poster and without `info`.
- **Audio.** Report `duration_ms` from an off-DOM `<audio>`'s `loadedmetadata`.
- **HEIC, AVIF and other images outside the server's formats.** When `createImageBitmap` decodes it, generate the thumbnail and report `width` and `height`.
  `downscale-preview.ts` already does most of this for the composer.
- **PDF.** Render page 1 with the lazily loaded pdf.js.
  This step comes last (section 9): few other clients show a thumbnail on `m.file`, so it mostly helps Axon's own file cards, which must learn to show it.
- Each thumbnail is staged as an ordinary `image` upload before `send-media`, and a generation failure only means the send goes without one.

Native share extensions (issues 819 and 820) may use the OS's thumbnailers instead (`QLThumbnailGenerator` or `AVAssetImageGenerator` on iOS, `ThumbnailUtils` on Android).
The API does not care who made the thumbnail, only that it meets section 2 and passes section 4's validation.

### 8. The asynchronous send

`send-media` will become asynchronous, so a large upload does not hold the request open (design in [issue 1206](https://github.com/matrix-axon/matrix-axon/issues/1206)).
This ADR is written so that change does not alter it.
Section 4's validation happens when the request is accepted, so its errors stay immediate `400`s.
Section 5's generation and section 6's upload run in the send itself, so in the asynchronous model they no longer delay the response.
Until then, section 5's budget bounds the extra latency of a synchronous send to 5 s at worst.

### 9. Not in this ADR

- **Blurhash** (MSC2448, `xyz.amorgan.blurhash`).
  All three Element clients send one.
  Both producers can compute it from the thumbnail later without changing this design.
- **`is_animated`** (MSC4230), voice messages (MSC3245) and audio waveforms.
- **Stripping EXIF or GPS data from originals.** The original is sent unchanged, as today; only thumbnails are clean.
- **Media already sent.** Nothing can add a thumbnail to an encrypted event after the fact.
- **A server video or PDF decoder** (see Alternatives).

## Consequences

- Images sent from any Axon client, encrypted or not, carry `w`, `h` and a thumbnail that Element, Element X and Axon render, as soon as the server steps land and with no client change.
- Videos sent from the web client arrive as `m.video` with a poster, dimensions and duration, and are streamed by ADR 0139.
  Videos sent from the TUI arrive as `m.video` too, without a poster.
- An image send costs one header read, and, when a thumbnail helps, one bounded decode.
  With one permit, a burst of large photos takes turns, and any send that waits more than 5 s goes without a thumbnail.
  How often that happens should be measured on production before the pool is changed.
- A full-size decode can briefly take about 200 MiB of server memory; the single permit keeps it to one at a time.
- The upload API grows by two kinds and two optional fields.
  The change is additive: an existing client is unaffected (AGENTS.md, ADR 0099).
- A thumbnail is a second upload to the homeserver, of tens of KiB.
- Video posters depend on the sender's browser.
  A codec the browser cannot decode means no poster, so the same file can get one when sent from Safari and none from Chromium on Linux.

## Alternatives considered

- **All thumbnails on the server, with ffmpeg and a PDF renderer.**
  One producer for everything, including TUI posters.
  Rejected because ffmpeg and pdfium are large native dependencies for every platform and the Docker image.
  ffmpeg is LGPL or GPL depending on its build, the copyleft question ADR 0101 left open for libheif, and mupdf is AGPL.
  It would also parse untrusted containers inside the server process.
  The clients that send video can already decode it.
- **All thumbnails in the client.**
  Matches issue 11's original sketch.
  Rejected for images: the TUI, the CLI and the share extensions would each need their own decoder, and today none of them sends a thumbnail at all.
  The server already decodes these formats safely, and holds the plaintext even for encrypted rooms.
- **WebP thumbnails.**
  Smaller, and decoded by every current browser.
  Rejected because "every Matrix client" is wider than "every current browser", and the saving at 800 × 600 is small.
- **Element Web's PNG thumbnail for every non-JPEG source.**
  Rejected because an opaque PNG screenshot makes a PNG thumbnail several times larger than the JPEG; alpha is the only reason to keep PNG.
- **Relying on the homeserver's thumbnails.**
  Synapse thumbnails unencrypted media on request, but cannot read encrypted media, which is most of what Axon sends.
- **Letting the client's dimensions win.**
  Rejected for formats the server can read, because the bytes are authoritative and a client bug would otherwise reach every recipient.

## Delivery

One silo per pull request (AGENTS.md).

1. This ADR ([1205](https://github.com/matrix-axon/matrix-axon/issues/1205)).
2. **Server:** the `video` and `audio` kinds (section 1, [1207](https://github.com/matrix-axon/matrix-axon/issues/1207)).
3. **Server:** `info` and `thumbnail_upload_id` on every send path (sections 4 and 6, [11](https://github.com/matrix-axon/matrix-axon/issues/11)).
4. **Server:** dimensions and image thumbnails (section 5, [1208](https://github.com/matrix-axon/matrix-axon/issues/1208)); needs step 3.
5. **Web:** the new kinds, video posters and audio durations (section 7, [24](https://github.com/matrix-axon/matrix-axon/issues/24)); needs steps 2 and 3.
6. **TUI:** the new kinds ([1212](https://github.com/matrix-axon/matrix-axon/issues/1212)); needs step 2.
7. **Web, last:** HEIC/AVIF thumbnails and PDF first pages, with thumbnails on Axon's file cards (section 7, [1209](https://github.com/matrix-axon/matrix-axon/issues/1209)); needs step 3.

Each step regenerates `openapi/openapi.json` and `clients/web/src/api/schema.d.ts` where it changes the API, and updates `docs/client-parity.md` and `docs/demo-coverage.md` where it changes what a client shows.
