# ADR 0095 — Persisted room summaries

## Status

Accepted.

## Context

`GET /v1/rooms` is backed by `Store::list_rooms`, which derived every room's
`last_activity_ts` / `last_event_id` by aggregating the whole `events` table
on every request. The aggregate used two filtered `MAX`es and two
`array_agg(... ORDER BY origin_ts DESC, id DESC)` calls so a room with real
messages would not jump to the top on a later membership change, redaction, or
still-undecrypted event.

That is structurally O(events). `array_agg` materializes and sorts every
event in the group to take element `[1]`;
`events_room_timeline_idx (account_id, room_id, origin_ts DESC)` cannot turn
it into a newest-row lookup. Cursor pagination would not help TTFB: the
`GROUP BY` must finish before `ORDER BY last_activity_ts` can rank anything.

Measured TTFB on a production account grew from 1.3 s at 1,752 rooms
(2026-07-29) to 12–21 s unfiltered / 5.5–7.9 s with `?account_id=`
(2026-08-19). The unfiltered path is 2–3× worse because
`($1::uuid IS NULL OR account_id = $1)` becomes `TRUE` and the index's
leading column stops narrowing. ADR 0085's client cache paints over a repeat
visit (~119–219 ms) but every background refresh still pays the full server
think-time. Issue #85.

A first landing that materialized only activity still took **29.7 s** at
3,601 rooms: sqlx logged `FROM room_summaries` with seven correlated
`room_state` subqueries and two `NOT EXISTS` anti-joins. The events
aggregate was gone; nested-loop point lookups over `room_state` (including
every room's membership) were the remaining cost.

Folding display and visibility onto the same row dropped the same account
to **80 ms TTFB / 170–196 ms total** across three curls (server
`GET /v1/rooms` 56 / 42 / 29 ms). sqlx's 1 s slow-query log no longer
fires. The display-column backfill added **128 ms** to that boot
(migrations start → listening), not tens of seconds.

## Decision

### A `room_summaries` row per `(account_id, room_id)`

Maintain `last_activity_ts`, `last_event_id`, `last_event_row_id`
(`events.id`, the same tiebreak as today's `ORDER BY`),
`last_activity_is_content`, the four display fields plus `room_type`, and
`hidden_left` / `hidden_tombstoned` (ADR 0037 leave/ban and tombstone)
incrementally as events and watched state are stored. `list_rooms` is an
indexed scan of one row per room plus a join to `room_unread_counts`
(ADR 0070). Unread stays in its own table: that table is one row per room,
not one row per member.

Display and visibility are refreshed from `upsert_room_state` (singleton
name/topic/avatar/alias/create/tombstone, and the local user's
`m.room.member` only) and on first insert of a room
(`refresh_room_summary_display`), so a name written before the first
timeline event still appears.

Two migrations, not one: `20260819120000` landed the activity columns and
was applied on the measuring production database before display columns
existed. `20260819160000` adds those columns. Neither file may be edited
on any database that has applied it.

### Same-statement writes, one comparison fragment

`Store::upsert_event` and `Store::update_decrypted_event` already append
`search_outbox` in the same statement (`SEARCH_FANOUT_TAIL`). Room-summary
activity maintenance is the same shape: a shared
`INSERT … ON CONFLICT DO UPDATE` fragment (`ROOM_SUMMARY_TOUCH_TAIL`) fed
by the leading write's `RETURNING`. A duplicate `ON CONFLICT DO NOTHING`
insert returns nothing, so it touches neither search nor the summary.

The `ON CONFLICT` predicate is the whole activity contract:

| Incoming    | Existing marker | Action                                 |
| ----------- | --------------- | -------------------------------------- |
| any         | no row          | insert                                 |
| content     | content         | replace iff `(origin_ts, id)` is newer |
| content     | not content     | **always replace**, even if older      |
| not content | content         | no-op                                  |
| not content | not content     | replace iff newer                      |

`content` means `decrypted_body_text IS NOT NULL`, the same signal
`list_rooms` already used. The "always replace" row is load-bearing: a
membership event at T=100, then a decrypted message at T=50, must show T=50.
"Only bump if newer" would be wrong.

Redaction stays read-time (ADR 0015). A redacted message keeps its stored
`decrypted_body_text` and still counts as activity. There is no "step back
to the previous message" path.

`purge_room` deletes the matching summary in the same statement that
deletes `events`. Account deletion cascades via the FK. A room is in the
list iff it has at least one `events` row, as today.

### Crash safety is the statement, not a boot sweep

The event row and the summary mutation commit together. A crash rolls both
back. The first migration backfills existing rooms once with `DISTINCT ON`
(acceptable as a one-time scan). `Store::rebuild_room_summaries` is the
operator-recovery hatch if a future write path forgets the hook.

Do **not** re-aggregate `events` on every process start. That would move
the 12–20 s from the request onto boot — the failure mode ADR 0070's
unread-count startup sweep already demonstrated.

### What this is not

- **Not `DISTINCT ON` as the shipped read.** Cheaper than `array_agg`, still
  O(events) per request, and the content-bearing filter makes it two passes.
  Used only for the migration backfill.
- **Not pagination.** Transfer size is issue #86's companion; a `LIMIT` on
  this query never cut TTFB. `GET /v1/rooms` stays `Vec<RoomDto>`. The
  remaining ~100 ms of curl `total` after 80 ms TTFB is sending the
  uncompressed body.
- **Not compression** (issue #86). Independent: bytes on the wire, not
  server think-time.

## Consequences

- `GET /v1/rooms` TTFB on a 3,601-room production account dropped from
  12–30 s to **80 ms** (server 29–56 ms). Unfiltered vs `?account_id=`
  converge because both are a scan of one row per room.
- The activity contract is now a write-time invariant. New event-insert
  paths must go through `upsert_event` (today there is only one) or call
  the same fragment. Watched state writes must call
  `refresh_room_summary_display`.
- The first boot after each of the two migrations runs the backfill inside
  the migration transaction before the process serves `/v1/rooms`. The
  display-column backfill measured **128 ms** on the same production
  account; the activity backfill is a one-time `events` scan and is
  heavier.
- Wire contract, OpenAPI, and both clients are unchanged.
