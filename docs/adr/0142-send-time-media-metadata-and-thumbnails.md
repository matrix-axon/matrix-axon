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

**Clients find the new kinds through `GET /v1/capabilities`.**
An older server rejects `kind=video` or `kind=audio` while parsing the staging query, so a newer client that used them unconditionally would lose sends an older client makes successfully as `file`.
The web shells and the TUI can connect to any server, so they meet this in both upgrade orders.

- `CapabilitiesDto` gains `media_upload_kinds`, the kinds staging accepts (`["image", "file", "video", "audio"]`), and `send_media_thumbnails: true`.
- **`send_media_thumbnails` means "accepts and applies section 4's `info`, `thumbnail_upload_id` and `txn_id`".**
  It does not depend on `media.send_thumbnails` (section 5), and stays true when that switch is off.
  The switch turns off only the server's own generation: a thumbnail a client supplies is still normalised and attached, so a poster a client stages is never discarded.
- A client stages `video/*` and `audio/*` files with the new kinds only when `media_upload_kinds` lists them.
  Otherwise it uses `file`, as today.
  It sends `info` and `thumbnail_upload_id`, and makes posters, only when `send_media_thumbnails` is true.
- **An older server ignores the new `send-media` fields rather than rejecting them.**
  `SendMediaRequest` has no `#[serde(deny_unknown_fields)]`, in the released v0.1.28, v0.1.29 and v0.1.30 or on `main`, so it skips fields it does not know.
  That is why `thumbnail_upload_id` needs the flag: without it a poster would be uploaded, never attached, and left staged until it expires.
  `txn_id` (section 4) needs no flag to be sent: an older server ignores it, and a retry there can post twice, as it does today.
  So the deduplication holds only against a server whose capabilities include `send_media_thumbnails`, which ships with `txn_id` in the same delivery step (3).
  The flag stands for all three fields.
- A server without the route (a `404`), or without the field, supports neither.
- Both orders are tested: a new client against an old server sends `file` and no thumbnail, and an old client against a new server is unchanged.

Voice messages (MSC3245, with a waveform) are not part of this ADR.

### 2. One thumbnail standard for every producer

Whichever side makes a thumbnail, it follows the same rules.

- **Size.** Fit inside 800 × 600, keeping the aspect ratio, and never upscale.
- **Format.** JPEG at quality 78.
  PNG only when the source has an alpha channel with at least one pixel that is not fully opaque, since JPEG would paint the transparent area black.
  No WebP or AVIF thumbnails: JPEG and PNG are the only formats every Matrix client decodes, and at 800 × 600 the size gap is small.
- **Orientation.** The thumbnail is upright: the EXIF orientation is applied to it, and its own bytes have no EXIF data, so a thumbnail never leaks the original's metadata.
  A producer that rotates pixels rotates the scaled thumbnail, not the full-size image (section 5).
- **Image thumbnails are skipped when they don't help**, using Element Web's rule.
  No thumbnail when the original is at most 32 KiB, or when the thumbnail saves at most 64 KiB _and_ at most 10 % of the original.
  The rule applies only to JPEG, PNG and GIF originals.
  Any other format (WebP, AVIF, HEIC/HEIF, SVG) always gets a thumbnail when its producer can make one, however small the original, because some clients cannot render the original at all.
  Element Web exempts WebP, AVIF and SVG for the same reason.
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

The server validates and re-encodes every thumbnail a client supplies (section 4), so it never publishes client bytes unchecked.

A client that makes no thumbnail still gets one for any image the server can decode.
The TUI makes none of its own: it has no video decoder, and the server covers its images.

### 4. `send-media` carries client-supplied metadata and a thumbnail

`SendMediaRequest` gains two optional fields:

```jsonc
{
  "upload_id": "…",
  "info": { "width": 1920, "height": 1080, "duration_ms": 31250 }, // each field optional
  "thumbnail_upload_id": "…",
  "txn_id": "…", // client-generated, kept across retries of this send
}
```

- **`info`.** `width` and `height` must come together, and each must be positive and fit a Matrix integer.
  `duration_ms` must be positive and fit a Matrix integer.
  `duration_ms` is accepted for `video` and `audio`, `width` and `height` for `image` and `video`; anything else is a `400`.
- **`thumbnail_upload_id`.** A second staged upload, staged with kind `image`.
  It is accepted for `image`, `video` and `file`, and refused for `audio`, which has no thumbnail in the Matrix spec.
  - **What the wire accepts.** It is claimed with `claim_preview_upload`, so it must be PNG, JPEG or WebP, at most 2 MiB and 4,000,000 pixels, with a type matching its bytes.
  - **What the server publishes.** The server does not forward the client's bytes.
    The validator has already decoded them, so the server applies their EXIF orientation, scales them into the section 2 box if they are larger, and re-encodes them as section 2's JPEG or PNG.
    So every published thumbnail meets section 2 whoever made it, carries no EXIF, and has a `thumbnail_info` that describes the bytes actually sent.
    This runs where the validation already does: in `spawn_blocking`, under the staging semaphore that `claim_preview_upload` holds.
    For a link preview, the wait for that semaphore is bounded by `media.upload_request_timeout_secs` (60 s).
  - **In `send-media`, the claim comes first and the thumbnail work has a 5 s budget.**
    The task claims the thumbnail (staged to `sending`, reading at most 2 MiB) before it waits for a permit, so it owns the row on every path below.
    The permit wait, the decode, the normalisation and the encode then share one 5 s budget.
    When the budget expires, the thumbnail is not attached and the send goes without it.
    A decode that is still running keeps its permit until it finishes, as section 5's does.
    The claimed thumbnail then settles with the send, like any other (see "shares the send's fate" below): consumed if the event is sent, released if the send fails.
    A busy or slow decoder is not a client error, so this is never a `400`; only a thumbnail whose validation finished and failed is.
- **Distinct ids.** `upload_id`, `thumbnail_upload_id` and every link preview's `image_upload_id` must all differ.
  This extends the check `bundled_previews::send` already makes, and runs before any claim, so a collision is a `400` that consumes nothing.
- **Validation before anything is sent.** A malformed `info`, an invalid thumbnail or a wrong kind is a `400`, and every upload the send had claimed is released.
- **One owner for every claim.** Today the plain `send-media` handler claims in the request future, and releases only by returning through the handler.
  A client that disconnects after the claim leaves the upload `sending` until the next boot, because the expiry sweeper removes only `staged` rows.
  So every `send-media`, not only the link-preview one, runs its claims, validation, send and settlement in one spawned task, as `bundled_previews::send` already does.
  The request future only waits for that task.
  The task settles every id it acquired: consumed on success, released on any failure, including a thumbnail that fails validation after the original was claimed.
  A disconnect therefore cannot strand an upload, and the same task becomes the send itself under section 8.
- **`txn_id`, so a retry does not post the media twice.**
  Because the task outlives the request, a client that loses the response cannot tell whether its media was sent.
  The web client's retry (`retrySend` in `clients/web/src/stores/timeline.ts`) then stages the file again and sends it again, and the room shows it twice.
  A retry with the same `upload_id` cannot do this, since a claimed or consumed upload is a `404`, but the web client does not keep the `upload_id`; it keeps the file.
  So `send-media` takes an optional `txn_id`: 1 to 64 characters of `[A-Za-z0-9._~-]`.
  The client generates one per send it shows the user (a UUID), and sends the same one on every retry of that send.
  - **The homeserver deduplicates the event.**
    The server passes the id to the event send as the Matrix transaction id (`with_transaction_id`), prefixed with `axon-media-`.
    When a send with a transaction id the homeserver has already seen reaches it, it returns the first event's id and creates nothing (client-server API, "Transaction identifiers").
    All of an account's Axon clients share one Matrix device, so the prefix and the client's UUID keep their ids apart from each other's and from the SDK's own.
  - **The server joins a retry to the first send.**
    It keeps each account's recent ids in memory, checked before any claim: an id in flight, and an id that succeeded within the last 10 minutes, at most 256 per account.
    An entry is keyed on the account, the room and the `txn_id`, no wider than the homeserver's scope.
    The spec scopes a transaction id to one device and one endpoint, and Synapse keys its transaction cache on the full request path, which names the room (`PUT /rooms/{roomId}/send/{eventType}/{txnId}`).
    So an id reused in another room, by a client bug or a script, is a separate send in both places.
    It never returns the first room's event as if it had been sent to the second.
    A retry whose id is in flight waits for the first task.
    That wait has no bound of its own: the first task's steps are each bounded (section 8), so the join waits no longer than the first send has left.
    A retry whose id succeeded gets the first event's id back, without uploading anything, and the retry's own staged uploads are deleted, because they were staged for the send that already happened.
    A failed send is not remembered, so its retry runs as a new send.
  - **After a restart** the map is empty, so a retry uploads the file again, but the homeserver still returns the first event's id for as long as it keeps transaction ids.
  - **What the client sees.** The retry's response carries the first send's event id, so the client resolves its failed echo to the event already in the timeline.
    Until it retries, the event from the live stream and the failed echo both show, as they do today.
  - The asynchronous send (section 8) uses the same id as its status key.
    Text and sticker sends have the same lost-response duplicate today; [issue 1223](https://github.com/matrix-axon/matrix-axon/issues/1223) gives them the same field.
- **The thumbnail upload shares the send's fate.** It is consumed when the event is sent and released when the send fails.
  That holds whether or not the thumbnail made it into the event: it was attached, its upload failed (section 6), or its 5 s budget ran out.
  A thumbnail that fails validation fails the send with a `400`, so it is released too.
- **Precedence.**
  - When the server can read an image's header, the dimensions it reads win; a client's `width` and `height` are used only for formats the server cannot read.
  - A supplied thumbnail is used, normalised as above, and the server generates none.
  - `duration_ms` and a video's dimensions always come from the client.
- **The link-preview path takes the same fields.** A media send with `url_previews` goes through `bundled_previews::send`, which accepts and applies `info` and `thumbnail_upload_id` the same way.

### 5. Server-side image thumbnails

When an `image` send arrives without a thumbnail, the server derives what it can.

- **Dimensions, always, from a bounded probe.** The probe reads the format, the dimensions, the EXIF orientation and, for a JPEG, whether it is progressive.
  Dimensions are swapped for orientations that rotate by 90°.
  It never copies the file: the `image` crate's `JpegDecoder::new` copies its whole input into a new `Vec` before it parses the headers, so the probe gives it only the first 1 MiB.
  The headers and EXIF sit before the first scan, and that is the only part a header parse reads.
  A JPEG whose headers end past 1 MiB is probed again from the whole file, inside the decode permit and budget below.
  The probe also covers images too large to thumbnail, sends where the pool is busy, and `media.send_thumbnails = false`.
- **A thumbnail, when section 2 says one helps.** For PNG, JPEG, WebP and GIF; this adds the `image` crate's `gif` feature.
  - A dedicated pool of **one** decode permit, separate from link previews' four.
    Everything below that holds more than the probe's bytes runs inside that permit.
  - **A decode-cost cap from the probe, checked before decoding.**
    The probe also reads the colour type and bit depth, so the server knows how many bytes each pixel takes once decoded, and how much scratch space its decoder adds:

    | Source            | Decoded bytes per pixel                                 | Decoder scratch per pixel                                                                                                         | Largest image under 192 MiB |
    | ----------------- | ------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------- | --------------------------- |
    | JPEG, baseline    | 1 (grey) or 3 (RGB; CMYK and YCCK are converted to RGB) | none                                                                                                                              | about 67 MP                 |
    | JPEG, progressive | 1 or 3                                                  | 2 per stored component, for the coefficients of the whole image: up to 6 for YCbCr, 8 for CMYK                                    | about 22 MP; 18 MP for CMYK |
    | PNG, 8-bit        | 1 to 4 (a palette expands to RGB or RGBA)               | none                                                                                                                              | about 50 MP for RGBA        |
    | PNG, 16-bit       | 2 to 8 (`Rgba16` is 8)                                  | none                                                                                                                              | about 25 MP for RGBA        |
    | WebP              | 3 or 4                                                  | up to 4: an opaque lossless image is decoded as RGBA and then copied to RGB; a lossy one keeps its YUV planes and its alpha plane | about 25 MP for RGBA        |
    | GIF, first frame  | 4                                                       | up to 4: a separate frame buffer, when the first frame is smaller than the canvas                                                 | about 25 MP                 |

    These come from `image` 0.25.10's colour-type mapping and from the decoders it uses (zune-jpeg 0.5.15, png 0.18.1, image-webp 0.2.4, gif 0.14.2).
    A thumbnail is made only when the pixel count times the decoded bytes and the scratch is at most 192 MiB, neither side is over 16,384 pixels, and a JPEG's compressed file is at most 50 MiB.
    The `image` crate's JPEG decoder copies that file, so its size counts too; no other decoder copies its input.
    An image over any of these gets dimensions and no thumbnail.
    A 48 MP phone photo is a baseline JPEG at 3 bytes per pixel, about 137 MiB, so it gets one.
    A 50 MP 16-bit RGBA PNG would need about 381 MiB, so it does not.
    These caps are the real bound; `Limits::max_alloc` (256 MiB) is a backstop, because the `image` crate applies it on a best-effort basis.

  - **Nothing copies or converts the full-size image.** It is scaled with `DynamicImage::thumbnail`, which writes the small image directly, with no full-size intermediate, in the source's own pixel format.
    The bounds are swapped for a 90° orientation.
    Everything else runs on the scaled image: the rotation, the alpha check of section 2, and the conversion to 8-bit RGB or RGBA for the encoder.
    `DynamicImage::apply_orientation` would allocate a second full-size buffer for a quarter turn, and converting a 16-bit image first would allocate another, which this avoids.
  - **A 5 s budget** for all of it: the wait for the permit, the decode, the scale, the alpha check, the rotation and the encode.
    The work runs in `spawn_blocking` and keeps its permit until it finishes, even after the budget expires, as `PreviewGenerationService` does.
    When the budget expires, the image is sent without a thumbnail.
    Because the permit is held until the work finishes, an abandoned decode never overlaps the next send's decode.
    The next send waits for the permit within its own budget, and goes without a thumbnail if the abandoned work does not finish in time.
    Generation starts when the send starts and runs while the original uploads (section 6), so on any upload of more than a few seconds it adds no time.
  - **A failure never fails the send.** A decode error, a busy pool or an expired budget sends the image without a thumbnail and logs the reason as an `error_kind`.
- **The memory budget of one decode**, all inside the single permit:

  | Held                                                                                         | At most                                                                                                                                                                 |
  | -------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
  | The JPEG decoder's copy of the compressed file                                               | 50 MiB, the JPEG file cap; other formats are read in place                                                                                                              |
  | Decoded pixels and decoder scratch                                                           | 192 MiB, the cost cap                                                                                                                                                   |
  | Row and MCU buffers, the scaled and rotated thumbnail, its 8-bit conversion and its encoding | under 10 MiB: a 16-bit RGBA thumbnail is 3.7 MiB, its 8-bit copy 1.8 MiB, the encoding at most 2 MiB, and no side over 16,384 pixels keeps the row buffers to a few MiB |

  So one decode peaks near 252 MiB, below the 256 MiB `max_alloc` backstop.

- **The whole process.** That figure is one decode.
  Image decoding in the whole process is bounded by three pools, each with its own permits:

  | Pool                                                                                    | Permits                           | Each at most                                                                                                       | Pool at most               |
  | --------------------------------------------------------------------------------------- | --------------------------------- | ------------------------------------------------------------------------------------------------------------------ | -------------------------- |
  | Send thumbnails (this section)                                                          | 1; an abandoned decode keeps it   | about 252 MiB                                                                                                      | 252 MiB                    |
  | Staging (`claim_preview_upload`): client thumbnails (section 4) and link-preview images | `media.max_concurrent_uploads`, 4 | 2 MiB of bytes, a decode of at most 4 MP under a 32 MiB `max_alloc`, and under 8 MiB for section 4's normalisation | about 42 MiB × 4 = 168 MiB |
  | Link-preview generation (`PreviewGenerationService`)                                    | 4                                 | 2 MiB of bytes and a decode under the same 32 MiB `max_alloc`                                                      | about 34 MiB × 4 = 136 MiB |

  So decoding peaks near 556 MiB across the process.
  This ADR adds the first pool and the normalisation share of the second, about 284 MiB; the rest exists today.
  An operator on a small machine turns off the largest part with `media.send_thumbnails = false`, and shrinks the second with `media.max_concurrent_uploads`.
  The staged originals are outside these pools.
  The `send-media` route reads the whole staged file into memory today, for every kind, before any of this runs: up to `media.max_upload_bytes` (100 MiB) for each send in flight.
  That is the send path's existing cost, not this ADR's, and [issue 1185](https://github.com/matrix-axon/matrix-axon/issues/1185) removes it.
  Issue 1208's implementation measures the process's peak memory with all three pools busy before the caps are kept.

- **Placement.** A consumer-owned port in `axon-api` (`SendThumbnailer`, called by the `send-media` route), with its adapter in `axon-server` next to `validate_preview_image` (ADR 0021).
  `axon-api` and `axon-sync` take no `image` dependency.
- **A kill switch.** `media.send_thumbnails` (default `true`) turns generation off.
  Dimensions are still read.
  It is for operators on small machines, and is documented with the other `media.*` settings.

The thumbnail adds no work for any other kind: a `file`, `video` or `audio` send is unchanged unless the client supplied a thumbnail.

### 6. One send path builds every media event, and a thumbnail upload is best-effort

`MediaAttachment` gains the optional width, height, duration and a verified thumbnail (bytes, type, width, height).
`attachment_info` builds `BaseImageInfo`, `BaseVideoInfo`, `BaseAudioInfo` or `BaseFileInfo` from them.

**`Room::send_attachment` is no longer used.**
With a thumbnail, it uploads the original and the thumbnail together (`try_join`), in plaintext and encrypted rooms alike, and returns an error before sending the event if either fails.
So a rejected thumbnail upload, for example one that alone hits a rate limit, would fail an attachment that sends today.
Retrying without the thumbnail would upload the original a second time.
Instead, every media send takes the steps the two hand-built paths (`send_thread_member_attachment`, `send_with_previews`) already take, `upload_media_source` and then `media_message_type`, in one shared function:

1. Upload the original with the room's encryption state (`upload_media_source`), bounded by `media.upstream_upload_timeout_secs`.
   A failure fails the send, as today.
2. If there is a thumbnail, upload it the same way, bounded by its own 10 s.
   A failure or timeout sends the event without a thumbnail, and is logged with an `error_kind`.
   This holds for a client-supplied thumbnail too.
   A thumbnail is optional metadata, and the client's thumbnail already passed section 4's validation.
3. Build the content with `thumbnail_source` and `thumbnail_info` when the thumbnail uploaded, and with the relation the request asked for.
   A thread member keeps its existing hand-built relation.
   A reply uses `Room::make_reply_event`, which is what `send_attachment` calls internally.
4. Send it with `Room::send`, with section 4's `txn_id` as the transaction id when there is one.
   Step 3's reply lookup and this send share one `sync.send_mutation_timeout_secs` (30 s) bound, as a text send's do.

**Each step has its own bound.**
Today one `media.upstream_upload_timeout_secs` (60 s) bounds the whole gateway call, upload and event send together.
If the thumbnail's upload ran inside that bound, it could push a slow original past it and fail the send, which is what this section exists to prevent.
The bounds and the new overall ceiling are in section 8.

**What the shared function keeps from `send_attachment`.**
Checked against matrix-sdk 0.19.1 (`Room::prepare_and_send_attachment` in `room/mod.rs`), for the options Axon passes today, which are `info`, `caption` and `reply`:

- **The encryption decision.** It reads `latest_encryption_state` and uploads encrypted or plain; `upload_media_source` already does the same.
- **The message type and its fields.** `make_media_type!` picks `m.image`, `m.video`, `m.audio` or `m.file` from the content type and sets the filename, the caption (plain or HTML), the info and the thumbnail.
  `media_message_type`, which both hand-built paths already use, does the same; it gains the `m.video` and `m.audio` arms for section 1's kinds.
- **The reply.** `Room::make_reply_event` is public in 0.19.1 (`room/reply.rs`), with the signature `(content: RoomMessageEventContentWithoutRelation, reply: Reply)`.
  The shared function passes it the same `Reply` that `attachment_reply` builds today, so the reply's thread handling and its `m.mentions` of the replied-to sender do not change.
- **The send.** `Room::send` checks that the room is joined and encrypts the event, as `send_attachment`'s final call does.

`send_attachment` does nothing else for Axon.
Options Axon has never set (`mentions`, `extra_content`, `store_in_cache`) are not reproduced.
It does not go through the SDK's `RoomSendQueue`, so there is no SDK local echo or queue to keep.
The one difference is deliberate: step 4 sets a transaction id.

Tests cover each relation shape: no relation, a reply, a thread member, and a reply inside a thread.
For each, they compare the new content with what `send_attachment` produced for the same input.
They also pin a rejected thumbnail upload with an accepted original, in a plaintext room and an encrypted room, and check that the event is sent without `thumbnail_*`.
The streamed upload of [issue 1185](https://github.com/matrix-axon/matrix-axon/issues/1185) also needs this hand-built send, so it replaces step 1 and keeps the rest.

An edit of a media caption already carries the original content forward, so it keeps the thumbnail without any change.

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
- **One `txn_id` per send, kept across retries** (section 4).
  The id is created with the echo.
  `retrySend` builds a fresh echo today, so the id moves to the new echo, or a retry would not be recognised.
  The client sends it to any server, but only a server with `send_media_thumbnails` deduplicates; against an older one a retry can post twice, as it does today.
- All of this, and the new kinds, only against a server whose capabilities list them (section 1).

Native share extensions (issues 819 and 820) may use the OS's thumbnailers instead (`QLThumbnailGenerator` or `AVAssetImageGenerator` on iOS, `ThumbnailUtils` on Android).
The API does not care who made the thumbnail, only that it passes section 4's validation; the server's re-encoding then makes it meet section 2.

### 8. The asynchronous send

`send-media` will become asynchronous, so a large upload does not hold the request open (design in [issue 1206](https://github.com/matrix-axon/matrix-axon/issues/1206)).
This ADR is written so that change does not alter it.
Section 4's validation happens when the request is accepted, so its errors stay immediate `400`s.
Section 5's generation and section 6's upload run in the send itself, so in the asynchronous model they no longer delay the response.
Until then, a synchronous send runs these steps in order:

| Step                                                                                                                       | Bound                                                              |
| -------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| 1. Validation and claims, including a client thumbnail's permit wait, decode, normalisation and encode (section 4)         | 5 s for all of the thumbnail work                                  |
| 2. The original's upload, with server generation (section 5) running alongside it, so the step takes the longer of the two | `media.upstream_upload_timeout_secs` (60 s); all of generation 5 s |
| 3. The thumbnail's upload (section 6)                                                                                      | 10 s                                                               |
| 4. Building and sending the event, including fetching the replied-to event for a reply                                     | `sync.send_mutation_timeout_secs` (30 s)                           |

So the worst case is 5 s, plus 60 s, plus 10 s, plus 30 s: 105 s with the default settings.
Every piece of thumbnail work is inside a bound, the client's in step 1 and the server's in step 2.
The only untimed work is what today's send also does untimed: reading the staged original from local disk at the claim.
A retry that joins an in-flight send (section 4) waits only for what is left of the first send, so it adds nothing to the ceiling.
Today steps 2 and 4 share one 60 s bound, so the ceiling rises by about 45 s.

**Amended 2026-11-23, in the review of delivery step 3.**
The shipped web client gives up on `send-media` after 65 s and sends no `txn_id`, so a send that succeeded past that would be retried and posted twice.
Until clients wait for the sum above, step 3 keeps one `media.upstream_upload_timeout_secs` budget for everything but the thumbnail's upload, as today: resolving the room, the original's upload, the reply lookup and the event.
The budget is paused while the thumbnail uploads under its own 10 s, so a slow thumbnail still never fails the send.
The per-step bounds above arrive in the same release as delivery step 5 ([issue 1243](https://github.com/matrix-axon/matrix-axon/issues/1243)).
Each step reaches its bound only when the homeserver is slow or failing; on a healthy one, the thumbnail adds well under a second, and generation adds nothing once the original takes longer than the decode.

A client that bounds `send-media` must allow for the new ceiling.
The web client derives its timeout from the upstream upload bound (`AXON_UPSTREAM_UPLOAD_MS` in `clients/web/src/api/client.ts`).
Step 5 of the delivery changes it to the sum above, keeping its existing `PAST_AXON_MS` margin for the untimed work and the network.
A client that still gives up early is covered by section 4's `txn_id`, against a server that advertises `send_media_thumbnails` (section 1): its retry joins the first send or gets the event it posted.
A joined retry can itself time out on the client if little of its own timeout is left.
The next retry with the same id joins again, or, once the first send has finished, gets its event.

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
- An image send costs a probe of at most 1 MiB, and, when a thumbnail helps, one bounded decode.
  With one permit, a burst of large photos takes turns, and any send that waits more than 5 s goes without a thumbnail.
  How often that happens should be measured on production before the pool is changed.
- A full-size decode can briefly take about 252 MiB of server memory (section 5's tables), whatever the format or bit depth; the single permit keeps it to one at a time.
- Axon stops using `send_attachment` for media and builds every media event itself.
  That is more of the event under Axon's control, and one path for issue 1185 to change.
- A client disconnect can no longer strand a claimed upload as `sending` until the next boot.
- A media send whose response is lost can be retried without posting twice, as long as the client keeps its `txn_id` and the server advertises `send_media_thumbnails`.
  Against an older server a retry can still post twice, as it does today.
  Text and sticker sends still can post twice until issue 1223.
- A synchronous media send can take up to about 105 s with the default settings, rather than 60 s, until the asynchronous send (section 8).
- Clients check `GET /v1/capabilities` before using the new kinds and fields.
- The upload API grows by two kinds and three optional fields.
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
- **Keeping `send_attachment` and retrying without the thumbnail when it fails.**
  Rejected because the retry uploads the original again, which for a large video is the expensive part.
- **Treating a validated client thumbnail as required.**
  Rejected: a send that would succeed without the thumbnail should not fail because of it.
- **Forwarding a validated client thumbnail unchanged.**
  Rejected because the validator accepts WebP, up to 4 MP and EXIF-tagged bytes, so the published thumbnail would not meet section 2.
- **Letting the client's dimensions win.**
  Rejected for formats the server can read, because the bytes are authoritative and a client bug would otherwise reach every recipient.
- **Accepting duplicate media on a lost response.**
  Rejected because section 4's task makes it more likely than today: the send now finishes after the client has given up.
- **Keying retries on `upload_id` alone.**
  A claimed or consumed upload is already a `404`, so a retry with the same `upload_id` cannot post twice.
  Rejected as the whole answer: the client cannot tell an upload in flight from one already sent, and the web client re-stages the file on retry.
- **Fixed pixel caps (50 MP, or 25 MP for progressive JPEG).**
  An earlier draft of this ADR used them, assuming 4 bytes per pixel.
  Rejected because a 16-bit PNG decodes to 8, so the same pixel count could take twice the memory.

## Delivery

One silo per pull request (AGENTS.md).

1. This ADR ([1205](https://github.com/matrix-axon/matrix-axon/issues/1205)).
2. **Server:** the `video` and `audio` kinds and the `media_upload_kinds` capability (section 1, [1207](https://github.com/matrix-axon/matrix-axon/issues/1207)).
3. **Server:** `info` and `thumbnail_upload_id`, the claim-owning task, `txn_id` and the in-flight map, the single hand-built send with best-effort thumbnail upload and per-step bounds, and the `send_media_thumbnails` capability (sections 1, 4, 6 and 8, [11](https://github.com/matrix-axon/matrix-axon/issues/11)).
4. **Server:** dimensions and image thumbnails (section 5, [1208](https://github.com/matrix-axon/matrix-axon/issues/1208)); needs step 3.
5. **Web:** the new kinds, video posters and audio durations, a `txn_id` kept across retries, and the `send-media` timeout raised to section 8's sum (sections 7 and 8, [24](https://github.com/matrix-axon/matrix-axon/issues/24)); needs steps 2 and 3.
6. **TUI:** the new kinds, used only when the server's capabilities list them ([1212](https://github.com/matrix-axon/matrix-axon/issues/1212)); needs step 2.
7. **Web, last:** HEIC/AVIF thumbnails and PDF first pages, with thumbnails on Axon's file cards (section 7, [1209](https://github.com/matrix-axon/matrix-axon/issues/1209)); needs step 3.

Each step regenerates `openapi/openapi.json` and `clients/web/src/api/schema.d.ts` where it changes the API, and updates `docs/client-parity.md` and `docs/demo-coverage.md` where it changes what a client shows.
