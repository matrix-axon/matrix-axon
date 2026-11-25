# ADR 0143 — Stream staged media uploads to the homeserver

**Status:** Proposed. Extends [ADR 0059](0059-media-send.md) and [ADR 0142](0142-send-time-media-metadata-and-thumbnails.md).
Design issue [1206](https://github.com/matrix-axon/matrix-axon/issues/1206), implementation issue [1185](https://github.com/matrix-axon/matrix-axon/issues/1185), tracking issue [1213](https://github.com/matrix-axon/matrix-axon/issues/1213).

## Context

Axon already streams incoming uploads to durable staging files.
The later claim reads the whole original into memory, and the SDK's upload takes a `Vec<u8>` and makes an encrypted copy for an encrypted room.
This puts the memory cost of a send in proportion to the file size.
Axon defaults to 100 MiB, while the production Synapse configuration reported in issue 1185 allows `1G`.
Even if staging and the upstream upload both stream, either transfer can take minutes on a healthy connection.

Issue 1206 records the decision to make `send-media` asynchronous.
The durable acceptance boundary lets a client stop waiting for HTTP while Axon uploads the file and sends the Matrix event.
It also gives a client that loses its response somewhere to recover the outcome.

## Decision

### 1. Accept the send asynchronously and keep the synchronous contract

Add `POST /v1/accounts/{account_id}/rooms/{room_id}/send-media/async` and advertise `send_media_async: true` on `GET /v1/capabilities` when the server supports its complete contract.
The endpoint takes the ADR 0142 send body, including captions, relations, `info`, `thumbnail_upload_id` and bundled `url_previews`, and requires ADR 0142's client-generated `txn_id`.
One client transaction identifies one logical send in an account and room.
Create it with the local echo and retain the submitted body, including upload IDs, until the client knows whether Axon accepted it.

Use the existing synchronous preflight and error mapping.
Check account, room and upload ownership, kind and MIME, formatting, relation and preview shape, distinct upload IDs, and all metadata and client thumbnail validation in ADR 0142 § 4 before acceptance.
A malformed `info`, invalid thumbnail that fails validation, or reused upload ID within the request still gets an immediate `400`.
Keep the existing thumbnail validation budget and omission behavior when that budget expires.
Use a metadata-only claim for the original, with bounded reads for any local validation.
Server-generated thumbnails, upstream uploads and event dispatch happen after acceptance.

Return `202 Accepted` with the committed status in the usual Axon response envelope and a `Location` pointing to `GET /v1/accounts/{account_id}/rooms/{room_id}/send-media/async/{send_id}`.
The status includes the server-issued `send_id`, client `txn_id`, room ID, attempt number, monotonic revision and state.
States are `queued`, `uploading`, `sending`, `sent` (with `event_id`), `failed` (with a stable error code and whether retry is allowed), and `outcome_unknown` (with `reconciliation_open` indicating whether the original session can still supply evidence).
The GET is restricted to the same account and room and returns the current status with `Cache-Control: no-store`.
A replay of the accepted POST returns `200 OK` with the same send ID and its current status, even if terminal.

The existing `POST …/send-media` continues to return `200` and `{event_id}` on success and keeps ADR 0142 § 8's synchronous timing and errors.
Existing TUI, web, shell and share clients can keep using it.
Both routes reuse validation, the streaming original uploader and the event builder.
A newer client uses the asynchronous route only when the capability is present, or the synchronous route for a fresh send to an older server.
Persist the chosen route with the pending echo so it survives reconnect and reload.
If the capability disappears while an asynchronous send may have been accepted, keep that send pending reconciliation and query the original route when available again.

### 2. Persist acceptance, retries and the dispatch boundary

The server keeps one PostgreSQL record per `(account_id, room_id, txn_id)` with a unique constraint.
The record holds a send ID, the originating Matrix homeserver, user and device, attempt and state, and the bounded request fields and staged upload references needed to run an active send.
Use an owned, bounded admission task and short transactions.
Reserve the transaction key and claim the uploads together, validate any claimed client assets outside that DB transaction, then commit the validated job and its claim ownership before returning `202`.
Serialize admissions by transaction key and look up attempt identity before inspecting any staged upload rows.
The canonical fingerprint covers the typed message and formatting fields, relations, declared `info`, the presence of a client thumbnail, and the ordered bundled preview fields and their image presence.
It preserves meaningful distinctions such as omitted versus explicitly empty previews.
The upload IDs and `retry_of` are outside the fingerprint.
The accepted attempt is identified by the transaction key, its fingerprint, and its retry selector, with an absent `retry_of` identifying the initial request.
An equivalent replay joins an in-progress admission for that selector or returns the committed status, even if the client has since staged replacement files or the original files have been removed.
A conflicting fingerprint returns `409` and leaves the caller's uploads available.
Failed preflight releases every claim and clears the provisional reservation for that attempt.
Disconnecting the client leaves the admission task responsible for completing these steps.

A supervised worker claims each committed job once through a DB state transition.
On a restart, requeue work that was durably accepted and has yet to reach event dispatch, applying the queue deadline in section 5.
Restart the upload from byte zero, redoing encryption if necessary.
An upload whose response was lost may leave an unreferenced object at the homeserver, which follows the homeserver's media retention policy.
Before enabling admission or workers at boot, release every abandoned provisional attempt's claims and delete its reservation in a single transaction.
Remove the transaction-key row if the initial request was never accepted.
For a provisional retry, retain the previously accepted job and its attempt history so the retry can be admitted again against the same failure.
Only committed jobs enter the recovery queue and count toward the initial admission load after this cleanup.
The existing reset of stale `sending` upload rows still releases legacy claims and preserves the claims of accepted jobs.
Settle job state and upload ownership in a single DB transaction, with idempotent filesystem cleanup and startup recovery for files whose unlink was interrupted.
Consume the originals and any client thumbnails or preview images on `sent`.
Release them on a confirmed failure, with the short retry retention in section 6.
Consume them after the active worker settles `outcome_unknown`, as another event dispatch for that send is unsafe and reconciliation needs only its identifying metadata.
The staging sweeper and explicit upload deletion respect accepted job ownership.
A worker resumes only under the original Matrix identity and device.

Keep the SDK's joined-room check, encryption-state lookup, member sync, key query and preshare, and local event encryption in the preparation phase.
These can all run before the room-event `PUT`.
If preparation fails, settle it as a definite pre-dispatch failure and release the staged files for retry where the error allows it.
A restart during preparation requeues the accepted attempt.

Commit a dispatch marker immediately before the prepared room-event `PUT` can reach the transport.
Matrix SDK 0.19.1 runs preparation and dispatch inside one [`Room::send`](https://docs.rs/matrix-sdk/0.19.1/matrix_sdk/room/struct.Room.html#method.send) future, so the async path needs a narrow SDK hook at the final request boundary.
Use an upstream SDK hook or carry a pinned patch that exposes this boundary while sharing the SDK's existing preparation path.
Verify both options in the [SDK feasibility spike](https://github.com/matrix-axon/matrix-axon/issues/1260) before implementing asynchronous send.
If a pinned patch is needed, assign an `axon-sync` maintainer to upstream it and revalidate it on SDK upgrades.
If neither option can guarantee the boundary, revise this design before implementing or advertising asynchronous send.
Run the hook for the final room-event request only, after serialization and before handing the request to the transport.
The hook must commit the marker successfully for the current attempt before the transport may proceed.
Use a server-derived Matrix transaction ID (`axon-media-async-<send_id>-<attempt>`) and pass it via `with_transaction_id`.
An explicitly retried, definitively failed attempt gets a fresh Matrix transaction ID, while an auth resend within an attempt keeps its ID.
Keep the SDK's room-event encryption and disable transient network retries on this send with `RequestConfig::disable_retry()`.
Any auth resend must follow an explicit Matrix rejection of the original attempt, as with a verified `M_UNKNOWN_TOKEN` and SDK refresh on the same device.
The [Matrix transaction rules](https://spec.matrix.org/v1.18/client-server-api/#transaction-identifiers) scope retransmissions to a device and an endpoint, and a server's deduplication history can expire.
Axon's durable record is what prevents a second dispatch after an ambiguous first one.

Commit `sent` on a valid success response.
Only a local failure known to precede the event request or an upstream response that proves rejection can produce `failed`.
A timeout, dropped connection, ambiguous upstream error or restart after the dispatch marker becomes `outcome_unknown` unless success or definitive failure has already been recorded.
This includes the small crash window between committing the marker and handing the prepared request to the transport.
The worker never dispatches that send again after an unknown outcome.
If the sending device observes a matching `unsigned.transaction_id` on an event from its own Matrix identity in the exact room, reconcile the job to `sent` using that event ID.
Record that positive evidence in the durable ingest path before the sync position can advance past it, including for encrypted events, and let it win over any racing unknown result.
A missing transaction ID or a timeline gap leaves the state unknown.
Keep the compact terminal records, canonical fingerprint and accepted attempt selectors for the account's lifetime so expiry cannot turn a replay into a new send.

On logout, block new admissions and new event requests, including auth resends, and settle active work using the dispatch boundary.
Before stopping the original device's sync and invalidating its session, allow that existing sync task up to five seconds to record positive evidence for outstanding ambiguous attempts.
Keep confirmed event IDs and close reconciliation durably for any remaining `outcome_unknown` sends, incrementing their status revision.
Startup completes this settlement for an interrupted logout before allowing the account to resume work.
A replacement session uses a new device and cannot supply the old device's transaction evidence.
The status GET remains readable for deactivated accounts under the usual account authorization so clients can observe the final status.

For a matching fingerprint, a POST without `retry_of` is always a status lookup after initial acceptance.
A POST with `retry_of` and no previously accepted attempt returns `409`.
A replay of any accepted retry selector also returns `200` with the current status, including after later attempts.
A new retry selector on an `outcome_unknown`, `sent`, in-progress, or non-retryable `failed` send returns `200` with the current status and allocates no attempt or Matrix transaction ID.
For a retryable `failed` send, an unmatched selector that differs from the current failed attempt returns `409`.
The same lock guards all three conditions for admitting a retry: the state is `failed`, retry is allowed, and `retry_of` equals the current failed attempt number.
Run preflight on that new attempt using the released staged IDs or replacements for the same files, with the same canonical fingerprint.
The server increments the attempt number and records the selector only when it commits acceptance, then returns `202` and the new status.

### 3. Reconcile the local echo over WebSocket and HTTP

Publish `media_send.updated` on `/v1/ws` with the standard account envelope and the committed status as its payload.
Correlate the echo by account, room and client transaction ID, so an update that arrives before `202` can still resolve it.
Ignore older revisions if the HTTP response, updates or timeline arrive out of order.
On `sent`, resolve by `event_id` and coalesce with any `timeline.event` already received, including for a thread echo.
On `failed`, preserve the composer's contents or retryable echo according to where the error occurred.
Show `outcome_unknown` as an unconfirmed send, with no automatic resend or ordinary retry action.
While reconciliation is open, keep the echo eligible for a later `sent` update.
When reconciliation closes, show a final unconfirmed status and retire the persisted pending echo and its status polling.
After re-login, render any event the new device receives as an ordinary timeline event.
Only an exact event ID from confirmed status can coalesce it with an echo.

The socket is lossy ([ADR 0061](0061-web-live-websocket-layer.md)).
Read the status of outstanding sends on reconnect or reload, after a detected missed update, and with bounded backoff while pending if updates are absent.
Persist the pending echo's identity and routing information until it settles so recovery can work after a document reload.
If the client lost `202` and has no send ID, repeat the original POST with the same transaction and staged IDs, then switch to GET when the ID is known.
After a definite admission error, show it immediately and let the user correct the request.
The normal timeline gap fill remains the source for the displayed event body.

### 4. Stream the original and keep ADR 0142's event content

Claim the immutable staged file and its length by reference, and hold the claim until its producer has stopped.
For an encrypted room, open the file in `spawn_blocking` and wrap the blocking reader in matrix-sdk-crypto 0.19.1's [`AttachmentEncryptor`](https://docs.rs/matrix-sdk-crypto/0.19.1/matrix_sdk_crypto/struct.AttachmentEncryptor.html).
It writes AES-256-CTR ciphertext into the supplied chunk buffer and updates SHA-256 over those bytes as it reads.
Use the same producer with a plain reader for an unencrypted room.
Start with 64 KiB chunks and a two-chunk bounded channel to a streamed request body.
Allow at most one chunk under construction outside the channel, and close the reader on cancellation or when the receiver drops.
Use a worker permit through producer teardown, including when `spawn_blocking` is still returning, and join the producer before releasing or reusing the file.
The original's application-owned buffers should fit within a few chunks per active upload, with transport buffering fixed by the transport's limits.
ADR 0142's independently bounded thumbnail decode and generation budgets still apply, including its separate compressed input cap.

Send an authenticated [`POST /_matrix/media/v3/upload`](https://spec.matrix.org/v1.18/client-server-api/#post_matrixmediav3upload) with the staged length as `Content-Length`, which AES-CTR preserves.
Use the joined account's resolved homeserver URL and current SDK access token, putting the token only in the authorization header.
Use a pooled HTTP client with the account transport's TLS and proxy settings, redirects disabled and no automatic retry of this streamed POST.
Send the effective MIME and a safely encoded filename for plain media, and `application/octet-stream` with no plaintext filename for ciphertext.
Bound the response body to 16 KiB before parsing it and require a valid `mxc://` `content_uri`.
Only accept success once the producer has read exactly the declared length, completed encryption through EOF and the body has been handed to the transport.
Combine the returned URI with the encryptor's `finish()` result to build `EncryptedFile`.
For an explicit `M_UNKNOWN_TOKEN`, use the SDK's bounded refresh path and reopen the file once under the same identity, with fresh key and IV for an encrypted retry.
No event is sent when the original's upload fails.

Reuse ADR 0142 § 6's hand-built content and SDK event send for every relation shape, including normal replies, thread members and replies in threads.
Keep the kind, filename, plain and formatted caption, dimensions, duration, reply mentions, thumbnail and bundled link previews.
Use `url` and `thumbnail_url` in plain rooms and `file` and `thumbnail_file` in encrypted rooms.
Use the SDK's room encryption state as the source of truth for both uploads and the event.
Use its first reading to choose how to upload, then have SDK event preparation expose the exact encryption decision it uses to build the room event.
Proceed to the dispatch marker only if the prepared event and all its media sources agree with that decision.
If SDK preparation sees that the room gained encryption during a plain upload, discard the prepared content and re-upload the original and any attached thumbnails and bundled preview images as ciphertext.
Leave the earlier plaintext objects unreferenced under the homeserver's retention policy.
Rebuild the content with encrypted media descriptors and prepare the event again before committing the dispatch marker.
For sending, encryption stays enabled once the SDK has observed it, as described in the [Matrix encryption guide](https://matrix.org/docs/matrix-concepts/end-to-end-encryption/#handling-an-m-room-encryption-state-event).
If preparation ever reports plaintext for a room already known to be encrypted, fail before dispatch and keep the media encrypted.
The original's upload can run alongside bounded image thumbnail generation.
A rejected thumbnail upload still permits sending the original under ADR 0142 § 6.

### 5. Bound stalled transfers and resource use

Give the upstream request a fixed connection bound (10 s initially, including DNS and TLS) and a body stall bound (`media.upstream_upload_stall_timeout_secs`, default 60 s).
After the last body byte is handed to the transport, use `media.upstream_upload_response_timeout_secs` (default 60 s) for the response headers and complete bounded response body.
Operators can raise the response deadline for homeservers and storage providers that take longer to finalize large media.
Expiration while uploading the original is a definite pre-dispatch failure and is retryable, even if the homeserver finished storing an unreferenced object.
Run the stall watchdog outside the body producer, so the timer still runs when the transport stops polling the body.
Reset it only on nonempty body chunks pulled by the backpressured transport.
This measures progress at the HTTP stack, which can lead the peer by the size of fixed transport and socket buffers.
Verify the bound for both HTTP/1.1 and HTTP/2 with a peer that stops reading.
Configure the custom upload client with no whole-request timeout, so it has neither the SDK HTTP client's default 30 s deadline nor the total `media.upstream_upload_timeout_secs` budget.
Keep the send mutation, thumbnail and bundled-preview steps under their existing separate deadlines and retain per-homeserver overload accounting for actual upstream requests.
Keep `media.upstream_upload_timeout_secs` as a wall-clock bound for the legacy path and its existing callers.

The client-to-Axon leg also needs a stall bound before offering large uploads, since the staging route currently gives its whole body 60 s.
On that route, cap the permit wait and finalization separately, and time out a body that stops making progress on disk (`media.upload_stall_timeout_secs`, default 60 s).
Leave the old `media.upload_request_timeout_secs` setting as the bound for admission and finalization.
Reject a known oversized `Content-Length` before taking a staging writer and still enforce the limit on the actual streamed byte count.
Enforce both the byte cap and the writer count during a slow but progressing transfer.
Keep the reservation and permit until any in-flight file write has stopped and abort cleanup has run.
Before rollout, verify any reverse proxy on either leg also allows the chosen size and a transfer that progresses beyond the old total deadline.

Staging writers and upstream workers have separate limits (`media.max_concurrent_uploads` and new `media.max_concurrent_upstream_uploads`, both default four).
Both send routes use the upstream pool, and the synchronous route's existing deadline includes waiting for a permit.
Allow at most 16 asynchronous sends globally in validation, queued or running at once, and schedule queued work across accounts in turn.
An asynchronous attempt that waits in the queue for 15 minutes without a worker settles as a retryable pre-dispatch failure and releases its claims under section 6.
Persist the queue deadline so a restart preserves it for an already queued attempt.
An interrupted worker that requeues pre-dispatch work starts a new queue wait.
Return `503` with `Retry-After` when admission is full, before claiming any files.
A retry looking up an already accepted send uses no additional slot.
Keep DB transactions short and release connections before file decoding and network work.
Log the stages and outcomes by account, room, send and event ID, with timings, byte counts and classified errors.
Exclude credentials, encryption keys and media content from logs and status responses.

### 6. Advertise the effective limit and bound the staging backlog

Read the authenticated [`GET /_matrix/client/v1/media/config`](https://spec.matrix.org/v1.18/client-server-api/#get_matrixclientv1mediaconfig) per account.
Use one bounded refresh per account at a time, caching a successful result for one hour and backing off for one minute after a failed or unsupported lookup.
Invalidate it when the account's homeserver or identity changes, and refresh after an upstream `413`, which can also come from a proxy.
Serve cached values on account reads without blocking the account list on network I/O.
On a cache miss or expired value, schedule a refresh and use the local limit until a fresh value is available.

Expose an optional `media_upload_limit` object on account detail and list entries, with `max_bytes` and a nullable `homeserver_max_bytes`.
An absent object means the server predates the feature.
A null homeserver value means Axon applied its own cap because the upstream limit is unknown.
Otherwise, `max_bytes` is the lower of `media.max_upload_bytes` and `m.upload.size`.
Use the same effective value in staging and recheck the latest known cap before starting a job, since it can change after staging.
Give buffered consumers such as room and profile avatars independent claim ceilings with the current 100 MiB default at most, and preserve existing tighter preview and thumbnail limits.
An upstream `413` is a definite upload failure, before any event dispatch.

Set the final default `media.max_upload_bytes` to **1 GiB (1,073,741,824 bytes)** after the client transport prerequisites and the synchronous-client compatibility gate in Delivery and verification.
Keep the current 100 MiB default in the earlier server releases and preserve explicit operator settings.
Four staging writers can take up to 4 GiB at the final default.
The writer limit alone permits an arbitrary backlog of completed and claimed files.
Add a global `media.max_staged_upload_bytes` budget, defaulting to `max_concurrent_uploads × max_upload_bytes`, with checked arithmetic.
Count incomplete writes, staged files and accepted claims until their bytes leave disk, including normalized thumbnail files when persisted.
The budget is shared across accounts and paths, so a handful of large jobs can temporarily exhaust it for both asynchronous and synchronous clients.
Keep active claims through a progressing transfer, and apply section 5's deadline to attempts still waiting for a worker.
On confirmed asynchronous failure, cap the retained staged files' expiry at the earlier of their existing expiry and 15 minutes after failure.
Clean up any derived files that cannot be reused when settling the attempt.
The expiry sweeper reclaims those files on its next pass, within five minutes under normal operation, and explicit deletion can release them sooner.
An eligible retry after expiry can stage replacement files for the same logical send.
Reserve from the budget before writing, incrementally for unknown-length bodies.
If the budget is full, return `503` with code `service_unavailable` and the message `staged upload budget is full`.
The generated-thumbnail quota keeps its existing error message.
Make the staged file and directory entry durable before committing the upload row where the platform supports directory sync.
Rebuild accounting from durable rows and reconcile temporary and orphan files before accepting new reservations after boot.
Document how the staging budget competes for space with the separate media cache when they share a volume.

## Alternatives considered

- **Change the existing POST to always return `202`.** Existing clients need an event ID in the success body, so the asynchronous route is an additive operation under [ADR 0099](0099-openapi-compatibility-guard.md).
- **Select async with a flag in the existing request.** A server that predates the flag can ignore it and send synchronously, which makes a stale capability or a downgrade hard to distinguish from a failed send.
- **Retry every ambiguous event send with the Matrix transaction ID.** Homeserver deduplication helps while that ID is still in scope and retained. A durable dispatch marker and positive reconciliation also cover restarts and expired upstream deduplication.
- **Use the SDK's buffered uploader.** It requires a complete byte buffer. Reusing the SDK's streaming encryptor and event sender preserves Matrix behavior while memory use follows chunk size.

## Delivery and verification

1. **Design** ([1206](https://github.com/matrix-axon/matrix-axon/issues/1206)). This ADR.
2. **Server limit discovery** ([1210](https://github.com/matrix-axon/matrix-axon/issues/1210)). Ship the account field and enforce it in staging at the current default. Cover a lower local cap, a lower upstream cap, missing and stale data, and a rejected body with an inaccurate or absent length.
3. **SDK dispatch feasibility** ([1260](https://github.com/matrix-axon/matrix-axon/issues/1260)). Prove that event preparation exposes its encryption choice and that the final request waits for a successful marker write. Compare an upstream hook with a maintained pinned patch and record the patch owner if needed.
4. **Server async send and streaming** ([1185](https://github.com/matrix-axon/matrix-axon/issues/1185)). Build on ADR 0142's shared send path after the metadata and thumbnail server steps and the SDK feasibility spike.
   - Test immediate `400`s, partial-claim rollback, disconnect and process death during validation of an initial POST and a retry. Verify the next POST can reclaim the same transaction and that abandoned reservations free capacity.
   - Exercise concurrent duplicate POSTs, a lost `202` on any attempt, replacement staged IDs, and stale or conflicting retry selectors. Verify an unknown send cannot allocate another attempt even when `retry_of` names its current attempt.
   - Exercise encryption setup failure, process death during key preparation, process death immediately after committing the dispatch marker, and success with a lost Matrix response. Assert at most one event across crash and replay cases with upstream transaction deduplication disabled.
   - Recover after losing WebSocket updates and exercise both logout outcomes. Confirm a matching sync event can resolve the send during the bounded drain, and an unresolved send closes permanently and retires its persisted echo even if a new device later sees the event.
   - Test plaintext and encrypted uploads, decryption and hashes, all relation shapes, and preview and thumbnail failure. Enable encryption mid-upload and verify that the single dispatched event refers only to encrypted media, with a simulated SDK downgrade failing before dispatch. Measure peak memory with a 500 MB file and with the worker pool full, including a slow reader.
   - Verify ingress and egress stall cancellation, a progressing upload exceeding the former deadline, and a homeserver finalization delay that succeeds with a longer response timeout. Show that a response timeout is retryable and produces zero room-event PUTs.
   - Fill the staging budget with queued and failed jobs, restart, and verify that queue expiry, shorter retry retention and the sweeper restore capacity. Exercise the staging quota's own `503` message and the legacy response with existing clients.
5. **Shell upload transport** ([921](https://github.com/matrix-axon/matrix-axon/issues/921)) and **web upload progress and stall handling** ([422](https://github.com/matrix-axon/matrix-axon/issues/422)). Stream slices through the native bridge with backpressure and cancellation. Use browser upload progress on the staging leg, using XHR where necessary, and show the subsequent async send state from the server. Confirm bounded memory on Android and iOS before lifting their cap.
6. **Web adoption and limit rollout** ([1211](https://github.com/matrix-axon/matrix-axon/issues/1211)). Use the account limit and async capability where present and keep 100 MiB as the older-server fallback. Give blob previews and retained attachment batches independent budgets. Verify a 500 MB video on desktop, iPhone browser and packaged mobile shells, and playback through an [ADR 0139](0139-stream-large-video-and-audio.md) stream ticket.
   - Keep the 100 MiB server default until issue 1211 selects and verifies how synchronous-only clients are protected from accepting uploads they cannot send within their wall-clock deadline. Exercise older TUI and web clients against a server configured for 1 GiB and verify that new async clients can use the higher limit.
   - Raise the default in a server-only change after the compatibility gate and client transport work are complete. The TUI can honor a lower advertised cap and keeps its current ceiling and synchronous send contract until it has bounded upload transport and adopts the async capability.

Keep server, shell and client work in their respective silos.
Update OpenAPI and generated contracts with API changes, the server configuration reference and environment examples with new settings, and client parity and demo coverage where behavior changes.
