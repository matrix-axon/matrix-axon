# axon web client

Preact + TypeScript + Vite SPA for axon (ADR 0046). This package is
self-contained: it is not a Cargo workspace member and has its own pnpm
lockfile.

**Status (through M-W10, plus M19-W1 through M19-W4):** a usable read/write client — SSO sign-in
through Axon OAuth Path A with token-paste fallback, the full account
lifecycle (under `/accounts`, including 4S recovery-key import with offline
key-format validation and success/error feedback), theme +
schema-versioned settings, history routing with the permanent deep-link
shape `/:accountId/rooms/:roomId?thread=&event=` (signed off), the
cross-account room list with TUI semantics (pinning ADR 0038, sort/filters
ADR 0042, member-derived DM titles, server-derived room unread counts), the room
timeline (cursor-paginated scroll-back, date jump, DOMPurify-sanitized
formatted bodies with spoilers, state-events toggle, ADR 0033 relations
display, UTD/redaction placeholders, `?event=` highlighting, plain-text URL
linkification), media (M-W8, ADR 0064: inline images and stickers with a
full-size lightbox, download cards for files/audio/video, all fetched through
the authenticated media proxy as blob URLs; inline `<img>` re-admitted to the
sanitizer for `mxc://` only), an inline preview for audio/video/PDF/text
attachments (ADR 0072) and syntax-highlighted code blocks and text attachments
(ADR 0073, highlight.js), media _send_ (M-W8.5, ADR 0065: attach by
paperclip, drag-and-drop onto the room or thread pane, or paste; the composer's
text becomes the caption; staged upload then `send-media`, with an optimistic
echo that shows the picked image while its bytes are still going up, and
Retry/Discard on failure — one file at a time, images and files only), full
messaging: composer with markdown-on-send, member/room autocomplete that emits
pill links in sent and edited text, reply (ADR 0032), edit,
redact-with-confirm, reaction toggle, and threads (badges, panel via
`?thread=`, send-in-thread, and an unread-thread drawer that keeps hidden
thread replies unread until their thread panel loads), full-text message
search (M-W10, ADR 0066: a URL-addressed overlay opened with `/`, `Ctrl-G`, a
topbar button, or `/search`, with chip/token filters and client-side
re-sorting), `/leave`, `/part`, and `/forget` room-membership slash commands
(M19-W1), `/invite` and `/cancel` room invite commands, room entry via
`/join <room-or-matrix-link>` and `/knock <room-or-matrix-link> [reason]`
(M19-W2: parses raw room ids/aliases, matrix.to links, and `matrix:` URIs;
intercepts Matrix room links in the signed-in shell, showing an unknown target
as a join pill; can register the web origin as a browser-level `matrix:`
protocol handler from Settings, with a matching PWA manifest declaration for
browsers that register manifest protocol handlers with the OS), an "Add a Room"
surface with Join, DM, Create, and Find flows (M19-W3/W4: direct room
ID/alias/Matrix-link entry, public room-directory search defaulting to the
account homeserver, DM creation/opening, and room creation), Room Information
actions for copying a room link, inviting users, canceling pending invites,
leaving with confirmation, opening member DMs, room-state details (access,
encryption, pinned messages, space relationships, and upgrade links), and a
browser-local Spaces picker that filters the room list, TUI-parity keyboard
shortcuts with a `/shortcuts` help overlay (ADR 0078, web keyboard shortcuts),
live WebSocket updates for timelines/room previews/unread state, and live
ephemeral overlays for typing indicators plus public read receipts.

Note for deployment: history routing means the host must rewrite unknown
paths to `index.html` (the Vite dev server already does). ADR 0030's
`sync_state` is rendered when present, but the server does not emit it yet —
see the note in `src/stores/accounts.ts`.

## Prerequisites

- **Node.js 24 (LTS)** — recommended, and what CI uses. The hard floor is
  Node 22.13: pnpm 11 requires the `node:sqlite` builtin (unflagged only in
  22.13+/23.4+), and Vite 8 requires 20.19+/22.12+. Older or odd-numbered
  releases (18, 20.x < 20.19, 21, 23.x < 23.4) fail with errors like
  `ERR_UNKNOWN_BUILTIN_MODULE: No such built-in module: node:sqlite`.
- **pnpm 11** — version pinned in `package.json`'s `packageManager` field.

## Environment setup from scratch

1. Install Node 24. With [nvm](https://github.com/nvm-sh/nvm):

   ```sh
   nvm install 24
   nvm alias default 24
   ```

   (Or use your OS package manager / [nodejs.org](https://nodejs.org)
   installer, as long as `node --version` reports ≥ 22.13.)

2. Install pnpm:

   ```sh
   npm install -g pnpm
   ```

3. Install this package's dependencies:

   ```sh
   cd clients/web
   pnpm install
   ```

4. Have an axon server to talk to. By default the dev server expects one at
   `http://localhost:8080`; see the repo's top-level README for running the
   server, or set `AXON_SERVER_URL` (below) to point at an existing one.

5. Start the dev server:

   ```sh
   pnpm dev
   ```

   Vite prints a local URL (default `http://localhost:5173`) — open it in a
   browser.

## Development

```sh
pnpm install
pnpm dev
```

The axon server serves no CORS headers, so the browser cannot call it
cross-origin. In development the Vite dev server proxies `/v1` (HTTP and
WebSocket) to the axon server instead — the app makes same-origin requests
and no CORS is involved. The proxy targets `http://localhost:8080` by
default; point it elsewhere with:

```sh
AXON_SERVER_URL=http://host:port pnpm dev
```

Vite only answers dev-server requests addressed to localhost. To test
through another hostname (a tunnel, a LAN name, a reverse proxy), allowlist
it without editing the config:

```sh
AXON_DEV_ALLOWED_HOSTS=axon-web.example.net,axon-dev.local pnpm dev
```

Deployed builds make real cross-origin calls and depend on server-side CORS
support (ADR 0046, milestone M-W1.5). Unlike `AXON_SERVER_URL` above (a
dev-server-only proxy target), a separately-hosted deployment bakes the
server's origin into the built bundle at build time:

```sh
VITE_AXON_SERVER_URL=https://axon.example.com pnpm build
```

Leave it unset for a same-origin deployment (the default Docker Compose
stack's single front door, or the Vite dev proxy) — the client then requests
`/` and relies on the reverse proxy or dev server to route it, with no CORS
involved.

### Build identity

The Settings page shows the static web bundle's build identity so testers can
confirm which copied client they are seeing. By default `pnpm build` bakes in
the current git commit, appending `-dirty` when `clients/web` has uncommitted
changes, plus the build timestamp.

For ad hoc test deploys, override the visible version label with:

```sh
VITE_AXON_WEB_VERSION=iphone-swipe-test-4 pnpm build
```

## SSO sign-in

The web client consumes Axon's OAuth authorization-code + PKCE flow
(`GET /v1/oauth/authorize` → `/oauth/callback` → `POST /v1/oauth/token`).
The server must have `oauth.enabled = true`, a matching `[[oauth.clients]]`
entry for `axon-web`, and an exact redirect URI for this web origin:

```toml
[[oauth.clients]]
client_id = "axon-web"
redirect_uris = ["https://myaxon.example.com/oauth/callback"]
```

Provider buttons are configured in the web build until the server exposes
provider discovery (tracked in issue #264):

```sh
VITE_AXON_OAUTH_PROVIDERS=google:Google,microsoft:Microsoft pnpm build
```

Use `VITE_AXON_OAUTH_CLIENT_ID` only if the server registered this web app
under a different OAuth client id. If no providers are configured, the sign-in
screen shows only the manual token form. Manual tokens minted with
`axon token issue` remain supported as a fallback.

## Generated API client

`src/api/schema.d.ts` is generated from the repo's OpenAPI contract
(`openapi/openapi.json`) by [openapi-typescript] and consumed through the
typed [openapi-fetch] factory in `src/api/client.ts`. After any change to the
contract, regenerate and commit the result:

```sh
pnpm gen:api
```

To check drift without rewriting the committed file, run:

```sh
pnpm check:api
```

CI and the optional pre-push hook both use `pnpm check:api`. The generated file
is excluded from ESLint and Prettier.

[openapi-typescript]: https://openapi-ts.dev/
[openapi-fetch]: https://openapi-ts.dev/openapi-fetch/

## Live WebSocket state

`src/api/ws.ts` opens the authenticated `/v1/ws` connection. The live layer
fans out decoded frames to stores by tag: timeline frames update room state,
device-state frames synchronize drafts/read markers, and
`ephemeral.passthrough` frames render live typing indicators plus public read
receipts. Ephemeral state is an in-memory overlay only; it is not replayed or
restored after reload.

## Auth seam

Per ADR 0031/0046, nothing outside `src/auth/` knows how tokens are obtained:
the API layer consumes the `AuthProvider` interface (`src/auth/provider.ts` —
`getToken()`, `onAuthFailure()`, a `LoginBootstrap` UI slot). The browser
implementation is a composite provider: OAuth 2.0 + PKCE stores Axon-issued
access/refresh tokens in `localStorage`, while token-paste remains available
for locally minted tokens from `axon token issue`. A Tauri OS-keychain provider
can still slot in later behind the same seam.

## Scripts

| Script                              | Purpose                                                                                                                                      |
| ----------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `pnpm dev`                          | Dev server with HMR and the `/v1` proxy                                                                                                      |
| `pnpm build`                        | Type-check and produce a deployable `dist/`                                                                                                  |
| `pnpm preview`                      | Serve the built `dist/` locally                                                                                                              |
| `pnpm gen:api`                      | Regenerate `src/api/schema.d.ts` from the spec                                                                                               |
| `pnpm check:api`                    | Check generated API types for drift                                                                                                          |
| `pnpm test`                         | Vitest, single run                                                                                                                           |
| `pnpm test:watch`                   | Vitest, watch mode                                                                                                                           |
| `pnpm test:e2e`                     | Playwright e2e suite (Chromium)                                                                                                              |
| `pnpm test:e2e:perf`                | The ADR 0071 timeline→room-list perf spec, also under WebKit (sets `PERF=1`, which gates the extra WebKit project in `playwright.config.ts`) |
| `pnpm lint`                         | ESLint + Prettier check                                                                                                                      |
| `pnpm format` / `pnpm format:check` | Prettier write / check                                                                                                                       |

An optional live round-trip suite runs against a real server when
`AXON_LIVE_URL` and `AXON_LIVE_TOKEN` are set (see
`src/api/client.live.test.ts`); it is skipped otherwise, including in CI.

CI runs the schema sync check, lint, format check, tests, and the build via
`.github/workflows/web-lint-and-test.yml` (path-filtered `pull_request` plus
manual `workflow_dispatch`). The repo-root `.pre-commit-config.yaml` can run the
same web checks at pre-push time for both git and jj users. Those hooks use the
normal web dependencies, so run `pnpm install --frozen-lockfile` here before
relying on them locally.
