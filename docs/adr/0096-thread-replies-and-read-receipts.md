# ADR 0096 — A thread view may name the room's read receipt

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

### The receipt has to move; the question is how far it may move

The naive patch is to let the room's receipt name a thread reply as soon as some view has displayed it.
That is not safe on its own.
A receipt with no thread scope acknowledges **everything at or before its target in stream order**, so naming an arrival-late thread reply also marks every main-timeline message below it read, including messages the user has never seen.
That is the "acknowledge what the client never displayed" failure ADR 0089 rejected a server-side resolver for; moving the same inference into the client does not make it true.

So the target may only advance past a thread reply when the client can honestly say it has displayed everything in between.
That condition is checkable, and the clients already compute most of it — which is what this ADR turns into a rule.

### Two facts about matrix-sdk 0.18, which rule out the tidier-looking fix

Both were read out of `crates/matrix-sdk/src/event_cache/caches/read_receipts.rs` at the `matrix-sdk-base-0.18.0` tag — the code behind ADR 0070's counter.

1. **A threaded (MSC3771) receipt is invisible to the room's unread counter.**
   `select_best_receipt` only ever admits a receipt whose scope is `Main` or `Unthreaded`:

   ```rust
   && matches!(receipt.thread, ReceiptThread::Main | ReceiptThread::Unthreaded)
   ```

   This is unconditional — it does not depend on any threading flag.
   An unthreaded receipt is therefore the only kind that can clear this badge today.

2. **Threading support is what would take thread replies out of the count.**
   `process_event` skips them outright when it is on:

   ```rust
   if with_threading_support && extract_thread_root(event.raw()).is_some() {
       return;
   }
   ```

   That flag comes from `ClientBuilder::with_threading_support(ThreadingSupport)`.
   Axon never calls it, so it runs at the builder's default of `Disabled`, which is why thread replies count toward the badge at all.

The first draft of this ADR proposed exactly that combination: enable threading support, and send per-thread receipts under MSC3771.
It was wrong, and the review question that killed it is worth recording, because the design reads well until you ask it.

**If the only new message in a room is in a thread, what does the room list show?**
Nothing.
`clients/web/src/components/RoomList.tsx` contains no reference to threads at all; the only per-room badge it renders is `rooms.unreadCount(key)`, which is `RoomDto.notification_count` and nothing else.
The thread-unread state that does exist is not a substitute: `threadUnread.count` drives one _global_ indicator in the app shell, and the store behind it is session-local — `reconcileSummary` runs only from `RoomPage`, for the room currently open, so a room the user has not opened this session contributes nothing after a reload.
Turning threading support on without first giving the room list something else to badge from would replace a stuck badge with no badge, on the very scenario in #207.

## Decision

**A room's unread count keeps counting thread replies, and a thread view may name the room's read receipt — gated so the receipt never runs ahead of what the client displayed.**

`ThreadingSupport` stays `Disabled`.
The receipt stays unthreaded, the route and its request body are unchanged, and the fix is entirely client-side.

### 1. The receipt target spans the whole room, not the main timeline

ADR 0089's rule is unchanged in substance and widened in scope: the target is the event with the greatest `arrival_order` **among all the events this client has displayed in this room** — main-timeline rows _and_ the members of any thread it has displayed.

The cross-device read marker (ADR 0048) does not widen with it.
It stays on the main timeline, in `origin_ts` order: it positions a "new messages" divider in the room stream, and a thread member has no position there.

### 2. The gate

A thread member may be named only when all of these hold for that room:

1. **The main timeline is at its live end** — the room view is unanchored and showing the newest events (`showingNewestEvents` in the web client).
2. **No thread in the room is unread** — the client's thread-unread set for that room is empty, which is true exactly when the user has read every thread it knows carries replies.
3. **The thread view is at its live end**, so its arrival-max member is the thread's newest, not the top of a page parked in history.
4. **The state the first three are read from has actually loaded** — thread summaries fetched, thread read markers hydrated.

Together these say: every event between the current receipt floor and the target has been displayed, either as a main-timeline row or as a member of a thread this client considers read.
That is the honest version of the claim an unthreaded receipt makes.

Condition 4 is not pedantry.
ADR 0070 records a fault of exactly this shape — a startup sweep pruned against a room list that had not loaded yet, and deleted the counts it existed to provide.
An empty thread-summary map means "not fetched yet" just as often as it means "no threads", and reading the second from the first opens the gate on a room the user has not seen.

When the gate is closed the receipt behaves exactly as it does today: it stops at the arrival-max **main-timeline** event, below the unread thread.
That is the correct answer in that state — there is genuinely unread content in the room.

### 3. `unreadThreadCutoff` inverts rather than disappears

The web client already computes the set this gate needs.
`unreadThreadCutoff` (`clients/web/src/pages/RoomPage.tsx`) today clamps the receipt target below the oldest unread thread reply; under this ADR the same set becomes condition 2.
Non-empty, and the clamp applies as it does now.
Empty, and the receipt may advance past thread members to the room-wide arrival-max.

Its clamp on the cross-device read _marker_ stays untouched — per § 1, that is a display question, and the answer to it does not change.

### 4. Client work, one silo per PR

There is no server change: no route, no DTO, no gateway, no `ClientBuilder` call, no migration.

- **Web.**
  `ThreadPanel`'s read effect calls `ephemeralSender.noteRead` with the thread's arrival-max displayed member, subject to the gate; the room-view effect keeps its own call, and the ephemeral sender's existing forward-only `arrival_order` floor merges the two without either needing to know about the other.
  The floor stays keyed per `(account, room)` — one room, one receipt.
- **TUI.**
  `read_targets_for` keeps computing the main-timeline target.
  Its thread-promotion machinery supplies condition 2, and its thread panel supplies a target the same way the web panel does.

`docs/client-parity.md` gains a row for the thread-scoped receipt target as each lands.

### 5. Where this is imprecise, and why that is acceptable

- **Reading one thread acknowledges the others.**
  An unthreaded receipt cannot say otherwise.
  But condition 2 means the receipt only ever advances past thread members when no thread is unread, so nothing is acknowledged that the user has not read — this is strictly more precise than what ships today, not less.
- **The gate trusts the thread-summary list.**
  `Store::room_threads` orders by `latest_reply_ts DESC` and caps at `RELATION_READ_CAP` (1000).
  A thread beyond that cap is, by construction, older than a thousand more-recently-active threads in the same room.
  If one of those carried an unread reply, the gate would open over it.
  Accepted: the alternative is paging every thread in a room before a receipt may advance.
- **The badge still cannot say _where_ the unread content is.**
  A room with one unread thread reply and a room with one unread main-timeline message look identical in the room list.
  That is a product gap, tracked as #9, not a correctness bug — and it is the reason § 6 exists.

### 6. What this defers, and what would have to be true to take it

Per-thread receipts (MSC3771) plus `ThreadingSupport::Enabled` remain the right long-term model: they are how Element and the wider ecosystem track thread reads, and they would let a room's count mean "unread in the main timeline" precisely.

They cannot be adopted until **the room list has a per-room thread-unread signal from the server** — because the moment `process_event` starts skipping thread replies, `RoomDto.notification_count` stops carrying them and nothing else in the room list does.
That signal needs axon to pair the threaded receipts matrix-sdk's state store already holds (`get_user_room_receipt_event` with `ReceiptThread::Thread(root)`) against its own `events` table, per thread, per room — bounded to recently-active threads, since an unbounded version is a per-thread lookup across 1755 rooms.

Recording the shape here so the next attempt starts from it rather than from the tidy-looking half:

- it is a milestone, not a follow-up PR;
- it changes what `RoomDto.notification_count` means, which is wire-visible with no schema change to announce it;
- and it must ship with its client consumers, not ahead of them.

Thread subscriptions (MSC4306 / MSC4308) are not a shortcut to it: matrix-sdk 0.18 reaches them through `get_thread_subscriptions_changes::unstable`, and axon's unread accounting should not rest on an unstable endpoint.

### Rejected: name the thread member with no gate

The two-line version.
Rejected because an unthreaded receipt acknowledges everything below its target in stream order, so it marks unseen main-timeline messages read — the failure mode in ADR 0089, reintroduced from the other side.

### Rejected: enable threading support now (the first draft of this ADR)

It clears the badge by making the counter stop looking at thread replies, and it is a fraction of the work.
It also makes the room in #207 render with no badge at all, since the room list badges from `notification_count` and nothing else.
See § 6 for what would have to land first.

### Rejected: a server-side receipt resolver

Already rejected in ADR 0089, for the reason that still holds: the server cannot prove what a client displayed.
Condition 2 of the gate is a fact about client state — which threads the user has opened — that exists nowhere else.

### What this does not change

- **ADR 0089 stands.** The client names the target by `arrival_order` among the events it displayed; the server does not resolve, substitute, or second-guess it.
- **`origin_ts` stays the display order.** Issue #133 is untouched.
- **Cross-device read markers (ADR 0048) stay on `origin_ts` and stay main-timeline.** They are a display artifact, not a Matrix receipt, and no unread count derives from them.
- **The read route stays best-effort.** `ReadReceiptRequest` is unchanged, failures still map to real HTTP statuses and still log at `warn`.
- **`RoomDto.notification_count` keeps its current meaning** — unread in the room, threads included.

## Consequences

- The room in #207 clears once its thread is read, and stays clear.
  A room whose only new activity is a thread reply still badges, which is the behaviour that made this route preferable to the first draft.
- A room stays badged until the user opens the thread, not merely the room.
  That is a visible behaviour change from "opening the room clears the badge until reload", and it is the honest one: before, the badge was lying in both directions.
- No server work, so no deployment coupling — each client fixes itself, and a client that has not been updated behaves exactly as it does today.
- Test seams: the gate is four conditions, and each deserves a case where it is the only one false — in particular the not-yet-loaded case, which is the one that fails open and is invisible in a test that hydrates everything first.
