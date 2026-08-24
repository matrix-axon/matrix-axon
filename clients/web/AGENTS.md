# clients/web — agent & developer notes

Preact + TypeScript + Vite SPA (ADR 0046). Setup, scripts, and env vars are
in [README.md](README.md); this file is the working knowledge that isn't
obvious from the code. The governing design is
`docs/adr/0046-web-client-framework-and-roadmap.md` — read its roadmap table
before starting a milestone.

## Ground rules

- **One silo per PR** (project-wide rule): a web PR touches only this
  package (plus its CI workflow). Server changes — even one-line ones this
  client needs — are separate PRs on separate bookmarks.
- **The OpenAPI contract is the boundary.** `src/api/schema.d.ts` is
  generated (`pnpm gen:api`) and committed; CI fails if it drifts from
  `openapi/openapi.json`. Never hand-edit it; never widen a type to work
  around the contract — if the server lacks a field, model the gap
  explicitly (see the `sync_state` note below).
- **TUI parity is the spec for shared semantics.** Room-list
  sort/filter/titles (`src/stores/room-list.ts`) are ported from
  `clients/tui/src/app/rooms.rs`; the HTML subset mirrors
  `clients/tui/src/html.rs`. When behavior is ambiguous, read the TUI
  source and match it — and leave a comment pointing at the Rust original.
- **Live testing:** message sends and other mutations against the live
  server go to the "Axon Testing" room only
  (`!SScJmZuEkBUnuydXdf:bostoncoop.net`). Everything else is a real account.
  Reads are safe anywhere. Mint tokens with `axon token issue`, revoke when
  done.

## Architecture

- **Service graph** (`src/services.ts`): auth provider → typed API client →
  stores, built once in `createServices()`, provided via context
  (`useServices()`). Tests build the same graph over msw + in-memory
  storage via `src/test/services.ts` — components never construct services.
- **State is @preact/signals** in plain store factories
  (`src/stores/*.ts`), unit-testable without rendering. Direct
  `signal.value = x` writes are the idiom; the `react-hooks/immutability`
  lint rule is disabled for exactly this reason.
- **Auth seam** (`src/auth/provider.ts`, per ADR 0031): `getToken()` (sync
  or async), `onAuthFailure()`, `LoginBootstrap` UI slot. Token-paste over
  `localStorage` is the alpha implementation; OAuth/PKCE and a Tauri
  keychain provider must fit this interface without consumer changes.
- **Settings** (`src/stores/settings.ts`): one schema-versioned
  `localStorage` envelope. Add fields with defaults (old envelopes must
  parse); bump the version only for incompatible reshapes. Anything
  unparseable resets to defaults.
- **Timelines** (`src/stores/timeline.ts`): server pages newest-first;
  stores hold ascending display order. One factory serves both the room
  timeline and thread timelines (`threadRoot` param). Sends render an
  optimistic local echo (`TimelineEvent.localEcho`, a client-only extension
  of `EventDto` — not server-driven, same pattern as `sync_state` below) with
  a synthetic `local:<uuid>` event id, then reconcile by re-fetching the
  confirmed event and patching it in place, or marking the echo `failed`
  (retryable via `retrySend`/discardable via `discardSend`) on error;
  edit/redact/react use the same re-fetch-and-patch shape against a real
  event id (scroll position survives throughout — no full reload).
- **Staged attachments outlive the room switch too**
  (`src/media/attachment-staging.ts`, issue #89): staging is retained per scope
  (`accountId\0roomId`, plus the thread root in `ThreadPanel`) with an LRU cap of
  3 — real file bytes, unlike the ~1 KB events the timeline cache holds. Three
  rules, each of which a plausible refactor undoes:
  - **It lives in the service graph, never in the component.** `RoomPage`
    unmounts whenever the route leaves a room, and on a phone that is _every_
    room change (back to `/`, then into the next room). A first version kept the
    buckets in `useAttachments`; it passed every desktop test, survived
    room-to-room switching, and did nothing at all on a phone.
  - The active batch is resolved **during render** from `scope`, never swapped in
    by an effect — an effect runs after the new room paints, leaving one frame in
    which the composer shows, and `Enter` sends, the previous room's files.
  - `shrink()`'s downscale resolves its item **by id across every scope**,
    because a decode outlives the room it started in; resolving against the
    active scope drops the result and leaks the full-size object url.

  Unmount is therefore _not_ a release point. The release paths are remove,
  send, LRU eviction, and `clearAll()` on sign-out
  (`connectAttachmentReset`).

- **Timeline stores outlive their mount** (`src/stores/timeline-cache.ts`, ADR
  0085 phase 1): `RoomPage` acquires from an account+room-keyed LRU instead of
  building a store per mount, so a room re-entered in one session paints its
  events at once and reconciles through `loadLatest()` — which over a populated
  slice _is_ `refreshHead`'s gap-fill merge. Two rules the cache exists to
  respect: a store holding a local echo is **never** evicted (eviction would
  destroy an unsent message and leak its preview url), and a slice parked in
  history (`!atEnd`) is **not** reused, because `refreshHead` deliberately
  refuses to move one, so re-entry would show history with nothing since. Those
  two rules collide when a parked store holds an echo — keep the store,
  `resumeAtHead()` the slice: the parked history and cursor go, the echo stays,
  and the head load lands on the newest page. `refreshHead`'s disjoint-replace
  branch keeps pending echoes for the same reason (an unsent send is not
  history, and no server page can speak to it). The
  whole cache is wiped on sign-out (`connectTimelineCacheReset`) — the service
  graph outlives the session and nothing reloads the document.
- **Durable content cache** (`src/stores/cache-store.ts`, ADR 0085 phase 2):
  IndexedDB behind a `CacheStore` port, with an in-memory adapter for tests —
  no `fake-indexeddb` anywhere. **Every operation is best-effort**: a refused
  open, a quota rejection, or a malformed record resolves to "no cache" and
  never rejects into a caller. Records are keyed by
  `(apiBaseUrl, token fingerprint)`, _not_ by account id — `GET /v1/rooms` is
  cross-account, and the token is what identifies the reader; a token swapped
  without a sign-out therefore misses the cache instead of showing the previous
  reader's rooms. What reaches disk is an explicit **allow-list projection**
  (`room-list-cache.ts`), so a future DTO field cannot carry message text onto
  disk ahead of phase 3's opt-in; the room-list _preview_ is message body text
  and is deliberately not cached. Two rules with teeth: the whole cache is
  wiped on any sign-out and whenever the setting is turned off, and a restore
  must never land on top of a settled refresh (`hydrate`'s guard). Any "load it
  if nobody else has" call site goes through `rooms.ensureLoaded()` — a restore
  makes both `loading` and `rooms.length` say "loaded" while the rows are still
  unconfirmed, which silently turned four guards into "never refresh" (#101).
  The corollary is stronger than it looks: cached content is fine to _render_,
  but a **mutation computed from a list must not run off an unconfirmed copy of
  it** — a stale render is corrected by the next refresh, a stale bulk write is
  not (#102). One record per reader is enforced by pruning every foreign key
  in the `rooms` area on each successful write, which assumes **one token per
  origin** — true while the token lives in `localStorage`. Per-tab tokens
  would make two tabs delete each other's record on every write, so neither
  reader ever gets a hit and the cache degrades to "off" without erroring.

  Two rules the cache needs that a wipe alone cannot give it, both found in
  review:

  1. **A response belongs to the session that asked for it.** A `/v1/rooms`
     request issued under one token can complete after a sign-out and the next
     paste; applying it then writes the previous reader's rooms under the new
     reader's cache key, which lands _after_ the sign-out wipe and so survives
     it. `resetSession()` bumps a generation, and completions that do not match
     are discarded — the WCR-03 request-generation guard applied to identity
     rather than ordering. It binds the **cache read** as well as the network
     one: a sign-out leaves precisely the pristine store (no rows, unsettled)
     that `hydrate()`'s freshness guard treats as safe to write into, so
     without the generation check a boot read still in flight would repaint
     the just-cleared list with the previous reader's rows.
  2. **A wipe is a barrier, not an event.** `clear()` bumps `CacheStore.generation`
     **synchronously**, and writers capture it before their first await and
     re-check before committing. Otherwise a write already in flight lands on
     top of the wipe that a sign-out or a disabled setting just requested, and
     "turning this off erases what was stored" is not true.

- **Sanitizer** (`src/html/sanitize.ts`): DOMPurify + Matrix subset +
  transforms (data-mx-color/bg → inline style, spoilers → click-to-reveal,
  legacy `font color`, mx-reply dropped with contents, bare-URL
  linkification skipping a/code/pre). Gotcha: custom attributes whose
  values look like URI schemes need `ADD_URI_SAFE_ATTR` or DOMPurify drops
  them. `<img>` is admitted for `mxc://` only (M-W8, ADR 0064): the
  `uponSanitizeElement` hook copies a safe `mxc://` src to `data-mxc` and
  always drops `src`, so a remote `http(s)` src (a tracking pixel we cannot
  proxy) never survives; `FormattedBody` resolves `data-mxc` after mount.
- **Timeline scrolling** (`RoomPage`, ADR 0076): the scroller is anchored by
  hand (`overflow-anchor: none`) — a held row plus the scroll offset it was
  measured at, so the measurement survives the reader's own scrolling. Do not
  re-capture the anchor per scroll event: reading geometry forces the layout
  that renders incoming rows, so the capture triggers the growth it means to
  measure. `.event-row` deliberately carries **no** `content-visibility`; its
  size guesses were the shifts. Windowing (#26) is the way back to a bounded
  row count.
- **Media** (`src/media/`, ADR 0064): a browser cannot put a bearer token on
  `<img src>`, so `MediaService` fetches every `mxc://` through the proxy and
  hands the DOM a blob URL. The cache **refcounts** — the timeline is not
  windowed, so a size-based LRU would revoke a URL a mounted `<img>` still
  points at; `acquire()` returns a handle whose `release()` the caller must
  call, and only zero-ref entries are eligible for the 32-entry LRU. Lazy-load
  via one shared `IntersectionObserver` (`useMediaBlob`), which falls back to
  eager acquire under jsdom. A 200 of raw ciphertext (server lacks the key)
  fails only at `<img>` decode, caught by `onError`.
- **Markdown-on-send** (`src/markdown/markdown.ts`): plain prose sends a
  bare body; detected formatting sends `org.matrix.custom.html`. The server
  never interprets Markdown. Raw inline HTML in composer input is escaped.
- **Routing**: history mode (signed off). The deep-link contract is
  `/:accountId/rooms/:roomId` + `?thread=<root_id>` + `?event=<event_id>` —
  search (M-W10) and the mobile clients build on it; do not change it.
  Deployment requires unknown-path → `index.html` rewrite.
- **Marking a room read** (`RoomPage`, plus one in `ThreadPanel`): four effects
  claim read state — the optimistic badge clear on mount, the summary-derived
  cross-device marker, the timeline-driven marker + receipt, and the thread
  panel's marker + receipt (ADR 0096, below). **Every one of them is gated on the
  view actually showing the room's newest events**, and the gates are not
  interchangeable: the optimistic badge clear checks `highlighted === null`
  (an `?event=` landing is parked in history by definition), while both marker
  effects need `highlighted === null` _and_ `timeline.atEnd`. The second half
  matters even without an event anchor: jump-to-date parks the slice in history
  without changing the URL. Conversely, once a room summary is known the head
  gets gap-filled, so a view anchored five days back can have the newest events
  loaded and `atEnd` true. Removing any of these gates makes the
  client claim messages as read that it never displayed: the badge vanishes for
  the session and returns on reload (`POST …/read` is a verbatim pass-through —
  the receipt lands, naming an event that does not cover the unread tail, and the
  server-derived count is recomputed from it, ADR 0089), and the marker is
  cross-device, so `connectReadMarkers` zeroes the badge on your _other_ devices
  too. The accepted cost is that an
  anchored view never marks the room read even after scrolling to the bottom;
  reopening the room normally does. The two conditions are derived once as
  `unanchored` / `showingNewestEvents` rather than restated per effect — add a
  new condition there, not in three places. The corollary caught in review: an
  anchor the server cannot resolve **must be dropped from the URL**
  (`dropUnsatisfiableAnchor`), because a lingering dead `?event=` leaves every one
  of these gates closed for the room's whole visit while the user reads the
  newest messages.
- **Which event is named** is a separate question from the gates above, and the
  timeline effect answers it twice from one candidate set (ADR 0089). The
  cross-device marker is a display-order artifact and names the **display-last**
  event on `origin_ts`; a Matrix receipt is interpreted in **arrival** order and
  names the greatest **`arrival_order`**, which is also the key
  `ephemeral-sender`'s forward-only floor runs on. **Do not collapse these into
  one pick.** They disagree whenever a homeserver delivers an event stamped
  earlier than events already held — routinely, for a bridge backfilling a
  conversation into a freshly created portal, where the room's only message is
  oldest by `origin_ts` and newest by arrival order. Naming the display-last
  event there receipts a portal state event that does not cover the message, so
  the room shows unread forever; and handing the arrival-newest event to
  `advanceReadMarker` instead would feed it an older `origin_ts` than it already
  holds, so the marker — forward-only on `origin_ts` — would simply stop
  advancing. Local echoes carry `arrival_order: -1` and are excluded by their
  `local:` id prefix; display _ordering_ stays on `origin_ts` (#133).
  **The candidate set is what actually rendered** — it runs through
  `isVisibleTimelineEvent`, the same predicate that builds `visible`. ADR 0089's
  contract is "among the events it has actually displayed", and under the default
  `stateEvents: 'important'` the events that predicate hides include the very
  `uk.half-shot.bridge` event from the bug report. Both picks read this set, so
  an unfiltered one lets an unrendered event become the receipt target _and_ the
  position of the "new messages" line. Caught in review on #165 — the first
  version filtered only local echoes and the thread cutoff.
- **The room-list badge and the Threads badge must agree.**
  The optimistic clear on room entry waits until every thread in the room is known read.
  Opening a room does not read its threads, and clearing early left a room with no badge in the list _and_ an unread count on the Threads button — two indicators disagreeing about the same room.
  It re-fires when the last unread thread is opened.
  The settledness rule is the same as everywhere else here: unfetched summaries or an unhydrated marker namespace mean "not known yet", so the clear waits rather than assuming the room is clean.
  The one exception is a summary fetch that _failed_ — waiting on success froze a room's badge for good after a single transient 5xx, so an error falls back to clearing.
- **`no-console` is on for `src/`, allowing only `info`/`warn`/`error`.**
  A debug `console.log` shipped to production from inside a hot memo, stringifying a room's whole timeline slice on every recompute — caught in review, by no gate.
  Tests, e2e specs and scripts are exempt.
- **`deviceState.revision` is per `(account, namespace)`, and that is load-bearing.**
  It exists so a `useMemo` reading values behind `threadReadMarker()` has a dependency it can name.
  A single global counter looked equivalent and was not: every draft keystroke bumped it, so the receipt scan re-ran on every character — most of the memoisation it was added to enable, undone.
- **A thread read marker carries two positions, and they are not interchangeable.**
  `eventId`/`originTs` is a display position — where the thread's "read to here" line sits — and `arrivalThrough` is how far the panel had read in _arrival_ order.
  Display order is not arrival order (ADR 0089), so a backfilled reply is display-early and arrival-late, and deriving one from the other understates it: the receipt path then treats a reply the panel rendered as still unread, makes it a blocker, and the room badge never clears.
  Write both from one filtered set of shown replies, and never re-derive either by looking the other's event up in a slice — that also fails outright once the event pages out.
  A marker written before the field existed parses `arrivalThrough` as `null`, which reads as "no arrival evidence", not zero.
- **"Loaded" and "settled" are different questions to ask a fetch.**
  `deviceState.hydrated()` means the data arrived; `hydrateSettled()` means the request finished, successfully or not.
  A read receipt waits for the first — a claim made on missing evidence is sent to the homeserver and cannot be withdrawn.
  A room badge waits for the second, because a repeating HTTP failure has no retry until the socket drops, and a badge frozen forever is worse than one cleared early.
  Two separate review findings were the same shape (`threads.error`, then device-state hydration); the terms are now shared between the two gates so a third namespace cannot be added to one and missed by the other.
- **Reconciliation waits for the markers.** Thread summaries come back before
  device state on a fresh load, so reconciling in between judges every thread
  against the _room_ marker and flags the ones whose replies are newer than the
  main timeline — a Threads badge that corrects itself a moment later, visible as
  a flash on every reload. An unhydrated namespace is "not known yet", not "no
  marker".
- **A thread member can be the room's receipt target, behind a gate** (ADR 0096).
  Thread members are hidden from the main timeline, so nothing the room view can name ever covers them — in a room whose arrival-newest events are all replies, the receipt stops below them and the badge returns on every load (#207).
  The panel may name its own arrival-max member when the room stream is at its live end **and has loaded** (`atEnd` starts `true` on a cold store, so the two are not the same check), and when its own slice reaches the thread's newest reply.
  **The bound is a window, not the room.** `threadReceiptCeiling` is the first _unread_ reply above the room view's own target belonging to a thread the panel is not showing; the panel names the highest member below it, **and so does the room view** — its target extends over read thread replies up to the same bound, which is the only thing that closes out a room whose newest events are all in threads.
  A panel names only its own thread and only while open, so reading two threads in the wrong order otherwise leaves the room one event short forever.
  **Read replies are exempt** — a thread marker covering one means it is not an obstruction, and without that exemption a room with two interleaved threads can never clear: whichever panel is open, the other's replies sit in the window, so reading them all changes nothing.
  No marker still blocks.
  Do not turn this back into "has the user read every thread here" — that version passed seven tests and never cleared a single badge on a dev server, because a real room is full of threads nobody opened this session, and their replies sit _below_ the room's own target where its receipt has already acknowledged them.
  The gate reads no marker, no summary and no unread state on purpose: each of those is a model of what the user has read, and each was wrong here in a different way.
  The window is a fact about the events in hand.
- **The room read marker is a _main-timeline_ position** (ADR 0048, ADR 0096 § 1), but `RoomDto.last_event_id` is `MAX(origin_ts)` over every event in the room, thread replies included.
  The summary-derived marker effect therefore skips a summary event the loaded slice shows to be a thread member, and lets the timeline effect name a main-timeline position instead.
  Without that skip the marker parks on a reply the main timeline never rendered, and — because `reconcileSummary` falls back to the room marker for a thread with no marker of its own — the fallback answers "read" for the very thread it came from, so the room badges while reporting no unread thread (#209).
  Only skippable when the slice can _show_ the event is a reply — so the effect is gated on `timeline.loading` (which starts `true` on a cold store), because the room list comes back from IndexedDB before the first timeline page and the check otherwise runs against an empty slice and seeds from the reply anyway.
  Gate on `loading`, **not** on emptiness: a loaded-but-empty or gap-filled slice is what this effect exists to serve.
  And because `advanceReadMarker` is forward-only, every account that opened such a room before this landed still carries the bad marker in device state — so `reconcileSummary`'s fallback withholds a room marker pointing at a thread member and substitutes the display-last main-timeline event.
  Withholding alone is not enough: no read position means `reconcileSummary` records nothing, which is silent in a different way.
- **A thread read in another client comes back as a receipt, not as device state** (`connectThreadReceipts`).
  Element sends a thread-scoped `m.read` (MSC3771) and it reaches us verbatim through ADR 0056's passthrough; axon's own receipts are always unthreaded (ADR 0096), so a `thread_id` on _our_ user's receipt can only have come from somewhere else — no echo suppression needed, unlike the `device_state.changed` paths.
  Write it through to a `thread_read_markers` entry rather than just clearing the in-memory flag: receipts are live-only and never replayed, so a session-scoped clear is undone by the next reload.
  `thread_id: "main"` is the room stream, not a thread, and is ignored — acting on it would be a claim about a read position that has its own owner.
- **Debounced device-state writes need an unload flush.** Every write — draft, read marker, thread read marker — sits behind an 800 ms debounce, and `flushPending` was wired only into the auto-refresh path (ADR 0087), which additionally returns early in dev.
  A reload inside that window silently dropped the write, which presented as a thread the user had just opened coming back unread.
  `connectDeviceStateFlush` listens on `visibilitychange` and `pagehide`.
  When testing this, note that a default 1 s `vi.waitFor` is satisfied by the debounce firing on its own — the assertion has to land well inside 800 ms or it passes with the listener removed.
- **Async work in `RoomPage` outlives its own world.** The page does not remount
  across a room switch or an `?event=`/`?thread=` change (ADR 0085), so every
  `await` inside an effect can resolve after the user has moved on — a second
  search hit, another room, a closed panel. Anything that acts on the result must
  re-check identity first, against a **render-bumped generation** covering the
  account, room, anchor, and thread the page is currently showing. The generation
  does not change when the page unmounts, so work that can navigate also needs an
  **effect-cleanup flag** and must check both immediately before routing. Two
  things that look like they'd work and don't: the closure's own `location` (the
  router hands each render its own object, so a stale continuation still sees
  the URL it was created for and passes the check), and comparing serialized
  paths (see the encoding trap under Testing traps). Found repeatedly in the
  #136 review — first as an in-page race, then across account changes and page
  unmount.
- **Ask the operation, not the store.** `error`, `atEnd`, and `events` describe
  the store, not your call. `error` is written by sends, edits, reactions,
  redactions (`mutate`/`mutateWithResult`) and `RoomPage`'s re-decryption retry;
  `refreshHead` declines to move a slice parked in history (the WCR-05 rule)
  without touching `events`, `error`, or `reachedEnd`, so a decline is
  indistinguishable from a success in every signal it leaves behind. When you
  need to know whether _your_ call did anything, the call has to say so —
  `loadLatest` resolves to whether a head page was applied for exactly this
  reason. Two separate bugs in the #136 review came from inferring it, one from
  each of those signals. The corollary is a design rule, not just a caution: when
  a third guard is going into the same function, the thing that's missing is
  usually an API that answers the question, not another check.
- **Live ephemerals** (`src/stores/ephemeral.ts`): `ephemeral.passthrough`
  frames are live-only overlays. `m.typing` is whole-list replace per room,
  self-expires, and clears on socket gaps. `m.receipt` is parsed from Matrix's
  nested raw content; the UI renders public read receipts only on the current
  user's own messages. Presence is still deferred.

## Guardrails (from the 2026-07 review)

Recurring failure modes from the M-W1–M-W8 review
(`docs/reviews/2026-07-web-client-review.md`); the WCR numbers below refer to
its findings. The repo-wide guardrails in the root `AGENTS.md` (notably
"user-entered text must survive a failed mutation" and "every view of server
state declares its freshness story") apply here too.

- **Keys go on the outermost element a `.map()` returns.** If a row needs a
  rendered sibling (a day separator), wrap both in `<Fragment key={id}>` —
  never a bare `<>`. Preact reconciles unkeyed fragments by index, so a
  prepend attaches per-row state (an open confirm, a picker) to the wrong
  row. (WCR-01; `RoomList.tsx` learned this once already.)
- **`openapi-fetch` rejects on network failure; only HTTP errors come back
  as the `{error}` envelope.** Every fire-and-forget call (`void
api.GET(...)`, a background `.then`) must attach a rejection handler: wrap
  it in `inBackground(...)` (`src/api/client.ts`) when failure needs no UI,
  or handle the rejection yourself when it does (`ServerStatus.tsx`,
  `EditHistory.tsx`). Corollary: a vitest run whose tests all pass but whose
  exit code is nonzero with an "Unhandled Errors" block is a **failing**
  gate; that block is the tripwire for exactly this bug class, never noise
  to ship past. (WCR-02/04.)
- **A store method that replaces or splices a signal-held collection must
  assume a sibling request is in flight.** Guard with a request-generation
  token and discard stale completions — two responses for the same resource
  can land out of order (pagination vs. reconnect gap-fill is the canonical
  interleaving). (WCR-03.)
- **Overlays follow one modal contract:** capture-phase Escape, focus saved
  on open and restored on close, Tab trapped inside. Use
  `useModalFocus()` (`src/components/use-modal-focus.ts`) for the focus
  half and a capture-phase `useShortcuts` Escape binding for the other;
  `Lightbox.tsx` shows both together. (WCR-14.)
- **Mobile overlay exits need a real-browser hit-test.** When a mobile flow
  opens content from an overlay or drawer — for example Settings back to a
  room, search result to a timeline, or Room Information member actions to a
  DM — add/update a Playwright layout spec that checks the destination pane's
  center with `expectPaneCenterUncovered()` (`e2e/helpers.ts`). A jsdom test
  and `toBeVisible()` can both pass while a stale fixed panel is still sitting
  on top of the timeline.
- **Composite in-memory cache keys join with `'\0'`** (as in
  `media-service.ts`), never a printable character — and always written as
  the _escape sequence_, never a raw control byte in source. A raw NUL sat
  in `device-state.ts` and rendered invisibly, making the code look like it
  joined on a space; it fooled the 2026-07 review into reporting exactly
  that (WCR-11's premise was this artifact, not a real space).
- **Never let "could not reach the server" discard a credential.** Only a
  server that _answered_ and refused may end a session — `OAuthRejectedError`
  vs `OAuthTransportError` in `src/auth/oauth.tsx`. Collapsing the two signed
  every OAuth client out on each deploy: the restart drops the socket, the
  reconnect refreshes an hour-old access token, and the refresh hits the
  process still restarting, so a 30-day refresh token died of a connection
  refused (ADR 0087). The same rule holds for any future credential path —
  a 5xx and a rejected `fetch` are "unknown", not "no".
- **Whatever serves `dist/` must 404 a missing `/assets/*`, not fall back to
  `index.html`.** The SPA history fallback is for routes; a content-hashed
  chunk is not one. Vite's own preview server rewrites _any_ unmatched GET
  whose `Accept` includes the wildcard — which is what `<script type="module">`
  and `import()` send — so a chunk deleted by a redeploy returns `200
text/html`, the browser cannot parse it as a module, and the app hangs with
  no useful error. Both servers that serve `dist/` are guarded:
  `vite.config.ts` via `configurePreviewServer`, and `deploy/web/Caddyfile` via
  a `handle /assets/*` block ahead of its `try_files`. Any new one needs the
  same. Verify with `curl -sI <origin>/assets/nope-abc123.js` — a `200` there
  is the bug.

## Freshness: the client's own build

Guardrail 8 in the root `AGENTS.md` ("every view of server state declares its
freshness story") applies to the bundle itself, not just to data. The story, per
ADR 0087:

- `src/stores/update-check.ts` polls `/version.json` — on live-socket reconnect
  (a deploy restarts the server, so this is the fast path), on
  `visibilitychange` to visible, on `online`, and every 15 minutes as a
  backstop. Every failure means "learned nothing", never "the build changed".
- `src/update-refresh.ts` owns the policy: reload silently only when the user is
  away and has no unsent echo, otherwise show `UpdateBanner`.
- `src/reload.ts` is the loop guard, and it is load-bearing. **Any new code path
  that reloads the document automatically must go through `reloadOnce`, with a
  target that names what it is reloading toward.** An origin whose
  `version.json` disagrees with the bundle it serves would otherwise reload
  forever, which is a far worse failure than the staleness being fixed. The
  guard keys on _(build we left, target)_ so one bad manifest does not disable
  refresh for the session, and caps total automatic reloads at `MAX_ATTEMPTS`
  per tab. `reloadNow` is for user-initiated reloads only.
- Adding a check trigger is cheap; adding an _automatic reload_ trigger means
  re-answering "what would this destroy?" — `timelines.hasUnsentWork` and
  `deviceState.flushPending()` are the two existing answers.
- **The whole mechanism is off under `vite dev`** (guarded in `app.tsx`). A dev
  stamp is a git hash plus `-dirty`, read once at server start, so it moves with
  your working tree; without the guard, restarting the dev server after a commit
  would make every open tab decide a new build had shipped. A self-refreshing
  dev tab is Vite — an HMR full reload, or the "optimized dependencies changed"
  reload after a dep re-optimize — not this. Every reload this code performs
  logs under `[axon:update]` first; filter on that before suspecting it.

## Diagnosing reports that only reproduce on someone else's device

The loop is in `docs/adr/0077-web-on-device-perf-readout.md`; the tooling is
**Settings → Performance instrumentation** (no URL editing, no tethering to a
Mac). Ask for a screen recording, read the on-screen readout out of the video,
and measure the behaviour from the same frames. These are the lessons that cost
the most time in the ADR 0076 investigation:

- **Try the scripted reproduction before asking for a recording.**
  `playwright.config.ts` has a **WebKit project at the iPhone 13 profile**
  (viewport, UA, scale factor, touch), added by ADR 0071 and running here on
  Linux — no Mac, no device. Layout bugs are input-independent, so a spec that
  scrolls and records an element's `getBoundingClientRect().top` measures a
  shift directly, in seconds per iteration. The ADR 0076 investigation ran nine
  record-and-analyse cycles to measure what `page.evaluate` would have returned
  as a number. Keep the device loop for what only a device can show: CPU-bound
  behaviour, real momentum scrolling, and confirming a fix in the reporter's
  hands.
- **Instrumentation must be live before the thing it measures.** `App` applies
  the stored `perfMarks` preference in an effect, which is far too late for
  anything marked while the service graph is built — those marks are dropped
  silently. `createServices` therefore applies it up front. Two things hid this
  for a while: `?perf=1` latches inside `perfEnabled()` before any store exists
  (so the e2e lane never saw it), and `setPerfEnabled` mirrors the flag into
  `sessionStorage`, which survives navigations within a tab — so only a genuine
  **relaunch**, which clears it, reproduces. A spec must clear `sessionStorage`
  immediately before the load it measures; any intervening load re-arms it.
- **The room-list boot has a one-line summary** (`boot:room-list`, ADR 0085
  phase 2): `hydrate`, `rows`, `net`, `saved = net - rows` — milliseconds since
  navigation, so they read against each other directly. Turn on Settings →
  Performance instrumentation, load twice, and read the second load's line off
  the recording; `saved` is the blank time the cache removed, and a **negative
  `saved` means the network won and the cache bought nothing on that load**.
  Its absence is data too: a resumed tab runs no new document, so only cold
  opens emit one.
- **Instrument for the device that has the problem.** iOS Safari has no
  on-device console, so a mark only a desktop console can reach does not help
  with the reports that most need help. `PerfOverlay` draws the tail of selected
  marks over the app precisely so a recording captures the numbers and the
  behaviour they explain in the same frames. When adding instrumentation, ask
  whether the reporter could read it.
- **Instrument the app before theorising about the engine.** Every
  browser-behaviour hypothesis in that investigation was wrong — missing WebKit
  scroll anchoring, inertial scrolling overriding `scrollTop`, the correction
  causing the jump — and every actual defect was ours. Reading a value back
  after writing it (`applied === requested`) falsified a day of theory in one
  recording.
- **Confirm the deployed bundle contains the fix before debugging it.** Mark
  names are string literals and survive minification, so
  `(await (await fetch(src)).text()).includes('some:mark:name')` settles it in
  one line. A stale deploy looks exactly like a fix that does not work.
- **A video is a measuring instrument.** Frame extraction plus 2D phase
  correlation gives per-frame displacement. Two traps: 1D row-mean profiles
  alias against the ~50px message-row pitch, and only near-still frames yield
  meaningful displacements — a collapsed correlation peak (< 0.5) means the
  content was _replaced_, not moved (a page landing, a slice replacement).
- **The overlay is a ten-line buffer.** A chatty mark crowds out everything
  else; a chain that re-armed on every scroll frame once hid the very marks
  under investigation. Curate `OVERLAY_PREFIXES` (`src/perf.ts`) when adding.
- **`body { position: fixed; inset: 0 }` does not stop iOS Safari scrolling the
  document.** It is widely recommended for exactly that, and it was committed
  here on that reasoning, but measured on a real iOS 26.5 simulator it prevents
  nothing: dragging `.timeline` with the soft keyboard up still drove
  `window.scrollY` to 150, then 202, then 237. What actually holds the layout is
  the reactive `window.scroll` reset in `app.tsx`. The rule was reverted; do not
  re-add it on the theory that it prevents the scroll without measuring first.
- **`keyboard:page-scroll-reset` is the oracle for that whole class of bug.** It
  only fires when the reactive reset found a non-zero `window.scroll*`, so _any_
  occurrence means something still scrolled the document. A preventive fix is
  proven by that mark's **absence** under a real keyboard-up drag — not by a
  screenshot looking right, which only shows the post-correction frame (the
  reset lands within ~2ms, so stills essentially never catch the bad frame).
- **The reactive reset cannot make the artifact invisible, and a 60fps recording
  shows why.** On a real iPhone, raising the keyboard displaced the whole shell
  by ~211 CSS px (the mark recorded `offsetTop=245`), leaving a band of page
  background above the topbar. The reset fired and `offsetTop` was back to 0
  within 9ms — but the _screen_ kept showing the displacement for another
  **167ms**, decaying smoothly frame by frame (211 → 188 → 166 → 139 → 114 → 85
  → 60 → 39 → 22 → 10). Nothing on our side animates that: no
  `scroll-behavior: smooth` anywhere, and `.shell` has no `transition` on `top`
  despite being positioned off `--app-viewport-top`. The decay is therefore
  engine-side — Safari easing its own viewport adjustment while the keyboard
  animates in, which means
  writing `window.scrollTo(0, 0)` retargets a value the compositor is already
  animating and does not cancel the animation. Correcting after the fact is
  therefore structurally incapable of removing this; only stopping the scroll
  from being initiated will. Untried lever: the seven programmatic
  `textarea.focus()` calls in `Composer.tsx` could pass `{ preventScroll: true }`
  — but note that only affects _programmatic_ focus, so it does nothing for a
  plain tap into the composer.
- **The correction _was_ the jitter.** Settled by a mark-proven A/B on a real
  iPhone (Settings → "Correct iOS keyboard page drift", which renames the mark
  to `page-scroll-observed` when off, so a recording proves which arm ran).
  Correcting: `offsetTop` oscillates 26 → 2 → 7 → 12 → 21 → 4 → 6 → 9 every
  frame, 15 measurable shell excursions in one capture, keyboard-raise transient
  366 CSS px over 183 ms. Not correcting: `offsetTop` settles to 414–417 and
  stays, 3 excursions, raise transient 130 CSS px over 83 ms. The mechanism is a
  feedback loop — `scrollTo(0, 0)` moves `visualViewport.offsetTop`, which is
  what `--app-viewport-top` positions `.shell` from, so each correction shoved
  the shell and Safari pushed back. `.shell` already compensates for the offset
  correctly on its own; the reset was fighting a fix that worked. Default is now
  off. The lesson generalises: before adding a correction, check whether
  something downstream already compensates, or the two will fight at frame rate.
- **What is still unsolved there, and the one untested lever.** With the
  correction off, a keyboard _raise_ still displaces the shell ~130 CSS px for
  ~83 ms, plus occasional single-frame blips of <= 15 CSS px that sit at the
  perceptual floor. That residual is a different failure from the feedback loop
  above: it is observation lag. Safari animates the viewport, the app learns
  about it from `visualViewport` events and writes `--app-viewport-top` from JS,
  so a JS-positioned shell can never be frame-perfect against a
  compositor-driven animation. The only lever that removes the observation step
  is making the _layout_ viewport track the keyboard —
  `interactive-widget=resizes-content` in the viewport meta of `index.html`.
  It was tried once and backed out, but **treat that as untested, not
  disproven**: the capture it was judged on had no perf marks, so the blank
  screen it correlated with was never tied to the meta, and the reasoning that
  it "worked" (`innerHeight` collapsing to the visual height) was wrong —
  `innerHeight` moves with `offsetTop`, so it said nothing either way. If it is
  retried, A/B it with the Settings checkbox and read the marks; a blank app is
  the specific risk to watch for. Weigh it against the fact that the residual is
  motion _during_ the keyboard animation, when motion is expected — unlike the
  jitter while scrolling, which is what actually read as broken.
- **A long-lived `e2e/mock-server.mjs` must never be reused by the suite.** An
  old process accumulates state, and one left running for device testing once
  collected four copies of a message and broke a strict-mode locator in
  `shortcuts.spec.ts`, twice, looking exactly like a real regression. Worse, a
  process on the shared port can belong to another jj workspace and serve that
  workspace's `dist/`. Playwright therefore requires a fresh server and fails
  when 4599 is occupied. Set `AXON_WEB_E2E_PORT` to an unused port when the
  existing process must stay up.

## Driving a real iOS Simulator by hand

For the bugs the WebKit e2e profile cannot reach — anything involving the real
soft keyboard, which is what resizes the visual viewport. The recipe:

```bash
pnpm build                       # the mock serves dist/, so build first
node e2e/mock-server.mjs         # dist/ + a real /v1/ws on 127.0.0.1:4599
xcrun simctl boot "iPhone 17 Pro"
xcrun simctl openurl booted "http://127.0.0.1:4599/<ACCOUNT_ID>/rooms/%21room%3Ahs?perf=1"
```

The simulator shares the host network stack, so `127.0.0.1` reaches the Mac's
server directly — no LAN address and no rebinding needed. Sign in by pasting
`e2e-token` (what `signIn` in `e2e/helpers.ts` puts in `localStorage`);
`ACCOUNT_ID` and the room id are exported from the same file. `?perf=1` turns on
the overlay, which is the only readable instrumentation on iOS.

- **The soft keyboard needs two things, and silently no-ops with one.** Set
  `defaults write com.apple.iphonesimulator ConnectHardwareKeyboard -bool false`
  **and** have `Simulator.app` actually running — the preference is read by that
  app, so a headless/panel-only attachment keeps the hardware keyboard connected
  and no soft keyboard ever appears. The failure is quiet and misleading: the
  composer focuses, an accessory bar appears, the viewport shrinks by ~2px, and
  it all looks plausible while the keyboard-resize path never runs at all. Check
  for a keyboard-sized `keyboard:viewport-update` (714 → 377 on an iPhone 17
  Pro) before trusting any keyboard result.
- **A fast synthetic swipe often is not a scroll.** Single-vector swipes left
  `.timeline` untouched while looking like they worked; only a multi-point drag
  with real inter-point delays scrolled it. Verify the content actually moved
  between screenshots before concluding anything from a gesture test.
- **The Simulator needs a Metal-capable GPU.** Under virtualised macOS with an
  emulated adapter (Bochs/QEMU, ~7 MB VRAM) SpringBoard composites fine but
  WebKit paints pure black and `simctl openurl` times out — which reads as a
  hang or a wedged device and is neither. Check `system_profiler
SPDisplaysDataType` before debugging the simulator itself.

## Test environment gotchas (all discovered the hard way)

- jsdom under Node 25 exposes `window.localStorage` as a bare object —
  inject `memoryStorage()` from `src/test/memory-storage.ts` instead.
- testing-library auto-cleanup needs vitest globals (not enabled): add
  `afterEach(cleanup)` in every component test file.
- msw handler paths: use `:param` segments for ids containing `$`/`:`
  (Matrix event/room ids) — literal or percent-encoded paths don't match.
- Generated free-form objects (`content`, `relates_to`) type as
  `Record<string, never>`; test fixtures take them loosely and cast
  `as unknown as EventDto`.
- preact-iso's `Router` type wants ≥ 2 children; add a `default` route.
- Run pnpm from this directory — from the repo root it fails with
  `ERR_PNPM_NO_PKG_MANIFEST`.

## Server gaps this client already accounts for

- **ADR 0030 `sync_state`** is unimplemented server-side. The accounts UI
  reads it opportunistically through one typed extension
  (`src/stores/accounts.ts`); when the server adds the field, `gen:api`
  makes it real and the extension alias gets deleted. Tracked in the parent
  repo's issues.
- **ADR 0055 `is_direct`** is docs-only; the DM heuristic (blank name +
  alias, `isLikelyDm`) is the interim, swapped in one function when the
  server field lands — same plan as the TUI.

## Roadmap position (ADR 0046 table)

M-W1–M-W8.5 are done (M-W7 was built before M-W6
deliberately — messaging is pure HTTP; M-W8.5, media send, was unblocked late
by M15's upload API and so sits between M-W8 and M-W9). Remaining: **M-W9**
(verification/SAS + trust glyphs), **M-W10** (search UI over `GET /v1/search`, deep-linking via
`?event=`), **M-W11** (hardening/a11y/parity audit), **M-W12** (Tauri —
no service workers, `document.cookie`, or `window.open` anywhere, ever).

## Testing traps

- **A jsdom `File` is not undici's `Blob`.** Hand a `File` to `fetch` as a
  request body under vitest and the body arrives at msw as the literal string
  `"undefined"` — the `Content-Type` still comes through, so the request _looks_
  right and only the bytes are silently wrong. Upload **bytes** therefore cannot
  be asserted in a unit test; `e2e/media-send.spec.ts` exists to assert them in
  a real browser (it compares a digest, since media is binary). Unit tests may
  still assert the query params, the headers, and the failure mapping.
- **`tsc --noEmit` is not the typecheck.** Only `pnpm build` (`tsc -b`) uses the
  project's real config. The generated schema types an event's `content` as
  `Record<string, never>`, and `--noEmit` waves through assignments to it that
  the build rejects — which is why local echoes cast (`as unknown as
TimelineEvent`).
- **The e2e lane serves `dist/`, not your source** — deliberately, so the specs
  and the ADR 0071 perf numbers describe the artifact that actually ships
  (minified, bundled, `import.meta.env.PROD`), which a dev server would not.
  The cost is that a source change is invisible to Playwright until the build
  runs again, and a stale bundle reports a fix as broken or a break as fixed,
  silently. `test:e2e` therefore builds first; if you invoke `playwright test`
  directly, build first yourself.
- **One e2e mock server serves the whole Playwright invocation.** It is fresh at
  the start, but still outlives each individual spec file. A spec that appends
  to its seeded `timeline` array pollutes every later spec; `send-media`
  deliberately only broadcasts and records for `/events/:id`.
- **A fixture that uses the same helper as the code agrees with it by
  construction.** A URL check compared `window.location.pathname` against
  `localRoomHref`, which runs the room id through `encodeURIComponent` — but a
  browser keeps the literal `:` of `!localpart:server` in a path, so the
  comparison was permanently false in every real browser and passed in every
  test, because the fixtures seeded the URL through that same helper. Seed
  anything that crosses the browser boundary — paths, ids, encoded params — the
  way the browser presents it, not the way our code writes it.
- **Run a new regression test against the unfixed code first.** A guard that
  reads the wrong state passes its test for the wrong reason, and looks correct
  while doing nothing. Every regression test added in #136 was checked this way,
  which is how two silently-inert guards were caught before review — including
  one whose test passed until the fixture stopped sharing an encoder with the
  code it tested.
- **`e2e/media.spec.ts` is flaky here:** headless `IntersectionObserver`
  sometimes never fires, so lazy-loaded media stays a skeleton and no proxy
  fetch is issued. It reproduces on unmodified code — don't chase it as a
  regression in your diff.

## The demo recording lane

`e2e/demo/` drives the ADR 0086 demo videos against a **real** seeded axon
(`smoke/local-stack --corpus`), not `mock-server.mjs`. It has its own config
(`playwright.demo.config.ts`) and its own `testIgnore` line in
`playwright.config.ts`, so `pnpm test:e2e` and the PR gate never collect it.
Procedure, env vars and the WebM→MP4 step are in [README.md](README.md)
§ Demo recording; what each scene covers is `docs/demo-coverage.md`.

It is not a test lane and is not a CI gate — it needs Docker and several
minutes — but its scenes are the only thing that walks these render paths
against a real backend, so a scene that stops passing is a real finding.

## Definition of done for a UI change

The milestone bar below is for a milestone. This is the bar for any PR that
changes what the client renders — `src/pages/`, `src/components/`, CSS, or the
stores behind them. Playwright is deliberately **not** in the pre-push hook (ADR
0092: browsers and minutes), so it is on you rather than on the gate.

- **Run `pnpm test:e2e` before pushing** a change to rendering or interaction.
  `web-e2e.yml` gates the PR on chromium, firefox and webkit-desktop anyway; the
  local round trip is minutes, and a CI round trip is not. It builds first, which
  matters — see "the e2e lane serves `dist/`" under Testing traps, and kill any
  long-lived `e2e/mock-server.mjs` before you start.
- **Anything whose failure mode is layout or a real browser API needs an e2e
  spec, not a jsdom test.** Viewport and keyboard geometry, scroll anchoring,
  `IntersectionObserver`, history and reload classification, and upload bytes are
  all invisible to jsdom — it will report your code correct. This generalizes the
  root `AGENTS.md` rule about `expectPaneCenterUncovered()`, which is one
  instance of it. Everything else stays a vitest interaction test, which is
  faster and more precise.
- **Layout, scrolling, or composer changes run the iPhone profile too:**
  `MOBILE_E2E=1 pnpm exec playwright test --project=webkit-iphone`. CI does not
  run that project on a pull request — only after the change merges to `main` —
  so a mobile regression that a PR introduces is found by whoever notices it on a
  phone.
- **A change that only touches stores, api, or types** does not need any of this;
  `pnpm test` covers it.

## Definition of done for a milestone

`pnpm test && pnpm lint && pnpm format:check && pnpm build` all green; the UI
bar above met for anything the milestone renders; new
logic has unit tests (stores) and interaction tests (pages, msw-backed);
README status paragraph updated; a human pass against the live server
(read-only outside the test room); **`docs/demo-coverage.md` updated in the same
PR if the change touches anything a demo scene renders** — a capability that
never reaches a demo is invisible twice, absent from the videos and unexercised
by the driver that would notice it breaking (ADR 0086); one commit on a jj
bookmark stacked on the previous milestone, described but not pushed unless
asked.
