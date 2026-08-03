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
- **The e2e mock server outlives a single spec file** (`reuseExistingServer`).
  A spec that appends to its seeded `timeline` array pollutes every later spec;
  `send-media` deliberately only broadcasts and records for `/events/:id`.
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

## Definition of done for a milestone

`pnpm test && pnpm lint && pnpm format:check && pnpm build` all green; new
logic has unit tests (stores) and interaction tests (pages, msw-backed);
README status paragraph updated; a human pass against the live server
(read-only outside the test room); **`docs/demo-coverage.md` updated in the same
PR if the change touches anything a demo scene renders** — a capability that
never reaches a demo is invisible twice, absent from the videos and unexercised
by the driver that would notice it breaking (ADR 0086); one commit on a jj
bookmark stacked on the previous milestone, described but not pushed unless
asked.
