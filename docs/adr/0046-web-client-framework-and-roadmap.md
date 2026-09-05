# ADR 0046 — Web client: Preact, basic client, and parity roadmap

## Context

ADR 0031 (multi-platform client strategy) sequences the web client first among
the post-MVP clients and leaves exactly one open question: the JavaScript
framework, which "must be resolved — and recorded as a follow-on ADR or
amendment — before `clients/web/` work begins." This ADR resolves that
question and records the basic browser-client milestone plus the longer parity
roadmap.

The long-term parity target is the TUI feature set **after PRs #162 (cross-user
verification) and #183 (indexed search) merge**: account lifecycle
(login/logout/recover/delete/status/switch), room list with pinning
(ADR 0038), sort and filter modes (ADR 0042), membership filtering
(ADR 0037), client-derived unread indication, cursor-paginated timeline with
live WebSocket updates (ADRs 0019/0020), formatted-body rendering
(`data-mx-color`, `data-mx-spoiler`), date jump, edit history,
send/reply/thread/edit/redact/reactions (ADRs 0021/0032/0033), inline media
via the authenticated media proxy, cross-account full-text search (ADR 0039),
SAS self- and cross-user verification with sender-trust glyphs and violation
alerts (ADRs 0027/0028/0040/0045), and settings/theming.

The first browser release is intentionally smaller: authenticated setup,
account lifecycle, room navigation, read-only timeline, live updates, and
reconnect recovery. Messaging, media, verification, search, hardening, and the
Tauri desktop shell follow as the parity roadmap once the basic client is
usable.

Facts established while planning, which shape the milestones below:

- `clients/web/` does not exist; `.gitignore` already anticipates it. It is
  not a Cargo workspace member, so the node package coexists with the Rust
  workspace with no build-system coupling.
- Repository topology stays in this monorepo through the basic browser client
  and parity audit. The web client will co-evolve with the OpenAPI contract,
  the local integration harness, and nearby server prerequisites; the
  one-silo-per-PR rule keeps the review boundary controlled. Revisit a
  separate repo only after M-W11 declares the SPA stable.
- **The server serves no CORS headers.** A separately-hosted SPA cannot call
  `/v1/` from a browser today. Development is unaffected (Vite dev-server
  proxy), but deployment needs server-side CORS support (decided below).
- Browser WebSocket auth is `Sec-WebSocket-Protocol: bearer.<token>`
  (ADR 0029); the server never echoes the token-bearing entry back. (Amended by
  #238: the client also offers a benign `axon` subprotocol, which the server
  echoes so the handshake is RFC 6455-compliant in Chrome.)
- The media endpoint (`GET /v1/media/{account_id}/{server_name}/{media_id}`)
  is header-auth only and returns raw bytes with no server-side
  thumbnailing. A bare `<img src>` cannot carry the bearer token; the client
  must fetch → `Blob` → `URL.createObjectURL`.
- TUI reconnect behavior (`clients/tui/src/api.rs`): exponential backoff
  1 s doubling to a 30 s cap; on reconnect it refreshes server-backed state
  for verification flows, read markers, and drafts. There is no timeline
  gap-fill, and the WebSocket has no resume cursor — `timeline.event` frames
  dropped while disconnected are silently lost. The web client must preserve
  the TUI's read-on-reconnect behavior for those three state classes and do
  better for timeline gaps (see roadmap M-W6 and the discussion list).
- WebSocket frames are `timeline.event`, `verification.requested`,
  `verification.sas`, `verification.done`, `verification.cancelled`, and
  `sender_trust.changed`. There is no typing/receipt/presence support
  anywhere in the system.
- The integration harness (`scripts/integration-test.sh`: Postgres + Synapse
  - axon with seeded encrypted rooms; `docs/integration-testing.md`) is
    reusable as the backend for browser end-to-end tests.

## Decision

### Framework: Preact

**The web client is a Preact + TypeScript + Vite SPA** (no SSR), closing the
open question in ADR 0031. Rationale, against the alternatives tabled there:

- Near-identical React API and excellent TypeScript support, so React
  ecosystem knowledge, patterns, and (via `preact/compat`) components
  transfer directly — without React's runtime weight (~3 KB core).
- The bundle-size ethos matches this client: a focused messaging UI over a
  small, clean API, and a Tauri shell later where a lean dist keeps the
  desktop app light.
- OpenAPI code-gen and Tauri integration tooling are the same as React's.
- Svelte's compile-time gains don't outweigh its smaller ecosystem and less
  mature TS/code-gen tooling for this team; Vue would be a second idiom with
  no offsetting advantage over the React-shaped one.

### Stack

- **State: `@preact/signals`.** Fine-grained reactivity suits a
  high-frequency live timeline (per-frame `timeline.event` updates) without
  context re-render cascades. State lives in plain store modules
  (`stores/{accounts,rooms,timeline,verification,connection}.ts`) exporting
  signals/computed values — mirroring the TUI's `app/` module split and
  unit-testable without rendering.
- **API layer: `openapi-typescript` + `openapi-fetch`,** generated from
  `openapi/openapi.json` (the contract, per ADR 0031). Types-only code-gen,
  a ~6 KB typed fetch wrapper, a `gen:api` script, and a CI check that the
  generated schema is in sync with the spec.
- **Auth: an `AuthProvider` seam.** ADR 0031 requires that clients not
  hard-wire the token-paste bootstrap. The client depends on an interface
  (`getToken()`, `onAuthFailure()`, a login-bootstrap UI slot); token-paste
  is one implementation, OAuth 2.0 + PKCE and a Tauri OS-keychain provider
  drop in later without touching consumers.
- **Token storage: `localStorage` by default** for the alpha. The token is
  self-minted (`axon token issue`) and revocable (`axon token revoke`), so
  the exposure model is the GitHub-PAT one; OAuth replaces the paste flow
  post-MVP. The storage choice lives behind the `AuthProvider` seam.
- **CORS: server-side `tower_http::CorsLayer` in `axon-api`,** with allowed
  origins configurable in server config. This is a small **server-silo PR**
  (M-W1.5 below), separate from all web PRs, and must land before the web
  client makes real cross-origin calls (M-W3).
- **Package manager: pnpm,** self-contained package at `clients/web/` with
  its own lockfile. No repo-root workspace: only one node package exists,
  and the package can stay independent while still living in this repo.
- **Lint/format: ESLint + Prettier.** This is the conventional Vite/Preact
  TypeScript stack, keeps room for a11y and testing-library rules once the UI
  appears, and avoids making M-W1 pick tooling during implementation.
- **Sanitization: DOMPurify** with an allowlist mirroring the Matrix HTML
  subset the TUI renders (`clients/tui/src/html.rs` is the reference,
  including `data-mx-color` and `data-mx-spoiler`).
- **Routing: `preact-iso`,** with URLs shaped
  `/:accountId/rooms/:roomId` plus `?thread=<root_id>` and
  `?event=<event_id>` for deep links (search-result navigation depends on
  these). M-W3 must sign off the final history-vs-hash routing mode before
  this shape becomes the search/navigation contract.
- **Media: fetch + blob URLs.** An authenticated `fetch` of the media proxy
  endpoint → `Blob` → `URL.createObjectURL`, with an LRU cache and
  `revokeObjectURL` eviction. No service worker (see discussion list).
- **Testing, per layer:** Vitest unit tests for stores and pure logic
  (unread derivation, sort/filter, reconnect backoff, cursor merge — the
  bulk of coverage, mirroring the TUI's unit-test-heavy style); msw for the
  HTTP layer and a scripted fake `WebSocket` for frame/reconnect handling;
  `@testing-library/preact` for components; Playwright end-to-end against
  the integration harness, starting at M-W6.
- **Tauri compatibility is a standing constraint from day one:** no hard
  dependency on service workers, `document.cookie`, or `window.open`, so
  the M-W12 desktop shell wraps the same dist unmodified.

### Roadmap

Each milestone is one silo — one PR or a small stack — per the project's
one-silo-per-PR rule. M-W1 through M-W6 produce the basic browser client; M-W7
through M-W12 complete parity, hardening, and the desktop shell; ADR 0102
adds M-W13 for the mobile targets.

| Milestone  | Scope                                                                                                                                                                                                                                                                                                                                                                  | Exit criterion                                                                                                      |
| ---------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| **M-W0**   | This ADR.                                                                                                                                                                                                                                                                                                                                                              | Reviewed and merged.                                                                                                |
| **M-W1**   | Scaffolding & CI: `create-vite` (preact-ts), pnpm, ESLint + Prettier, Vitest with a smoke test, `.github/workflows/web-lint-and-test.yml` (`workflow_dispatch` if it uses GitHub-hosted runners), README covering the Vite dev-proxy setup.                                                                                                                            | `pnpm build` produces a deployable dist; CI green; `cargo` untouched.                                               |
| **M-W1.5** | **Server silo:** CORS via `tower_http::CorsLayer`, configurable allowed origins.                                                                                                                                                                                                                                                                                       | Cross-origin `GET /v1/accounts` succeeds from a browser.                                                            |
| **M-W2**   | Generated API client + auth seam: `gen:api`, `openapi-fetch` factory, `AuthProvider` interface with the token-paste implementation, 401/error-envelope handling, WS URL + subprotocol helper.                                                                                                                                                                          | Authenticated `GET /v1/accounts` round-trip; msw-backed tests.                                                      |
| **M-W3**   | App shell, routing, settings, accounts: layout, finalized routing mode and deep-link URL shape, theme + schema-versioned `localStorage` settings, full account lifecycle UI (login/logout/recover/delete-with-confirm/status/switch; sync-readiness per ADR 0030).                                                                                                     | Full account lifecycle usable in-browser.                                                                           |
| **M-W4**   | Room list: server membership filtering (ADR 0037), sort modes recent/oldest/az/za (ADR 0042), filters all/dms/groups/unread/favorites/name, pinning persisted in settings (ADR 0038), unread as a client-derived signal store (live-fed in M-W6).                                                                                                                      | Navigable room list matching TUI semantics.                                                                         |
| **M-W5**   | Timeline, read-only: cursor pagination with infinite scroll-back, sanitized formatted rendering with spoiler click-to-reveal, state-events toggle, date jump, edit-history view, reaction/reply aggregation display (ADR 0033).                                                                                                                                        | Read parity for history.                                                                                            |
| **M-W6**   | Live WebSocket layer: subprotocol auth, frame router, reconnect backoff (1 s → 30 s, matching the TUI), connection-state UI, read-on-reconnect refresh for verification flows, read markers, and drafts, **gap-fill on reconnect by refetching the open room's timeline head and reconciling by event id** (improving on the TUI), live unread. First Playwright lane. | Basic browser client usable for live read-only Matrix browsing; two browser tabs see each other's messages live.    |
| **M-W7**   | Messaging: send (textarea, markdown-on-send), reply (ADR 0032), edit, redact with confirm, reaction toggle, threads (list + thread timeline + send-in-thread).                                                                                                                                                                                                         | Full write parity.                                                                                                  |
| **M-W8**   | Media: MediaService (fetch → blob → object URL, LRU + revoke), lazy-load via IntersectionObserver with a concurrency cap (no server thumbnails), full-size lightbox.                                                                                                                                                                                                   | Encrypted attachment from the integration harness renders inline.                                                   |
| **M-W8.5** | Media send (ADR 0065): staged upload (`POST …/media/uploads`) then `POST …/rooms/{room_id}/send-media`, attach via file picker / drop / paste, composer text as caption, optimistic echo rendering the local file while it uploads. Unblocked after the fact by M15 (ADR 0059), which is why this sits between M-W8 and M-W9 rather than in the original numbering.    | A picked image and a file both land in a room, with and without a caption, and render back through the media proxy. |
| **M-W9**   | Verification & sender trust (PR #162 scope): verification frame handling, SAS modal (emoji + decimal, confirm/cancel), self- and cross-user (`@user`) initiation, incoming-request gating setting, reconnect flow re-discovery, per-event verification-bundle view (ADR 0045), trust glyphs + violation alerts. May be a 2-PR stack (flows, then trust display).       | Web ↔ TUI SAS ceremony completes against the integration Synapse.                                                   |
| **M-W10**  | Search (PR #183 scope, ADR 0039): UI over `GET /v1/search`, results with room/account context, deep-link into the room at the hit via `?event=`.                                                                                                                                                                                                                       | Search-to-message navigation works.                                                                                 |
| **M-W11**  | Hardening & parity audit: core keyboard shortcuts, a11y pass (modal focus, spoilers, SAS dialog), error/empty/loading states, bundle-size budget, parity checklist signed off against the target above.                                                                                                                                                                | full feature parity with TUI; "SPA stable" trigger for Tauri per ADR 0031.                                          |
| **M-W12**  | Tauri desktop shell: `clients/web/src-tauri/`, macOS + Windows + Linux builds (macOS added by ADR 0102), Tauri `AuthProvider` (OS keychain — first payoff of the M-W2 seam), CI build lane.                                                                                                                                                                                                              | Desktop builds ship from the same dist.                                                                             |
| **M-W13**  | Mobile targets and store distribution (added by ADR 0102): iOS and Android builds from the same shell, camera/safe-area/icon/local-network platform work, App Store and Play submission. Push notifications are explicitly not in scope.                                                                                                            | Builds installed from TestFlight and the Play internal track.                                                       |

**Addendum (Tauri prep, pre-M-W12):** `services.ts`/`ws.ts`/`oauth.tsx` were
decoupled from hardcoded same-origin assumptions (`apiBaseUrl()` as the single
server-base accessor; `OAuthAuthOptions.redirectUriBase`) so M-W12 only needs
to set env vars / pass a deep-link scheme, not touch these call sites. Two
items remain open and are *not* resolved by that prep:

- **CORS (M-W1.5) config shape**, for whoever implements it: add
  `cors_allowed_origins: Vec<String>` to `ServerConfig`
  (`crates/axon-core/src/config.rs`), following the existing
  `#[serde(default)]` / figment `AXON_SERVER__*` env-override pattern already
  used there. No `CorsLayer` exists yet anywhere in `crates/axon-api` — this
  is the recommended shape, not a restructuring. Adding the Tauri origin
  later is then a one-line addition to that list.
  *(ADR 0102 § 2 removes this from the shell's critical path: the packaged app
  goes through Tauri's Rust-side HTTP and WebSocket plugins, so it is never a
  CORS client. M-W1.5 stays unbuilt and stays owed to separately-hosted browser
  deployments.)*
- **`file://` routing hash-fallback under Tauri** (open question 5 below)
  remains unresolved and unimplemented — see `clients/web/src/app.tsx:134`,
  which flags it explicitly as "M-W12's problem."
  *(Resolved by ADR 0102 § 5: history routing is kept on every target, with a
  Rust URI-scheme handler in the shell serving `index.html` for any non-asset
  path — the same rule `deploy/web/Caddyfile` already implements.)*

### Out of scope for this roadmap

OAuth 2.0 + PKCE (seam only; revisit after this lands in the server),
web push, typing indicators, read receipts, presence (no server support), room
join/leave/create (no API), server-side
thumbnailing, an i18n framework, a rich-text editor, and macOS/mobile Tauri
targets. *(ADR 0102 brings macOS into M-W12 and the mobile targets into the new
M-W13; the rest of this list stands.)*

Media upload was on this list when the roadmap was written ("no API"). M15
(ADR 0059) built that API, so it moved _into_ scope as M-W8.5 above.

### Open questions for team discussion

Recommendations are stated; none blocks M-W1. Items that would change routing
or WebSocket behavior must be resolved before the milestone called out below,
not after dependent UI ships.

1. **WS reconnect gap-fill ambition.** M-W6 does client-side refetch of the
   open room's head. A server-side WS resume cursor (`since` on connect)
   would fix the dropped-frame problem for all rooms and benefit the TUI
   too — but is new server work. Decide before M-W1.5 whether that server
   PR should include the cursor, or whether M-W6 intentionally starts with
   client-side gap-fill.
2. **Media auth transport.** Fetch + blob is simple but buffers whole files
   in memory. A service worker injecting the `Authorization` header would
   allow native `<img>` URLs and streaming, at the cost of SW lifecycle
   complexity and Tauri WebView uncertainty. Revisit if blob memory
   pressure appears.
3. **Component library.** Recommend hand-rolled primitives — the UI surface
   (lists, timeline, modals) is narrow. `preact/compat` + a headless
   library (e.g. Radix) remains available if modal/focus work balloons.
4. **Compose UX.** Recommend plain textarea with markdown-on-send for MVP
   (the TUI's semantics); a rich editor is a ProseMirror-class dependency
   and a post-MVP silo.
5. **Routing mode.** History routing is cleaner; hash routing is the safe
   answer under Tauri `file://`. Recommend history in the browser with a
   Tauri-conditional fallback. Resolve this before M-W3, because the
   deep-link shape becomes a search/navigation contract.
   *(Closed by ADR 0102 § 5: history everywhere, no hash fallback. Tauri v2
   serves from a custom scheme rather than `file://`, so the shell can answer
   the fallback itself and the deep-link contract stays one shape.)*
6. **Responsive posture.** Recommend desktop-first, non-broken at narrow
   widths; native mobile clients follow per ADR 0031, so mobile-web is a
   stopgap, not a target.
   *(Overtaken by events, then by ADR 0102: ADR 0031's native mobile clients
   are withdrawn, so the responsive layout is the mobile client. This ADR's own
   ADR 0062 two-pane work, ADR 0075 swipe-back, and ADR 0077 on-device perf
   readout had already stopped treating it as a stopgap.)*
7. **Keyboard-shortcut parity.** The TUI is keyboard-everything with a
   fully rebindable keymap. Recommend a fixed core set for MVP (room nav,
   compose focus, search, reply/edit on selection); rebindability later.
8. **i18n.** Recommend hardcoded English (TUI precedent) with strings kept
   in a single module to ease later extraction.
9. **CI triggers.** GitHub-hosted runner minutes are the reason to keep
   hosted workflows manual-dispatch-only. If a self-hosted web-capable runner
   is available, a path-filtered `push`/`pull_request` lane for `clients/web/`
   is low-risk; otherwise keep the M-W1 workflow `workflow_dispatch`.

## Consequences

- ADR 0031's last open question is resolved; `clients/web/` work may begin
  once this ADR is accepted. ADR 0031 is amended to point here.
- `clients/web/` stays in this monorepo through the basic browser client and
  parity audit. The team can revisit extraction once M-W11 declares the SPA
  stable and the release/CI boundary is real rather than speculative.
- A small server-side change (CORS, M-W1.5) enters the roadmap; it is the
  only non-web silo in the plan.
- The web client improves on the TUI in one behavior (reconnect gap-fill)
  while preserving the TUI's read-on-reconnect refreshes for verification,
  read markers, and drafts; if discussion item 1 chooses a server resume
  cursor, that supersedes the client-side timeline refetch approach.
- The `AuthProvider` seam and the Tauri-compatibility constraint are
  standing architecture rules for `clients/web/` from the first commit.
- The web client becomes the design reference for the mobile clients
  (ADR 0031): its screen flows and URL structure inform the SwiftUI and
  Compose implementations. **Superseded by ADR 0102**, which withdraws the
  SwiftUI and Compose clients: the web bundle is no longer a reference the
  mobile clients imitate, it is the mobile client, wrapped in a Tauri shell.
