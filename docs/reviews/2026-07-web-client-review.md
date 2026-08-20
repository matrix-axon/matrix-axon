# Web client code review — July 2026 (M-W1 → M-W8)

Full-codebase review of `clients/web` at `web/m-w8-media` (20c727e), covering
correctness, redundancy, performance, security, and readability. Scope: all
hand-written source (~8.5k lines), unit tests, and Playwright e2e; the
generated `src/api/schema.d.ts` was excluded (verified referenced only, never
hand-edited). Findings are judged against the project's own rules
(`clients/web/AGENTS.md`, ADRs 0046/0061–0064, TUI parity).

## Summary

**No P0s.** The security surface is in genuinely good shape: the DOMPurify
configuration is careful and well-tested (the mxc-only `<img>` hook, hex-only
color validation, class stripping, linkify-after-sanitize are all covered by
`sanitize.test.ts`), the WS token never rides in an echoed subprotocol, media
refcounting is sound, and the settings parser is properly defensive. The
architecture (service graph, signal stores, windowed room list) is coherent
and unusually well-commented.

The real themes are:

1. **One rendering bug** (unkeyed timeline fragments) that RoomList's own
   comments already warn about.
2. **A systemic robustness gap**: fire-and-forget API calls with no rejection
   handler, which is also what makes `pnpm test` exit non-zero today.
3. **Timeline concurrency**: pagination and reconnect gap-fill can interleave
   destructively.
4. **Live-update gaps** left over from M-W6: threads and the room list don't
   consume the socket.

| Severity | Count | Meaning                                                    |
| -------- | ----- | ---------------------------------------------------------- |
| P1       | 4     | bug or broken gate; fix before further feature work        |
| P2       | 10    | likely bug, real perf/UX weakness, or maintainability debt |
| P3       | 6     | polish                                                     |

Top five: WCR-01, WCR-02, WCR-03, WCR-04, WCR-06.

### Gate results (evidence, recorded 2026-07-10)

| Gate                      | Result                                                                                                                    |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `pnpm test`               | **exit 1** — 301 passed, 2 skipped, but 3 unhandled MSW rejections from `RoomPage.messaging.test.tsx` (see WCR-02/WCR-04) |
| `pnpm lint`               | pass                                                                                                                      |
| `pnpm format:check`       | **exit 1** — `src/stores/members.ts` (line 58 exceeds print width)                                                        |
| `pnpm build` (tsc + vite) | pass                                                                                                                      |

---

## P1 findings

### WCR-01 · correctness · Timeline rows render as unkeyed fragments

`src/pages/RoomPage.tsx:476-507`

`visible.map(...)` returns a bare `<>…</>` per event, holding an optional
day-separator `<li>` plus the keyed `<EventRow>`. The keys sit on the
fragment's _children_; the fragments themselves — the actual list children —
have none, so Preact reconciles the list **by index**. Two consequences:

- `loadOlder()` prepends a page, shifting every index: per-row `useState`
  (open reaction picker, **"Confirm delete"**, open edit-history modal)
  stays at its position and attaches to a _different message_. Concretely:
  click Delete on a message, scroll up so a page prepends, and the
  confirm-delete now sits on whatever message landed at that index.
- Every prepend (and every live append that reorders separators) rebuilds
  the whole list's DOM instead of moving existing nodes.

`RoomList.tsx:540-543` documents this exact trap ("Wrapping each in a
fragment put the key on the fragment instead, so Preact matched rows by
position…") — the room list fixed it; the timeline didn't.

**Fix:** import `Fragment` and key it: `<Fragment key={event.event_id}>`,
keeping the inner keys or dropping them.

### WCR-02 · robustness · Fire-and-forget API calls have no rejection path

Systemic; the pattern is `void api.GET(...).then(({data}) => …)` or an
un-try/caught `await api.GET(...)` on a path outside `mutate()`:

- `src/stores/timeline.ts:203-216` (`resolveReplyTargets` by-id fetches)
- `src/stores/timeline.ts:248-260` (`refreshEvent` — **awaited by
  `edit`/`redact`/`toggleReaction` after `mutate()` returns**, so a network
  drop between the mutation and the refetch rejects the promise the UI
  discarded with `void`)
- `src/stores/rooms.ts:48-88` (`fetchTitle` rejections propagate through
  `Promise.all` in `resolveUnnamedTitles`, which `refresh` fires with `void`)
- `src/stores/threads.ts:54-70` (`resolveRoots`)
- `src/stores/device-state.ts:136-157, 159-176` (`fetchScope`, `flush`'s PUT)
- `src/components/EditHistory.tsx:38-57`, `src/pages/RoomPage.tsx:113-123`
  (deep-link event fetch)

`openapi-fetch` rejects (rather than returning an `error` envelope) whenever
`fetch` itself fails — server unreachable, connection dropped mid-flight —
which is precisely the reconnect/offline scenario this client otherwise
handles carefully. Each of these becomes an unhandled promise rejection.
This is not hypothetical: the three unhandled MSW rejections that make
`pnpm test` exit 1 originate here (requests still in flight after the
messaging tests tear their handlers down).

`src/pages/ServerStatus.tsx:19-35` is the one call site that does it right
(a second rejection callback) — the pattern to copy, or better: one small
helper (e.g. `swallowNetwork(promise)` or a `fetchOrNull` wrapper) so every
background read/write funnels through a single rejection policy.

### WCR-03 · correctness · Timeline pagination races the reconnect gap-fill

`src/stores/timeline.ts:219-241 (replaceSlice), 560-574 (loadOlder)`

No request-generation guard exists, so two interleavings corrupt the slice:

1. **`loadOlder` in flight when `loadLatest` (reconnect gap-fill,
   RoomPage.tsx:181-186) replaces the slice.** The older page was fetched
   against the _old_ cursor; its `events.value = [...page.events, ...]`
   prepend lands on the _new_ newest-page slice — old events sit directly
   above the newest 50 with a silent gap, and `nextCursor` now points at
   the old walk. Reproduce: scroll back two pages, drop the socket, click
   "Load older messages" while the reconnect fires.
2. **Two `replaceSlice` calls racing** (date jump while the gap-fill is in
   flight): last-response-wins, which may be the older request.

**Fix:** a monotonically increasing generation token captured per request;
discard results whose generation is stale. (~10 lines, fixes both.)

### WCR-04 · process · The definition-of-done gates are red on the branch tip

`pnpm test` exits 1 (unhandled rejections, WCR-02's symptom — the suite
itself is green) and `pnpm format:check` fails on `src/stores/members.ts:58`.
AGENTS.md's definition of done requires all four gates green; CI on this
branch should be failing. The prettier fix is `pnpm format`; the test exit
follows from fixing WCR-02 (or, tactically, awaiting/cancelling in-flight
work in the messaging tests' teardown).

---

## P2 findings

### WCR-05 · UX-correctness · Reconnect gap-fill discards scroll-back

`src/stores/timeline.ts:219-241`, `src/pages/RoomPage.tsx:181-186`

`loadLatest()` on reconnect replaces the whole slice with the newest page.
A user reading three pages back is silently teleported to the newest 50
events (the quiet-gapfill fix stopped the _blanking_, not the truncation).
Consider: keep the loaded slice and refetch only the head page, merging by
event id (the `ingestLive` reconcile shape), falling back to replace only
when the head page doesn't overlap the loaded slice.

### WCR-06 · feature gap · Open threads never update live

`src/components/ThreadPanel.tsx` (whole file)

Only RoomPage subscribes to the socket, and it filters thread members out of
the main timeline. The thread store (`createTimelineStore(..., rootId)`) has
a working `ingestLive`, but nothing feeds it, and nothing watches
`live.reconnects` for the panel. An open thread goes stale until closed and
reopened — the one surface where a user is most likely waiting on a reply.
Mirror RoomPage's two effects (subscribe + gap-fill) inside ThreadPanel.

### WCR-07 · correctness · Echo reconciliation can drop or duplicate a send

`src/stores/timeline.ts:294-298, 437-445`

Two related weaknesses in the own-send race:

- `ingestLive` filters out **every** pending echo matching sender+body. Send
  "ok" twice quickly: the first frame removes both echoes. The second send's
  reconcile then no-ops (its `localId` is gone), so if the second frame is
  lost — the bus is explicitly lossy — the second message vanishes from
  view despite being sent. Remove only the first match.
- `send()`'s reconcile maps `localId → fetched event` without checking
  whether that `event_id` is already in the slice (appended by the live
  frame when `confirmsEcho` failed to match — e.g. `senderId` was undefined
  because rooms hadn't loaded, so the echo's sender is `''`). Result: two
  rows with the same `event_id`/key. Guard: if the confirmed id already
  exists, _remove_ the echo instead of replacing it.

### WCR-08 · feature gap · The room list never updates during a session

`src/stores/rooms.ts`, `src/components/RoomList.tsx:98-100`

`rooms.refresh()` runs once when RoomList mounts (and RoomList stays mounted
for the app's life), plus RoomPage's populate-if-empty. No live frame or
reconnect touches the store, so for the whole session: `last_activity_ts`
freezes (the default "recent" sort stops being recent — unread badges move
but rows don't), newly joined rooms never appear, and renames don't land.
Cheapest fix: bump the matching room's `last_activity_ts` on each
`timeline.event` frame and re-`refresh()` on `reconnects` changes.

### WCR-09 · correctness · `?event=` deep links: no reaction in-room, no reveal

`src/pages/RoomPage.tsx:104-128, 431-436`

- The load effect deliberately depends on `[timeline]` only, so navigating to
  `?event=<id>` while the room is already open changes `highlighted` but
  never jumps. M-W10's search results will hit this constantly.
- Even on a cold deep link, after `jumpTo` the Timeline's mount effect
  scrolls to the _bottom_ of the page; nothing scrolls the highlighted row
  into view, so the target event is usually off-screen.

Fix together: react to `highlighted` changes (jump when the id isn't in the
loaded slice), and after load, `scrollIntoView` the `[data-event-id]` row.

### WCR-10 · UX-correctness · A failed edit loses the user's text

`src/pages/RoomPage.tsx:341-357`, `src/components/Composer.tsx:94-102`

`onSubmit` clears the action (banner) immediately and the composer clears its
draft before awaiting. For sends that's covered by the retryable failed echo;
for **edits** there is no echo — a failed `PUT` leaves only the error banner,
and the edited text is unrecoverable. Either keep edit mode on failure
(`timeline.edit(...).then(ok => !ok && restore)`) or reuse the failed-echo
pattern for edits.

### WCR-11 · consistency · Composite-key separators disagree with their comments

`src/stores/device-state.ts:15` — `SEP = ' '` under a comment saying "NUL
never appears in ids/namespaces"; the code uses a **space**, not NUL.
`media-service.ts:96` uses `'\0'`; `room-list.ts` uses `'/'` (justified).
Today nothing collides (Matrix ids can't contain spaces), but the comment
documents a guarantee the code doesn't implement. Use `'\0'` (and note that
`scopeKey`'s `split(SEP)` in the reconnect effect must keep working).

### WCR-12 · robustness · A failed device-state PUT silently loses the batch

`src/stores/device-state.ts:159-176`

`flush()` removes the batch from `pending` before the PUT settles and never
re-queues on failure — a draft or read-marker written while the server was
briefly unreachable is lost (local cache keeps it, so the loss surfaces only
on the next device/session). Re-queue the batch (merging with any newer
writes) on rejection or non-2xx, with the debounce as natural retry pacing.

### WCR-13 · performance · `sortRooms` does per-comparison work on a hot path

`src/stores/room-list.ts:126-149`, `src/components/RoomList.tsx:580-616`

The comparator calls `roomKey` + `pinned.indexOf` (O(pins)) and
`title().toLowerCase()` per _comparison_ — O(n log n) string builds and
linear pin scans on ~1,600 rooms, re-run on every RoomList render, which
includes each incremental DM-title arrival during `resolveUnnamedTitles`
(dozens of full sorts right after load). Decorate once (key, rank, lowered
title), sort the decorated array, unwrap. The unread work is already kept
off this path; this closes the remaining gap.

### WCR-14 · a11y · EditHistory lacks the modal focus contract Lightbox has

`src/components/EditHistory.tsx`

Lightbox's comment says it follows "the existing modal focus/Escape pattern
(`EditHistory`): … focus is saved and restored, and Tab is trapped" — but
EditHistory implements only the Escape half. No initial focus, no restore on
close, no Tab trap: keyboard focus stays behind the overlay. Extract the
save/restore + trap from Lightbox into a small `useModalFocus` hook and use
it in both (ShortcutsHelp too).

---

## P3 findings

- **WCR-15 · outgoing HTML honesty** — `src/markdown/markdown.ts`: marked
  passes `[x](javascript:alert(1))` through as a live href in the
  `formatted_body` we _send_. Our own renderer (DOMPurify) blocks it and
  receiving clients must sanitize, but "keeps our own output honest" (the
  module's words) argues for running our own sanitizer over outgoing HTML.
- **WCR-16 · duplication** — the failed-echo status block (Retry/Discard)
  and the `toLocaleTimeString` stamp are copy-pasted between
  `RoomPage.tsx:556-589` and `ThreadPanel.tsx:119-152`; the dismissable
  error banner appears 4× (RoomPage, ThreadPanel, RoomList, AccountsPage).
  Extract `<EchoStatus>`/`<EventTime>`/`<ErrorBanner>`.
- **WCR-17 · magic breakpoint** — `RoomPage.tsx:99` hardcodes
  `matchMedia('(max-width: 47.99rem)')`, duplicating the CSS breakpoint
  (`index.css:90,254`). Export a constant from `layout.ts` (which exists for
  exactly this kind of shared layout knowledge).
- **WCR-18 · thumbnail failure hides a loadable image** —
  `MediaImage.tsx:37`: `displayUrl = thumbnailUrl ?? url`; a malformed or
  non-mxc _thumbnail_ shows "Could not load image" even though the full
  image would fetch fine. Fall back to `media.url` when the thumbnail
  acquire fails.
- **WCR-19 · emote sender** — `EventBody.tsx:46` renders raw `event.sender`
  for `m.emote` while every other surface resolves display names. Thread
  `members.displayName` through (or accept the raw id deliberately, with a
  comment).
- **WCR-20 · copy** — `NotFound.tsx:6`: "Back to accounts" links to `/`,
  which is Rooms.

Noted, no action suggested: `relativeTime` stamps in the room list go stale
without a re-render tick (TUI has the same property per redraw model);
`settings.ts`'s persistence `effect` can throw on `localStorage` quota
(private-mode edge); read markers advance while the tab is unfocused
(documented "while it is open" semantics).

## What was checked and found sound

Worth recording so the next review doesn't re-litigate it: DOMPurify config
incl. the `uponSanitizeElement` img hook and `ADD_URI_SAFE_ATTR` handling;
linkify (https?-only, after sanitize, skips a/code/pre); WS auth subprotocol
(benign `axon` echoed, bearer entry never); token-paste seam; media
refcount/LRU incl. the inflight-share and double-release guards;
`use-media-blob` cancellation; shared IntersectionObserver; frame decoding
(malformed frames dropped, never thrown); live-connection backoff and stale
-socket close handling; unread per-room signal design; settings envelope
parser; `virtual-window` math incl. the hidden-viewport case; shortcuts
chord normalization and the staged-Escape design; CSS has no dead classes.

## Suggested fix batches (one silo each, in order)

1. **`web/review-a-timeline-correctness`** — WCR-01 (keyed fragments),
   WCR-03 (generation guard), WCR-07 (echo dedupe). Pure `timeline.ts` +
   `RoomPage.tsx`; unit-testable (a prepend-preserves-row-state test, a
   race test with delayed msw responses, a double-send test).
2. **`web/review-b-rejection-handling`** — WCR-02 (one rejection-policy
   helper applied at every `void`/background call site), WCR-12 (flush
   re-queue), WCR-04 (prettier fix rides along). Turns the gates green.
3. **`web/review-c-live-gaps`** — WCR-06 (thread live ingest + gap-fill),
   WCR-08 (room-list activity bumps + reconnect refresh), WCR-05 (merge
   instead of replace on gap-fill). All ADR 0061 follow-through.
4. **`web/review-d-ux`** — WCR-09 (`?event=` in-room + reveal; do before
   M-W10), WCR-10 (edit failure keeps text), WCR-14 (`useModalFocus`).
5. **`web/review-e-cleanups`** — WCR-11, WCR-13, WCR-15…20.

Batches 1–2 are prerequisites for trusting further feature work; 3 can fold
into M-W6 follow-up; 4–5 can ride behind anything.
