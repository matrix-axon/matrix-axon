# ADR 0140 — Fill the holes that limited syncs leave in room history

**Status:** Accepted. Amends [ADR 0043](0043-history-backfill-engine.md) (backfill extends history only behind the oldest stored event); supersedes nothing.
Delivery step 1 is implemented; steps 2 and 3 are pending.
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
  Step 1 changed its default from `now()`, the transaction's start, to `clock_timestamp()`, read when the row is inserted.
  `upsert_event` takes the room's write lock after its transaction starts, so with `now()` an event that waited on the lock was stamped earlier than events committed before it in the same room.
  Now, within a room, `received_at` follows commit order.
- **Backfill runs only when enabled.** `run_account` spawns `backfill::run` only when `sync.backfill_enabled` is true.

## Decision

### 1. Record every hole that a limited response leaves

A new per-account watcher subscribes to the room-update broadcast before the sync service starts, beside the push watcher.
For each joined room whose timeline is `limited`, it records a hole in a new table.
It is not a task of its own: the live ingestion handler drains it before storing an event (see "Recording is durable" below).
The table:

```sql
CREATE TABLE room_timeline_gaps (
    id                    BIGSERIAL   PRIMARY KEY,
    account_id            UUID        NOT NULL REFERENCES accounts ON DELETE CASCADE,
    room_id               TEXT        NOT NULL,
    origin                TEXT        NOT NULL
                          CHECK (origin IN ('limited', 'lag_probe', 'token_rejected')),
    from_token            TEXT,                  -- next page's `from`; NULL = the live end
    stored_before         TIMESTAMPTZ,           -- the stop boundary (decision 2); NULL until resolved
    batch_event_ids       TEXT[]      NOT NULL DEFAULT '{}',  -- the limited batch (decision 1)
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
The implementation adds two columns to this sketch.
`batch_checked` records that the first-window test below has run and found a hole, so it is not evaluated again on every sweep.
`stop_event_ids` holds a lag probe's first-window stop (decision 3), taken when the probe is recorded.
The surrogate key lets one room hold any number of holes and probes.
It also means replacing `from_token` after each page can never collide with another row.
The watcher only inserts rows, so it keeps up with the broadcast; the paging happens elsewhere (decision 2).

**Recording is durable before the SDK moves past the response.**
The SDK sends a response's broadcast before it runs that response's handlers, awaits the handlers, and only then saves its sliding-sync position.
So the live ingestion handler drains the watcher before it stores an event, and the response's rows commit before any of its events, and before a restart could resume past it.
A row recorded by a separate task instead could still be pending when the position was saved, and a restart would forget the hole (found in review of step 1).
An insert the database refuses is retried for up to 30 seconds while the handler waits, which outlasts several of the default 3-second lock deadlines.
If it is still refused, it is kept in memory exactly as the response described it (room, token and batch) and retried until it commits; a restart in that window is the one way a hole is lost, and it is logged at `ERROR`.
Later holes then queue behind it at once, so an outage stalls sync for one budget, not one per room.
It does not fall back to a lag probe, which could run before the batch is stored, find nothing to probe, and drop the hole.
A small task drains the watcher between live events too, so responses without timeline events do not fill the broadcast into a lag.

A limited response with a `prev_batch` gets an `origin = 'limited'` row whose `from_token` is that `prev_batch`.
A limited response without a `prev_batch` gets a row that starts from the live end (`from_token` NULL).
Either row's `stored_before` stays NULL until the filler picks it up, and is then set to the earliest `received_at` among the batch's stored events.
(This plan first took the insert time for a row with a `prev_batch`; the batch's time is equally valid, and does not depend on when a retried insert ran.)
`batch_event_ids` holds the ids of the limited batch's events, at most `timeline_limit` of them; the first-window test below uses it.
`sync.timeline_limit` is validated to be at least 1 at startup, so a limited response always carries events for the handler to store.
Events can still all be dropped when they fail to parse.

**The first window of a room is not a hole.**
A room's first sliding-sync response is usually `limited`.
Paging that row would fetch the room's whole history.
That is backfill's job, and `sync.backfill_enabled = false` and `sync.backfill_target_depth` exist to limit it.
So such a row is dropped unpaged.

A row is a first window when nothing was stored in the room before its batch:

> the room has no stored event outside `batch_event_ids` whose `received_at` is earlier than the earliest `received_at` among the batch's stored events.

Only an earlier sync response, or backfill, can have stored such an event.
Events that live sync stores after the batch fail the comparison, however long the row waits.
So the test gives the same answer whenever it is evaluated.

The simpler "the room has no stored events" when the row is inserted would not do.
Backfill can page a newly joined room before its first response's handlers run, and a row retried from memory is inserted after its batch, or a later response's events, are stored.
Either would make a first window look like a hole and page the whole room.

**When a row is picked up.**
There is no fixed wait.
A row is picked up as soon as at least one of its batch's events is stored, which is normally moments after it is inserted, because the response's own handlers store the batch.
The first stored batch event fixes the batch's earliest `received_at`, so the test's answer is settled from then on.
Until one is stored, the row is skipped and re-checked on each sweep.
It is not dropped on its first check, so slow handlers under database pressure delay it rather than lose it.
A row whose batch still has no stored event 10 minutes after `detected_at` has a batch that will never be stored (all of it failed to parse).
It is dropped and logged at `WARN` with the room and `from_token`.
Such a row is either a first window or a hole whose newest events are unreadable, and paging it could walk the whole room.
The management re-verify action of decision 7 is the intended recovery for the second case.
`history_gaps.awaiting_batch` in `/v1/status` counts rows still waiting.
Every other row is paged, whatever the backfill settings, including one for a room whose backfill cursor is already behind the hole, since backfill never walks forward to it.

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

For an `origin = 'limited'` row, `stored_before` is the earliest `received_at` among its batch's stored events.
The events just before the hole were stored by an earlier sync response.
The SDK finishes that response's timeline handlers before it processes the next response, so they were stored before any of this response's batch.
So those events always satisfy the boundary, and the walk stops at them.

Holes nest correctly.
Suppose two limited responses leave holes H1, then H2.
Filling H2 walks through anything stored after H2 was detected, including events another row filled inside H1, and stops at H1's limited batch, which was stored before H2 was detected.
H1 is filled from its own row, and events of H1 that H2's walk already stored do not stop H1 early, because they were stored after H1 was detected.

Failures follow ADR 0121: each failed page is classified, backed off with `retry_at`, and parked after repeated failure, with the state on the gap row.
A `400` for a saved token (one the homeserver no longer honours) restarts the row from the live end.
It sets `origin = 'token_rejected'` and `from_token` to NULL, and sets `stored_before` to the earliest `received_at` among the batch's stored events, unless the boundary already resolved is earlier: it never moves later, which a purged batch event could otherwise cause.
The walk from the live end then passes through the batch and everything stored after it, and stops at the first event stored before the batch.
That is the history before the hole.
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

The boundary comes from the stored rows, not from any clock the watcher keeps.
The watcher remembers the event ids of the last few updates it processed that carried timeline events.
It also reads the database clock once when it subscribes, before the sync service starts.
On lag, `stored_before` is the latest `received_at` among the stored events of the newest remembered update that has any.
If none of them has a stored event, `stored_before` is that subscription time.

This holds however far behind the watcher was.
The lost responses came after that update, and the SDK finished that update's handlers before it took them.
So every event a lost response stored has a later `received_at` than any event of that update.
A stalled watcher, the condition that causes a lag in the first place, therefore cannot move the boundary into the lag window.
It needs no margin: falling back to an older update or to the subscription time only moves the boundary earlier, which makes walks longer, never wrong.

The watcher then records an `origin = 'lag_probe'` row, starting from the live end, for every joined room with an event whose `received_at` is at or after the boundary.
Each probe walks back through everything stored during the lag, filling every hole in that window, however many there are.
It stops at the first page holding an event stored before the boundary.
A probe that finds nothing missing costs one or two pages.

A room first seen during the lag has nothing stored before the boundary, and its probe would walk to the room's start.
So a probe also closes at the first page holding the room's lowest-`id` event, the first event Axon stored there, which precedes every hole a later response could leave.
It takes that one event, not up to `timeline_limit` of the lowest: when a room's first window is shorter than that, they include events of later responses, above the holes, and the probe would close before reaching them (found in review of step 1).
The cost is up to one extra page, through the rest of the first window.
History before a room's first window stays backfill's job.
The lag is logged at `WARN` with the boundary and the number of rooms probed.

One case is not covered: a lagged limited response whose every event failed to store (for example, all failed to parse).
That room stored nothing during the lag, so it is not probed.
The management action of decision 7 re-verifies such a room.

### 4. Repair the holes that already exist with a one-time verification pass

Holes created before this change have no rows, and no client-side signal can locate them.
The only way to find them is to walk each room's history again.

A limited-sync hole can only lie in history that live sync stored.
Backfill pages are contiguous, and each starts where the last ended.
The oldest event live sync stored in a room is the room's lowest `events.id`, because every row before it was in the room's first window.
So the verification pass walks each room from the live end back to that event.
The stop points are recorded when a room is queued:

- **The first window.** The pass closes at the first page that contains any of the room's lowest-`id` events, up to `timeline_limit` of them.
  That is the room's first window, and all of it would have to be missing from `/messages` for this to fail.
- **A count backstop.** The room's stored count and highest `events.id` are recorded when it is queued.
  The pass counts the events on its pages that were already stored with an `id` at or below that highest id.
  Once it has seen as many as the room held, it has passed every event stored then, and it closes.
  Events stored after queueing (new live events, gap fills) are not counted, so they cannot close it early.
- **The room's start.** If a homeserver withholds stored events from `/messages`, filtering them or having purged them, neither stop fires and the walk ends at the room's start.
  That is logged at `WARN` with the room, because it can store history older than the first window.

The pass stores every page, and already-stored events are no-ops.
The first-window stop ends it above any history that backfill stored.
The count backstop ends it no later than the oldest event Axon held when the room was queued.
In a room with backfilled history, that means the backstop re-reads that history without adding to it.
Short of the third case, it never goes past the oldest event Axon holds, so it neither extends history nor depends on `sync.backfill_enabled` or `sync.backfill_target_depth`.

Progress lives in its own table, `room_history_verify (account_id, room_id, from_token, stop_event_ids, seeded_count, seeded_max_id, seen_stored, complete, events_recovered, …)` plus ADR 0121's failure columns, keyed by `(account_id, room_id)` with `ON DELETE CASCADE` from `accounts`.
It is not kept on `room_backfill`, because a room with stored events can have no `room_backfill` row: with backfill off, or before its first page is saved.
The migration seeds one row for every `(account_id, room_id)` that has stored events.
The stop points are taken from the room's rows at that time.
Rooms first seen after the migration need no row, because their holes are recorded from the start.

**Scheduling.**
The pass shares the backfill task, its gate and its guards.
Open gap rows still come first.
They are bounded, since each closes when it meets stored history, and they cover the newest history.
After them, the task alternates one verification page with one ordinary backfill page, so neither starves the other.
A room is not verified while it has an open gap row, because that row is filling the same newest history; other rooms' verification continues.
`history_verify.rooms_pending` counts every incomplete room, and `history_verify.rooms_held` counts those waiting on an open gap row.
The pass is persisted per page, and it is not repeated after it completes.
It runs on its own after the upgrade, with nothing for a user or operator to do.
Users should not need to know their history ever had holes in order to get it back.
`sync.history_verify_enabled` (default `true`) turns it off, for an operator who must avoid the one-off load on the homeserver.

On Adam's production account there are 2.47 M stored events across 15,388 rooms, so the pass is at most about 25,000 pages at the default page size.
It is fewer in practice, because history stored by backfill is not re-read.
Most rooms are small, and at backfill's pacing it finishes in hours, not days.
While ordinary backfill is also busy, the pass gets every other page, which at most doubles that.

### 5. Clients learn that a filled hole changed history

A room's newest events are unaffected by a fill, but a client that already holds a slice of the room is now stale inside it.
A head refresh does not help, because it merges only the newest page.

- **A durable revision.**
  Each room gets a `history_rev` column on `room_summaries`.
  It changes when a gap-fill or verification page inserted at least one event; ordinary backfill, which only extends behind the oldest event, does not change it.
  Its new value is the next value of a database sequence, not the old value plus one.
  So a value never repeats, even when a purge or rebuild re-derives the summary row, and a client never mistakes a changed room for an unchanged one.
  It is exposed on each room in `GET /v1/rooms`.
- **The room-list delta.**
  `history_rev` is added to the column list of the [ADR 0134](0134-incremental-room-list-reads.md) trigger, as that ADR requires of every new `RoomDto` field.
  A change to it alone then stamps `list_changed_at`, and the room is in the next `since` delta.
  The write is coalesced with the frame below, so a room's summary row is written at most once a second however fast its pages land.
  Only rooms where a page actually inserted events change.
  So a delta read after the verification pass names just the rooms that were repaired, not every room it walked.
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
  A kept room's incomplete verification row is re-queued with fresh stop points and counts, because the purge deleted events its count backstop was waiting to see.
- **Not in `delete_event_batches`' `reset_backfill` CTE.**
  That CTE resets the backfill cursor so that deleted history can be fetched again.
  A gap row that survives it stays correct, because its walk needs an event stored before the hole was detected, and it walks further when those were deleted.

While a purge is pending, the room is not joined, so the filler leaves its rows alone.
`persist_backfilled_event`'s purge-on-leave skip still applies to any page in flight.
A room rejoined before the purge finishes keeps its rows (the preserve guard).
Events the filler then stores get `id`s above the purge watermark, so the purge does not remove them, as with backfill after a rejoin.

### 7. Logging, status and a developer switch

- Every log line carries `account_id` and `room_id`.
  Detecting a hole logs at `INFO` with `prev_batch` and the batch's oldest event id; dropping a first window logs at `DEBUG`.
  Each page logs at `DEBUG` with its `inserted` and `already_stored` counts.
  Closing a hole logs at `INFO` with `pages_fetched` and `events_recovered`.
- `GET /v1/status` gains `history_gaps: {open, parked, awaiting_batch, events_recovered}` and `history_verify: {rooms_pending, rooms_held, rooms_complete, events_recovered}` beside the existing `backfill` block.
- The management API ([ADR 0109](0109-management-api.md)) gains an action that re-queues the verification pass for one room or a whole account, so an operator can re-verify after a suspected loss without SQL.
  A re-queued room takes its stop points and counts from the room's rows at that time.
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
   Unit tests cover nested holes, a token rejection restarted from the live end with a crash after its first page, and a lag window with two holes in it, including a lag while the watcher was stalled for longer than the lag and a room first seen during the lag.
   They also cover the first-window test when the watcher reads the broadcast after this response's handlers, or a later response's, have stored their events, with backfill on and off.
2. The verification pass, its table and seeding migration, its config switch, its interleaving with backfill, and the management action (decisions 4 and 7).
   Tests cover each stop point, including a first window that `/messages` never returns.
   Before it ships, it is run against a copy of production data, recording how often the room-start fallback fires.
   If that fallback fires because stored rows are absent from `/messages`, the count backstop counts only events the pager can return.
3. `history_rev`, the `timeline.history_filled` frame and their OpenAPI entries (decision 5), followed by separate web and TUI PRs to consume them.
