# ADR 0126 — Room detail acquisition, state coverage, and change notification

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
Matrix state is never deleted, only replaced, so a room that never had an `m.room.server_acl` event has no ACL.
Axon cannot say so today: a missing tuple means either "the room has none" or "Axon has not been told", and ADR 0111 forbids guessing between them.

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

### 1. The supplementary transport requests all eight metadata types

Extend the ADR 0124 transport's allowlist to every singleton state type `/metadata` reports: canonical alias, creation, join rules, encryption, power levels, server ACL, history visibility, and guest access.
Creation and power levels are already on the list; guest access and server ACL close the coverage gap; the remaining four are requested so that coverage (decision 2) is uniform across all eight snapshots.
The transport becomes a general supplementary-state transport rather than an image-pack transport: rename the module and use connection ID `axon-state`.
Its position is in memory only (ADR 0124), so the rename costs one fresh connection, which every process restart already starts.

Everything else in ADR 0124 is unchanged: no SDK sync processing of these responses, only allowlisted `required_state` reaches `room_state`, one request in flight, SDK retries disabled, the same deadlines and backoff, and the same timestamp, redaction, and departed-room ingestion rules.
The primary `SyncService` remains the sole owner of SDK room state.

This is preferred over repairing each room with `GET /rooms/{roomId}/state/{type}`.
Per-room repair is one upstream request per room per type, needs its own refresh schedule, and still misses later changes, whereas the transport delivers cold-start state in its existing paged range and later changes live.
The added content is a few small singletons per room on a connection that already carries power levels, which is usually the largest of them.

### 2. Coverage records what the homeserver has answered for

A room that appears in a transport response with `initial: true` carries the room's full current state for every requested type ([MSC4186](https://github.com/matrix-org/matrix-spec-proposals/pull/4186)).
After that room's batch is processed, Axon records a coverage row for each of the eight types, keyed by account, room, and type, with `covered_at` from the PostgreSQL clock.
A type is not covered from a batch in which an event of that type was malformed, failed to persist, or was skipped by the departed-room rule; a skipped or broken event must never read as absence.
Later non-initial responses keep covered state current through ordinary deltas, and a restart re-covers each room as its fresh connection reaches it.

`/metadata` gains a nullable `coverage` object on each snapshot, `{ "covered_at": <Unix ms> }`, or `null` when the type has not been covered.
The snapshot `status` enum is unchanged, so existing clients keep working.

| `status`  | `coverage` | Meaning                                                                         |
| --------- | ---------- | ------------------------------------------------------------------------------- |
| `unknown` | `null`     | Axon has not been told; as today, nothing can be inferred.                      |
| `unknown` | present    | The homeserver reported no such state as of `covered_at`: known absent.         |
| any other | either     | A tuple exists; coverage only says the transport has answered for it as well.   |

Known absent is a statement about the room's current state, not a manufactured value: the API still returns no content.
A client may apply a Matrix default only where the specification defines what the absent state means, such as no `m.room.encryption` meaning the room is not encrypted, or no `m.room.server_acl` meaning no servers are denied.
`covered_at` is the time of the last initial answer, not a freshness guarantee; it is not advanced by deltas, and the transport's health, like sync health for member counts, bounds how current an absence is.

Coverage rows are deleted on a local leave, in the same `room_summaries.hidden_left` transition the member-count trigger fences on, and on room purge and account removal.
A rejoin starts uncovered until its next initial answer.
Existing installations start with no coverage and fill as the transport's first range sweep reaches each room.

### 3. Homeserver details are a separate account-scoped cache

A new table holds one row per account, room, and detail kind (`local_aliases`, `directory_visibility`).
It is never merged into `room_state`, so homeserver-local aliases stay distinct from the advertised aliases in `m.room.canonical_alias`, and synced state stays authoritative for everything it covers.
Rows are deleted on account removal (foreign-key cascade), room purge, and local leave (the same transition as decision 2), so a removal is never served from cache.
The table is bounded by the account's room count times two; it needs no separate eviction.

`GET /v1/accounts/{account_id}/rooms/{room_id}/homeserver_details` returns the cached rows immediately and never waits for the homeserver.
Each detail separates the last conclusive answer from the state of attempts to refresh it, as ADR 0111 requires, so a failed refresh leaves usable stale data visible.

- **Value status:** `unknown` (never answered), `available`, `partial` (some aliases failed validation and were withheld), `too_large`, `forbidden` (403), `not_found` (404 `M_NOT_FOUND`), or `unsupported` (404/405 `M_UNRECOGNIZED`).
- **Value timing:** `fetched_at` and `expires_at`, both server clock; a past `expires_at` means stale.
- **Refresh:** `idle`, `queued`, or `running`, with the last failure's category (`timeout`, `server_error`, `rate_limited`, `transport`, `unauthorized`, `gated`) and `next_attempt_at`.

A transient failure updates only the refresh fields.
A conclusive answer replaces the value, including `forbidden` after a previous `available`, so lost permission removes the cached list.

The read schedules a refresh when a detail is unknown or expired and its `next_attempt_at` has passed; otherwise it is a pure read.
Repeated reads and client polling therefore cannot amplify upstream traffic beyond the expiration and backoff schedule.
Acquisition is limited to rooms the account has joined; any other room, including retained rooms after a leave and unknown room IDs, returns `unknown` with no request, so this endpoint cannot be used to probe arbitrary rooms through the homeserver.
Issue 622 decides whether discovered unjoined rooms are added.

### 4. One paced acquisition worker per account, behind the homeserver gate

Each account run owns one acquisition worker, canceled and joined with the run like the member-count and redaction workers.
The API reaches it through a consumer-owned port in `axon-api` with an `axon-server` adapter (ADR 0021), so `axon-api` does not depend on `axon-sync`.

- **Queue:** a bounded per-account queue keyed by (room, kind), which coalesces duplicates; a full queue drops the new request, leaves the row as it was, and lets a later read request it again.
- **Concurrency:** one request at a time per account, with a minimum interval between jobs, so one account's large room list cannot crowd out another account's work.
- **Gate:** every request takes a `HomeserverGate` permit as background work; waiting for a permit is not part of the request's timeout and is never recorded as a failure.
  Gate timeouts hold their slot (ADR 0122 decision 4), which is what keeps a slow peer from accumulating abandoned work.
- **Request:** sent with reqwest and the account's current access token rather than the SDK's `send`, because the SDK buffers and parses the whole body.
  The body is read through the bounded reader the Matrix OAuth path already uses, moved to a shared module, and refused past the byte limit before parsing.
  A 401 is recorded as `unauthorized` and retried on backoff; token refresh stays with the account's sync.
- **Validation:** aliases that do not parse as room aliases are withheld and reported as `partial`; a list longer than the entry limit is `too_large`.
- **Freshness guard:** a job records the database clock when it starts, and its completion commits only if the account still exists and no leave transition is newer than that start.
  A late result therefore cannot resurrect a removed account, a left room, or a deleted row.
  No lock is held across the request.
- **Restart:** values, expiration, and `next_attempt_at` persist; queue membership and `queued`/`running` are in memory only, so a crash cannot leave a permanent pending state, and backoff survives a restart.

In keeping with ADR 0121 decision 3, the limits are constants:

| Constant                | Value       | Why                                                                                        |
| ----------------------- | ----------- | ------------------------------------------------------------------------------------------ |
| `QUEUE_CAPACITY`        | 32          | Matches the member-count and redaction hint queues; a panel needs two jobs per room        |
| `JOB_INTERVAL`          | 250 ms      | Paces a burst of panel opens without delaying a single one noticeably                      |
| `REQUEST_TIMEOUT`       | 10 s        | Both responses are tiny; a slower answer is load, and ADR 0122 counts under 5 s as fast    |
| `MAX_RESPONSE_BYTES`    | 64 KiB      | Far above any plausible answer, small enough to buffer per account                         |
| `MAX_LOCAL_ALIASES`     | 256         | A homeserver-local list beyond this is pathological; withheld as `too_large`, not cut      |
| `AVAILABLE_TTL`         | 15 min      | Alias and directory changes are rare and usually made from the same client that reads them |
| `CONCLUSIVE_DENIAL_TTL` | 1 h         | `forbidden` and `not_found` change only with permissions or publication                    |
| `UNSUPPORTED_TTL`       | 24 h        | A homeserver gains an endpoint only on upgrade                                             |
| `FAILURE_BACKOFF`       | 1 to 30 min | Doubles per consecutive failure, at least any `Retry-After`; matches ADR 0122's cooldown   |

### 5. `room_details.changed` replaces panel polling

A new live frame, `room_details.changed`, carries `account_id`, `room_id`, and `sources`, a set naming what to re-read:

- `metadata`: a projection row for one of the eight types was inserted, replaced, or changed its redaction evidence, from either sync connection or from redaction repair; or a type's coverage was first established or removed.
- `member_counts`: the member-count worker changed the persisted pair or cleared it.
- `homeserver_details`: a detail's value or status changed.

The frame is a signal to re-read the named endpoint, not a payload; clients never apply a diff from it.
The store's upserts report whether they changed a row, so identical rewrites, a restart's re-coverage that only advances `covered_at`, and refreshes that confirm the same value publish nothing.
Each room's processing step publishes at most one frame, with the union of its sources, so a sync batch touching several types of a room sends one frame.
The first sweep after an upgrade publishes about one frame per room as guest-access and server-ACL tuples and coverage arrive, paced by the transport's range growth; this matches the display-name fill in ADR 0118.

With this frame, a client keeps a panel fresh by re-reading on the frame, on reconnect, and on reopen, and drops its polling.
Bus lag is handled as for every other frame: a lagged client re-reads on its reconnect.

### 6. Diagnostics

The acquisition worker logs under `axon_sync::acquisition` with `account_id`, `room_id`, `kind`, `outcome`, `elapsed_ms`, and queue depth, at debug for each job and warn for repeated failures.
The supplementary transport adds per-batch counts of covered types and skipped types to its existing debug line.
Neither logs alias values, response bodies, or tokens; failure categories are allowlisted, as for the member-count worker.
Existing tracing controls suffice; no developer option is added.

## Alternatives considered

- **Per-room `GET /state/{type}` repair for guest access and server ACL.** Rejected in decision 1: request count grows with rooms and types, and changes after the repair are missed.
- **A new `absent` value in the snapshot `status` enum.** Simpler for clients to read, but changes an enum existing clients switch over; a separate field is additive.
- **Coverage only for the four types the transport already requested.** Leaves four snapshots without an absence signal and gives clients two rules; requesting four more small singletons is cheaper than that inconsistency.
- **Fetch homeserver details for every joined room in the background.** Keeps panels warm but spends upstream requests on rooms nobody opens.
- **Acquire through the SDK's `send`.** Handles token refresh, but cannot bound the response before the SDK buffers and parses it.
- **A separate `POST …/refresh` endpoint.** An explicit trigger costs clients a second round trip; expiration and backoff already make refresh-on-read safe to repeat.
- **One frame per source.** Several frames for one room change multiply re-reads for no gain.

## Consequences

- Guest access and server ACL become available for every joined room after the transport's first sweep, and all eight snapshots can report known absence.
- Each account's supplementary connection carries four more state types; the transport's existing bounds still apply.
- Homeserver-local aliases and directory publication are visible within one request of a panel opening, and stay visible when stale, with their staleness and failures explicit.
- A slow or failing homeserver delays acquisition through the gate rather than accumulating work, and other accounts' homeservers are unaffected.
- Clients can drop 30-second polling of `/metadata` and `/info` once they consume `room_details.changed`; issue 657 and issue 623 can rely on it.
- `room_state` gains a second writer for six more types; it already accepts transport writes for creation and power levels under the same ordering rules.
- New schema: a coverage table, the homeserver-details table, and a leave-transition cleanup; all are forward-only migrations with no backfill.

## Delivery

Each step is one server-silo PR, with OpenAPI, the regenerated web schema, `docs/room-metadata.md`, and `docs/client-parity.md` updated in the same PR:

1. **Supplementary state, coverage, and `room_details.changed`:** decisions 1, 2, and 5, including the `member_counts` source.
2. **Homeserver details:** decisions 3, 4, and 6. Starts after PR 836 (ADR 0122) merges, so its requests use the real gate.

## Verification

- **Transport:** request shape lists all eight types; an initial batch records coverage; a malformed, failed, or departed-room-skipped event leaves its type uncovered; deltas keep state current without moving `covered_at`; leave, purge, and account removal delete coverage; restart re-covers.
- **Frames:** each source fires on a real change and stays silent on identical rewrites and re-coverage; one frame per room batch.
- **Acquisition, under paused tokio time against a mock homeserver:** a hung peer holds one account's worker without affecting another account; a full queue drops without blocking the API; concurrent reads produce one request; each status and its expiration; `Retry-After`; an oversized body is refused before parsing; invalid aliases give `partial`; a leave or account removal during a request discards its result; values and backoff survive a restart while `queued` does not; unjoined and unknown rooms make no request.
- **Live, on a disposable Synapse stack:** add and remove a local alias and directory publication and observe the frame and the re-read; set and replace server ACL and guest access; confirm a room without them reads as known absent.
