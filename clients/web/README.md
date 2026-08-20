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
(M19-W1), an incoming-invite inbox (`/invites`, Accept/Reject and Accept
all/Reject all; an Invites row appears at the top of the room list when any
are pending), `/invite` and `/cancel` room invite commands, room entry via
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
live WebSocket updates for timelines/room previews/unread state, live
ephemeral overlays for typing indicators plus public read receipts, and an
offline-first content cache (ADR 0085: warm per-room timeline stores across
room switches in one session, plus a durable IndexedDB copy of the room list
that paints before `/v1/rooms` answers and reconciles in place, marked
"Updating…" until it does — room metadata only, no message bodies; on by
default and switchable off under Settings → Room list, which also erases what
was stored).

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
   `http://localhost:8080`; see [CONTRIBUTING.md](../../CONTRIBUTING.md) for running the
   server, or set `AXON_SERVER_URL` (below) to point at an existing one.

5. Start the dev server:

   ```sh
   pnpm dev
   ```

   Vite prints a local URL (default `http://localhost:5173`) — open it in a
   browser.

On older MacOS Intel-based devices, you may see this error executing pnpm:

```
[ERROR] Cannot verify the identity of the @pnpm/exe.darwin-x64 native binary: it is missing from pnpm-lock.yaml.
```

This is an upstream bug. If that occurs, switch to `corepack` to install pnpm:

```sh
cd clients/web
npm install -g corepack@latest
corepack enable
corepack use pnpm@11
```

## Development

```sh
pnpm install
pnpm dev
pnpm exec playwright install  # if you want to run playwright (browser simulation) tests
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

The Settings page shows the bundle's build identity so testers can confirm which
copied client they are seeing. It reads `release+version` — for example
`0.1.0+ab12cd34ef56`:

- **release** — `version` from this package's `package.json`. The human-readable
  number; bump it when you cut one.
- **version** — the exact build id. By default the current git commit
  (`--short=12`), with `-dirty` appended when `clients/web` has uncommitted
  changes. This is what identifies a build; the release alone cannot, since
  every build of a release shares it.

For ad hoc test deploys, override the build id with:

```sh
VITE_AXON_WEB_VERSION=iphone-swipe-test-4 pnpm build
```

A build also writes the same values to `dist/version.json`:

```json
{
  "release": "0.1.0",
  "version": "ab12cd34ef56",
  "builtAt": "2026-08-02T14:03:11.204Z"
}
```

### Automatic refresh on a new build

A running client watches `/version.json` and applies a new build on its own
(ADR 0087), so testers no longer have to force-kill the PWA or reload by hand
after a deploy. It checks when the live socket reconnects (a deploy restarts the
server, so this fires within seconds), when a hidden tab becomes visible, on
`online`, and every 15 minutes as a backstop.

What it does about an update depends on what it would cost:

- Returning to a tab that has been hidden a minute or more: flushes drafts and
  reloads silently.
- You are using the app, or you have an unsent message or an upload in flight:
  a banner under the topbar, with a Reload button. Nothing reloads under you.
- A lazy-loaded chunk that the deploy deleted: reloads immediately, since that
  failure is unrecoverable.

Every automatic reload is guarded so it can happen at most once per build per
tab — if the origin's `version.json` disagrees with the bundle it actually
serves, the client reloads once and then falls back to the banner rather than
looping. **Settings → Check for updates** asks on demand.

**None of this runs under `pnpm dev`.** HMR already owns reloading there, and
the dev stamp is not a deployment identity: it is a git hash plus a `-dirty`
flag, read once when the dev server starts, so it moves whenever your working
tree does. Without the guard, restarting the dev server after a commit or an
edit would make every open tab decide a "new build" had shipped and reload
itself. If a dev tab is refreshing on its own, it is Vite — an HMR full reload,
or the "optimized dependencies changed" reload after a dep re-optimize — not
this.

Every automatic reload announces itself first, so a refresh is never a mystery:

```
[axon:update] reloading to pick up a new build: ab12cd34ef56 → ef78ab90cd12 (attempt 1/3 this window)
[axon:update] declining to reload ab12cd34ef56 → ef78ab90cd12: already attempted from this build. The banner is the remaining path.
```

Filter the console on `[axon:update]`. Nothing there means the reload came from
somewhere else. The guard's state is readable directly, too:

```js
JSON.parse(sessionStorage.getItem('axon:update-reload'))
// { from, targets: [...], spent, windowStartedAt }
```

Note for anyone serving the built bundle: the SPA history fallback must not
apply to `/assets/*`. A missing hashed chunk has to return `404`, not
`index.html` with a `200`, or the browser tries to parse HTML as a module and
the app hangs — the exact failure this feature was built to fix. Both shipped
servers are guarded (`vite.config.ts` for `pnpm preview`, `deploy/web/Caddyfile`
for the Docker stack); a third needs the same. Check yours with:

```sh
curl -sI https://your-host/assets/nope-abc123.js   # must be 404, not 200
```

Cache headers follow from the same distinction: `/assets/*` is content-hashed
and can be `immutable`, while `index.html`, `version.json`, and the manifest
change under a fixed name and must be revalidated.

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

| Script                              | Purpose                                                                                                                                        |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- |
| `pnpm dev`                          | Dev server with HMR and the `/v1` proxy                                                                                                        |
| `pnpm build`                        | Type-check and produce a deployable `dist/`                                                                                                    |
| `pnpm preview`                      | Serve the built `dist/` locally                                                                                                                |
| `pnpm gen:api`                      | Regenerate `src/api/schema.d.ts` from the spec                                                                                                 |
| `pnpm check:api`                    | Check generated API types for drift                                                                                                            |
| `pnpm test`                         | Vitest, single run                                                                                                                             |
| `pnpm test:watch`                   | Vitest, watch mode                                                                                                                             |
| `pnpm test:e2e`                     | Playwright e2e suite (Chromium; CI also gates Firefox and desktop WebKit, then runs iPhone-profile WebKit after a web change merges to `main`) |
| `pnpm test:e2e:perf`                | The ADR 0071 timeline→room-list perf spec, also under WebKit (sets `PERF=1`, which gates the extra WebKit project in `playwright.config.ts`)   |
| `pnpm demo`                         | Record the ADR 0086 demo videos against a seeded local stack (needs `DEMO_MANIFEST`; see § Demo recording)                                     |
| `pnpm lint`                         | ESLint + Prettier check                                                                                                                        |
| `pnpm format` / `pnpm format:check` | Prettier write / check                                                                                                                         |

An optional live round-trip suite runs against a real server when
`AXON_LIVE_URL` and `AXON_LIVE_TOKEN` are set (see
`src/api/client.live.test.ts`); it is skipped otherwise, including in CI.

### Playwright browser targets

`pnpm test:e2e` builds the client and runs Chromium. The Playwright mock server
serves the built `dist/` directory, so run `pnpm build` before invoking
Playwright directly; otherwise the browser can exercise stale or absent build
assets instead of the checked-out source.

```sh
# One desktop engine at a time.
pnpm build
pnpm exec playwright test --project=chromium --fail-on-flaky-tests
pnpm exec playwright test --project=firefox --fail-on-flaky-tests
pnpm exec playwright test --project=webkit-desktop --fail-on-flaky-tests

# The iPhone 13 WebKit profile (the post-merge lane).
MOBILE_E2E=1 pnpm exec playwright test --project=webkit-iphone --fail-on-flaky-tests

# Every configured desktop and iPhone-profile target.
MOBILE_E2E=1 pnpm exec playwright test --fail-on-flaky-tests
```

`--fail-on-flaky-tests` turns a pass-on-retry into a failure — which means it
does nothing at all unless retries are enabled, and `playwright.config.ts` sets
`retries: process.env.CI ? 1 : 0`. So:

- **Locally** there are no retries, so there is no "flaky" outcome to begin with:
  a test that fails its first attempt is simply a failure, with or without the
  flag. The flag is harmless here but redundant.
- **On CI** `retries: 1` applies, and the lanes pass the flag, so a test that
  fails its first attempt still fails the job even when its retry passes.

The practical consequence is that both environments reject a first-attempt
failure, while CI still retries to capture a trace and distinguish a flaky test
from a persistent failure. To reproduce the gate exactly, set `CI=1` and pass
`--fail-on-flaky-tests`; `CI=1` also makes Playwright require a fresh mock server
rather than reusing one already listening on port 4599.

**A local full-suite Firefox run should pass.** #157 traced its former
order-dependent failures to the mock backend leaving every client-initiated
WebSocket close half-finished. The retained connections eventually prevented
Firefox from opening another socket, so whichever spec next entered shared
setup (`openRoom`/`expectLive`) appeared to hang. The mock now completes the
close handshake and the CI lanes use `--fail-on-flaky-tests`, so a retry can no
longer hide a recurrence.

CI runs the peer-dependency check, the schema sync check, lint, format check,
tests, and the build via `.github/workflows/web-lint-and-test.yml`
(path-filtered `pull_request` plus manual `workflow_dispatch`). The repo-root
`.pre-commit-config.yaml` runs the same web checks at pre-push time for both git
and jj users, and rustfmt/clippy/`cargo test` when rust sources change — it is
the only list of pre-push checks the repo has (ADR 0092). The web hooks use the
normal web dependencies, and the `web-install` hook runs
`pnpm install --frozen-lockfile` for you whenever the manifest, lockfile, or
`pnpm-workspace.yaml` changes, so they cannot silently test a stale tree.

Playwright is deliberately not in that hook (browsers and minutes);
`web-e2e.yml` gates pull requests, and `AGENTS.md` § "Definition of done for a
UI change" says when to run it locally.

## Demo recording

The web demo videos (ADR 0086 phase 3) are recorded by Playwright against a
**real** axon: a throwaway Synapse + Postgres + axon stack seeded from
`smoke/local-stack/corpus/demo.toml`. Search results therefore come from a real
Tantivy index, media really traverses the media proxy, and the WebSocket is the
real one. `e2e/mock-server.mjs` is not involved and `pnpm test:e2e` is
untouched — the lane has its own config, `playwright.demo.config.ts`.

Needs Docker with Compose v2, so it is Unix-only and is not a CI gate.

```sh
# 1. seed a demo world and leave it running (from the repo root)
cargo run -p axon-smoke-local-stack -- up \
    --manifest /tmp/demo.json \
    --corpus smoke/local-stack/corpus/demo.toml --keep-up

# 2. record (from here)
DEMO_MANIFEST=/tmp/demo.json pnpm demo

# 3. tear the world down (from the repo root)
cargo run -p axon-smoke-local-stack -- down --manifest /tmp/demo.json
```

Output lands in `demo-artifacts/<test>/video.webm`, one clip per scene, at
1440×900 for `demo-desktop` and 780×1328 for `demo-mobile` (an iPhone 13
descriptor, so WebKit, real touch, and a device pixel ratio of 3; its viewport
is 390×664, the screen less the browser chrome).

Both recordings are pinned to the **light** theme, and to a light emulated
`prefers-color-scheme` to match. The shipped default follows the system
preference, which would otherwise make a take's appearance depend on the
machine that made it.

**Record a real take against a freshly seeded stack.** The mutating scenes undo
themselves by redacting what they sent, and a redaction leaves a permanent
"message deleted" row that nothing can remove. Reruns while authoring are fine —
the driver clears leftovers over the API and warns when it finds any — but those
tombstones accumulate in the take.

Useful while authoring:

| Variable        | Effect                                                    |
| --------------- | --------------------------------------------------------- |
| `DEMO_PACE=0`   | Strip every dwell — a full dry run in well under a minute |
| `DEMO_PACE=1.5` | Slow the whole take down for a narrated cut               |
| `DEMO_PORT`     | Move the preview server off 4600                          |

`pnpm demo --grep rooms` plays a single scene; `--project demo-mobile` a single
form factor.

Playwright writes **WebM**. The release assets and `demo.html` are MP4 (ADR
0086), so convert before uploading:

```sh
ffmpeg -i video.webm -c:v libx264 -pix_fmt yuv420p -movflags +faststart out.mp4
```

Then attach the MP4s to the `demo-2026-08` release and re-run the `api-docs`
workflow by hand — re-uploading a release asset changes no path in the
repository, so nothing in the workflow's `paths:` trigger fires and the site
keeps serving the previous recording.

Adding or changing a scene means updating `docs/demo-coverage.md` in the same
PR. Two rules the scenes are built on, both learned the hard way (ADR 0086):
a step that changes state needs an assertion **only the new state satisfies**
(assert on what _left_, not on what stayed), and a scene that mutates
**scripts its own undo**, or its second run passes vacuously on what its first
run left behind. Both are stated in full, with the Playwright specifics, in the
`e2e/demo/demo.ts` module docstring — change them there first.
