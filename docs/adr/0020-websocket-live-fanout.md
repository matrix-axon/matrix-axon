# ADR 0020 — `/v1/ws` live event fan-out

## Context

M5 ("Client API v0") is split three ways — **5a** the read-only HTTP API
(ADR 0019), **5b** the `/v1/ws` live fan-out (this ADR), **5c** interactive SAS
verification. With 5a a client can read rooms, timelines, and events; 5b adds the
push half: a client opens one WebSocket and sees events as the sync engine
persists them, across all of this Axon's accounts, without polling.

5a deliberately laid the seam for this: `AppState` extracts its pieces via
`FromRef`, "so 5b can add a `broadcast::Sender` field with zero churn to existing
handlers." This ADR records how the fan-out is wired and what the wire contract
is.

## Decision

### A `tokio::sync::broadcast` bus, owned by the sync engine

The sync engine is the sole producer of live events; every `/v1/ws` connection is
a consumer. That is exactly the shape `tokio::sync::broadcast` models: one sender,
many receivers, each receiver with its own cursor.

`SyncEngine` creates the channel in `start()` and exposes a producer handle via
`SyncEngine::live_events() -> broadcast::Sender<LiveEvent>`. The binary hands that
clone to `AppState::new`, and the `/v1/ws` handler calls `subscribe()` once per
connection. The channel exists for the engine's whole lifetime regardless of
account count, so `/v1/ws` is valid (just silent) even with zero accounts.

**Why broadcast and not mpsc/watch.** A slow or stalled WebSocket client must
never apply back-pressure to sync — persistence is the system's job of record,
and a wedged client cannot be allowed to block it. `broadcast` gives bounded
per-channel buffering: a consumer that falls more than `capacity` events behind
receives `RecvError::Lagged(n)` and resumes at the live edge, dropping the
backlog. We log the lag and keep the connection open. `watch` would only keep the
latest value (we need each event); `mpsc` is single-consumer.

Capacity is a named constant (`LIVE_EVENT_CHANNEL_CAPACITY = 1024`), generously
sized for a personal-scale server where live traffic is low. The producer skips
building/cloning a `LiveEvent` entirely when `receiver_count() == 0` (the common
case for a headless server with no client attached), so the bus costs nothing
when unused.

### `LiveEvent` lives in `axon-core`; `axon-api` owns the wire shape

The producer (`axon-sync`) and the consumer (`axon-api`) are sibling crates —
neither depends on the other. Their only shared crate is `axon-core`, so the
broadcast item type lives there: `axon_core::LiveEvent`, a wire-neutral struct
carrying the fields the read API's event shape needs.

The HTTP/WebSocket envelope stays owned by `axon-api`, which maps
`LiveEvent → EventDto` (`impl From`). This keeps the wire contract in one place
(the API layer) while letting the sync engine stay ignorant of it — the same
separation the read API already has between store rows and DTOs.

### Wire contract: the `{type, account_id, payload}` envelope

Every frame is a JSON text frame with the project-wide envelope shape:

```json
{ "type": "timeline.event", "account_id": "<uuid>", "payload": { …EventDto } }
```

The `payload` for a timeline event is the **same `EventDto`** the HTTP read API
returns, so a client has one event shape to model. `type` is namespaced
(`timeline.event`) so later frame kinds — e.g. M5c verification events — extend
the protocol without colliding. A freshly synced event is never
already-redacted (a redaction arrives as its own later event), so live frames
always carry `redacted: false`.

### Re-decrypted events (#948)

When re-decryption fills a stored UTD, the server publishes an update after the write commits.
If retries overlap, only the retry that changes the row publishes the update.

```json
{
  "type": "timeline.event_decrypted",
  "account_id": "<uuid>",
  "payload": {
    "room_id": "!room:example.org",
    "event_id": "$event",
    "arrival_order": 42
  }
}
```

Read the current event from `GET /v1/accounts/{account_id}/events/{event_id}`.
The read applies normal redaction masking.
Use `(account_id, event_id)` to reconcile a cached event in place, keeping its original `arrival_order`.
Apply the returned event type and relations as on a timeline reread.
The update can arrive before a concurrent live-sync delivery, so reconcile with the current read if the original UTD frame comes later.
A historical event may have no earlier frame.
Keep the existing arrival cursor and unread count when applying the update.
Clients that do not recognize this additive tag ignore it.
Refresh displayed UTDs after reconnecting or detecting a gap in live delivery.
Each update keeps the row's original arrival order, so a query for later arrivals alone cannot recover a missed update.
The server logs broadcast lag and keeps the socket open, which means the client may receive no signal that updates were dropped.
Issue #1048 tracks a server-side way to trigger resync after lag.

### Live tail, not replay

`/v1/ws` delivers events that arrive *after* a client connects. It is not a
backlog replay or a resumable cursor stream; history is the HTTP read API's job
(`GET …/timeline`). The handler `subscribe()`s while handling the upgrade —
before the `101` the handshake waits on — so no event that arrives mid-handshake
is missed.

### Not in the OpenAPI document

A WebSocket upgrade isn't expressible in OpenAPI 3.1, so `/v1/ws` is absent from
`openapi/openapi.json` and the golden-file test is unaffected. The frame protocol
is documented in the `ws` module and here instead.

## Consequences

- **Pro:** the `AppState`/`FromRef` seam paid off — read handlers are untouched;
  the bus is one field plus one extractor. Sync can never be back-pressured by a
  client. Clients model one event shape across HTTP and WS. The namespaced `type`
  leaves room for M5c.
- **Con:** delivery is best-effort — a lagging client silently skips events (it
  must reconcile via the read API). No auth yet: bearer-token validation on the
  upgrade lands with the rest of auth in **M8** (`Sec-WebSocket-Protocol` or
  initial-envelope token, per the implementation spec).
- **Scope:** live-sync persistence publishes timeline arrivals.
  Successful re-decryption back-fills (ADR 0014) publish the update described above.
- **Revisit** if clients need a resumable/replaying stream (a cursor on the WS,
  or server-sent backfill on connect).
