# ADR 0139 — Stream large video and audio media

**Status:** Proposed. This ADR is the plan only; implementation waits for its review.

## Context

Large videos often fail to play in the web client and in the desktop and mobile shells.
When they do play, they start only after the whole file has arrived.
The file is fully buffered in three places on its way to the `<video>` element, and the limits on that buffering turn "slow to start" into "never starts".

### 1. The server buffers the whole object before it sends a byte

`SdkMediaProxy::download` (`crates/axon-sync/src/media.rs`) calls matrix-sdk's `Media::get_media_content`, which returns a `Vec<u8>`.
For an encrypted attachment it runs `AttachmentDecryptor` with `read_to_end` over the whole ciphertext.
`MediaCache` then writes those bytes to disk, and only then does `GET /v1/media/…` answer.
ADR 0045 recorded this as an SDK limitation and bounded it with two settings, both at their defaults on the production instances:

- `max_object_bytes` (100 MiB). A larger object is refused with `too_large`, so it never plays.
  Production Synapse accepts uploads up to 1 GiB (`max_upload_size: "1G"`), and federated media has no Axon-side bound at all.
- `fetch_timeout_secs` (60 s). It limits the whole upstream download, not a stall.
  A large file over a slow homeserver link times out even while it is arriving steadily.

The route itself already supports single-range `Range` requests, `ETag` and `If-None-Match` (ADR 0045).
That support only helps once the object is complete on disk.

### 2. The web client buffers the whole object before it sets `src`

A `<video>` element cannot send an `Authorization` header, and every `/v1` route requires a bearer token.
So `MediaPreview` loads a video the way ADR 0064 loads an image: `useMediaBlob` `fetch()`es the whole object into a `Blob` (`media-service.ts`) and hands the element a `blob:` URL.
That fetch holds one of six media slots (`MAX_CONCURRENT`) and is abandoned after 120 s (`MEDIA_TRANSFER_TIMEOUT_MS`), however steadily it is arriving.
The download starts when the user opens the preview, and the player stays on a skeleton until the last byte arrives.

### 3. The shells buffer it again, and cannot load the server URL at all

The Tauri shells (ADR 0102) send that same `fetch()` through the `axon-http` plugin (ADR 0127), with its own 120 s `REQUEST_TIMEOUT_MS`.
Pointing a `<video>` straight at the server would not work there even with credentials sorted out:

- The shell CSP (`tauri.conf.json`) is `media-src 'self' asset: http://asset.localhost blob:`.
  A self-hosted server's origin is not known at build time, so it cannot be listed.
- The page loads from a custom scheme, so a plain-http LAN server is mixed content.

### What already exists

- **Range serving and a single shared download per object.** `MediaCache` already makes concurrent requests for one object share one download.
- **What the SDK exposes.** matrix-sdk 0.19.1 exposes `Client::http_client()`, `Client::homeserver()`, `Client::access_token()` and `Client::refresh_access_token()`.
  So Axon can make the authenticated download request itself and read the body as a stream, without patching the SDK.
- **How encrypted attachments are built.** They use AES-256-CTR with a SHA-256 over the ciphertext (the `EncryptedFile` descriptor).
  CTR keeps the plaintext exactly as long as the ciphertext, so the upstream `Content-Length` is the plaintext length.
  It can also be decrypted chunk by chunk.
  The hash can only be checked after the last byte.
- **The crypto crates.** `aes`, `ctr` and `sha2` are already in `Cargo.lock` through matrix-sdk-crypto.

## Decision

### 1. Four phases in four PRs, and a deferred fifth

| Phase | What                                                        | Silo                                                  | PR    |
| ----- | ----------------------------------------------------------- | ----------------------------------------------------- | ----- |
| 1     | Streaming upstream download, serving a partial file         | server (`crates/`)                                    | one   |
| 2     | Stream tickets: a URL a `<video>` can load without a header | server (`crates/`)                                    | one   |
| 3     | The web client plays video and audio from a ticket URL      | web (`clients/web/src`)                               | one   |
| 4     | The shells proxy ticket URLs through a custom scheme        | web (`clients/web/src-tauri`, plus the platform glue) | one   |
| 5     | Deferred: pass seek ranges through to the homeserver        | server                                                | later |

Phase 1 removes the server's own 100 MiB and 60 s limits for video and audio, and the first byte leaves Axon as soon as the first chunk arrives.
It does not lift any client's limits.
The TUI's `get_media` (`clients/tui/src/api.rs`) still buffers the whole body, refuses anything over 20 MiB (`MAX_MEDIA_BYTES`) and gives up after 60 s (`MEDIA_TIMEOUT`).
Lifting those is a separate TUI change, outside this ADR.
Phases 2 to 4 let the player start before the download finishes and seek within what has arrived.
Until phase 4 lands, the shells keep the blob path, which phase 1 already makes faster and no longer limited to 100 MiB.

### 2. Phase 1: download from the homeserver as a stream

`MediaFetcher` (the SDK-free port `axon-media` owns, ADR 0045) gains a streaming method beside the buffered one:

```rust
async fn download_stream(&self, account_id: Uuid, mxc_url: &str, encrypted_file: Option<Value>)
    -> Result<MediaStream, FetchError>;

struct MediaStream {
    /// From the upstream `Content-Length`; `None` for a chunked response.
    len: Option<u64>,
    body: BoxStream<'static, Result<Bytes, FetchError>>,
}
```

`SdkMediaProxy` implements it with the SDK client's own `reqwest::Client`, homeserver URL and access token.
It sends a GET to the authenticated media endpoint (`/_matrix/client/v1/media/download/{server}/{id}`), falling back to the legacy `/_matrix/media/v3/download` path only when the server lacks authenticated media, which is the same choice the SDK makes.
A `401` with `M_UNKNOWN_TOKEN` gets one `refresh_access_token()` and one retry, so MAS-issued tokens (ADR 0097) keep working.
The method returns once the response headers arrive.

That client comes with a whole-request timeout this request must not inherit.
Axon sets no `RequestConfig`, so matrix-sdk's default 30 s `timeout` is copied onto the `reqwest::Client` that `Client::http_client()` returns, and reqwest applies a client timeout to the whole body, not just the headers.
The SDK's own media download avoids it by setting the per-request timeout to `Duration::MAX` (`matrix-sdk` `media.rs`, "Downloading a file should have no timeout").
The stream request does the same, so the shared client's 30 s never cuts a body off.
The deadlines that do apply are the ones in section 3: `fetch_timeout_secs` until the response headers, then `stall_timeout_secs` on each body read.

Encrypted media is decrypted as it streams.
Malformed descriptors keep today's errors:

- The descriptor is deserialized into ruma's `EncryptedFile` before any network work, as today.
  ruma rejects a wrong-length key or IV there, because `k` and `iv` are fixed-size arrays, along with anything else that does not parse.
  Those failures stay `GatewayError::Invalid`, which becomes `400 bad_request`, exactly as `SdkMediaProxy::download` maps them now.
- The two checks `AttachmentDecryptor::new` makes, a `v2` version and a SHA-256 hash present, are made next, also before any network work.
  Failing either is `Undecryptable` (`422 media_undecryptable`), as it is today.
- Each chunk updates a SHA-256 over the ciphertext, then goes through `Ctr128BE<Aes256>` and on to the cache.
- After the last chunk, a hash mismatch makes the object `Undecryptable` (section 4).

`get_media` uses the streaming method for every object whose served type is `video/*` or `audio/*`.
Images, files and thumbnails keep the buffered path and its check-then-serve guarantee (section 4 says why).

### 3. Phase 1: serve from a file that is still being written

The shared download in `MediaCache` gains a progress channel (`tokio::sync::watch`) that reports bytes written to the staging file, the length once known, and the final outcome.
The download runs as its own task, detached from the request that started it.

- A request whose range starts inside what has been written is answered at once with `206`, `Content-Range: bytes start-end/len` and the full range length.
  Its body reads the staging file and waits on the progress channel whenever it catches up, so it streams at download speed.
- A request for bytes past the written point waits on the same channel.
  So a seek forward waits for the download to reach that point, and phase 5 is about removing that wait.
- With no upstream `Content-Length`, the length is unknown until the end.
  Such a response is a plain `200` with a chunked body and no `Accept-Ranges`, until the download completes.
  Encrypted media declares `info.size`, but that value is sender-controlled, so it is a hint only and never written into a `Content-Range`.
- When the download completes and, for encrypted media, the hash matches, the staging file is promoted into the LRU exactly as today.
  Later requests are ordinary cache hits.
- The download keeps going while any request has been attached within the last 60 s, so a player that closes and reopens connections while seeking does not restart it.
  After 60 s with no readers it is cancelled and the staging file is deleted.

The limits change with it:

- `fetch_timeout_secs` keeps its name and becomes the deadline for the upstream response headers on this path, enforced with `tokio::time::timeout` around the send.
  Its documentation changes to say so.
- A new `stall_timeout_secs` (default 30) cancels a download that delivers no bytes for that long, enforced with a `tokio::time::timeout` around each body read.
  There is no total-duration limit on a streamed download, which is why section 2 clears the inherited reqwest timeout rather than leaving it in force.
- The download task holds a `max_concurrent_downloads` permit (the `downloads` semaphore `fetch_capped` takes today, default 16) for as long as the upstream request is open.
  Over the limit, downloads queue as they do today.
  Size and stall time are bounded per download, so the permit is what bounds the number of open homeserver connections.
  A streamed video holds its permit for minutes rather than seconds, which leaves fewer permits for images while several videos download.
  Videos download only when the user opens one, so the default is kept, and the permit wait gets a `debug` log so contention shows up if it happens.
- A new `max_stream_bytes` (default 2 GiB) replaces `max_object_bytes` for the streaming path only.
  `max_object_bytes` was protecting process memory, and a streamed object no longer passes through memory.
  An object over `max_stream_bytes` is refused with `too_large` before the body is read, when `Content-Length` says so, or when the running total passes it.
- Each in-flight streamed download counts its bytes against the LRU's `max_bytes` total cap as they are written, and the cache evicts to make room.
  A download that cannot fit even after evicting everything else is refused with `too_large`.

### 4. Phase 1: unverified plaintext reaches the player

This is the one real trade-off in the plan.
Today no byte of an encrypted attachment leaves Axon until its SHA-256 has been checked.
A streamed video's bytes go out as they are decrypted, and the check happens only after the last one.

What the check protects against is a homeserver, or anyone between it and Axon, altering the ciphertext.
CTR mode has no integrity of its own, so a flipped ciphertext bit flips the same plaintext bit.
Such an attacker cannot read the plaintext or choose it without knowing it, but can corrupt known positions.
For a video or audio stream, the consequence is a damaged frame or an audio glitch in a decoder that already handles damaged input.
That is a much smaller risk than an image or document decoder handed altered bytes, which is why section 2 restricts streaming to `video/*` and `audio/*`.

When the hash fails:

- Every response still reading the object is cut off mid-body, so the player sees a network error rather than a clean end.
- The staging file is deleted and the object is never cached.
- The object's key is held as `undecryptable` for 10 minutes, so the client's retry gets an immediate `422 media_undecryptable` instead of downloading 1 GiB again to fail the same way.
- The failure is logged at `warn` with the account, the MXC URI and the byte count.

### 5. Phase 2: stream tickets

A `<video>` cannot carry a bearer header, so the server issues a URL that stands in for one, valid for one object only.

- `POST /v1/media/{account_id}/{server_name}/{media_id}/stream-ticket` (bearer-authenticated) checks the object exactly as `get_media` does: the account, the owning event, and the served type.
  It refuses anything whose served type is not `video/*` or `audio/*` with `400`.
  It returns `{ "data": { "path": "/v1/media-stream/<ticket>", "expires_at": … } }`.
  The path is relative, so the client resolves it against its own server base.
- `GET /v1/media-stream/{ticket}` is mounted outside `require_bearer`, beside the OAuth callback router.
  It serves the object exactly as `get_media` does, ranges and all, and also sends `Cache-Control: private, no-store` and `Referrer-Policy: no-referrer`.
- A ticket is 32 random bytes, base64url-encoded, held in an in-memory table keyed by the ticket.
  The table records the account, the MXC URI, the event id, the issuing token's id and session id, and the times the ticket was created and last used.
  Restarting the server drops every ticket, and the client simply asks for a new one (section 6).
- A ticket expires 30 minutes after its last use and never more than 12 hours after it was issued.
- Every use also re-checks the issuing token.
  A new read-only store lookup by token id rejects the ticket unless that token is unrevoked and unexpired.
  So a ticket is accepted exactly when the bearer token that issued it would be.
  - This covers every way a credential ends, with no callback into the ticket table: `axon token revoke` and the management API's revoke both go through `Store::revoke_token`'s `remove_token`.
    Identity unbind, from the CLI or the management API, revokes that identity's tokens inside the store.
    An OAuth session's access tokens simply expire.
  - A non-expiring operator token (`expires_at` is `None`) imposes no expiry of its own, so its tickets rely on that per-use check and the 30-minute and 12-hour limits.
  - An OAuth access token lives an hour (`access_token_ttl_secs`), so its tickets stop working within the hour even while the client keeps refreshing.
    The client then requests a new ticket and resumes, as section 6 describes.
  - The lookup is one primary-key read per range request.
    That is the same cost every bearer request already pays.
- The table holds at most 4096 tickets and drops the least recently used past that.
- Tickets are never logged.
  The app's outermost `TraceLayer` (`crates/axon-api/src/lib.rs`) records `path = %request.uri().path()` on an `INFO` span for every request, including routes outside `require_bearer`.
  So its `make_span_with` changes to record `/v1/media-stream/{ticket}` in place of any path under that prefix.
  A handler span of its own would not stop that outer one recording the raw path.
  Ticket-route errors do not carry the ticket in their `Display` text or their error envelope.

The alternatives to tickets are covered under Alternatives considered.

The ticket route is additive, so it needs no `API-Breaking-Change` trailer (ADR 0099).
The OpenAPI spec documents both routes.

### 6. Phase 3: the web client plays from the ticket URL

For `video` and `audio` previews, `BlobPreview` in `MediaPreview.tsx` stops using `useMediaBlob`:

- On mount it requests a ticket.
  That is a small JSON call through the normal API client and does not take a media slot.
  Then it renders `<video src={ticketUrl} preload="metadata">`, or the `<audio>` equivalent.
  The browser fetches the media itself, with its own range requests and buffering, and none of the media service's slots or deadlines apply.
- The poster keeps the blob path, because it is a small image.
- A media element reports a `403` and an unsupported codec the same way (`MEDIA_ERR_SRC_NOT_SUPPORTED`).
  So on `error` the component probes the ticket URL with `fetch` and `Range: bytes=0-0`:
  - A `2xx` probe means the bytes are fine, and it shows today's "This browser cannot play …" message.
  - A `401`, `403` or `404` from the ticket route means the ticket expired or the server restarted.
    It requests a new ticket once, sets the new `src` and restores `currentTime`.
  - A `422` shows the undecryptable message, with no retry.
  - Anything else shows the generic load failure with a Retry button.
- The Download button keeps the blob path in this phase.
  A large download is still buffered there and still subject to the 120 s deadline.
  Moving it onto the ticket route is a separate, deferred change (Consequences).

The ticket URL is the server's own origin when the web client is served by the server (ADR 0116), so no CORS or CSP change is needed there.
It also works cross-origin, as in the Vite dev proxy: a `<video>` without a `crossorigin` attribute needs no CORS headers.

### 7. Phase 4: the shells proxy ticket URLs through a custom scheme

The shell registers an `axon-media` scheme with `register_asynchronous_uri_scheme_protocol`.
In the shells, `platform/tauri.ts` maps a ticket path to a scheme URL with Tauri's `convertFileSrc(ticket, 'axon-media')`.
That call produces `axon-media://localhost/<ticket>` on macOS, iOS and Linux, and `http://axon-media.localhost/<ticket>` on Windows and Android, so the page never spells out the per-platform form.
The handler:

- Forwards the request to `<server base>/v1/media-stream/<ticket>` through the shared `axon-http` client (ADR 0127), passing `Range` and `If-None-Match` through and copying back the status, `Content-Range`, `Content-Length`, `Content-Type`, `Accept-Ranges` and `ETag`.
- Needs no bearer token, because the ticket is the credential.
- Learns the server base from a new `set_media_stream_base` command, which the page calls whenever its server changes.
  Today `axon-http` takes a full URL on every request and the shell keeps no server base of its own.
  The handler refuses to run until the base is set, and only ever requests that base's `/v1/media-stream/` prefix, so the scheme cannot become a general fetch from the page.
- Clamps every range to at most 4 MiB.
  It turns an open-ended `bytes=N-` into `bytes=N-(N+4 MiB-1)` and answers `206` with the true total length.
  Tauri's scheme responder takes a whole body, not a stream, so this clamp is what keeps each response bounded.
  Players handle a short `206` by asking for the next range, and Tauri's own `streaming` example works the same way.
  A request with no `Range` header is answered as `bytes=0-` with the same clamp, and as a `206`.

The CSP gains `axon-media: http://axon-media.localhost` in `media-src`, and in `connect-src` for the section 6 probe.

Before this phase merges, it must be checked on a real iOS device, a real Android device, macOS, Windows and Linux.
iOS (`WKURLSchemeHandler` feeding AVFoundation) and Android (`shouldInterceptRequest`) are the platforms where media through a custom scheme is least predictable.
If one of them cannot play through the scheme, that platform keeps the blob path, which phase 1 has already improved, and the gap is filed as an issue.

### 8. Phase 5, deferred: seeking ahead of the download

Some MP4s, commonly ones recorded on Android, put their index (`moov`) at the end of the file.
The player reads that first, so with a front-to-back download it waits for the whole file before it can start.
Seeking far ahead in a long video waits the same way.

CTR mode can start decrypting at any 16-byte block, with counter = IV + offset / 16, so the server could fetch just the requested range from the homeserver and decrypt it in place.
Those bytes can never be hash-checked, since the hash covers the whole ciphertext, and Synapse's support for `Range` on authenticated media downloads has not been checked.
This phase begins by checking that support against production Synapse, and gets its own ADR section or ADR if it goes ahead.

## Consequences

- In the web client and the shells, video and audio over 100 MiB play, up to `max_stream_bytes`.
  Playback starts once the player has its first few hundred KiB, not once the whole file has arrived.
- For streamed video and audio, an encrypted attachment's integrity check moves from before serving to after.
  A tampered stream is cut off and never cached, but bytes before the cut have already been played (section 4).
- A ticket is a credential in a URL.
  It is limited to one object, a 30-minute idle lifetime and its issuing token's validity.
  Axon's own request log records the route template instead (section 5), but Caddy's access log records the ticket on every range request.
  Operators who ship those logs elsewhere should know that.
- `axon-media` adds a second custom scheme to the shells and widens their CSP by one source.
- The shell's 4 MiB range clamp means each scheme response is buffered in the shell process, up to 4 MiB.
- The Download button is unchanged: a large download is still buffered in the page and limited to 120 s.
  Making it stream, by pointing it at the ticket route in the browser and saving through the shell's own download path in the shells, is a follow-up.
- The TUI is unchanged by this plan.
  It still refuses media over 20 MiB and still gives up on a transfer after 60 s, whatever the server allows.
  Raising those limits, or streaming to disk, is a separate TUI change.

## Alternatives considered

- **A service worker that adds the `Authorization` header.**
  It would let `<video>` load the existing route unchanged.
  It was rejected because the shells' custom-scheme pages cannot reliably register one, iOS Safari's service-worker handling of media range requests is a known source of playback failures, and it puts a long-lived, page-wide request interceptor into the app for one feature.
- **HMAC-signed, stateless tickets.**
  They would survive a server restart.
  But they need a persistent signing secret, which Axon does not otherwise have.
  They cannot be revoked with their token without a deny list, which is state again.
  And a restart already costs the client only one more ticket request.
- **A cookie for media requests.**
  It would make the bearer token, or a stand-in for it, ambient on every request to the server, which reopens the CSRF exposure that bearer-only auth avoids.
  It would also not reach the shells' custom-scheme pages.
- **Media Source Extensions in the page, fed by `fetch`.**
  It would keep bearer auth, but means demuxing MP4 in JavaScript, plus WebM and every other container users send.
  iOS Safari on iPhone has only the restricted `ManagedMediaSource`.
  It would also keep the shells' 120 s request deadline in the path.
- **Raising `max_object_bytes` and `fetch_timeout_secs` only.**
  It would remove the outright failures, but every large video would still buffer fully in server memory and in the page before playing, and still be bounded by the clients' 120 s deadline.
