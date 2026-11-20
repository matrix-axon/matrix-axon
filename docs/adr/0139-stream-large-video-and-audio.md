# ADR 0139 — Stream large video and audio media

**Status:** Accepted; phase 1 (section 3) is implemented in [PR 1157](https://github.com/matrix-axon/matrix-axon/pull/1157), phase 2 (section 5) in [PR 1174](https://github.com/matrix-axon/matrix-axon/pull/1174), phase 3 (section 6) in [PR 1190](https://github.com/matrix-axon/matrix-axon/pull/1190), phase 4 (section 7) is in progress (the macOS, iOS, Windows and Android shells), and phase 5 is not yet started. Part of [issue 22](https://github.com/matrix-axon/matrix-axon/issues/22).

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
A `429` or `5xx` answer is sent again up to 3 more times, as the SDK retries those for its buffered download.
Each retry waits for the homeserver's `retry_after_ms` or `Retry-After` when given, and otherwise for 500 ms, doubling each time, at most 10 s.
The whole sequence stays within the `fetch_timeout_secs` deadline of section 3.
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
  A request without `Range` then gets a plain `200` with a chunked body and no `Accept-Ranges`.
  A request with `Range` waits until the download completes and the length is known, then is answered like any other range.
  That wait is bounded at 10 s, a constant: long enough for a small object to finish, and short enough that no player, WebView or proxy times out first.
  Past 10 s, the request is answered with a `200` and the whole body as it streams, ignoring the range, which RFC 9110 allows.
  A browser `<video>` plays that, without seeking, and the section 6 probe reads only its status.
  The shell treats it as a `200` for an ignored range (section 7), so in the shells such an object fails to play until it has fully arrived, then plays on Retry.
  This is the degraded case: such an object plays only after it has fully arrived, as everything does today.
  It is also why every `206`, including those the shell returns in section 7, carries a known total length.
  It is expected to be rare, and the phase 1 PR records whether production Synapse ever omits `Content-Length` on a media download.
  Encrypted media declares `info.size`, but that value is sender-controlled, so it is a hint only and never written into a `Content-Range`.
- The download runs to completion once started, whether or not anyone is still reading.
  A player that has buffered ahead may close its connection, and a user may pause for minutes.
  Cancelling a reader-less download would delete the staging file and turn the next read at a large offset into a download from byte 0, the very re-download this ADR removes.
  The only things that stop a download early are the stall timeout, the size limits and the reservation rule below, the length check, a hash mismatch and server shutdown.
  A video opened and abandoned at once still downloads in full.
  That waste is bounded by the separate stream permits and the reservation rule below, and the object is then in the cache for the next play.
- When the download completes, the length check passes and, for encrypted media, the hash matches, the staging file is promoted into the LRU exactly as today.
  Later requests are ordinary cache hits.

The upstream body has to be exactly as long as promised, because unencrypted media has no hash to catch a truncated file:

- The request sends `Accept-Encoding: identity`.
  matrix-sdk turns on reqwest's `gzip` feature, so a `gzip` response is decoded transparently anyway.
  reqwest then drops the `Content-Encoding` and `Content-Length` headers, and the download goes ahead as one of unknown length.
  A response with any other `Content-Encoding` fails the download as `Upstream`.
  reqwest cannot decode those encodings, so the body would otherwise be cached still compressed, with a `Content-Length` that is not the media's length.
- A body that ends before its `Content-Length`, or runs past it, fails the download.
  The staging file is never promoted, and every response still reading the object is aborted, exactly as for a hash mismatch in section 4.
  A chunked body counts as complete only when hyper reports a clean end of stream, so a connection dropped mid-chunk is also a failure.

The length check and the hash check both give their verdict only after the last byte has arrived.
So a reader that has caught up with the download must not be handed a clean end of body before then:

- Until the verdict, no response sends the object's last 64 KiB.
  While the length is unknown, it holds back the most recent 64 KiB written instead, since any chunk could be the last.
- A response whose range ends before that tail completes normally.
  Those bytes are already covered by the section 4 trade-off.
- A response that reaches the tail waits there for the verdict.
  The verdict follows the last chunk at once, so a reader that has caught up waits only for one hash finalisation.
- A ranged request that starts in the tail, such as `bytes=-N` or `bytes=<len-N>-` for an MP4 index (`moov`) at the end of the file, therefore waits for the whole download.
  It would wait for most of it anyway, since those bytes arrive last (the "past the written point" case above).
  For a known-length object only the stall timeout bounds that wait, and in the shells the 120 s `axon-http` deadline turns a longer one into a `502`.
  The player's Retry then succeeds once the download has finished and the object is a cache hit.
  So phases 1 to 4 do not make a moov-at-end video start early: it starts when its download completes, as today, but without today's size and time limits.
  Phase 5 is what would change that (section 8).
- On a pass, the tail and the end of the body go out.
- On a failure, the response is aborted, not finished.
  Its body stream yields an error, so hyper closes an HTTP/1.1 connection short of its `Content-Length`, or omits a chunked body's final chunk, and resets an HTTP/2 stream.
  The player sees a network error.

The guarantee is that an object failing either check never reaches a player whole, with a clean end.
It is not that none of it plays: ranges that end before the tail, which includes most of the shell's 4 MiB responses, may already have been played.

The limits change with it:

- `fetch_timeout_secs` keeps its name and becomes the deadline for the upstream response headers on this path, enforced with `tokio::time::timeout` around the send.
  Its documentation changes to say so.
- A new `stall_timeout_secs` (default 30) cancels a download that delivers no bytes for that long, enforced with a `tokio::time::timeout` around each body read.
  There is no total-duration limit on a streamed download, which is why section 2 clears the inherited reqwest timeout rather than leaving it in force.
- Streamed downloads take a permit from their own semaphore, sized by a new `max_concurrent_streams` (default 4), for as long as the upstream request is open.
  They do not use the `downloads` semaphore that `fetch_capped` takes today (`max_concurrent_downloads`, default 16).
  A streamed video holds its permit for minutes rather than seconds.
  Sharing the 16 permits would let a few open videos queue every image, avatar and thumbnail behind them.
  With their own semaphore, buffered media never waits on a stream.
  Together the two semaphores bound the number of open homeserver connections, since size and stall time are already bounded per download.
  A stream's wait for its permit and its wait for the upstream response headers share one deadline of `fetch_timeout_secs`, measured from when the request arrives.
  Past it the stream fails with `503`, which the client treats as retryable.
  So a request under load waits at most 60 s at the defaults before that answer, not 60 s for a permit and another 60 s for the headers.
  Today's wait for a `downloads` permit has no bound, and that is unchanged.
- A new `max_stream_bytes` (default 2 GiB) replaces `max_object_bytes` for the streaming path only.
  `max_object_bytes` was protecting process memory, and a streamed object no longer passes through memory.
  An object over `max_stream_bytes` is refused with `too_large` before the body is read, when `Content-Length` says so, or when the running total passes it.
  An object the limit allows must be able to fit beside the room kept for buffered media (below), so the limit is bounded by `max_bytes` minus `max_object_bytes`.
  When `max_stream_bytes` is unset, its 2 GiB default is lowered to that bound, so a deployment with a smaller cache keeps booting after the upgrade.
  If that leaves the limit below `max_object_bytes`, streaming would refuse objects the buffered path serves, so video and audio stay on the buffered path and startup logs a warning.
  An explicit `max_stream_bytes` above the bound is rejected at startup.
- A streamed download reserves its space in the LRU before it reads the body.
  The order is fixed: the stream permit first, then the upstream request until its headers arrive, then the reservation.
  The reservation needs the `Content-Length` from those headers, so it cannot come first.
  - With a known `Content-Length`, it reserves the full length at once.
    The cache evicts completed entries, least recently used first, to make room, which is the LRU's ordinary behaviour.
    Reservations of downloads still in flight are never evicted.
  - The reservation is tried once and never waited for.
    When completed entries cannot be evicted far enough because in-flight reservations hold the rest, the download releases its permit, closes the upstream connection and fails with `503`.
    So a permit is never held while waiting for room, and a download waiting for room cannot sit on a permit that another stream needs.
    It is not refused as `too_large`: the object is within `max_stream_bytes`, and the client should retry rather than report it as too big.
  - With no `Content-Length`, the reservation grows as bytes arrive.
    If it cannot grow, the download fails with `503`, and the partial file is deleted.
  - In-flight stream reservations together may hold at most `max_bytes` minus `max_object_bytes`.
    So there is always room for one buffered object, and images, avatars and thumbnails never wait behind a stream's reservation.
    A buffered object that still cannot fit beside the reservations is served from memory and not cached, rather than waiting.
  - At the defaults, the 4 stream permits times 2 GiB is 8 GiB, more than the 4.9 GiB that reservations may hold.
    So reservations, not arithmetic between the settings, are what keep the cache inside its cap.
    A third concurrent 2 GiB video therefore gets an immediate, retryable `503`.
    Several multi-GiB videos at once are unusual on a personal server.

### 4. Phase 1: unverified plaintext reaches the player

This is the one real trade-off in the plan.
Today no byte of an encrypted attachment leaves Axon until its SHA-256 has been checked.
A streamed video's bytes go out as they are decrypted, and the check happens only after the last one.

What the check protects against is a homeserver, or anyone between it and Axon, altering the ciphertext.
CTR mode has no integrity of its own, so a flipped ciphertext bit flips the same plaintext bit.
Such an attacker cannot read the plaintext or choose it without knowing it, but can corrupt known positions.
For a video or audio stream, the consequence is a damaged frame or an audio glitch in a decoder that already handles damaged input.
That is a much smaller risk than an image or document decoder handed altered bytes, which is why section 2 restricts streaming to `video/*` and `audio/*`.

That restriction rests on the event's `info.mimetype`, which the sender chooses, so any blob can be labelled `video/mp4` and opted into verify-after-serve.
This ADR does not add a content sniff, because the label does not widen the risk:

- **The sender** controls the plaintext anyway and can attach any bytes with a valid hash, so skipping the hash check gives a sender nothing it lacked.
- **A homeserver** is the party the hash check guards against, and it cannot relabel an encrypted attachment.
  `info.mimetype` sits inside the encrypted event content, which it cannot read or alter.
  For unencrypted media there is no hash to check, before or after this ADR.
- **Where the bytes go.** Whatever the bytes really are, they reach only a `<video>` or `<audio>` element.
  They are served as the declared `video/*` or `audio/*` type with `X-Content-Type-Options: nosniff`, and the ticket route refuses any other type.
  The residual exposure is the browser's media decoder parsing a mislabelled or bit-flipped file, which it must already survive from any untrusted video.

When the hash fails:

- Every response still reading the object is aborted at the withheld tail (section 3), so the player sees a network error rather than a clean end.
- The staging file is deleted and the object is never cached.
- The object is held as `undecryptable` for 10 minutes, so the client's retry gets an immediate `422 media_undecryptable` instead of downloading 1 GiB again to fail the same way.
  - The negative entry is keyed on the object's representation key, the same key the cache and its single-flight use.
    Since [ADR 0138](0138-matrix-link-previews.md) section 4, that key binds a digest of the whole encrypted-file descriptor as well as the account and the MXC URI.
    A request whose descriptor differs therefore misses the entry and downloads again.
    So an event that reuses another event's MXC URI with a bogus descriptor cannot mark the URI undecryptable for the event whose descriptor is correct.
- The failure is logged at `warn` with the account, the MXC URI and the byte count.

### 5. Phase 2: stream tickets

A `<video>` cannot carry a bearer header, so the server issues a URL that stands in for one, valid for one object only.

- `POST /v1/media/{account_id}/{server_name}/{media_id}/stream-ticket` (bearer-authenticated) checks the object exactly as `get_media` does: the account, the owning event, and the served type.
  It refuses anything whose served type is not `video/*` or `audio/*` with `400`.
  It returns `{ "data": { "path": "/v1/media-stream/<ticket>", "expires_at": … } }`.
  The path is relative, so the client resolves it against its own server base.
- `GET /v1/media-stream/{ticket}` is mounted outside `require_bearer`, beside the OAuth callback router.
  It serves the object exactly as `get_media` does, ranges and all, and also sends `Cache-Control: private, no-store` and `Referrer-Policy: no-referrer`.
  `no-store` keeps the plaintext of an encrypted attachment out of the browser's disk cache, as today's in-memory blob does.
  The cost is that an engine may re-request ranges it has already played when the user seeks back.
  Those re-requests are reads of Axon's own cache file, not new homeserver downloads.
  The device checks in sections 6 and 7 record whether seeking back re-requests and whether playback suffers.
  If it does, relaxing `no-store` is decided then, weighing smoother seeking against plaintext on disk.
- A ticket is 32 random bytes, base64url-encoded, held in an in-memory table keyed by the ticket.
  The table records the account, the MXC URI, the issuing token's id and session id, and the times the ticket was created and last used.
  It records no event id, because the checks below resolve the event from the MXC URI, exactly as `get_media` does.
  Restarting the server drops every ticket, and the client simply asks for a new one (section 6).
- A ticket expires 30 minutes after its last use and never more than 12 hours after it was issued.
  A use is a request served without error, so a ticket whose requests keep failing neither stays alive nor ranks as recently used for the cap below.
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
- Every use also re-runs `get_media`'s own checks (`resolve_media_content`), since the ticket route serves through the same code.
  `resolve_media_content` takes no event id: `get_event_by_mxc_url` resolves the URI to the oldest stored event that references it.
  The checks require that the account still exists and that this event is still stored with its content.
  Redaction leaves the content `NULL`, which already fails closed with `404`.
  So a ticket stops working on its next request once its account is removed or the resolved event is redacted, rather than serving for up to 12 hours.
  Room membership is not checked, which matches the bearer route: `get_media` does not check it either.
  - The event checked is the one the URI resolves to, not necessarily the one the user opened.
    If two events share a URI and only the newer one is redacted, the ticket keeps working, as the bearer route does.
  - Media authorized only by an image pack or an inline message has no owning event.
    `resolve_media_content` serves it as a sniffed raster image (`raster_only`), never as `video/*` or `audio/*`, so the mint refuses a ticket for it.
    Every ticketed object therefore has an owning event.
- Each credential session holds at most 32 live tickets, and a 33rd evicts that session's own least recently used ticket.
  So one client minting tickets in a loop churns only its own tickets.
  The cap counts the session, not the token, because an OAuth refresh mints a new access token in the same session, and a token cap would hand the session a fresh allowance every hour.
  A non-expiring token is its own session.
- The table holds at most 4096 tickets, as a memory bound, and past that drops the least recently used ticket, whoever holds it.
  With 32 per session, that takes 128 sessions holding live tickets at once, and each session is a sign-in or an operator-minted token.
  So the per-session cap is what isolates one client from another, and the table cap protects only memory.
  A single viewer needs only a few tickets at once: one per video or audio preview the user has opened (section 6).
  A rendered timeline mints none.
- Tickets are never logged.
  The app's outermost `TraceLayer` (`crates/axon-api/src/lib.rs`) records `path = %request.uri().path()` on an `INFO` span for every request, including routes outside `require_bearer`.
  So its `make_span_with` changes to record `/v1/media-stream/{ticket}` in place of any path under that prefix, matched by prefix.
  A path under the prefix that matches no route, which falls to the `/v1/{*path}` catch-all, is redacted too.
  The layer's `on_response` and `on_failure` events log inside that span and add no URI of their own, so the one change covers them.
  A handler span of its own would not stop that outer one recording the raw path.
  An audit of `crates/` for `.uri()`, `OriginalUri` and `MatchedPath` found no other reader of the request path in the request pipeline.
  The OAuth rate limiter and callback read only their own query strings, and there is no panic-catching layer.
  The phase 2 PR repeats that audit, and a test asserts that a ticket request's log output does not contain the ticket.
  Ticket-route errors do not carry the ticket in their `Display` text or their error envelope.
- The route ignores `If-Range`, as `get_media` does today.
  That is correct here: the `ETag` is a hash of the MXC URI and an MXC object never changes, so a range is always taken from the current representation.

The alternatives to tickets are covered under Alternatives considered.

The ticket route is additive, so it needs no `API-Breaking-Change` trailer (ADR 0099).
The OpenAPI spec documents both routes.

### 6. Phase 3: the web client plays from the ticket URL

For `video` and `audio` previews, `BlobPreview` in `MediaPreview.tsx` stops using `useMediaBlob`:

- `BlobPreview` mounts only when the user opens an item.
  The timeline shows a video as a `MediaTile` with the sender's thumbnail, and audio as a row.
  Clicking either sets `expanded` in `MediaAttachment.tsx`, and only then does `MediaPreview` render the player.
  So a ticket is minted, and a download started, only for media the user has chosen to play.
  Scrolling a media-heavy room mints no tickets and starts no downloads.
  The phase 3 PR keeps that rule, and a test asserts that rendering a timeline makes no ticket request.
- On mount it requests a ticket.
  That is a small JSON call through the normal API client and does not take a media slot.
  Then it renders `<video src={ticketUrl} preload="metadata">`, or the `<audio>` equivalent.
  The browser fetches the media itself, with its own range requests and buffering, and none of the media service's slots or deadlines apply.
- The poster keeps the blob path, because it is a small image.
- Only an object the server will serve as `video/*` or `audio/*` uses a ticket, since the mint refuses anything else.
  A missing, generic or parameterised `info.mimetype` is served as `application/octet-stream`, so that object keeps the blob path and its 100 MiB ceiling.
  This covers the iPhone `.mov` sent as `m.file` that the client types from its extension.
  Streaming those objects too is [issue 1182](https://github.com/matrix-axon/matrix-axon/issues/1182).
  A ticket path has no client-side size ceiling: the server's `max_stream_bytes` is the bound.
- A media element reports a `403` and an unsupported codec the same way (`MEDIA_ERR_SRC_NOT_SUPPORTED`).
  So on `error` the component probes the ticket URL with `fetch` and `Range: bytes=0-0`:
  - A `2xx` probe means the bytes are fine, and it shows today's "This browser cannot play …" message.
  - A `401`, `403` or `404` from the ticket route means the ticket expired or the server restarted.
    It requests a new ticket once, sets the new `src` and restores `currentTime`.
  - A `422` shows the undecryptable message, with no retry.
  - Anything else shows the generic load failure with a Retry button.
- A stall is probed the same way.
  WebKit's AVFoundation player (Safari on iOS and macOS) never raises `error` for a refused range once playback has begun.
  It re-requests the range, tens of times a second, for as long as the page is open, and it does so for a `401`, `403`, `404` or `410` alike, so no status the server could send would end it.
  It keeps playing from its buffer meanwhile, and even reports `networkState` idle, so neither a frozen playhead nor the network state gives it away.
  On a real iPhone, a server restart left a video playing on for 74 s while AVFoundation sent 442 refused requests, and nothing reached the page but `stalled`.
  So the component probes:
  - 1 s after `stalled`, unless a `progress` event (data arriving) came in meanwhile;
  - 5 s after `waiting`, if `currentTime` has not moved and the player is seeking, playing or short of data (`readyState` below `HAVE_FUTURE_DATA`).
    `readyState` alone is not enough, because AVFoundation reports `HAVE_ENOUGH_DATA` throughout a seek whose ranges are refused.
  - Stall probes are at most one every 10 s.
    AVFoundation also raises `stalled` every few seconds on a healthy stream whose buffer is full, so a video playing in Safari costs a one-byte probe every 10 to 15 s.

  A reachable probe means a slow link, and the player is left to its own retries.
  Any other answer is handled as for `error`, and replacing or removing the `src` ends the request loop.
  The same path catches a `422` after a hash mismatch, which AVFoundation would otherwise also retry for good.

- A replacement ticket is allowed once per successful load (`loadeddata`).
  So a ticket that fails at once ends in Retry rather than a loop, and a long video outlives several hour-long OAuth access tokens.
- The Download button keeps the blob path in this phase.
  A large download is still buffered there and still subject to the 120 s deadline.
  Moving it onto the ticket route is a separate, deferred change (Consequences).

Before this phase merges, it is checked in Safari on iOS and in Chrome and Firefox on a desktop.
Each check plays a large video, seeks back into the part already played, and records whether that seek re-requests ranges under `no-store` (section 5) and whether playback stalls.

#### Phase 3 check results (2026-11-19)

The checks ran the real web client against a local Synapse and Axon.
A logging proxy in front of Axon throttled ticket responses to 3 MB/s, and could swap `no-store` for `private, max-age=3600` (the A/B column) or answer old tickets `404` as a restart would.
A harness page drove the client in a same-origin iframe: open a 5-minute video, play 15 s, seek to 60 %, play 10 s, seek back to 5 s, then close.

| Engine                                              | First requests                      | Start after click | Seek back into played part                                                                    | With `max-age` instead              | Expired ticket                                                                                                                                                                                       |
| --------------------------------------------------- | ----------------------------------- | ----------------- | --------------------------------------------------------------------------------------------- | ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Google Chrome 157 (headless, Linux), 305 MB 720p    | `bytes=0-`                          | 0.3–0.45 s        | re-requests from byte 294912; plays after 1.75 s                                              | the same: re-requests, 1.77 s       | `error` (network), probe `404`, new ticket, resumed in 0.2 s                                                                                                                                         |
| Firefox 155 (Playwright), 305 MB                    | `bytes=0-`                          | 0.75 s            | no request; 0.22 s                                                                            | not run                             | the same, 0.3 s                                                                                                                                                                                      |
| WebKit 26.6 (Playwright, Linux GStreamer), 305 MB   | no `Range`, answered `200`          | 2.8 s             | no request; buffers the whole file in the background                                          | not run                             | the same, 0.4 s                                                                                                                                                                                      |
| Safari, iOS 18.6 simulator, 150 MB 360p, video only | `bytes=0-1`, then `bytes=0-<len-1>` | 1.4–1.65 s        | AVFoundation drops what it played after a far seek, so it re-requests; resumes in about 0.8 s | the same: re-requests, about 0.76 s | no `error`: `stalled`, then a 75–90-per-second request loop. With the stall probe: new ticket and resumed 5.2 s after the expiry after a seek, 9 s while playing on its buffer (26 refused requests) |

- **`no-store` stays.** Neither engine that re-requests on seeking back (Chrome and AVFoundation) reused bytes when the response was cacheable, and neither stalled.
  Relaxing it would put plaintext on disk for no gain.
- **Closing the preview** ended the browser's connection in every engine.
- **The iOS check is a simulator, not a device.** The simulator ran on a QEMU macOS VM without a GPU or an audio device.
  There it could not decode a 720p main-profile H.264 stream, or any file with an AAC track ("Media failed to decode").
  The app handled that correctly, probing and then showing the "cannot play" message.
  A plain file failed the same way, so the cause is the VM.
  The run therefore used a 360p baseline, video-only file over the 100 MiB blob ceiling.
  A real iPhone is still worth a check before phase 4, which section 7 already requires.
- **A real iPhone** (iOS 18.7 Safari, against production): a 104 MiB Element X video played in Safari and in Chrome on Windows, and so did audio.
  A first play started while Axon was still downloading.
  A server restart mid-playback showed the buffered-loop case above, which the `progress`-based rule was then added for and checked in the simulator.
  Each ticket request costs about 180 ms on that server before its first byte, most of it the MXC-to-event lookup that every media request runs, and AVFoundation's many small ranges multiply it ([issue 1188](https://github.com/matrix-axon/matrix-axon/issues/1188)).
- WebKit on Linux opening without `Range` matters for phase 4 (section 7): [issue 1184](https://github.com/matrix-axon/matrix-axon/issues/1184).

The ticket URL is the server's own origin when the web client is served by the server (ADR 0116), so no CORS or CSP change is needed there.
Only the `<video>` load would be exempt from CORS if it were cross-origin.
The section 6 probe is a `fetch` with a `Range` value that is not CORS-safelisted, so cross-origin it would need a preflight.
That is no new constraint: the server sends no CORS headers at all (`crates/axon-api/src/web.rs`), so every API call the web client makes already requires the same origin.
The Vite dev proxy is same-origin from the browser's view.
The ticket route therefore adds no CORS handling, and the probe has the same origin requirement as the rest of the client.

### 7. Phase 4: the shells proxy ticket URLs through a custom scheme

The shell registers an `axon-media` scheme with `register_asynchronous_uri_scheme_protocol`.
In the shells, `platform/tauri.ts` maps a ticket path to a scheme URL with Tauri's `convertFileSrc(ticket, 'axon-media')`.
That call produces `axon-media://localhost/<ticket>` on macOS, iOS and Linux, and `http://axon-media.localhost/<ticket>` on Windows and Android, so the page never spells out the per-platform form.
The handler:

- Forwards the request to `<server base>/v1/media-stream/<ticket>` through the shared `axon-http` client (ADR 0127), passing `Range` and `If-None-Match` through and copying back the status, `Content-Range`, `Content-Length`, `Content-Type`, `Accept-Ranges` and `ETag`.
  A single-range `GET` goes through a read-ahead window first (amended below).
- Needs no bearer token, because the ticket is the credential.
- Learns the server base from a new `set_media_stream_base` command, which the page calls whenever its server changes.
  Today `axon-http` takes a full URL on every request and the shell keeps no server base of its own.
  The handler refuses to run until the base is set, and only ever requests that base's `/v1/media-stream/` prefix, so the scheme cannot become a general fetch from the page.
- Returns at most 4 MiB of body per response.
  Tauri's scheme responder takes a whole body, not a stream, so this bound is what keeps each response's memory in the shell fixed.
- Answers `502` if the upstream body errors or ends before the bytes the handler means to return.
  A server-side abort at the withheld tail (section 3) therefore reaches the player as an error, never as a short but clean `206`.

The bound is applied to what comes back, not by rewriting what is asked for.
The handler forwards the player's `Range` header unchanged, reads at most 4 MiB of the upstream body, then drops the upstream connection.
That one rule covers every range form, because the server always answers a ranged ticket request with a `206` whose `Content-Range` carries the known total (section 3):

- **`bytes=a-b` within 4 MiB** comes back whole, and is returned as the server sent it.
- **Open-ended `bytes=N-`, or `bytes=a-b` wider than 4 MiB.**
  The handler returns the first 4 MiB as a `206`, with `Content-Range: bytes N-(N+4 MiB-1)/len` and a matching `Content-Length`.
- **A suffix `bytes=-N`** is resolved to an absolute start by the server.
  The upstream `Content-Range` then says where it begins, at `len-N`.
  If N is over 4 MiB, the handler returns the first 4 MiB from that start, with the `Content-Range` rewritten to the bytes it actually returns.
  Players reading an MP4 index from the end of the file use either form, and both arrive with the total length the player needs for its next absolute request.
- **A range past the end** is answered by the server with `416` and `Content-Range: bytes */len`, and the handler passes that through.
  Because the handler never widens or invents a range, its own bound can never produce a `416`.
- **A matching `If-None-Match`** gets a `304` from the server with no body, passed through as is.
  `If-Range` is passed through and ignored by the server, which is correct for an immutable object (section 5).

Once a response holds 256 KiB and 1 s has passed since its headers, the handler returns what it has rather than waiting for the rest of the 4 MiB.
Otherwise a slow link would hold the player's first frame until 4 MiB had arrived, about 16 s at 2 Mbit/s.

A shorter `206` than requested is valid HTTP: the response's `Content-Range` says which bytes it holds.
The Chromium-based WebViews (Windows, Android) and AVFoundation then request the next range, and Tauri's own `streaming` example relies on the same behaviour.

A request with no `Range` header is the one case that does not fit.
A `206` is not a valid answer to it, and a `200` would have to carry the whole object.
The handler forwards it as is.
If the server's `Content-Length` is at most 4 MiB, the handler returns the `200` whole.
Otherwise it answers `502` and logs a `warn` with the platform.
The same applies to a `200` that the server sends for a `Range` it ignores, such as a multi-range request.
Media elements open with a ranged request (Chromium sends `bytes=0-`, AVFoundation `bytes=0-1`), and the section 6 probe sends `bytes=0-0`, so nothing in the app is expected to reach this case.
The phase 3 checks found one exception: WebKit on Linux, the engine behind the Linux shell, opens without `Range` ([issue 1184](https://github.com/matrix-axon/matrix-axon/issues/1184)).
The device check below records the first request each platform's player actually sends.
If one of them turns out to send a request without `Range`, that platform keeps the blob path until the handler can stream a response body, rather than the ADR answering it with an invalid `206`.

#### Amendment (2026-11-20): a read-ahead window in the shell

The first macOS check showed that, through WebKit's custom-scheme path, AVFoundation asks for each audio and video sample on its own.
Over plain HTTP it opens with `bytes=0-<len-1>` (section 6); through the scheme it sent 4,505 requests in 66 s of a 5-minute 720p video.
That is about 60 a second, 8.7 KB each on average, and never an open-ended range.
So the 4 MiB bound never applied, and forwarding each range unchanged would cost the server a ticket, token and event check per sample, at about 180 ms each on production ([issue 1188](https://github.com/matrix-axon/matrix-axon/issues/1188)).

The handler therefore no longer forwards a single-range `GET` unchanged:

- The object is read in 4 MiB blocks on a fixed grid: a range whose block is not held fetches that block (`bytes=K-(K+4 MiB-1)`, where `K` is the range's first byte rounded down to a multiple of 4 MiB, which the server clamps at the end).
  Later ranges inside the block are answered from memory, as `206`s with the object's total in `Content-Range`, and a range that runs past the block's end gets the block's part.
- A request waits only for its own bytes while the block goes on filling, and requests that arrive while a block is being fetched share it.
  So a 9 KB sample read does not wait for 4 MiB.
- Once a read is served from a complete block, the next block is fetched, unless it is held or past the end.
  That keeps one block ahead of what the player has read, and fetches nothing while the player reads nothing.
- The window holds at most 8 blocks (32 MiB) across all tickets, drops a block unused for 60 s, and drops everything when the page reloads.
  It is memory only, so `no-store` (section 5) still means nothing reaches disk.
- A block fetch that gets no usable `206` is not held.
  The server's answer, such as a `404` for an expired ticket or a `416` past the end, reaches the requesting player as before, and requests that were waiting on that fetch go upstream themselves.
- A block whose body fails still answers the requests its bytes already cover, and `502`s the rest; the next request fetches anew.
- A block outlives its ticket's revocation by at most its own 4 MiB, the same bound the Consequences accept for a response already in progress.
  The section 6 probe sends an `Axon-Stream-Probe` header, which the window never answers, so an expired ticket is seen as soon as the server refuses it.
  The server ignores the header.

The first version started each block at the requested range's first byte.
On production (2026-11-20), the iOS shell's audio and video reads often started a few KiB before a held block, and each such read fetched a block of its own: three overlapping 4 MiB blocks within a second, and 2.2–2.7 times the distinct bytes over a session.
On a slow link those fetches split the connection between them, and the extra blocks pushed useful ones out of the 8.
Blocks on the grid cannot overlap.
The same version fetched the next block only when the player asked for it, which left the 1.4–2 s gaps seen on Windows; hence the read-ahead.

A suffix range whose object length no block has learned yet, a multi-range request, a conditional request and a `HEAD` still go upstream as sent.
Rewriting the outgoing `Range` also settles the connection-reuse question above: each block request is bounded, so the server's response ends where the handler stops reading.

#### Phase 4 check results

The checks run a debug shell whose page is a harness, with the real CSP and plugin, against a local server that stands in for the ticket route.
That server serves a 147 MB, 5-minute 720p H.264 and AAC MP4 with `no-store`, throttled to 3 MB/s, and logs every request.
The harness plays for 15 s, seeks to 60 %, plays 10 s, seeks back to 5 s, plays 10 s, then has the server expire the ticket and plays on for 20 s.
It then plays the same file as audio, and loads an already-expired ticket.

| Platform                                                   | First requests                                                | Start                        | Seek to 60 % / back | Upstream requests                          | Probe through the scheme              | Expired ticket                                                                                                                                                                                 |
| ---------------------------------------------------------- | ------------------------------------------------------------- | ---------------------------- | ------------------- | ------------------------------------------ | ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| macOS 26 (WKWebView), unwindowed                           | `bytes=0-1`, then per-sample closed ranges                    | 1.2 s                        | 0.24 s / 0.23 s     | 4,505 video, 2,861 audio                   | `206` and `404` readable (CORS works) | at load: `error` (code 4) at once                                                                                                                                                              |
| macOS 26 (WKWebView), windowed                             | the same                                                      | 0.38 s                       | 0.44 s / 0.21 s     | 9 video (4 MiB each), 5 audio              | the same; skips the window            | mid-play: plays on from its buffer, 2 upstream `404`s in 20 s and no request loop; probe sees `404`                                                                                            |
| iOS (device)                                               | not yet run                                                   |                              |                     |                                            |                                       |                                                                                                                                                                                                |
| Windows (WebView2), real server                            | not visible in the server log (the window rewrites ranges)    |                              |                     | 4 MiB blocks, one at a time, 1.4–2 s apart |                                       | plays, but buffers often on a link that delivered 0.4–0.5 MB/s to every client on it, against the 1.37 MB/s the 557 MB, 408 s test video needs                                                 |
| Android 15 emulator (WebView 124), harness, wry 0.57.0     | `bytes=0-`                                                    | 1.1 s (video), 1.8 s (audio) | —                   | 1 per block, as on macOS                   | `206` and `404` readable from byte 0  | every range after byte 0 fails with `net::ERR_FAILED`, so playback stops at the first 4 MiB and seeking never completes ([issue 1194](https://github.com/matrix-axon/matrix-axon/issues/1194)) |
| Android 15 emulator (WebView 124), harness, wry 0.57.3     | `bytes=0-`                                                    | 1.1 s (video), 1.3 s (audio) | 2.72 s / 1.69 s     | 13 video, 7 audio (4 MiB, on the grid)     | `206` and `404` readable              | mid-play: plays on from its buffer, then the WebView retries the `404` about twice a second (12 in 14 s) until the source changes; the probe sees `404`                                        |
| Linux (WebKitGTK 2.52, GStreamer 1.28), harness under Xvfb | none: the player refuses the scheme before it sends a request | —                            | —                   | —                                          | `206` and `404` readable              | every media element fails with `MEDIA_ERR_SRC_NOT_SUPPORTED` within 150 ms; Linux keeps the blob path ([issue 1192](https://github.com/matrix-axon/matrix-axon/issues/1192))                   |

**Linux cannot play through the scheme, so it keeps the blob path.**
WebKitGTK's GStreamer player admits only the URL schemes in its allow-list (`isProtocolAllowed` in WebKit's `GStreamerCommon.cpp`: `blob`, `data`, `file`, `http`, `https`, and any named in `WEBKIT_GST_ALLOWED_URI_PROTOCOLS`).
With the scheme added through that variable, the URL reaches GStreamer's `uridecodebin`, which has no source element for `axon-media`.
That pipeline runs in WebKit's web process, where the shell cannot register one.
The page's `fetch` of the same URL works, which is how the probe gets through, but the media element cannot use it.
So `platform/tauri.ts` sets `mediaStreamSrc` to `null` on Linux, as this section provides for a platform that cannot play through the scheme, and the options for streaming there are [issue 1192](https://github.com/matrix-axon/matrix-axon/issues/1192); the proposed one, a loopback HTTP server in the shell, is [issue 1197](https://github.com/matrix-axon/matrix-axon/issues/1197).

**Android streams once wry is 0.57.1 or later.**
Android WebView applies a request's `Range` to an intercepted response itself: it treats the response body as the whole resource and skips to the range's first byte ([Chromium issue 571186732](https://issues.chromium.org/issues/571186732)).
Under wry 0.57.0 the WebView got a byte-array stream over the body the handler returned, which holds only the requested bytes, so every range that starts after byte 0 failed with `net::ERR_FAILED`, a page `fetch` of `bytes=1000-1999` included.
wry 0.57.1 ([wry PR 1865](https://github.com/tauri-apps/wry/pull/1865)) puts a prefix as long as the `Content-Range` start in front of a single-range `206` body, so the WebView's skip consumes the prefix rather than the bytes ([issue 1194](https://github.com/matrix-axon/matrix-axon/issues/1194)).
The shell takes wry 0.57.3, and with it the run above plays, seeks and recovers as on macOS, so Android streams like the other shells.
On an expired ticket the WebView retries the `404` about twice a second; the section 6 probe runs one second after the `stalled` that follows and replaces the ticket, which ends it.
The loopback server that Linux needs is [issue 1197](https://github.com/matrix-axon/matrix-axon/issues/1197).

The page's own origin in the shells is `axon://localhost` (`http://axon.localhost` on Windows and Android), the shell's `axon` app scheme, so that is the origin the handler's CORS headers name.

The CSP gains `axon-media: http://axon-media.localhost` in `media-src`, and in `connect-src` for the section 6 probe.
In the shells the probe is cross-origin: the page is `tauri://localhost` or `http://tauri.localhost`, and the probe goes to the `axon-media` scheme.
So the handler answers the probe's `OPTIONS` preflight itself, allowing the `Range` request header for the app's own origin.
It also adds `Access-Control-Allow-Origin` for that origin, and `Access-Control-Expose-Headers: Content-Range`, to its responses.
The device check includes the probe path, by playing an expired ticket.

Before this phase merges, it must be checked on a real iOS device, a real Android device, macOS, Windows and Linux.
On each, the check records the player's first request and confirms that playback, seeking and a short `206` all work.
It also records how many scheme requests a minute of playback takes, and whether seeking back re-requests ranges under `no-store` (section 5).
It records whether the upstream connection is reused between scheme requests or torn down after each 4 MiB, since the handler drops it mid-body.
If teardown turns out to matter, rewriting the outgoing `Range` to a bounded `bytes=N-(N+4 MiB-1)` for the absolute forms would let the connection be reused, leaving suffix and unknown-length requests as they are.
It plays an OAuth-session video across its access token's expiry, to confirm that the probe, the new ticket and the `currentTime` restore in section 6 recover without a visible stall.
If they do not, binding a ticket's life to the OAuth session rather than the access token is the follow-up, filed by the phase 4 PR.
At 4 MiB a response, a 1 GiB video plays through about 256 requests.
Each re-runs the ticket, token and event checks: an in-memory lookup and two indexed reads.
If that count turns out to matter, the follow-up is a streaming response body in the scheme handler, which Tauri's responder does not offer today.
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
  A tampered stream is aborted before its last 64 KiB and never cached, but bytes before that may already have been played (sections 3 and 4).
  The sender's own `info.mimetype` decides which objects stream, which section 4 explains does not widen that risk.
- A streamed download runs to completion even if the viewer closes the preview at once, so abandoned videos still cost their full upstream transfer.
  Four stream permits and the cache reservation bound that cost.
  The permits are server-wide, so one user who abandons four large previews can make others' video wait for a `503`; a per-account share is #1139.
- A ticket is a credential in a URL.
  It is limited to one object, a 30-minute idle lifetime, its issuing token's validity and the owning event still being stored.
  Axon's own request log records the route template instead (section 5), but a reverse proxy's access log records the ticket on every range request unless it is told not to.
  The packaged nginx and Apache sites log `/v1/media-stream/{ticket}` in its place, and the packaged Caddy site, which keeps no access log by default, shows a `log` block that does the same.
  A proxy's error log can still record a ticket when a proxied request fails, so it must stay private.
  Operators who run their own proxy configuration, or ship its logs elsewhere, need the same redaction (`docs/technical-reference.md`, Media streaming).
  Redaction relies on a prefix rule in the `TraceLayer` plus an audit and a test; a router-owned redaction helper is #1140.
- A ticket's checks run when a request starts, not while its response streams.
  So a response already in progress when its token is revoked, or its event redacted, runs to the end of its range.
  In the shells that is at most 4 MiB.
  In the browser, an open-ended range can deliver the rest of the object.
  This is accepted: the token's holder was entitled to that object when the response began, and the next request is refused.
- `axon-media` adds a second custom scheme to the shells and widens their CSP by one source.
- Each shell scheme response is buffered in the shell process, up to 4 MiB, so a 1 GiB video takes about 256 scheme requests (section 7).
  A request without `Range` for an object larger than that is refused (section 7), so the device check is what confirms no platform's player sends one.
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
