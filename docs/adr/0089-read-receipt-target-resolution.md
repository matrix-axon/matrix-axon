# ADR 0089 — Read receipts name a stream-ordered event, not the client's display-newest one

## Context

Axon sorts every read surface by `origin_ts`: `room_timeline` pages
`ORDER BY origin_ts DESC, id DESC`, the pagination cursor is `(origin_ts, id)`,
and `list_rooms` derives `last_activity_ts`/`last_event_id` from
`MAX(origin_ts)`. Clients inherit that order — the web client reverses a
timeline page and treats its last element as "the newest event I have shown the
user", which is what it passes to `POST …/rooms/{room_id}/read` (ADR 0067).

A Matrix read receipt is not interpreted that way. It names one event, and the
homeserver clears exactly the events that precede that event **in stream
order**; matrix-sdk's client-side unread counter (ADR 0070) does the same
against its own event-cache chunk. Display order and stream order normally
agree, so this went unnoticed.

They disagree whenever a homeserver delivers an event whose `origin_server_ts`
predates events we already hold. A mautrix bridge does exactly that on every
portal creation: it creates the room, emits its own state (`m.room.create`,
membership, name, avatar, `m.bridge`, `uk.half-shot.bridge`), and then backfills
the pre-existing conversation carrying its *real*, older timestamps.

Observed in a LinkedIn portal (`!cmq_HNozYgWPTPorDDOazOGNzUe-oRH1GEt76Fha44c`),
which is what prompted this ADR:

| event | `origin_ts` | Synapse `stream_ordering` | axon row `id` |
|---|---|---|---|
| `m.room.create` | 1785928306622 | 2747050 | 1871406 |
| `uk.half-shot.bridge` | 1785928309453 | 2747068 | 1871424 |
| **`m.room.message`** | **1785928304987** | **2747070** | **1871426** |

The only message in the room is *oldest* by `origin_ts` and *newest* by both
Synapse's stream order and axon's own ingest order — its timestamp is 1.6 s
before the room it lives in was created. The client therefore marked the room
read at `uk.half-shot.bridge` (stream 2747068), which does not cover the message
at 2747070. Consequences:

- matrix-sdk counted 1 unread notification — **correctly**, given that receipt —
  so `room_unread_counts` held 1 and every fresh client load showed a badge.
- Re-reading the room could never fix it. Both the client's receipt floor and
  its cross-device read marker advance forward-only *by `origin_ts`*, and the
  message's `origin_ts` is below the floor already sent, so no later read could
  ever name it.
- The room read as unread in Element too, since the receipt on the homeserver
  genuinely did not cover the newest event.

This is not the phantom-count case ADR 0070 already guards against. There is no
unmatched receipt and no fallback anchor: the receipt matched, and the count
derived from it was right. The receipt named the wrong event.

## Decision

`SdkGateway::send_read_receipt` resolves the event to name, rather than passing
the client's event id straight through. The client keeps stating what it means —
"I have displayed everything up to here" — and axon translates that into stream
order, in one place, for every client.

### The candidate: the last event of the room's event-cache chunk

matrix-sdk's per-room event-cache chunk *is* the stream ordering — sync appends
to its end, back-pagination prepends to its front — and it is the same structure
`compute_unread_counts` anchors a receipt against. Naming its last event is
therefore what actually drives the room's notification count to zero, which is
the behavior this ADR exists to restore. The chunk is already subscribed in
axon's process: `matrix-sdk-ui`'s room-list service calls
`EventCache::subscribe()` when `SyncService` starts.

Rejected alternatives for the candidate:

- **`Room::latest_event()`** reads a `LatestEventValue` that is only computed
  once something has called `Client::latest_events()`, which axon never does. It
  returns `None` for every room here, so a fix built on it would have been
  silently inert.
- **The newest row in axon's own `events` table by `id`.** Ingest order tracks
  stream order for live sync, but M10 backfill inserts *older* events with
  *higher* row ids, so this would select a backfilled event and walk the receipt
  backwards. Distinguishing the two ingest paths means a new column on `events`,
  a migration, and a `NewEvent` field — and it would still fix nothing for rooms
  whose rows predate the migration, including every room affected today.

### The guard: substitute only when both orderings agree

The candidate replaces the requested event only when
`supersedes_requested_receipt` holds — a pure predicate over two
`TimelineCursor`s (`Store::event_positions` supplies them):

- `latest.id > requested.id` — the candidate reached *this account* after the
  requested event, so naming it moves the read position forward, never back.
- `latest.origin_ts <= requested.origin_ts` — the candidate sorts at or before
  the requested event in display order, so the client had it on screen.

The second condition is what keeps this honest. A client reading history —
scrolled back, or landed mid-timeline from a search hit — passes an old event id,
and the room's stream-latest event is one the user has *not* seen; the guard
fails and the caller's event stands. Marking a room read past what a client
displayed would be a worse bug than the one being fixed.

Everything unresolvable falls back to the requested event, which is what this
route sent unconditionally before: an empty or unhydrated chunk, an event id
neither the chunk nor the store knows, or a store failure. This route is
best-effort by contract (ADR 0067), and a fallback is never worse than the prior
behavior.

### What this does not change

- **`origin_ts` stays the display order.** Clients still sort and paginate by it,
  and a bridge-backfilled message still *renders* above the portal's creation
  notices. That is the other half of the same divergence, deliberately out of
  scope, tracked in issue #133 — changing the display order touches the
  pagination cursor, the room-list sort, the web scroll anchors (ADR 0076), and
  the thread-unread cutoff.
- **The cross-device read marker** (`read_markers` device state, ADR 0048) still
  records the client's `origin_ts`-newest event. It is a display-order artifact —
  where to draw the "new messages" line — not a Matrix receipt, and no unread
  count derives from it.
- **No new read-position model.** ADR 0070's rule holds: matrix-sdk stays
  authoritative for unread semantics. This ADR corrects the input we hand it. In
  particular, an earlier proposal to *clamp* the SDK's count against axon's own
  event rows was rejected once the telemetry came back — the count was correct,
  and clamping it would have hidden a genuinely unread message.
- **No client change.** The fix lands server-side, so the TUI and the web client
  are both corrected without touching either, and any future client inherits it.

## Consequences

- A bridge portal's backfilled conversation is acknowledged on the first read,
  in axon and in every other Matrix client reading the same receipt. Existing
  stuck rooms self-heal on the next read: the substitution depends on the
  event-cache chunk and the `events` rows, both of which already exist for them.
- One extra store round trip per `POST …/read` (a two-id lookup on the
  `(account_id, event_id)` unique index), plus a clone of the room's in-memory
  chunk. The route is debounced per room by clients (800 ms in the web client)
  and fires on room open, not per event.
- The receipt axon sends may differ from the event id the client asked for. The
  substitution logs at debug with both ids; the request body's schema documents
  it as part of the route's contract.
- A client whose display order *is* stream order (a future client that sorts on
  an arrival key) is unaffected: its requested event is already the chunk's last
  event, and the substitution short-circuits.
