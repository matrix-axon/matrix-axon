# ADR 0101 — HEIC images in the web client

## Status

**Partly decided, deliberately.**

§1–§3 are **implemented in the PR that adds this ADR**: the failure placeholder
now says what it actually knows and offers a download, so nobody is told their
photo failed to decrypt when it did not. That is a stopgap, and it addresses
only the honesty half of the problem — the photo still does not render outside
WebKit.

Everything from "Open question" onwards is **not decided and not scheduled**.
Making a HEIC actually appear means shipping a decoder, which turns out to be a
license and packaging decision rather than a rendering one, and it is recorded
here for discussion rather than resolved. Nothing in this ADR authorises adding
a dependency.

## Context

A photo sent from an iPhone rendered in the web client as the placeholder
**"Encrypted media — server could not decrypt"**. The media was not
undecryptable. It was HEIC, and the browser could not decode it.

The placeholder was wrong in a way worth spelling out, because it sent at least
one investigation in the wrong direction:

1. `MediaImage.tsx` and `Lightbox.tsx` rendered that text for **any** `<img>`
   `onError`. Neither branch consulted `media.encrypted`, the declared
   mimetype, or the bytes. Decode failure was simply *assumed* to mean
   ciphertext — so the message also appeared for plaintext media, where
   decryption cannot be the cause and no key is even involved.
2. That assumption is ADR 0064's, stated outright in
   § "The ciphertext-fallback 200": the proxy returns raw ciphertext with a
   200, so the only place the failure can surface is at decode, and therefore
   the decode-failure branch says "could not decrypt". True as far as it goes —
   the ciphertext case is real and still needs its message — but it was the
   only cause considered, and it is not the only cause.
3. Nothing in the transport is broken. `is_inline_safe`
   (`crates/axon-api/src/routes/media.rs`) admits every non-SVG `image/*`, so
   `image/heic` is served with its declared type and the bytes arrive intact.
   The fetch succeeds, `state.status` is `ready`, and the `<img>` then fails to
   decode.
4. Browser support is the whole of it: WebKit decodes HEIC in `<img>`,
   Chromium and Gecko do not. So the same room reads correctly on an iPhone and
   shows placeholders on desktop Chrome — which is also why this is easy to
   misfile as a decryption or key-sharing problem.
5. The thumbnail path does not save it, in either direction.
   `useThumbnailFallback` asks the homeserver for a thumbnail only for
   plaintext media (`resolve_thumbnail_spec` rejects encrypted objects), and
   Synapse cannot thumbnail HEIF without `pillow-heif`, which is not a default.
   So the server thumbnail 4xx's, the hook correctly falls back to the
   full-size original — and the original is still HEIC. For encrypted media
   there was never a server thumbnail to try.

The TUI already gets this right and is the model for §1. `decode_image`
(`clients/tui/src/app/media.rs`) blames encryption **only** when magic-byte
sniffing comes back unknown, and `sniff_format` in the same file has an
explicit `HEIC` arm alongside `AVIF` and `HEIF`. The web client never received
that logic; `parse-media.ts` is a documented port of the TUI's media parsing,
but the *failure* path was never ported with it.

HEIC has been the iPhone camera default since iOS 11, and Matrix clients upload
camera originals untouched. For anyone with iOS correspondents this is not an
edge case; it is most of their photos.

## Decision

Scoped to what the placeholder says and offers. The rendering question is left
open below.

### 1. The placeholder names what it knows, and no more

`imageDecodeFailureMessage` (`src/media/image-format.ts`) resolves a decode
failure to one of three messages, mirroring the TUI's condition rather than
inventing a new one:

- **A format we can name** — "HEIC image — this browser can't display it".
- **Encrypted media whose format we cannot name** — "Encrypted media — server
  could not decrypt". ADR 0064's case, now correctly narrowed to it.
- **Anything else** — "Could not display this image".

The wording can be definite ("this browser can't") because these helpers only
ever run after a real decode failure: if we are rendering the message, this
browser did try and could not. The same table therefore lists formats that are
not universally unsupported — WebKit reads HEIC and TIFF happily — because the
question is never "is this format supported in general" but "we just failed on
this, is it worth naming".

Third place is not a cop-out. Unidentifiable bytes on a plaintext event are a
case we genuinely have no explanation for, and the previous message was the one
answer that was certainly wrong.

### 2. A file we cannot draw is still a file — but only if it is a file

A named format is a real file that Preview, Photos, or any image tool will
open: the failure is ours, not the file's, so those bytes must not become a
dead end. Bytes we cannot identify are most likely the ciphertext-fallback
200, and writing those to `photo.jpg` is worse than offering nothing — the
reader gets a file that no tool can open and no indication why.

So **saving is offered exactly when §1 could name the format**, and that one
rule governs both surfaces:

- The inline placeholder gains a **Download** button, reusing `downloadMedia`
  and its share-sheet-then-anchor path, shown when
  `unrenderableImageFormat(media) !== null`.
- The pageable viewer's existing Save control was gated on `decoded` and
  therefore *withdrew itself* exactly when it became the only useful action on
  screen. `LightboxImage` now reports a typed outcome rather than a boolean, so
  the viewer keeps Save for `unsupported-format` and withholds it for
  `undecodable`.

The distinction is the one ADR 0064 was reaching for with a signal too coarse
to express it. Stating it as a single rule over both surfaces is deliberate:
the first draft of this change applied it only in the viewer and left the
inline placeholder offering Download for anything with an `mxc://` URI, which
put the ciphertext hazard on the *more* commonly hit surface — the one that
needs no extra click. Caught in review of the implementing PR (#328).

### 3. Two tiers of identification, and explicitly not byte-sniffing

Declared `info.mimetype` first, then the filename extension, against closed
tables. This follows ADR 0072 § "`info.mimetype` cannot be relied on", which
found real phone media arriving as `application/octet-stream` with the filename
as the only signal. A *specific* image declaration we do not list returns no
name rather than falling through to the extension, so `photo.heic.jpg` — a
transcoded file whose old extension survived mid-name — is not mislabelled.

Magic-byte sniffing, which is what the TUI does and is strictly more reliable,
is deliberately **not** done here. The caller holds an object URL, not a
buffer; getting the bytes means either a second proxy download or plumbing the
`Blob` out through `MediaService`'s refcounted cache, and `fetch()` on a
`blob:` URL is unavailable under jsdom besides. Since any real decoding work
needs those bytes anyway, sniffing belongs with it rather than in a stopgap.
The cost of deferring it is bounded and known: a HEIC that declares no type and
has no extension gets the generic message instead of a named one.

## Open question — should the web client decode HEIC itself?

**Undecided.** The stopgap above stops the misinformation; it does not make the
photo appear on Chrome or Firefox, which is the actual complaint. What follows
is the shape of the problem, not a plan, and the reason it is not a plan is that
the blocking considerations are legal and packaging ones rather than technical.

### The technical shape is the easy part

Roughly: keep native decode first, so `onError` *is* the capability probe and no
WebKit user ever fetches a decoder; on failure, lazily import a wasm decoder,
decode in a worker (`OffscreenCanvas` → `convertToBlob`, following the
`pdfjs-dist` worker precedent in `PdfViewer.tsx`) under a pixel ceiling
mirroring the TUI's `MAX_DECODED_PIXELS`, since a 12 MP HEIC is a ~48 MB RGBA
buffer and a hostile one declaring 60000×60000 must fail as an unrenderable
image rather than as an OOM; apply the container's `irot`/`imir` transforms,
because a HEIC's rotation lives there rather than in EXIF and dropping it turns
every portrait photo sideways; and re-admit the transcoded object to the
existing media cache under its own key so paging a run of HEICs does not
re-decode each time. None of that is controversial.

### The license question is the blocker

Every practical HEIF decoder for the browser is libheif compiled to wasm
(`heic-to`, `libheif-js`, `heic2any`). libheif and its HEVC decoder libde265 are
**LGPL-3.0**. This repo is Apache-2.0, and every current dependency — 546 crates
and the whole web tree — is permissive or MPL-2.0. This would be the first
copyleft component we ship.

It is compatible, but not free. Attribution is already handled: the
`axon-thirdparty-licenses` Vite plugin generates the Licenses page from the pnpm
production tree, so an entry would appear in the UI automatically. The
obligation needing a deliberate choice is LGPL §4's relinking requirement,
which a bundler defeats by construction — satisfying it means shipping the
decoder's JS and wasm as standalone, unmodified, replaceable assets rather than
minified into an application chunk.

And there is a trap in that mitigation: vendoring the wasm *outside* the pnpm
production tree to keep it replaceable would silently drop its disclosure entry,
satisfying the relinking obligation by breaking the attribution one, with no
build failure to notice. Any implementation would need the disclosure asserted
by a test rather than left to the generator.

### What it would commit the Tauri shell (M-W12) to

**The decoder would be mandatory on both shipping targets.** "No WebKit user
pays for it" is a browser-only saving. ADR 0031 fixes the Tauri webviews as Edge
WebView2 on Windows and WebKitGTK on Linux. The former is Chromium and does not
decode HEIC. The latter, despite being WebKit, does not inherit WKWebView's HEIC
support either — on Apple platforms that decode comes from the OS Image I/O
framework, which WebKitGTK does not have. The one webview that would decode
HEIC for free is WKWebView, and ADR 0046 puts macOS and mobile Tauri targets
explicitly out of scope. So every desktop user would carry the decoder. Both
webview claims should be checked against a real build before anyone relies on
them; neither is something this repo has tested.

**Asset embedding would defeat the license mitigation.** ADR 0031 sells the
shell at a "~5–10 MB installer with no bundled Chromium" and ADR 0046 at
"Desktop builds ship from the same dist". Tauri serves that dist from a custom
scheme — already flagged as an open question in ADR 0085's service-worker
discussion — with the assets compiled into the executable. That turns
"standalone, replaceable asset" into something much closer to static linkage
into a distributed binary. A Tauri build would have to ship the decoder as a
**resource** resolved from the filesystem at runtime, which is a constraint on
the shell's asset pipeline and much cheaper to know before the shell exists than
after.

**Distribution channel.** 1–2 MB against a 5–10 MB installer is real but
tolerable, and it is the smaller point. The larger one is that LGPL and the
Apple app stores are a documented conflict — VLC's removal being the canonical
case — because store terms restrict the modification and redistribution LGPL §4
requires. Nothing is blocked today: ADR 0031 targets direct-download installers
for Windows and Linux and no store channel is planned for any target. But a
decoder in the shared bundle would hand a future macOS or iOS target a license
problem on precisely the two platforms whose webviews decode HEIC natively, so a
per-platform exclusion would need to exist from the outset rather than be
retrofitted at submission time.

### Three possible homes, not one

The browser is only one candidate, and choosing the wasm route would be choosing
it for all three:

- **The browser bundle** (the shape sketched above). One code path for every
  deployment; worst license posture, since a wasm blob inside an application
  bundle is the case LGPL §4 handles least comfortably.
- **The media proxy** (`axon-media`, a server silo). Fixes every client at once
  — the TUI included, which today has the same limitation and no decoder either
  — and keeps clients ignorant of the format. Costs libheif as a C dependency in
  the deployed image and the Docker build, and means the proxy
  decrypts-then-transcodes for encrypted media.
- **The Tauri Rust shell.** Best license posture of the three: linking libheif
  as a genuine shared library — the distribution's own on Linux, a shipped DLL
  on Windows — is the arrangement the LGPL was written for. But it forks the
  media path (wasm in browsers, IPC under Tauri), puts a C toolchain into the
  desktop CI lane, and spends exactly the "near-zero marginal cost" and "same
  dist" properties that justify the Tauri approach in ADR 0031.

### What would make this decidable

Three things, in this order. First, whether the team will accept a copyleft
component at all, and in a shipped binary specifically — that is a policy
answer, not a technical one, and everything else is moot without it. Second,
whether a permissively licensed HEIF decoder is viable; the dependency-free
alternative today is WebCodecs `VideoDecoder` with codec `hvc1`, borrowing the
platform's own HEVC decoder, which Chromium exposes where the OS supports it —
set aside for now because Gecko does not offer HEVC there so it would not fix
Firefox, and because it obliges us to hand-parse ISOBMFF item boxes
(`iprp`/`hvcC` for parameter sets, `iloc` for extents, plus `grid`-derived
images, which iOS does emit) over attacker-supplied bytes. Third, how often this
actually bites now that the placeholder names the format — the stopgap makes
that observable for the first time, and if HEIC turns out to be rare among real
correspondents, "download and open it locally" may be the right permanent
answer.

## Consequences

Of the stopgap, which is what shipped:

- "Could not decrypt" becomes rare, and therefore worth believing when it
  appears. That is most of the value here, independent of any decoding.
- A HEIC still does not render on Chrome or Firefox. The reader is now told why,
  and given the file.
- The Save control in the pageable viewer changes behaviour: previously
  withdrawn on any decode failure, now withdrawn only for bytes matching no
  known format. `LightboxImage`'s `onDecoded` boolean became an `onOutcome`
  callback over a small union to carry that distinction, and it takes `media`
  instead of `mxcUrl` + `alt`, which both call sites were deriving identically
  anyway.
- Identification is only as good as what the sender declared (§3). A HEIC with
  neither a declared type nor a `.heic` name falls to the generic message.
- `docs/client-parity.md` gains a HEIC row. The TUI is a genuine gap for the
  same underlying reason — the `image` crate has no HEIF decoder — and its error
  message is already honest, so the row records a shared limitation rather than
  a web-specific one.

## Alternatives rejected

For the stopgap:

- **Fix the message and stop there, with no download.** Leaves the reader
  correctly informed and still stuck; naming a format is only actionable if they
  can get at the file.
- **Sniff the bytes now.** Strictly better identification, rejected as
  disproportionate for a stopgap — see §3.
- **Ask the homeserver for a thumbnail harder.** Cannot work: it fails for
  encrypted media by construction, and for plaintext media it depends on a
  Synapse plugin we neither control nor can require of anyone's homeserver.
- **Transcode every image through a decoder, sidestepping identification
  entirely.** Removes the format question, at the cost of a wasm download and a
  decode pass for every JPEG the browser was about to handle natively and faster
  — and it presumes the decoder decision this ADR leaves open.
