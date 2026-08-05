# ADR 0070 — Persisted per-room unread counts

## Context

A freshly loaded or reloaded web client can show a durable "unread dot" from
its own locally-persisted read markers (ADR 0048, ADR 0067), but it cannot
show a *numeric* unread count until it has personally observed live message
events in the current session: `RoomDto`'s only activity signal was
`last_activity_ts`, an unfiltered `MAX(origin_ts)` over every event type, and
the web client's own unread counter (`stores/unread.ts`) is live-only,
resetting to zero on every reload. Issue #313 asks for a real,
server-persisted count so a fresh session shows the right number immediately.

Real Matrix unread/notification state is **per-room derived state**, not an
ephemeral event, so the M18 ephemeral-passthrough path (ADR 0056) — a raw,
lossy, no-replay forward of allowlisted `m.typing`/`m.receipt`-style events —
structurally cannot carry it: there is no ephemeral event to forward, and ADR
0056's passthrough only carries per-event `content` verbatim, not a room-level
counter.

The scope of this ADR is **server-only**: expose the data through `RoomDto`
and a new live WS frame. Web-client consumption (retiring
`stores/unread.ts`'s live-only counter, `hasRoomUnread`'s reload-resetting
heuristic) is a deliberate follow-up, not covered here.

## Decision

### Source of truth: matrix-sdk's client-side counters

Axon's `matrix-sdk` dependency (v0.18.0) already computes per-room unread
notification and mention counts locally:
`Room::num_unread_notifications()` and `Room::num_unread_mentions()`. These
counters are derived from the SDK's synced room state and read-receipt state,
and match the unread badge semantics Matrix clients expose. Axon does not
reimplement read-position tracking over the `events` table; it captures and
persists values matrix-sdk already computes. This keeps the SDK as the single
source of truth for unread-count semantics, rather than introducing a second,
Axon-maintained read-position model that could drift from the real receipts
the existing `POST …/rooms/{room_id}/read` endpoint (ADR 0067) already writes
through to Synapse.

The first implementation used `Room::unread_notification_counts()`, which
reads the sync room-summary `unread_notifications` counters. Live testing
against Synapse showed those summary fields stayed at zero in Axon's
sliding-sync path even when Synapse's `event_push_actions` table had 14
notification rows after the user's read receipt, and other Matrix clients
(Element, Element X, gomuks) displayed the room as unread. Switching to the
SDK's client-side counters made Axon's `/v1/rooms` counts and
`unread_counts.changed` frames match those clients.

### Capture mechanism: a watcher task, not a request-time query

A new `watch_unread_counts` task in `crates/axon-sync/src/engine.rs` (same
pattern as `watch_sender_trust`/`watch_verification`): the in-memory dedup
cache is seeded from whatever is already persisted (`Store::room_unread_counts`)
before anything else runs, so a restart's startup sweep only re-upserts and
re-broadcasts rooms whose counts actually changed since the last run — not
unconditionally every joined room. The watcher then subscribes to
`Client::room_info_notable_update_receiver()`, then runs the (now-seeded)
startup sweep over every currently-joined room (`client.joined_rooms()`), so
a notable update that lands mid-sweep is queued on the receiver rather than
missed — replaying it afterward is a harmless no-op once
`capture_unread_counts`'s dedup check sees the sweep already observed the
same value. Sweeps (both this startup one and the periodic backstop below)
run with a small bounded concurrency rather than one Postgres round trip at a
time, and prune stale state for rooms the account has since left — both the
in-memory dedup cache and, via `Store::delete_stale_room_unread_counts`, the
persisted `room_unread_counts` row itself, so a left room's row doesn't sit
in the table forever (previously only `ON DELETE CASCADE` on account
deletion cleaned these up; harmless — `list_rooms` already filters left
rooms — but unbounded growth for a long-lived account that churns through
many rooms).

Live testing turned up two faults in that sweep, both of which fire on
process start, and both now corrected. First, **pruning ran against a room
list that had not loaded yet.** `client.joined_rooms()` is only authoritative
once the SDK has hydrated the account's rooms; on the startup sweep it can
return a handful or none, and `delete_stale_room_unread_counts` then deletes
every row for a room absent from that list. On a 1755-room account the
startup sweep left five rows standing and re-inserted 758 over the following
second — so every restart destroyed the persisted counts this ADR exists to
provide, and a fresh client load got nothing to read until the values were
re-derived. Pruning is housekeeping with no urgency, so the startup sweep no
longer prunes at all; only the periodic re-sweep does, five minutes in, and
it skips pruning when the room list comes back empty rather than treating
that as "the account has left every room".

Second, **matrix-sdk's counters are not always trustworthy at the moment they
are read.** `RoomReadReceipts` carries a `pending` ring buffer of receipts
the SDK could not match to an event in the room's in-memory linked chunk;
while one sits there, the counts beside it were computed from a *fallback*
anchor, because `select_best_receipt` settles for the most recent event it
can find in the chunk when the real receipt's target is missing, then counts
everything after it. Restarting rehydrates the event cache and can lose the
previous anchor, so a silent room reports fresh notifications: in one
observed restart, eleven rooms went nonzero in the same second with newest
events between 23 minutes and nearly two days old, one of them reporting 32
notifications against four hours of silence. `capture_unread_counts` now
declines to *raise* a count while `pending` is non-empty, while still
accepting a decrease so a room already carrying a bad count self-corrects.
The accepted cost is that a genuine new message arriving during that window
has its count held back until `pending` drains and the next re-sweep runs.

Rooms bridged by mautrix are hit disproportionately, which is what surfaced
this: a bridge's portal creation backfills the pre-existing conversation with
its real (older) timestamps *after* the room's own creation events and after
the user's own first message, so the fallback anchor lands ahead of genuine
unread messages rather than at the room's tail. In an unbridged room the
fallback usually lands on the user's own last message or the true tail and
the recount yields zero.

Bridged rooms had a *second*, unrelated fault with the same symptom — a durable
badge on every fresh load — that this guard does not address and should not be
mistaken for: the read receipt itself named the wrong event, because clients
derive their read position from axon's `origin_ts` display order while a receipt
is interpreted in stream order. There the count was correct and the input was
wrong. See ADR 0089.

The watcher reacts to **every** notable update regardless of its `reasons`
bitflag. `RoomInfoNotableUpdateReasons` has no dedicated "notification count
changed" bit — the closest candidates (`RECENCY_STAMP`, `LATEST_EVENT`,
`READ_RECEIPT`) are not documented as exhaustive triggers for a count change,
and the bitflags include a `NONE` sentinel described upstream as a temporary
hack. Filtering on specific reasons would be a bet on an implementation
detail; reacting to every update and dedup-ing on the actual value diff
(`capture_unread_counts`'s in-memory `HashMap<OwnedRoomId, (u64, u64)>`) is
correct regardless of how upstream's reason bits evolve.

A lagged broadcast receiver is not specially recovered: the watcher always
re-derives the *current* value from `Room::num_unread_notifications()` and
`Room::num_unread_mentions()`
rather than diffing the missed notification, so a dropped update for a room
self-heals the next time anything about that room changes. A periodic
re-sweep (`UNREAD_COUNTS_RESWEEP`, every 5 minutes) is a backstop against a
room going quiet immediately after a lag.

### Storage: one row per `(account_id, room_id)`, looked up by primary key

`room_unread_counts(account_id, room_id, notification_count, highlight_count,
updated_at)`, upserted via `Store::upsert_room_unread_counts`. `list_rooms`
reads it via two more correlated sub-selects, in the same style as the four
existing display-field sub-selects (`name`/`topic`/`avatar_url`/
`canonical_alias`) — a single-row PK lookup, not an aggregate.

This directly **supersedes one sentence of ADR 0055**: that ADR deferred
member/unread counts from the Tier 1 `RoomDto` projection as "the expensive
case," reasoning that list latency should not scale with the priciest field.
That concern was about an aggregate query computed at read time (e.g.
`COUNT(*)` over events since a read marker); a stored scalar looked up by
primary key has the same cost profile as the fields ADR 0055 already put in
Tier 1. The rest of ADR 0055 (the Tier 1/Tier 2 split, `is_direct`/
`room_type`/`tags` reasoning) is unaffected.

### Wire delivery

- `RoomDto.notification_count` / `RoomDto.highlight_count` (`i64`, always
  present, `0` until the watcher has captured a value) — what a fresh
  `GET /v1/rooms` load returns.
- `unread_counts.changed` on `/v1/ws` (`UnreadCountsFrame` →
  `UnreadCountsFramePayload`) — live updates to an already-connected client,
  following the `SenderTrustFrame`/`sender_trust.violation` pattern exactly.
  Not built on ADR 0056's ephemeral passthrough (see Context).

## Consequences

- A fresh or reloaded client can show a real unread count immediately,
  without waiting to observe a live event in-session — the literal
  acceptance criterion in issue #313.
- Axon gains no new read-position source of truth: matrix-sdk remains
  authoritative for the local unread counters, and the existing read-receipt
  round trip (ADR 0067) is what clears the count.
- Web-client consumption (rendering these fields, retiring the live-only
  counter) is deferred to a follow-up PR.
- *(Amended by ADR 0090.)* The prune above keys on `client.joined_rooms()`, which
  cannot see a room the homeserver has stopped serving or a room that has been
  tombstoned — both stay "joined" locally, and their rows freeze at whatever the
  watcher last wrote. Those rooms are pinned to zero instead.
