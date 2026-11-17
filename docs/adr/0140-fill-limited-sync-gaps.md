# ADR 0140 — Fill the holes that limited syncs leave in room history

**Status:** Proposed. Amends [ADR 0043](0043-history-backfill-engine.md) (backfill extends history only behind the oldest stored event); supersedes nothing.
This ADR is the plan only; implementation waits for its review.
Tracking issue: #1125.

## Context

Axon stores a room's timeline from two sources.
Live sliding sync delivers each room's newest events, at most `sync.timeline_limit` (default 20) per response ([ADR 0015](0015-event-store-schema.md)).
History backfill ([ADR 0043](0043-history-backfill-engine.md)) pages `/messages` backward from behind the oldest event Axon has, until the room's start.

When more than `timeline_limit` events reach a room between two sync responses, the homeserver returns the newest 20 with `limited: true` and a `prev_batch` token that points just before them.
Axon stores the 20 and never looks at `limited` or `prev_batch`.
Everything between those 20 and the last event Axon already had is never fetched.
Backfill cannot reach it, because it only works behind the oldest stored event, and in a room that was synced before the burst it has usually finished.
The missing events are absent from every Axon timeline read and from search, while other Matrix clients show them.

### What happened on production

A bulk Slack history import on 2026-10-18 sent tens of thousands of events into the shared rooms over about four hours (#1125).
Comparing every non-outlier Synapse event in `!XKbIrDlyIdACmPxPlB:bostoncoop.net` with Adam's `events` table:

| Measure                                                                | Value                           |
| ---------------------------------------------------------------------- | ------------------------------- |
| Synapse events in the room                                             | 29,559                          |
| Missing from Adam's Axon                                               | 8,740 (30%), in 470 holes       |
| Missing from Steve's Axon                                              | 9,344                           |
| Missing from Jamie's Axon                                              | 138                             |
| Runs Axon stored between two holes during the import that were 20 long | 464 (of ~470)                   |
| When the room's backfill had finished                                  | 2026-10-18 11:06, ~10 h earlier |

The runs of exactly 20 events are the tails of limited responses.
That Jamie's instance kept almost everything shows the loss depends on sync timing, not on the events themselves.
An import is only the most dramatic trigger.
A busy room while sync is slow, a reconnect after an outage, and a room coming back into the sliding-sync window after a quiet period can all produce a limited response.

### What exists already

- **The signal is already delivered.**
  matrix-sdk 0.19 passes each sliding-sync room's `limited` and `prev_batch` to `Client::subscribe_to_all_room_updates()` as `JoinedRoomUpdate.timeline` (`From<&v5::response::Room> for Timeline` in the vendored `matrix-sdk-base`).
  Axon already subscribes to that broadcast for push (`push::watch_room_updates`), but reads only the events.
  The broadcast holds 32 responses; a slow subscriber gets `RecvError::Lagged` and loses updates.
- **A backward pager with every guard Axon needs.**
  Backfill's `fetch_page` sends `/messages` without the SDK event cache (see the `backfill.rs` module docs), decrypts each event, and passes the homeserver load gate ([ADR 0122](0122-homeserver-load-gate.md)).
  `persist_backfilled_event` stores through the live ingestion path, so hot columns, redactions, aggregation, search and purge-on-leave all apply.
  `upsert_event` is `ON CONFLICT DO NOTHING`, so storing an event twice is harmless.
  [ADR 0121](0121-slow-homeserver-backfill-and-mutation-timeouts.md) classifies failed pages, backs them off persistently and parks rooms that keep failing.
- **Timeline reads order by `origin_ts`, then `id`** (`crates/axon-store/src/events.rs`), so an older event stored late still appears in its place.
- **`events.received_at`** is the time a row was first stored, from the database clock; `ON CONFLICT DO NOTHING` never changes it.
- **Backfill runs only when enabled.** `run_account` spawns `backfill::run` only when `sync.backfill_enabled` is true.

## Decision

### 1. Record every hole that a limited response leaves

A new per-account watcher subscribes to the room-update broadcast before the sync service starts, beside the push watcher.
For each joined room whose timeline is `limited`, it records a hole in a new table:

```sql
CREATE TABLE room_timeline_gaps (
    id                    BIGSERIAL   PRIMARY KEY,
    account_id            UUID        NOT NULL REFERENCES accounts ON DELETE CASCADE,
    room_id               TEXT        NOT NULL,
    origin                TEXT        NOT NULL
                          CHECK (origin IN ('limited', 'lag_probe', 'token_rejected')),
    from_token            TEXT,                  -- next page's `from`; NULL = the live end
    stored_before         TIMESTAMPTZ NOT NULL,  -- the stop boundary (decision 2)
    anchor_event_id       TEXT,                  -- oldest event of the limited batch, for logs
    detected_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    pages_fetched         INTEGER     NOT NULL DEFAULT 0,
    events_recovered      BIGINT      NOT NULL DEFAULT 0,
    consecutive_failures  INTEGER     NOT NULL DEFAULT 0,
    last_failure          TEXT,
    retry_at              TIMESTAMPTZ,
    parked                BOOLEAN     NOT NULL DEFAULT false,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX room_timeline_gaps_room_idx ON room_timeline_gaps (account_id, room_id);
```

`updated_at` gets the shared `trigger_set_updated_at()` trigger.
The surrogate key lets one room hold any number of holes and probes.
It also means replacing `from_token` after each page can never collide with another row.
The watcher only inserts rows, so it keeps up with the broadcast; the paging happens elsewhere (decision 2).
Recording is durable, so a restart, a crash or a supervised account restart never forgets a hole.

A limited response with a `prev_batch` gets an `origin = 'limited'` row.
Its `from_token` is that `prev_batch` and its `stored_before` is the insert time.
A limited response without a `prev_batch` gets a row that starts from the live end (`from_token` NULL), with the margin of decision 3.
`anchor_event_id` is only for logs and is NULL when the batch was empty.
No rule depends on it.
`sync.timeline_limit` is validated to be at least 1 at startup, so a limited response always carries events for the handler to store.
Events can still all be dropped when they fail to parse.

**The first window of a room is not a hole.**
The filler drops a row unpaged when the room has no event stored for this account outside the limited batch.
The test is on stored events, not on the `room_backfill` row.
With backfill on, such a room has no backfill cursor yet, so backfill's walk from the live end covers the range.
With backfill off, history from before Axon's first sync of a room is deliberately not fetched.
Any other row is paged, including one for a room whose backfill cursor is already behind the hole, since backfill never walks forward to it.

### 2. Fill each hole by paging back until history stored before it

Each row is paged backward with backfill's `fetch_page` and stored with `persist_backfilled_event`, one page (`sync.backfill_page_size`, default 100) at a time.
The first page starts at `from_token`, or at the live end when `from_token` is NULL.
Progress is saved after each page by setting `from_token` to the page's `end`, so a restart resumes where it stopped.

**The stop rule is the same for every row and does not change after the first page.**
Before a page is written, its event ids are looked up in `events`.
The row closes, and is deleted, after the first page that contains an event whose row was stored before the row's `stored_before`.
That page is written in full first, so the hole's events on it are kept, and the already-stored ones are no-ops.
A page with no `end` (the room's start) also closes the row.
Stored events newer than the boundary are walked through and do not stop the walk.
That covers the limited batch itself, events that live sync stored during a lag, and events another row filled after this one was detected.

Ordering never uses `origin_ts`.
The walk follows `/messages` stream order, and the boundary compares `events.received_at`, the time a row was first stored.
`ON CONFLICT DO NOTHING` never changes that time.
This matters for the case that prompted this ADR: imported events carry `origin_ts` values years older than the events Axon stored around them.

For an `origin = 'limited'` row, `stored_before` is its insert time.
The events just before the hole were stored by an earlier sync response.
The SDK finishes that response's timeline handlers before it processes the next response, and only then sends the next response's broadcast, which the watcher receives before inserting the row.
So those events always satisfy the boundary, and the walk stops at them.

Holes nest correctly.
Suppose two limited responses leave holes H1, then H2.
Filling H2 walks through anything stored after H2 was detected, including events another row filled inside H1, and stops at H1's limited batch, which was stored before H2 was detected.
H1 is filled from its own row, and events of H1 that H2's walk already stored do not stop H1 early, because they were stored after H1 was detected.

Failures follow ADR 0121: each failed page is classified, backed off with `retry_at`, and parked after repeated failure, with the state on the gap row.
A `400` for a saved token (one the homeserver no longer honours) restarts the row from the live end.
It sets `origin = 'token_rejected'` and `from_token` to NULL, and moves `stored_before` back by the margin of decision 3.
The row's limited batch may have been stored a moment after the row was inserted, and the margin keeps it from counting as history before the hole.
Because the stop rule is the same in both modes, the restart survives a crash after its first page.

**Scheduling.**
Gap rows are paged inside the backfill task, ahead of ordinary backfill pages, newest row first, because they cover recent history a user is likely to read.
They share the load gate, the disk-space guard and the throttle with backfill.
Only rows for joined rooms are paged; a row for a room the account has left waits, and is deleted if the room is purged (decision 6).
`run_account` spawns that task whatever `sync.backfill_enabled` says.
When the flag is off, the task pages gap rows and the verification pass (decision 4) and skips the `room_backfill` walk.
Gap rows ignore `sync.backfill_target_depth`.
They restore what live sync should have delivered, not extra history.

### 3. A lagged broadcast probes the rooms that were active

On `RecvError::Lagged`, the watcher cannot know which skipped responses were limited.
The SDK sends a response's broadcast before its timeline handlers run, and finishes them before taking the next response.
So any response that fell out of the 32-slot channel had already stored its events.
Rooms with a lost hole are therefore rooms that stored events while the broadcast was lagging.

The watcher notes when it processed each update, on its own monotonic clock.
On lag, it computes the boundary in the database's clock, so the two clocks are never compared directly:
`stored_before = db now() − (time since the last processed update) − margin`, with a margin of 60 s.
It then records an `origin = 'lag_probe'` row, starting from the live end, for every joined room with an event whose `received_at` is at or after that boundary.
Each probe walks back through everything stored during the lag, filling every hole in that window, however many there are.
It stops at the first page holding an event stored before the lag.
A probe that finds nothing missing costs one or two pages.
A boundary set too early only makes the walk longer, never wrong, which is why the margin errs that way.
The lag is logged at `WARN` with the number of rooms probed.

One case is not covered: a lagged limited response whose every event failed to store (for example, all failed to parse).
That room stored nothing during the lag, so it is not probed.
The management action of decision 7 re-verifies such a room.

### 4. Repair the holes that already exist with a one-time verification pass

Holes created before this change have no rows, and no client-side signal can locate them.
The only way to find them is to walk each room's history again.

A limited-sync hole can only lie in history that live sync stored.
Backfill pages are contiguous, and each starts where the last ended.
The oldest event live sync stored in a room is the room's lowest `events.id`, because every row before it was in the room's first window.
So the verification pass walks each room from the live end back to the page that contains that event, or to the room's start.
It stores every page, and already-stored events are no-ops.
It never walks into history that backfill stored or that backfill was configured not to fetch, so it neither extends history nor depends on `sync.backfill_enabled` or `sync.backfill_target_depth`.

Progress lives in its own table, `room_history_verify (account_id, room_id, from_token, stop_event_id, complete, events_recovered, …)` plus ADR 0121's failure columns, keyed by `(account_id, room_id)` with `ON DELETE CASCADE` from `accounts`.
It is not kept on `room_backfill`, because a room with stored events can have no `room_backfill` row: with backfill off, or before its first page is saved.
The migration seeds one row for every `(account_id, room_id)` that has stored events.
`stop_event_id` is the room's lowest-`id` event at that time.
Rooms first seen after the migration need no row, because their holes are recorded from the start.

The pass runs only when no gap rows are open and ordinary backfill is idle, through the same gate and guards.
It is persisted per page, and it is not repeated after it completes.
`sync.history_verify_enabled` (default `true`) turns it off.

On Adam's production account there are 2.47 M stored events across 15,388 rooms, so the pass is at most about 25,000 pages at the default page size.
It is fewer in practice, because history stored by backfill is not re-read.
Most rooms are small, and at backfill's pacing it finishes in hours, not days.

### 5. Clients learn that a filled hole changed history

A room's newest events are unaffected by a fill, but a client that already holds a slice of the room is now stale inside it.
A head refresh does not help, because it merges only the newest page.

- **A durable revision.**
  Each room gets a `history_rev` counter (on `room_summaries`).
  Every gap-fill or verification page that inserted at least one event increments it; ordinary backfill, which only extends behind the oldest event, does not.
  It is exposed on each room in `GET /v1/rooms`, and so in the room-list deltas of [ADR 0134](0134-incremental-room-list-reads.md).
- **A frame.**
  The server sends an additive WebSocket frame, `timeline.history_filled`, with payload `{room_id, oldest_ts, newest_ts, inserted, history_rev}`.
  Frames are coalesced per room to at most one per second.
  A coalesced frame carries the union of the pages' `[oldest_ts, newest_ts]` ranges, the sum of their `inserted` counts, and the latest `history_rev`, never just the last page.
- **The client contract.**
  On the frame, a client drops or refetches every cached slice of that room whose loaded `origin_ts` span overlaps the range, whether or not the room is on screen.
  Display order is `origin_ts`, so this is the right comparison for what a client holds.
  The `/v1/ws` bus is best-effort, so a frame can be missed.
  Each cached slice therefore records the `history_rev` it was loaded at.
  After a reconnect or a lagged socket, the client compares that with the room list's value and drops only slices whose revision moved.
  Dropping every cache on every reconnect would undo the warm-cache savings of [ADR 0085](0085-web-offline-first-cache.md).

How each client implements the contract is decided in that client's own PR.

### 6. Purge and teardown remove holes with the history

There are three deletion sites, and each is named here so the implementation follows all three:

- **Account deletion.** `room_timeline_gaps` and `room_history_verify` rows go by `ON DELETE CASCADE` when `delete_account_row` deletes the `accounts` row, the same as `room_backfill`.
- **Room purge.** `purge_room`'s closing statement deletes the room's gap and verification rows beside `del_backfill`, under the same `NOT preserve_room_after_purge($1, $2, $3)` guard.
  A purge that deleted no events still removes them, and holes recorded after a rejoin survive.
- **Not in `delete_event_batches`' `reset_backfill` CTE.**
  That CTE resets the backfill cursor so that deleted history can be fetched again.
  A gap row that survives it stays correct, because its walk needs an event stored before the hole was detected, and it walks further when those were deleted.

While a purge is pending, the room is not joined, so the filler leaves its rows alone.
`persist_backfilled_event`'s purge-on-leave skip still applies to any page in flight.
A room rejoined before the purge finishes keeps its rows (the preserve guard).
Events the filler then stores get `id`s above the purge watermark, so the purge does not remove them, as with backfill after a rejoin.

### 7. Logging, status and a developer switch

- Every log line carries `account_id` and `room_id`.
  Detecting a hole logs at `INFO` with `prev_batch` and `anchor_event_id`.
  Each page logs at `DEBUG` with its `inserted` and `already_stored` counts.
  Closing a hole logs at `INFO` with `pages_fetched` and `events_recovered`.
- `GET /v1/status` gains `history_gaps: {open, parked, events_recovered}` and `history_verify: {rooms_pending, rooms_complete, events_recovered}` beside the existing `backfill` block.
- The management API ([ADR 0109](0109-management-api.md)) gains an action that re-queues the verification pass for one room or a whole account, so an operator can re-verify after a suspected loss without SQL.
  A re-queued row takes the room's current lowest-`id` event as its stop point.
- A non-zero `events_recovered` from the verification pass is itself evidence of past loss; it is logged at `WARN` once per room with the count.

## Consequences

- An event that live sync skips is fetched within one backfill sweep of the response that skipped it, instead of never, including on servers that run with `sync.backfill_enabled = false`.
- The backfill task now runs on every account, so a server with backfill disabled makes `/messages` requests when holes appear and once for the verification pass.
- The production rooms affected by the 2026-10-18 import are repaired by the verification pass with no manual step.
- Each limited response costs at least one extra `/messages` page.
  On a quiet account that is rare.
  During an import it is one page per 100 missing events, which is the minimum needed to fetch them.
  A room that comes back into the sliding-sync window can be reported `limited` with nothing missing.
  That costs one page that closes on its first stored event.
  The implementation PR measures how often this happens on a large account before shipping, and logs it separately.
- The verification pass re-reads, once, the history of each room that live sync stored, a one-off load on the homeserver bounded by the load gate.
- Filled events get `events.id` values larger than newer events, as backfilled events already do.
  Code that treats `id` as arrival order ([ADR 0089](0089-read-receipt-target-resolution.md), #505) must already handle backfilled rows, and gap rows are no different.
  The implementation PR checks each `id`-ordered query against that.
- Events inside a hole still never trigger push notifications; filled events are stored as history, like backfill.
  A mention inside a hole is therefore still not notified.
  That is unchanged from today and is out of scope.

## Alternatives considered

- **Raise `timeline_limit`.**
  It shrinks holes but cannot remove them, since any limit can be outrun.
  It also makes every room-list response larger.
- **Use matrix-sdk's event cache, which already tracks gaps.**
  Axon deliberately keeps history out of the event cache, because doing so duplicated all of history on disk (see the `backfill.rs` module docs).
  The event cache's gaps are also in-memory structures, not a durable record Axon can resume from.
- **Patch the vendored `matrix-sdk-base` to persist `limited`.**
  Unnecessary: the public broadcast already carries the flag, and a third patch adds upgrade cost.
- **Detect holes from event continuity.**
  Client-format events do not carry `prev_events`, so Axon cannot tell from the events alone that something is missing between two of them.
- **Re-walk all history periodically instead of detecting holes.**
  It finds every hole but only after hours, at the cost of a full re-read each time.
  Detection fills a hole within a sweep, and the one-time pass handles what came before.

## Delivery

All server-side (`crates/`), in this order:

1. Gap table, the detecting watcher with its lag probe, the `timeline_limit` validation, the filler inside the backfill task, and that task's spawn whatever `backfill_enabled` says (decisions 1–3, 6, and the logging and status of 7).
   Verified by a Synapse integration test that sends more than `timeline_limit` events between two syncs and asserts every one is stored, with backfill both on and off.
   Unit tests cover nested holes, a token rejection restarted from the live end with a crash after its first page, and a lag window with two holes in it.
2. The verification pass, its table and seeding migration, its config switch, and the management action (decisions 4 and 7).
3. `history_rev`, the `timeline.history_filled` frame and their OpenAPI entries (decision 5), followed by separate web and TUI PRs to consume them.
