# ADR 0096 — Threaded read receipts, and what a room's unread count means

## Context

A room whose arrival-newest events are all thread replies shows an unread badge that no amount of reading can clear.
Opening the room drops the badge for the session; the next load brings it back, forever.
Reported in #207, observed on a live account in `!WAeSIcZSBicSKBvREs:bostoncoop.net`:

| `events.id` (arrival order) | type             | `rel_type` | `origin_ts`   |
| --------------------------- | ---------------- | ---------- | ------------- |
| 2852599                     | `m.room.message` | `m.thread` | 1787150654157 |
| 2852598                     | `m.room.message` | `m.thread` | 1787150631167 |
| 2852597                     | `m.room.message` | `m.thread` | 1787150603263 |
| 2852586                     | `m.room.message` | `m.thread` | 1787149590342 |
| 2852585                     | `m.room.message` | —          | 1787149537106 |

`room_unread_counts` held `notification_count = 2` for that room, and the server log showed three successful `POST …/rooms/{room_id}/read` calls against it with no failures.
The receipts were sent, they succeeded, and they named event 2852585 — the newest event the client was allowed to name.

Every layer involved is individually correct.

- **ADR 0067** sends one receipt per room, from the client's existing debounced read choke point.
- **ADR 0089** makes the client name the event with the greatest `arrival_order` **among the events it has displayed**, because only the client knows what it displayed.
- **ADR 0070** takes the badge from matrix-sdk's client-side, receipt-derived counter, so axon never maintains a second read-position model.
- **Both clients hide thread members from the main timeline.**
  The web client's `isVisibleTimelineEvent` (`clients/web/src/pages/RoomPage.tsx`) rejects any event with a thread root, and the TUI's `read_targets_for` (`clients/tui/src/app/read_markers.rs`) filters through `should_show_event` **and** `thread_visible` for exactly the reason ADR 0089 gives.

Compose them and the room is trapped.
A thread reply is never displayed in the main timeline, so it is never a legal receipt target; the receipt therefore stops below it; matrix-sdk keeps counting it; every fresh load re-badges the room.
Neither client has any other path to a receipt — `ThreadPanel` advances the `thread_read_markers` device state (ADR 0048) and clears its local unread-thread entry, but never calls `ephemeralSender.noteRead`, and the only `noteRead` call site in the web client is `RoomPage`.
Third-party clients show the room unread too, because the receipt on the homeserver genuinely does not cover the replies.

This is the same _shape_ as ADR 0089 — the count was right and the receipt named the wrong event — with a different origin.
There the divergence came from a bridge backfilling older `origin_ts` values; here it comes from the client's own thread filter, which means it fires in any room where a thread is the most recent conversation, bridged or not.

### The obvious patch is wrong

Letting the room's receipt name a thread reply once the panel has displayed it looks like a two-line fix.
It is not safe.
A receipt with no thread scope acknowledges **everything before its target in stream order**, so naming an arrival-late thread reply also marks every main-timeline message below it read, including messages the user has never seen.
That is precisely the "acknowledge what the client never displayed" failure ADR 0089 rejected a server-side resolver for; doing it in the client instead does not make it true.

### Two facts about matrix-sdk 0.18 that shape the design

Both were read out of `crates/matrix-sdk/src/event_cache/caches/read_receipts.rs` at the `matrix-sdk-base-0.18.0` tag, the code behind ADR 0070's counter.

1. **A threaded receipt is invisible to the room's unread counter.**
   `select_best_receipt` only ever admits a receipt whose scope is `Main` or `Unthreaded`:

   ```rust
   && matches!(receipt.thread, ReceiptThread::Main | ReceiptThread::Unthreaded)
   ```

   So sending MSC3771 threaded receipts, on its own, would change nothing about the badge.

2. **Threading support is what takes thread replies out of the count.**
   `process_event` skips them outright when it is on:

   ```rust
   if with_threading_support && extract_thread_root(event.raw()).is_some() {
       return;
   }
   ```

   That flag comes from `ClientBuilder::with_threading_support(ThreadingSupport)` — "this will affect how timelines are setup, how read receipts are sent and how room unreads are computed".
   Axon never calls it, so it runs with the builder's default of `Disabled`, which is why thread replies count toward the badge today.

Neither piece is sufficient alone, which is why this ADR takes both.

## Decision

**Adopt MSC3771 threaded receipts end to end, and enable matrix-sdk's threading support, so that the receipts axon sends and the counter axon reads agree on what a room's unread count means.**

A room's unread count comes to mean _unread in the main timeline_.
Thread unread is tracked per thread, acknowledged per thread, and never folded back into the room's number.

### 1. Two receipt scopes, both chosen by the client

- **The room view** names the greatest-`arrival_order` event among the main-timeline events it displayed, and sends it as `ReceiptThread::Main`.
- **A thread view** names the greatest-`arrival_order` event among the members of that thread it displayed, and sends it as `ReceiptThread::Thread(root_event_id)`.
- **The `m.fully_read` marker stays unthreaded**, and stays on the room path only.
  The spec and the SDK both require `Unthreaded` for `FullyRead`, and it doubles as the read position a thread-unaware client can still make sense of.

ADR 0089's rule is unchanged; it now applies once per scope rather than once per room.
The server still sends the id it is given, verbatim, and still resolves nothing.

### 2. Wire shape

`ReadReceiptRequest` gains an optional `thread_root: Option<String>`.
Absent means the room's main timeline — today's callers keep working unchanged, which matters because the TUI and the web client will move in separate PRs.

`SdkGateway::send_read_receipt` gains the same parameter.
The thread path cannot use the existing batch call: `send_multiple_receipts` maps to `POST /rooms/{roomId}/read_markers`, whose body has no `thread_id` field at all — MSC3771 added thread scope only to `POST /rooms/{roomId}/receipt/{receiptType}/{eventId}`.
So:

- **Room path** — `send_single_receipt(FullyRead, Unthreaded, id)` plus `send_single_receipt(Read, Main, id)`.
  Two calls where there is one today, because a `Main` receipt cannot ride the `read_markers` batch either.
- **Thread path** — `send_single_receipt(Read, Thread(root), id)`, and no fully-read marker.

Both stay best-effort and fire-and-forget on the client side, and both keep ADR 0067's structured success/failure logging — a receipt that never lands is still the failure that reads to a user as "this room will not stop showing unread".

### 3. Enable `ThreadingSupport::Enabled { with_subscriptions: false }`

In `client_builder` (`crates/axon-sync/src/client.rs`), for every production client.

`with_subscriptions` stays off deliberately.
Thread subscriptions ride `get_thread_subscriptions_changes::unstable` (MSC4306 / MSC4308), and an unstable endpoint is not something axon's unread accounting should depend on.

Consequences of the flag, all intended:

- Thread replies stop inflating `RoomDto.notification_count` / `highlight_count`.
- The user's own thread messages stop acting as an implicit room read receipt.
- Rooms currently stuck with a phantom badge self-heal through ADR 0070's existing watcher — the next notable update, or the five-minute re-sweep, recounts without the thread replies and upserts the lower value.
  No migration, no manual reset, nothing to backfill.

### 4. Per-thread unread stays a client concern

Axon gains no server-side per-thread read-position model.
ADR 0070's rule holds: matrix-sdk stays authoritative for room counts, and axon does not build a second read-position store over the `events` table.

The web client already has the per-thread half — `thread_read_markers` device state (ADR 0048), reconciled against thread summaries — and it is cross-device precisely because it lives in axon's device state.
What changes is that the threaded receipt is written from the _same_ choke point as that marker, so axon's own model and the homeserver stop disagreeing.

Exposing a per-thread unread count on the wire (a `RoomDto` field, or a frame) is deliberately **not** in this ADR.
It would need either the model this ADR declines to build or the unstable subscriptions endpoint, and no client is blocked on it: the TUI's gap here is #9, not this.

### 5. What each client owes, in its own PR

One silo per PR, so this lands as server first, then the two clients; `docs/client-parity.md` gains an "outbound threaded read receipts" row as they do.

- **Web.**
  `ThreadPanel`'s read effect calls `ephemeralSender.noteRead` with the thread's arrival-max displayed member and its root.
  The sender's forward-only floor becomes per `(account, room, thread)` rather than per `(account, room)`.
  `unreadThreadCutoff`'s clamp on the _receipt_ target goes: it exists only because an unthreaded room receipt would swallow an unread thread, which a `Main` receipt no longer does.
  Its clamp on the cross-device read _marker_ stays — that is a display question about where the "new messages" divider sits, not a receipt question.
- **TUI.**
  `read_targets_for` keeps its main-timeline filter, which stops being a trap and becomes merely correct.
  A promoted thread reply — one the main timeline does display — must now be acknowledged as a thread receipt rather than named in the room's `Main` receipt, so the promotion set feeds the thread path, not the room path.

### Rejected: name the thread reply in the room's receipt

The two-line patch, rejected for the reason given in Context: an unthreaded or `Main` receipt naming an arrival-late thread reply acknowledges main-timeline messages the user never saw.
It also would not have worked as a badge fix without the threading flag, since the count includes thread replies either way.

### Rejected: enable threading support and stop there

This clears the badge — the counter simply stops looking at thread replies — and is a fraction of the work.
It is a trap of its own.
Nothing would ever acknowledge a thread reply to the homeserver, so a thread's read position would exist only in axon's device state: Element would show the thread unread forever, axon would show it read, and the two would never converge.
It also quietly makes the room badge under-report for anyone who lives in threads, with no per-thread signal anywhere to make up for it.

### Rejected: server-side per-thread unread counts

Either axon builds the read-position model ADR 0070 exists to avoid, or it depends on an unstable endpoint.
Neither is worth it for a signal the web client already computes correctly from data it already has.

### What this does not change

- **ADR 0089 stands.** The client names the target by `arrival_order` among the events it displayed; the server does not resolve, substitute, or second-guess it.
- **`origin_ts` stays the display order.** Issue #133 is untouched.
- **Cross-device read markers (ADR 0048) stay on `origin_ts`.** They are a display artifact, not a Matrix receipt, and no unread count derives from them.
- **The read route stays best-effort.** Failures still map to real HTTP statuses and still log at `warn`.

## Consequences

- The room in #207 clears itself once the watcher re-sweeps, and the class of room it represents stops accumulating.
- **`RoomDto.notification_count` changes meaning** — main timeline only.
  This is a wire-visible semantic change with no schema change to announce it, so it belongs in the OpenAPI field docs, in `docs/client-parity.md`, and in the ADR trail from 0070.
  A thread-heavy room will show a quieter badge than it does today.
- The room-read path costs two homeserver round trips instead of one.
  It is debounced at 800 ms and fire-and-forget, so the cost is bounded and off the user's critical path.
- Threaded receipts are stable Matrix 1.4, so any current Synapse serves them.
  A homeserver that rejects `thread_id` fails the send and surfaces through the existing `warn` line rather than silently degrading.
- Other users' thread-unaware clients may stop rendering our read avatar in a room, since our public `m.read` becomes `Main`-scoped.
  That is cosmetic, it only affects what _other people_ see of us, and the unthreaded `m.fully_read` marker still carries a room-level position for our own clients.
- Test seams: the API gains a `thread_root` round-trip test, the gateway a scope-selection test, and the server smoke lane's unread assertions need to account for thread replies no longer counting (#202 is already flaky in that lane and should be fixed before it is leaned on).
