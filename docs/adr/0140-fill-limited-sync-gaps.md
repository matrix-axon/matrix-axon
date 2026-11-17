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

## Decision

### 1. Record every hole that a limited response leaves

A new per-account watcher subscribes to the room-update broadcast before the sync service starts, beside the push watcher.
For each joined room whose timeline is `limited` with a `prev_batch`, it records a hole in a new table:

```sql
CREATE TABLE room_timeline_gaps (
    account_id            UUID        NOT NULL REFERENCES accounts ON DELETE CASCADE,
    room_id               TEXT        NOT NULL,
    prev_batch            TEXT        NOT NULL,  -- token just before the limited batch
    anchor_event_id       TEXT        NOT NULL,  -- oldest event of the limited batch
    detected_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    pages_fetched         INTEGER     NOT NULL DEFAULT 0,
    events_recovered      BIGINT      NOT NULL DEFAULT 0,
    consecutive_failures  INTEGER     NOT NULL DEFAULT 0,
    last_failure          TEXT,
    retry_at              TIMESTAMPTZ,
    parked                BOOLEAN     NOT NULL DEFAULT false,
    updated_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (account_id, room_id, prev_batch)
);
```

`updated_at` gets the shared `trigger_set_updated_at()` trigger.
The watcher only inserts rows, so it keeps up with the broadcast; the paging happens elsewhere (decision 2).

Recording is durable, so a restart, a crash or a supervised account restart never forgets a hole.
A response that is limited because the room is new to this account is still recorded.
The filler drops it unpaged when the room has no stored event outside the limited batch, because backfill starts such a room from its live end and covers the same range.

### 2. Fill each hole by paging back until known history

Each hole is paged backward from `prev_batch` with backfill's `fetch_page` and stored with `persist_backfilled_event`, one page (`sync.backfill_page_size`, default 100) at a time.
Progress is saved after each page by replacing `prev_batch` with the page's `end`, so a restart resumes where it stopped.

A hole is closed, and its row deleted, at the first page that contains an event Axon already stored for that account.
Because the anchor batch lies after `prev_batch`, any stored event on a backward page belongs to the history before the hole.
The whole page is stored before closing, so the events of the hole on that page are kept, and the overlapping ones are no-ops.
A page with no `end` (the room's start) also closes the hole.

Holes nest correctly.
If two limited responses leave holes H1 then H2, filling H2 stops when it reaches the 20 events stored after H1, and H1 is filled from its own row.

Failures follow ADR 0121: each failed page is classified, backed off with `retry_at`, and parked after repeated failure, with the state on the gap row.
A `400` for the saved token (a token the homeserver no longer honours) restarts that hole from the live end instead.
In that mode, stored events newer than the anchor are expected and skipped.
The hole closes only at the first stored event found after the anchor event has been passed.

Holes are scheduled inside the backfill task, ahead of ordinary backfill pages and newest hole first, because they cover recent history a user is likely to read.
They share the load gate, the disk-space guard and the throttle with backfill.
Unlike ordinary backfill, they run even when `sync.backfill_enabled = false` and ignore `sync.backfill_target_depth`.
They restore what live sync should have delivered, not extra history.

### 3. A lagged broadcast probes the rooms that were active

On `RecvError::Lagged`, the watcher cannot know which skipped responses were limited.
Live events still reach the database through the event handler, so only rooms with events stored while the broadcast was lagging can have a lost hole.
The watcher keeps the time of the last update it processed.
On lag, it records a probe for every joined room with an event received since then.
A probe is a hole with no `prev_batch` that is paged from the live end, so it overlaps known history on its first page unless something is missing.
Probes use the same table, with the anchor being the newest event stored at the moment of the lag, and the live-end mode of decision 2.
It is logged at `WARN` with the number of rooms probed.

### 4. Repair the holes that already exist with a one-time verification pass

Holes created before this change have no rows, and no client-side signal can locate them.
The only way to find them is to walk each room's history again.
A new verification pass does that once per joined room.
It pages from the live end back to the room's start, or to `backfill_target_depth` when one is set, and stores every page.
Already-stored events are no-ops, and missing ones are inserted.

Its progress lives on `room_backfill` in new columns: `verify_token`, `verify_complete`, `verify_events_recovered`.
A migration marks every existing `room_backfill` row as needing verification.
Rooms without a row have never been backfilled, and their first backfill from the live end already covers everything.
The pass runs only when no holes are open and ordinary backfill is idle, through the same gate and guards.
It is persisted per page, and it is not repeated after it completes.
`sync.history_verify_enabled` (default `true`) turns it off.

On Adam's production account the pass is about 2.47 M stored events across 15,388 rooms, roughly 25,000 pages at the default page size.
Most rooms are small, and at backfill's pacing it finishes in hours, not days.

### 5. Clients learn that a filled hole changed history

A room's newest events are unaffected by a fill, but a client that already holds a window of the room is now stale inside it.
After each page that inserted at least one event, the server sends a new additive WebSocket frame, `timeline.history_filled`.
Its payload is `{room_id, oldest_ts, newest_ts, inserted}`, giving the `origin_ts` range of the inserted events.
A client drops or refetches any cached window that overlaps the range.
The verification pass uses the same frame.
Frames are coalesced per room to at most one per second, so a large repair does not flood a client.
How each client responds is decided in that client's own PR.

### 6. Purge and teardown remove holes with the history

Room purge and account deletion delete the room's gap rows in the same batch that resets its backfill cursor, so a hole is never refilled after its history was deliberately deleted.
Ingestion of a gap page goes through the existing purge-on-leave check, the same as backfill.

### 7. Logging, status and a developer switch

- Every log line carries `account_id` and `room_id`.
  Detecting a hole logs at `INFO` with `prev_batch` and `anchor_event_id`.
  Each page logs at `DEBUG` with its `inserted` and `already_stored` counts.
  Closing a hole logs at `INFO` with `pages_fetched` and `events_recovered`.
- `GET /v1/status` gains `history_gaps: {open, parked, events_recovered}` and `history_verify: {rooms_pending, rooms_complete, events_recovered}` beside the existing `backfill` block.
- The management API ([ADR 0109](0109-management-api.md)) gains an action that clears `verify_complete` for one room or a whole account, so an operator can re-verify after a suspected loss without SQL.
- A non-zero `events_recovered` from the verification pass is itself evidence of past loss; it is logged at `WARN` once per room with the count.

## Consequences

- An event that live sync skips is fetched within one backfill sweep of the response that skipped it, instead of never.
- The production rooms affected by the 2026-10-18 import are repaired by the verification pass with no manual step.
- Each limited response costs at least one extra `/messages` page.
  On a quiet account that is rare.
  During an import it is one page per 100 missing events, which is the minimum needed to fetch them.
- The verification pass re-reads all of each account's history once, a one-off load on the homeserver bounded by the load gate.
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

1. Gap table, the detecting watcher with its lag probe, and the filler inside the backfill task (decisions 1–3, 6, and the logging and status of 7).
   Verified by a Synapse integration test that sends more than `timeline_limit` events between two syncs and asserts every one is stored.
2. The verification pass, its migration, its config switch, and the management action (decisions 4 and 7).
3. The `timeline.history_filled` frame and its OpenAPI entry (decision 5), followed by separate web and TUI PRs to consume it.
