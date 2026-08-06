# ADR 0089 — Read receipts name an event in arrival order, chosen by the client

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

Expose each event's **arrival order** on the wire and let the client name its own
receipt target. `EventDto.arrival_order` (`i64`, always present, on timeline
reads *and* `/v1/ws` frames alike) is the event's `events.id` — the monotonic
sequence in which this account ingested events. A client marking a room read
names the event with the greatest `arrival_order` **among the events it has
displayed**, and `POST …/rooms/{room_id}/read` sends that id to the homeserver
verbatim.

Two consequences worth stating plainly, because both were violated by the first
attempt at this:

- **`origin_ts` order is not receipt order.** Display order is
  `(origin_ts, id)`; receipt order is arrival order. A client must therefore
  keep its forward-only receipt floor on `arrival_order` too, or the message it
  most needs to acknowledge — a backfilled one, stamped below the floor — can
  never be named.
- **Only the client knows what it displayed.** Nothing else does, so nothing else
  may choose the target.

### Rejected: resolving the target server-side

The first implementation of this ADR resolved the target inside
`SdkGateway::send_read_receipt`: it took the last event of the room's matrix-sdk
event-cache chunk (the stream ordering) and substituted it for the client's event
whenever it had arrived later (`id`) *and* sorted at or before it in display
order (`origin_ts`), the second condition standing in for "the client displayed
it".

That inference is unsound, as review caught before merge. A client pages by
display order — the web client loads the newest 50 by `(origin_ts, id)` — so an
event stamped older than the page floor is not in the page at all, however
recently it arrived. In a room mixing backfilled history with current traffic
(one live account has a bridged room with 2018-stamped events among 2026 ones),
the chunk-latest event can sit hundreds of positions below the loaded page,
satisfy both conditions, and be substituted — acknowledging messages the client
never rendered, which is exactly the mid-history invariant the guard was
introduced to protect.

The `origin_ts`-only comparison was wrong on its own terms too: `(origin_ts, id)`
is the display sort key, so an equal-timestamp event with a higher `id` sorts
*after* the client's event in display order and therefore was not in its page
either. Bridges stamp bursts within a single millisecond, so those ties are
common rather than exotic.

The general lesson, and the reason the resolution moved to the client: **the
server cannot prove what a client displayed.** Any server-side rule for that is a
guess about page size, page count, and scroll state, and a wrong guess silently
marks unread messages read. Making the client name the event removes the question
instead of answering it badly — the client can only ever name an event it holds.

Passing the client's *window* to the server (an explicit oldest-displayed bound
alongside the newest) was considered and rejected as the more complicated way to
reach the same place: it keeps a resolver, an inference, and a fallback path on
the server, for no capability the client doesn't already have once it can see
arrival order.

### What this does not change

- **`origin_ts` stays the display order.** Clients still sort and paginate by it,
  and a bridge-backfilled message still *renders* above the portal's creation
  notices. That is the other half of the same divergence, deliberately out of
  scope, tracked in issue #133 — which this ADR's `arrival_order` field also
  unblocks, since a display-order change needs exactly that key.
- **The cross-device read marker** (`read_markers` device state, ADR 0048) stays
  on `origin_ts`. It is a display-order artifact — where to draw the "new
  messages" line — not a Matrix receipt, and no unread count derives from it.
- **No new read-position model.** ADR 0070's rule holds: matrix-sdk stays
  authoritative for unread semantics. This ADR corrects the input we hand it. An
  earlier proposal to *clamp* the SDK's count against axon's own rows was
  rejected once telemetry came back — the count was correct, and clamping would
  have hidden a genuinely unread message.

## Consequences

- A bridge portal's backfilled conversation is acknowledged on the first read,
  in axon and in every other Matrix client reading the same receipt — once a
  client sends the right event id. The server change alone fixes nothing; the
  client work is the other half and ships alongside it.
- `Store::upsert_event` now returns the row's arrival order, so the live-event
  path can carry it. The insert already used `INSERT … RETURNING` to feed the
  search outbox atomically; it now also selects that id out. A duplicate
  delivery, which returns no row, costs one extra indexed read to report the
  existing position rather than none.
- Every client that marks rooms read must adopt `arrival_order`. Until it does,
  its receipts keep landing on the display-newest event, i.e. today's behavior —
  wrong for bridged rooms, but no worse than before.
- The server no longer reasons about read positions at all: `POST …/read` is a
  pass-through again. The route's failure logging (added alongside this ADR)
  stays, because a receipt that never lands is otherwise invisible and presents
  as a room that will not stop showing unread.
