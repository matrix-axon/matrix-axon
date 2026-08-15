# ADR 0067 — Sending real Matrix read receipts to Synapse

## Context

Axon tracks "read" state today, but only for its own purposes. ADR 0048 added
a per-device key-value store (`PUT/GET /v1/devices/{device_id}/state/{namespace}`)
so that a user's *other Axon devices* can sync unread badges via a
`read_markers` namespace. That store is explicitly opaque to the server — Axon
"never interprets the value" — and it never calls out to the homeserver. As a
result, reading a message in an Axon client (TUI or web) has no effect on
Synapse's own read-receipt state.

This is a real gap: third-party Matrix clients (e.g. gomuks) read their
"unread" indicator from the homeserver's receipt/read-marker state, not from
anything Axon-specific. A room read in Axon still shows unread everywhere
else. A repo-wide search confirms no code path today calls Matrix's C-S API
receipt endpoints (`POST /rooms/{roomId}/receipt/{receiptType}/{eventId}` or
`POST /rooms/{roomId}/read_markers`) — this has simply never been built.

Two facts from the existing implementation shape the design:

- `crates/axon-sync/src/gateway.rs` already has the pattern for outbound,
  per-room homeserver calls (`send_message`, `send_media`): resolve a
  `matrix_sdk::Room` handle via `Gateway::room(account_id, room_id)`, then
  call an SDK method and map its error.
- matrix-sdk 0.18.0 (the pinned version) exposes
  `Room::send_multiple_receipts(Receipts)`, which wraps
  `POST /rooms/{roomId}/read_markers` and can set the public read receipt
  (`m.read`) and the private fully-read marker (`m.fully_read`) together in a
  single request — this is what other clients read to show a room as read.
- Both clients already have a single, debounced choke point where "this room
  was just read" is decided: `App::note_room_read` → `spawn_read_put` in
  the TUI (`clients/tui/src/app/read_markers.rs`), and `advanceReadMarker` in
  the web client (`clients/web/src/stores/device-state.ts`). Both currently
  do nothing but PUT to the internal device-state endpoint.

## Decision

### A new, explicit route — not folded into the device-state PUT

`put_device_state` treats its payload as opaque per ADR 0048's own framing,
and is shared by two namespaces (`drafts`, `read_markers`). Branching inside
it on `namespace == "read_markers"` to reach into the payload and fire a
Synapse call would break that invariant, couple homeserver-failure handling
into a route whose contract says nothing about the homeserver today, and mean
any device-state write — including test or replayed traffic — could silently
send a live receipt.

Instead, add `POST /v1/accounts/{account_id}/rooms/{room_id}/read`, body
`{ "event_id": "$..." }`. This mirrors the existing `send`/`send_media`
mutation pattern: a trait port (`ReadReceiptSender`, separate from
`MessageSender` since it has different failure tolerance) implemented by an
adapter over `axon-sync`'s `SdkGateway::send_read_receipt`, which calls
`Room::send_multiple_receipts` with both the public read receipt and the
fully-read marker set to the same event. *(Amended by ADR 0089 — a receipt is
read in arrival order, not the display order this route's `event_id` used to
state. Which event to name is now decided by the **client**, from
`EventDto.arrival_order`; this route sends whatever it is given, verbatim. Do not
reintroduce a resolution or inference step here — ADR 0089 records the
server-side version that was tried and why it cannot be made correct.)*

Clients call this route as a **second, fire-and-forget action alongside** the
existing internal device-state PUT — not instead of it. ADR 0048's
cross-Axon-device sync is unaffected and unmodified.

### Reuse existing triggers and debounce, add no new ones

Both clients already have exactly one place that knows "a room was
genuinely just read" and already debounces the corresponding device-state
PUT (`spawn_read_put` in the TUI, the flush path behind `advanceReadMarker`
on the web). The Synapse-facing send piggybacks on that same call site and
timer. No new trigger points, no second debounce mechanism. Actions that only
locally dismiss an unread badge (e.g. `RoomList`'s hover/click-to-clear) are
explicitly not "room opened" events and do not trigger a receipt send.

### Best-effort, fire-and-forget from the client's perspective

A failed Synapse receipt call must never block or surface as a user-facing
error for the local read-marker UX — it is logged and the client moves on.
This matches how ADR 0048 already frames device-state failures as
degraded-but-local-only. Server-side, the new route behaves like any other
endpoint with real HTTP status codes; it's the client's choice not to await
or surface failures that makes the feature best-effort, not the server
hiding them.

## Consequences

- Rooms read in Axon will show as read in any third-party Matrix client
  reading standard receipt/read-marker state from the homeserver.
- A new `ReadReceiptSender` trait and route join `axon-api`'s existing
  mutation surface (`MessageSender`/`send`, `send_media`), following the same
  trait-port-and-adapter shape for testability.
- `axon-sync`'s `SdkGateway` gains one new outbound call
  (`send_read_receipt`), reusing its existing room-resolution and
  error-mapping helpers.
- Both clients gain one additional network call at their existing
  room-read choke point; no new debounce timers, no new trigger points, no
  changes to ADR 0048's device-state semantics.
- Not in scope: per-thread read receipts, typing notifications, and
  Synapse-facing sends for purely-local badge-dismiss actions (e.g.
  `RoomList` hover/click-to-clear).
