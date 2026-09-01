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

A thread member may be named only when all of these hold:

1. **The room stream is at its live end** — unanchored, and its loaded slice reaches the present.
2. **That stream has actually loaded.**
   Not implied by the first: `atEnd` starts `true` on a cold store, because "nothing newer is known to exist" is vacuously true before the first page lands.
3. **The thread view is at its live end**, so its arrival-max member is the thread's newest, not the top of a page parked in history.
4. **The named member sits below the ceiling** — the first _unread_ reply above the room view's own target belonging to a thread this panel is not showing.
   A reply the user has already read (a thread marker covers it) is not an obstruction; one with no marker still is, which is the never-opened case.
   Without that exemption the bound is permanent in any room with two interleaved threads: whichever panel is open, the others' replies sit in the window, and reading them all changes nothing — so the badge can never clear, which is exactly what a live test room did.

Conditions 1-3 say the two views have displayed what they are claiming.
Condition 4 is the one that took two attempts, and § 3 is about why.

When the gate is shut the receipt behaves exactly as it does today: it stops at the arrival-max **main-timeline** event.
Condition 4 does not shut it so much as bound it — the pick stops at the last member below the ceiling rather than being abandoned, because the members below a foreign reply are still honest to acknowledge.

### 2b. The room view closes the room out, not just the panel

A thread panel names only its own thread's members, and only while it is open.
That is not enough on its own, and the gap is a sequencing one that a live account found (#207).

Two threads, read in the wrong order: opening the newest first names nothing, because the older thread's reply still holds the bound down; opening the older one next names only _its_ reply.
The newest thread is eligible by then — but its panel is closed, and nothing revisits it.
The room stays one event short of clear, permanently, with every thread in it read and the unread-threads list correctly empty.

So the **room view's** own target extends past the main timeline too: over thread replies it knows are read, up to the same bound.
That claim keeps ADR 0089's rule rather than bending it — a thread marker exists because some client of this user displayed those replies, this panel earlier or Element via `connectThreadReceipts`.

### 3. The question is a window, not the whole room

The first implementation asked "has the user read every thread in this room?", and it was wrong in a way no unit test caught and a dev server found in minutes: in the room from #207 the answer is permanently **no**, so the receipt never advanced and the badge never cleared.

Two compounding reasons, both instructive.

A real room is full of threads nobody has opened _this session_, and a thread's read position comes from a marker that mostly does not exist yet.
Worse, the fallback is poisoned in exactly this shape of room: a thread with no marker of its own falls back to the room's, and the room marker is seeded from `RoomDto.last_event_id` — `MAX(origin_ts)` over every event, replies included — so when the room's newest event _is_ a reply, opening the room parks the marker on that reply, and it then answers "read" for the thread it came from.
Withholding the marker in that case (the second attempt) makes every thread's state `'unknown'` instead of falsely-read, which is more honest and equally unopenable.

The mistake was the question.
A receipt acknowledges everything at or below its target in arrival order, and the room view has _already_ named the arrival-max event it displayed — so everything at or below that is acknowledged whether or not a thread view does anything.
Extending to a thread member only adds the window above it.
Older threads sit below that window entirely: their replies were acknowledged by the room's own receipt, and asking whether the user has read them is asking about a receipt that has already been sent.

So the only threads that matter are ones with a reply **inside the window**, and the client can see them directly — the room timeline it holds contains thread replies, which is what makes this checkable without any read-position model at all.
The ceiling is the lowest such reply; the panel names the highest member below it.

That the gate consults no marker, no summary, and no unread state is the point, not an accident.
Every one of those is a model of what the user has read, and every model of that was wrong here in a different way.
The window is a fact about the events in hand.

### 4. Client work, one silo per PR

There is no server change: no route, no DTO, no gateway, no `ClientBuilder` call, no migration.

- **Web.**
  `ThreadPanel`'s read effect calls `ephemeralSender.noteRead` with the thread's arrival-max displayed member, subject to the gate; the room-view effect keeps its own call, and the ephemeral sender's existing forward-only `arrival_order` floor merges the two without either needing to know about the other.
  The floor stays keyed per `(account, room)` — one room, one receipt.
  Condition 3 lands as the panel's own `showingNewestReplies`, the exact pair of checks the room stream derives as `showingNewestEvents`, and closing **#154** is a prerequisite rather than a bonus: that issue is the panel claiming read state from a view parked in history, and every condition here is worthless if the panel's own claim is not honest first.
- **TUI.**
  `read_targets_for` keeps computing the main-timeline target.
  Its thread-promotion machinery supplies condition 2, and its thread panel supplies a target the same way the web panel does.

`docs/client-parity.md` gains a row for the thread-scoped receipt target as each lands.

### 5. Where this is imprecise, and why that is acceptable

- **Reading one thread acknowledges the others below it.**
  An unthreaded receipt cannot say otherwise, and this is true of the room view's own receipt today — extending it to a thread member changes nothing about the events below that receipt's existing floor.
  What the ceiling protects is the window the extension actually adds.
- **A backfilled reply is invisible to the summary check, and the summary cannot be made to show it.**
  Replies the client has not loaded are covered by comparing the thread summary's `latest_reply_ts` against its read position — the only signal there is for them.
  That misses a reply stamped _older_ than the thread's current newest, because `MAX(origin_ts)` does not move and neither does `latest_reply_event_id`; only `reply_count` changes.
  Such a reply still has a recent arrival position, so if it falls below the extension's target the receipt acknowledges it unseen.
  Closing it needs `MAX(events.id)` per thread on the summary — filed as #234, with the reasoning for why no client-side comparison substitutes.
- **The ceiling can only see the slice the client holds.**
  A reply paged out of the room timeline, or one backfilled with an old `origin_ts` and a high `arrival_order`, is outside it and would be acknowledged unseen.
  That is the same coarseness the room's own receipt already has — the client names a target and the homeserver clears everything below it, loaded or not — and closing it means a read-position model per thread, which is § 6.
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
- **The room-list badge waits for the room's threads to be read**, rather than clearing the moment the room opens.
  Entering a room does not read its threads, and the old optimistic clear produced a room showing no badge in the list while the Threads button showed unread — the two surfaces contradicting each other about one room.
- **The room now tells the user which thread is unread**, which it did not before and which the receipt fix alone would not have given it.
  The summary-derived marker no longer seeds from a `last_event_id` that the loaded slice shows to be a thread member, so `reconcileSummary`'s fallback stops answering "read" for the thread it came from, and the root row's "New" chip and the Unread threads panel light up.
  Getting there took two corrections that only a live instance produced, both worth keeping:
  - **The check needs a slice to look at.** The room list is restored from IndexedDB (ADR 0085 phase 2) and is on screen before the first timeline page exists, so the effect ran with `loadedEvents: 0` and seeded the marker from the reply anyway. It is now gated on `timeline.loading`, which starts `true` on a cold store — not on the slice being non-empty, because a loaded-but-empty or gap-filled slice is exactly what this effect exists to serve.
  - **Preventing new poisoning does not heal old poisoning.** `advanceReadMarker` is forward-only on `origin_ts`, so every account that ever opened such a room carries a marker parked on a reply, durably, in device state. The fallback therefore withholds a room marker whose event the slice shows to be a thread member — and _replaces_ it with the display-last event the main timeline actually rendered, because withholding alone yields no read position, and `reconcileSummary` records nothing when it has nothing to compare against. That heals on the next load, with no migration.
    That change is strictly conservative in the direction that matters: the marker advances _less_ far, so every consumer of it — the sibling-device badge clear, `unreadThreadCutoff`, the receipt ceiling, the "new messages" divider — claims less, never more.
    What it costs is more persistent unread, with one sharp edge: a thread read in Element would otherwise stay flagged here forever.
- **So axon consumes the thread-scoped receipts other clients send** (MSC3771), which arrive verbatim through ADR 0056's passthrough and were previously parsed away.
  Axon's own receipts are always unthreaded, so a `thread_id` on this user's own receipt can only have come from another client — there is no echo to suppress.
  It is recorded as a durable `thread_read_markers` entry rather than an in-memory clear, because receipts are live-only and never replayed: a session-scoped clear would come back on the next reload, which is the complaint it exists to answer.
  This is also a better source of truth than the marker fallback ever was — it is what the homeserver actually knows about what the user read.
  That path is live-only — nothing backfills the receipts Synapse already holds, so a thread read in Element before the tab opened stays unread here and blocks the room until it is opened again (#213).
  What remains of #209 is the cross-room half: the unread-thread store is fed from `RoomPage` and from live frames, so on a cold load it knows only about rooms visited this session.
- **Flushing device state on `visibilitychange` changes timing elsewhere, not just durability.**
  Debounced writes had no unload flush at all, so a reload inside the 800 ms window dropped them — a thread just opened came back unread, and drafts had always had the same exposure.
  `connectDeviceStateFlush` closes that, and in doing so it usually leaves ADR 0087's pre-reload flush with nothing pending.
  That flush used to await a network round trip; now it resolves immediately, so an auto-refresh reload can begin _synchronously_ inside the `visibilitychange` handler.
  Nothing user-visible depends on the delay, but anything driving that event has to tolerate the navigation starting at once — `e2e/update-refresh.spec.ts`'s away/return helper did not, and failed on CI while passing locally.
  `visibilitychange` is shared infrastructure; a listener added to it is not local in effect.

- **A thread read marker records two positions.**
  Review found that deriving the arrival one from the display one reproduces this ADR's own bug through a second door: a backfilled reply is display-early and arrival-late, so the marker understated what the panel had shown, the receipt path read that as "still unread", and the room could never be claimed past it.
  `arrivalThrough` is now written alongside `eventId`/`originTs` from one filtered set of shown replies, and the two advance independently.
  Markers written before the field parse it as `null` — no arrival evidence, which blocks rather than claims.

- Test seams, and a caution about them:
  - Every gate condition needs a case where it is the only one false, and each such test must be checked to fail when _its_ condition alone is removed.
    Verifying against the unfixed code is not enough: the first five tests written here all passed against the unfixed client, because with no thread receipt implemented at all, "sends nothing" is true for every reason at once.
  - **A suite of "must not send" tests will happily certify a gate that never opens.**
    The version this ADR rejected passed seven of them and reached a dev server that never cleared a badge.
    The must-send cases carry the weight, and they have to be built from a room shaped like a real one — several threads, most never opened — not from the minimal fixture that reproduces the bug.

## Addendum (false-positive unread threads on stale accounts)

Confirmed on the production instance: the Unread threads drawer fills with
threads whose latest reply is months or years old and which contain nothing new.
This ADR's Consequences section welcomed the drawer lighting up — "the root row's
'New' chip and the Unread threads panel light up" — without bounding _which_
threads it lights up for. On an account whose room marker is old (or has been
withheld and replaced with the display-last main-timeline event, as this ADR
itself does for a marker parked on a thread member), `reconcileSummary`'s
room-marker fallback flags **every** thread that received a reply after the
room's last main-timeline message, however long ago. In a room where the
conversation moved into threads and the main channel went quiet — the #207 shape
— that is most of the thread list.

Two mechanisms feed it:

- **The reconcile fallback.** With no per-thread marker, `reconcileSummary`
  compares `latest_reply_ts` against the room marker, which is a main-timeline
  position by construction (§1). Any post-main-timeline reply clears that bar
  regardless of age. Run over up to 1000 summaries per `GET /threads` on every
  room open, this accumulates the whole historical thread backlog into a global
  drawer as the user browses.
- **The live path.** `connectLiveThreadUnread` → `recordLiveEvent` had no
  age gate. After an axon restart or a gappy sliding-sync resume, the initial
  per-room sync window is replayed on the live bus (only back-pagination is
  suppressed server-side, `axon-sync/src/engine.rs`), so a dormant room
  re-delivers its last reply as "just arrived" and badges a years-old thread.

### The stopgap

- **Live gate.** `connectLiveThreadUnread` drops a `timeline.event` stamped
  before the live connection was wired, minus a five-minute slack for homeserver
  clock skew and the gap to the first frame. Real-time replies badge; dormant
  replays do not. `recordLiveEvent` stays an unconditional primitive — it has no
  read position to weigh against, so freshness is the connection layer's to
  enforce.
- **Recency window on the room-marker fallback (route a′).** When the read
  position comes from the room marker (no per-thread marker), `reconcileSummary`
  promotes only a reply within a 14-day window of now. A per-thread marker is an
  exact position and is exempt — it keeps promoting whatever the gap's age,
  which is the case this ADR added and which is correct. Clearing is unchanged.

Both are strictly conservative in the same direction as this ADR's marker
withholding: the drawer claims _less_, never more. The live gate and the recency
window are injected clocks (`createThreadUnreadStore(now)`,
`connectLiveThreadUnread(…, now)`) so fixed-date fixtures can anchor them; the
`RoomPage` "must-send" tests here encoded the eager fallback as intended and were
rewritten with an anchored clock — the same test caution this ADR already
records, from the other side.

### What the stopgap does not cover

A thread the user has **never opened on this account**, whose only reply arrived
while they were offline more than 14 days ago, will not surface: there is no
per-thread marker, and the window has passed. The client has no per-room "caught
up as of" wall-clock (the read marker stores an event `origin_ts`, not a visit
time) and cannot get one without new per-device persisted state that is itself
silent until a room's first post-fix visit. This is the case §6's server-side
per-room thread-unread signal is for; it remains the real fix, and the recency
window and live gate are removable (or demotable to a pure offline fallback)
once it lands.
