# ADR 0126 — Room detail acquisition, confirmed state absence, and change notification

**Status:** Proposed; tracked in [issue 621](https://github.com/matrix-axon/matrix-axon/issues/621), part of [issue 618](https://github.com/matrix-axon/matrix-axon/issues/618).
Records the concrete acquisition policy that [ADR 0111](0111-cached-progressive-room-metadata.md) deferred to the implementation, and amends [ADR 0124](0124-isolated-image-pack-sync.md)'s transport scope.

## Context

ADR 0111 sets the architecture for cached room metadata: typed local reads, bounded acquisition for what sync does not supply, and explicit availability.
Issues 619 and 620 shipped the typed `/metadata` read and authoritative member counts.
Three gaps remain, and each has a different source.

**Two metadata types never arrive through sync.**
matrix-sdk-ui 0.19.1's `SyncService` requests canonical-alias, creation, join-rule, encryption, power-level, and history-visibility state, but not `m.room.guest_access` or `m.room.server_acl`.
Those two `/metadata` snapshots stay `unknown` unless an unrelated timeline event happens to carry them.
The primary service exposes no supported way to add required state (ADR 0124's context).

**`unknown` cannot become "absent".**
A room that never had an `m.room.server_acl` event has no ACL, but Axon cannot say so: a missing tuple means either "the room has none" or "Axon has not been told", and ADR 0111 forbids guessing between them.
Sync cannot settle it.
A Sliding Sync response gives no completeness guarantee for `required_state` (ADR 0124's consequences), and a server or proxy can cap or ignore entries without saying so.
matrix-sdk 0.19.1 reaches the same conclusion for encryption: `Room::request_encryption_state` does not trust sync's silence, and asks the room-state endpoint, treating only `404 M_NOT_FOUND` as absent.
A false "absent" for encryption is security-relevant, because a client would present an encrypted room as unencrypted.

**Two values exist only on the account's homeserver.**
The aliases the homeserver maintains for a room ([`GET /rooms/{roomId}/aliases`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3roomsroomidaliases)) and whether it publishes the room in its directory ([`GET /directory/list/room/{roomId}`](https://spec.matrix.org/v1.19/client-server-api/#get_matrixclientv3directorylistroomroomid)) are not room state.
They need a remote request, a cache, and a policy for permission failures and unsupported servers.

**Clients poll because nothing tells them what changed.**
Required-state-only updates are persisted without a live frame, so the TUI's `/whereami` and the planned web panel re-read `/metadata` and `/info` every 30 seconds (ADR 0114, issue 657).
Remote acquisition makes this worse: a panel that opens before a fetch completes has no way to learn that it did.

Two existing mechanisms bear directly on the choices below.
ADR 0124's isolated state-only Sliding Sync transport already covers every non-invite room, pages through them 100 at a time, and already requests creation and power-level state for each.
ADR 0122's `HomeserverGate` (PR 836, not yet merged) is the one place background requests to a homeserver are admitted and backed off.

## Decision

### 1. The supplementary transport also requests guest access and server ACL

Add `m.room.guest_access` and `m.room.server_acl` (state key `""`) to the ADR 0124 transport's allowlist.
The transport becomes a general supplementary-state transport rather than an image-pack transport: rename the module and use connection ID `axon-state`.
Its position is in memory only (ADR 0124), so the rename costs one fresh connection, which every process restart already starts.

Everything else in ADR 0124 is unchanged: no SDK sync processing of these responses, only allowlisted `required_state` reaches `room_state`, one request in flight, SDK retries disabled, the same deadlines and backoff, and the same timestamp, redaction, and departed-room ingestion rules.
The primary `SyncService` remains the sole owner of SDK room state.

This is preferred over repairing each room with `GET /rooms/{roomId}/state/{type}`.
Per-room repair is one upstream request per room per type, needs its own refresh schedule, and still misses later changes, whereas the transport delivers cold-start state in its existing paged range and later changes live.
The added content is two small singletons per room on a connection that already carries power levels.

What the transport delivers is positive evidence only.
A type it does not deliver remains `unknown`; the transport never establishes absence (decision 2).

### 2. Known absence is confirmed by the room-state endpoint

A missing tuple becomes known absent only when the account's homeserver answers `GET /rooms/{roomId}/state/{type}/` with `404 M_NOT_FOUND`.
For a joined member that response is the specification's statement that the room has no such state, unlike sync's silence.

- **When:** a `/metadata` read schedules confirmation for the types that have no tuple and are due, under the same rule as decision 3: a type is due when it has no acquisition row, its confirmation has expired, or its `next_attempt_at` has passed.
  The read itself returns immediately.
  A type with a tuple is never confirmed: its status comes from the tuple.
- **Where:** a room's due types form one `absence` job on the account's worker (decision 4), not one job per type, so they share one queue slot and the worker's gate permits, pacing, and freshness guard.
  The job confirms types in a fixed order with encryption first, because a false "unencrypted" is the costliest mistake, then server ACL, join rules, history visibility, guest access, power levels, canonical alias, and creation.
  It publishes one frame when it finishes (decision 5).
- **Rows:** each type keeps its own acquisition row (decision 3) with the same refresh fields as the homeserver details, so a row can exist without a confirmation, for example after a `403` or while failures back off.
- **Outcomes:**
  - `404 M_NOT_FOUND` records the confirmation, with `confirmed_at` from the PostgreSQL clock.
  - `200` means the state exists but has not reached `room_state`; no confirmation is recorded, the body is not read or stored, and Axon logs a warning naming the account, room, and type as a sync coverage gap.
    The endpoint's content-only response lacks the event ID, sender, and timestamp a `room_state` row needs, and ADR 0111 keeps acquired data out of synced state.
  - `403` records no confirmation, because the account may no longer be joined; the row's value becomes `forbidden` with the conclusive-denial expiration, so the type is not due again on the next read.
  - Every other outcome is a failure: it updates only the refresh fields and takes the failure backoff.
- **Lifetime:** a confirmation stays valid until a tuple of that type arrives, the account leaves the room, the room is purged, or the account is removed, which delete it.
  It is also re-confirmed on read after `ABSENCE_TTL`, which bounds the effect of a state event sync failed to deliver.
- **Cost:** only absent types are confirmed, once per room per `ABSENCE_TTL`; a typical room lacks two or three of the eight.

`/metadata` gains two nullable fields on each snapshot: `absence_confirmed_at` (Unix ms) and `absence_check`, which is decision 3's refresh object (`idle`, `queued`, or `running`, the last failure's category, and `next_attempt_at`).
Both are `null` for a type with a tuple, and `absence_check` is also `null` when the type has never been attempted, so a client can tell "no confirmation yet, but one is coming" (`queued` or `running`) from "confirmation failed or was denied" (`idle` with a failure or a future `next_attempt_at`).
The snapshot `status` enum is unchanged, so existing clients keep working.

| `status`  | `absence_confirmed_at` | Meaning                                                                       |
| --------- | ---------------------- | ----------------------------------------------------------------------------- |
| `unknown` | `null`                 | Axon has not been told; as today, nothing can be inferred.                    |
| `unknown` | present                | The homeserver confirmed the room has no such state as of that time.          |
| any other | `null`                 | A tuple exists; its status describes it, and no confirmation is ever shown.   |

Known absent is a statement about the room's current state, not a manufactured value: the API still returns no content.
A client may apply a Matrix default only where the specification defines what the absent state means, such as no `m.room.encryption` meaning the room is not encrypted, or no `m.room.server_acl` meaning no servers are denied.

`/metadata` keeps its contract of no upstream request on the read path; scheduling a job is local, and the route never waits for it.

### 3. Homeserver details are a separate account-scoped cache

One acquisition table holds one row per account, room, and kind: `local_aliases`, `directory_visibility`, and the absence confirmations of decision 2, one kind per state type.
It is never merged into `room_state`, so homeserver-local aliases stay distinct from the advertised aliases in `m.room.canonical_alias`, and synced state stays authoritative for everything it covers.
Rows are deleted on account removal (foreign-key cascade), room purge, and local leave (the `room_summaries.hidden_left` transition), so a removal is never served from cache.
The table is bounded by the account's room count times ten kinds; it needs no separate eviction.

`GET /v1/accounts/{account_id}/rooms/{room_id}/homeserver_details` returns the cached `local_aliases` and `directory_visibility` rows immediately and never waits for the homeserver.
Each detail separates the last conclusive answer from the state of attempts to refresh it, as ADR 0111 requires, so a failed refresh leaves usable stale data visible.

- **Value status:** `unknown` (never answered), `available`, `partial` (some aliases failed validation and were withheld), `too_large`, `forbidden` (403), `not_found` (404 `M_NOT_FOUND`), or `unsupported` (404/405 `M_UNRECOGNIZED`).
- **Value timing:** `fetched_at` and `expires_at`, both server clock; a past `expires_at` means stale.
- **Refresh:** `idle`, `queued`, or `running`, with the last failure's category (`timeout`, `server_error`, `rate_limited`, `transport`, `unauthorized`) and `next_attempt_at`.
  A job waiting for a gate permit is `queued`; the gate's own state is in `GET /v1/status` (ADR 0122 decision 6), and waiting is never a failure.

A transient failure updates only the refresh fields.
A conclusive answer replaces the value, including `forbidden` after a previous `available`, so lost permission removes the cached list.

The read schedules a refresh when a detail is unknown or expired and its `next_attempt_at` has passed; otherwise it is a pure read.
Repeated reads and client polling therefore cannot amplify upstream traffic beyond the expiration and backoff schedule.
Acquisition, including absence confirmation, is limited to rooms the account has joined; any other room, including retained rooms after a leave and unknown room IDs, returns `unknown` with no request, so neither endpoint can be used to probe arbitrary rooms through the homeserver.
Issue 622 decides whether discovered unjoined rooms are added.

### 4. One paced acquisition worker per account, behind the homeserver gate

Each account run owns one acquisition worker, canceled and joined with the run like the member-count and redaction workers.
The API reaches it through a consumer-owned port in `axon-api` with an `axon-server` adapter (ADR 0021), so `axon-api` does not depend on `axon-sync`.

- **Queue:** a bounded per-account queue keyed by (room, kind), which coalesces duplicates; the kinds are `local_aliases`, `directory_visibility`, and `absence`, so a panel needs at most three entries.
  When it is full, the oldest queued job is dropped to admit the new one, because the newest request is the panel a user is looking at now.
  A dropped job leaves its row unchanged and no backoff, so the next read of that room requests it again; nothing can be starved while it is still being read.
- **Concurrency:** one request at a time per account, with a minimum interval between jobs, so one account's large room list cannot crowd out another account's work.
- **Gate:** every request takes a `HomeserverGate` permit as background work; waiting for a permit is not part of the request's timeout and is never recorded as a failure.
  Gate timeouts hold their slot (ADR 0122 decision 4), which is what keeps a slow peer from accumulating abandoned work.
- **Request:** sent with reqwest and the account's current access token rather than the SDK's `send`, because the SDK buffers and parses the whole body.
  The body is read through the bounded reader the Matrix OAuth path already uses, moved to a shared module, and refused past the byte limit before parsing; a successful absence-confirmation response is never read.
  A 401 is recorded as `unauthorized` and retried on backoff; token refresh stays with the account's sync.
- **Validation:** aliases that do not parse as room aliases are withheld and reported as `partial`; a list longer than the entry limit is `too_large`.
- **Freshness guard:** `room_summaries` gains a `membership_epoch` counter that its leave and rejoin transitions increment, in the same trigger that fences member counts.
  A job records the epoch when it starts, and its completion commits only if the account still exists and the epoch is unchanged.
  A leave during the request therefore rejects the result even if a rejoin follows before it completes, so neither confirmations nor details survive into a new membership.
  An absence confirmation also commits only if no tuple of its type exists at commit time, so state that sync delivers during the request wins.
  No lock is held across the request.
- **Restart:** values, expiration, and `next_attempt_at` persist; queue membership and `queued`/`running` are in memory only, so a crash cannot leave a permanent pending state, and backoff survives a restart.

A change made elsewhere to a room's local aliases or directory publication while it is cached is not observed until the entry expires, so `AVAILABLE_TTL` is the staleness bound for those two details.
If Axon later adds alias or directory writes, they invalidate the entry.

A room's jobs drain at `JOB_INTERVAL` per request, so a room with all eight types absent takes about 2.5 seconds to fill completely, and a typical room with two or three absent types about one second; this is accepted because the panel opens immediately from local data and each job's frame updates it.
Opening more than about ten rooms in quick succession evicts the first rooms' entries; they are requested again if those panels are read again.

In keeping with ADR 0121 decision 3, the limits are constants:

| Constant                | Value       | Why                                                                                        |
| ----------------------- | ----------- | ------------------------------------------------------------------------------------------ |
| `QUEUE_CAPACITY`        | 32          | Matches the member-count and redaction hint queues; holds about ten rooms' panels          |
| `JOB_INTERVAL`          | 250 ms      | Paces a burst of panel opens without delaying a single one noticeably                      |
| `REQUEST_TIMEOUT`       | 10 s        | These responses are tiny; a slower answer is load, and ADR 0122 counts under 5 s as fast   |
| `MAX_RESPONSE_BYTES`    | 64 KiB      | Far above any plausible answer, small enough to buffer per account                         |
| `MAX_LOCAL_ALIASES`     | 256         | A homeserver-local list beyond this is pathological; withheld as `too_large`, not cut      |
| `AVAILABLE_TTL`         | 15 min      | Alias and directory changes are rare and usually made from the same client that reads them |
| `CONCLUSIVE_DENIAL_TTL` | 1 h         | `forbidden` and `not_found` change only with permissions or publication                    |
| `ABSENCE_TTL`           | 24 h        | Sync delivers a later state event live; re-confirmation only bounds a delivery failure     |
| `UNSUPPORTED_TTL`       | 24 h        | A homeserver gains an endpoint only on upgrade                                             |
| `FAILURE_BACKOFF`       | 1 to 30 min | Doubles per consecutive failure, at least any `Retry-After`; matches ADR 0122's cooldown   |

### 5. `room_details.changed` replaces panel polling

A new live frame, `room_details.changed`, carries `account_id`, `room_id`, and `sources`, a set naming what to re-read:

- `metadata`: a projection row for one of the eight types was inserted, replaced, or changed its redaction evidence, from either sync connection or from redaction repair; or an absence confirmation was recorded or removed.
- `member_counts`: the member-count worker changed the persisted pair or cleared it.
- `homeserver_details`: a detail's value or status changed.

The frame is a signal to re-read the named endpoint, not a payload; clients never apply a diff from it.
The store's upserts report whether they changed a row, so identical rewrites, re-confirmations that only advance `confirmed_at`, and refreshes that confirm the same value publish nothing.
Each room's processing step publishes at most one frame, with the union of its sources, so a sync batch touching several types of a room sends one frame.
The first sweep after an upgrade publishes about one frame per room that has guest-access or server-ACL state, paced by the transport's range growth; this matches the display-name fill in ADR 0118.

With this frame, a client keeps a panel fresh by re-reading on the frame, on reconnect, and on reopen, and drops its polling.
Bus lag is handled as for every other frame: a lagged client re-reads on its reconnect.

### 6. Diagnostics

The acquisition worker logs under `axon_sync::acquisition` with `account_id`, `room_id`, `kind`, `outcome`, `elapsed_ms`, and queue depth, at debug for each job and warn for repeated failures and for a `200` absence-confirmation response (a sync coverage gap).
The supplementary transport's existing debug line gains per-batch counts of guest-access and server-ACL events.
Neither logs alias values, state content, response bodies, or tokens; failure categories are allowlisted, as for the member-count worker.
Existing tracing controls suffice; no developer option is added.

## Alternatives considered

- **Per-room `GET /state/{type}` repair for guest access and server ACL.** Rejected in decision 1: request count grows with rooms and types, and changes after the repair are missed.
- **Infer absence from a transport response with `initial: true`.** Free, and complete on Synapse in practice, but Sliding Sync does not guarantee complete `required_state`, so a capped or filtering server would produce a false "absent", including "unencrypted".
- **Confirm only security-relevant types and infer the rest from sync.** Fewer requests, but two rules for one field, and the cost saved is a few requests per opened room.
- **A new `absent` value in the snapshot `status` enum.** Simpler for clients to read, but changes an enum existing clients switch over; a separate field is additive.
- **Store the state-endpoint response in `room_state` when it returns `200`.** It lacks the event provenance a row needs and would make acquisition a writer of synced state.
- **Fetch homeserver details for every joined room in the background.** Keeps panels warm but spends upstream requests on rooms nobody opens.
- **Acquire through the SDK's `send`.** Handles token refresh, but cannot bound the response before the SDK buffers and parses it.
- **Drop new requests when the queue is full.** A client reading many rooms would keep re-requesting the earliest while later rooms never get in.
- **Fence late results with the leave time.** A clock comparison needs its own leave watermark, and the member-count watermark also advances on ordinary observations; an epoch has neither problem.
- **A separate `POST …/refresh` endpoint.** An explicit trigger costs clients a second round trip; expiration and backoff already make refresh-on-read safe to repeat.
- **One frame per source.** Several frames for one room change multiply re-reads for no gain.

## Consequences

- Guest access and server ACL become available for every joined room after the transport's first sweep.
- Known absence costs a few upstream requests the first time a room's metadata is opened, then one per absent type per day while it is read; it rests on the homeserver's authoritative answer, not on sync's silence.
- Each account's supplementary connection carries two more singleton state types; the transport's existing bounds still apply.
- Homeserver-local aliases and directory publication are visible within one request of a panel opening, and stay visible when stale, with their staleness and failures explicit.
- A slow or failing homeserver delays acquisition through the gate rather than accumulating work, and other accounts' homeservers are unaffected.
- Clients can drop 30-second polling of `/metadata` and `/info` once they consume `room_details.changed`; issue 657 and issue 623 can rely on it.
- New schema: the acquisition table, `membership_epoch`, and the leave-transition cleanup; all are forward-only migrations with no backfill.

## Delivery

Each step is one server-silo PR, with OpenAPI, the regenerated web schema, `docs/room-metadata.md`, and `docs/client-parity.md` updated in the same PR:

1. **Supplementary state and `room_details.changed`:** decisions 1 and 5, with the `metadata` and `member_counts` sources.
2. **Acquisition:** decisions 2, 3, 4, and 6, the `homeserver_details` source, and `absence_confirmed_at`. Starts after PR 836 (ADR 0122) merges, so its requests use the real gate.

## Verification

- **Transport:** request shape lists both new types; their events reach `room_state` under the existing ingestion rules; a type the response omits stays `unknown` with no confirmation.
- **Frames:** each source fires on a real change and stays silent on identical rewrites and re-confirmations; one frame per room batch.
- **Absence confirmation, against a mock homeserver:** `404 M_NOT_FOUND` confirms; `200` confirms nothing, stores nothing, and warns; `403` confirms nothing, records `forbidden`, and schedules nothing on the next read; a failing type backs off and repeated reads schedule nothing until `next_attempt_at`; `absence_check` reports `queued`, `running`, and `idle` with the failure; one read of a room with several absent types queues one `absence` job in the documented order and publishes one frame; a tuple arriving during the request wins; a later tuple deletes the confirmation; re-confirmation after `ABSENCE_TTL`; types with a tuple are never requested.
- **Acquisition, under paused tokio time against a mock homeserver:** a hung peer holds one account's worker without affecting another account; a full queue drops its oldest job and still admits the newest; concurrent reads produce one request; each status and its expiration; `Retry-After`; an oversized body is refused before parsing; invalid aliases give `partial`; values and backoff survive a restart while `queued` does not; unjoined and unknown rooms make no request.
- **Membership races:** a job that starts before a leave and completes after it commits nothing, including when a rejoin happens before it completes, and the rejoined room starts with no confirmation or details; account removal during a request likewise commits nothing.
- **Live, on a disposable Synapse stack:** add and remove a local alias and directory publication and observe the frame and the re-read; set and replace server ACL and guest access; confirm a room without them reads as known absent only after the confirmation request.
