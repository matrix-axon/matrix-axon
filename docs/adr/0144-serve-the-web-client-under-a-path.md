# ADR 0144 — Serve the web client and the API under a configurable path

**Status:** Proposed.

## Context

Axon assumes it owns the root of its origin.
An operator who wants `https://example.com/axon/` instead of `https://axon.example.com/` cannot get a working web client today, whether the server serves the bundle itself (ADR 0116) or a separate front door does (ADR 0052).

The assumption lives in a small number of places, and the API is not one of them.

**The API already copes with a prefix.**

- The web client's `apiUrl` joins `/v1/...` onto a base by concatenation, because `normalizeServerUrl` promises to keep a path (`clients/web/src/server-url.ts`).
- The TUI builds every request as `format!("{}/v1/...", base_url)`.
- `oauth.external_base_url` accepts a path, and `crates/axon-server/src/oauth.rs` already tests `https://axon.example/prefix/`.

**The web bundle does not.**

- `vite.config.ts` sets no `base`, so `index.html` loads `/assets/...`, `/favicon.svg` and `/manifest.webmanifest` from the root.
- `manifest.webmanifest` has `id`, `start_url`, `scope`, the icons and the `matrix:` protocol handler at `/`.
- The client fetches `/version.json` (`stores/update-check.ts`) and `/emoji-picker-data-en.json` (`emoji.ts`) from the root.
- The browser's default API base is `/` (`platform.defaultApiBaseUrl`).
- preact-iso has no base path.
  `LocationProvider` hands `location.pathname` to the router unchanged, so `<Route path="/settings">` matches only `/settings`.
  The client also calls `location.route('/privacy')`, renders `<a href="/settings">`, and tests `window.location.pathname === '/oauth/callback'`.
- The OAuth `redirect_uri` is `new URL('/oauth/callback', window.location.origin)`, which drops any path.

**The server's own HTML does not.**

- The bootstrap page posts to `action="/bootstrap/{code}/token"` and links to `/bootstrap/{code}/oauth/{provider}`.
- When the server serves the web client and `web_client_url` is unset, the bootstrap handoff goes to `/`.
  ADR 0116 § First sign-in already tells an operator behind a prefix to set `web_client_url` by hand.

**Browser storage is per origin, not per path.**
`localStorage` (`axon.token`, `axon.oauth.session`, `axon.settings`, and some 15 more), `sessionStorage` and the `axon-cache` IndexedDB database (ADR 0085) are all keyed by origin.
At the root that is one instance per origin by construction.
Under a path it is not: `example.com/adam/` and `example.com/jamie/` would read and overwrite each other's token and cache.

**Two facts constrain the fix.**

- The embedded bundle is built once, in release CI, and shipped in a `.deb`, an `.rpm` and a Homebrew bottle (ADR 0116 step 2).
  The operator cannot rebuild it.
  The path must therefore be chosen when the server starts, not when the bundle is built.
- Reverse proxies differ on whether they strip a path prefix before forwarding.
  Caddy's `handle_path` and Traefik's `StripPrefix` strip it, Caddy's `handle` does not, and nginx strips it or not depending on whether `proxy_pass` ends in a URI.
  `tailscale serve --set-path` behaviour is not something most operators have checked.
  A design that works with only one style will be misconfigured often, and the symptom (a blank page, or `401`s on the wrong paths) points nowhere near the cause.

## Decision

### 1. One setting: `[server] base_path`

`[server] base_path` (`AXON_SERVER__BASE_PATH`) is the path at which clients reach this server.
It defaults to `/`, which is today's behaviour exactly.

It names the whole server, not only the web client.
The bundle and the API must share an origin (ADR 0116 § Context), and the client finds the API at its own base, so a web client at `/axon/` means an API at `/axon/v1/`.
A separate `web_base_path` would describe a layout that cannot work.

The value is validated at startup:

- It must begin with `/`, and it may have several segments (`/apps/axon`).
- A trailing slash is accepted and stored without it; `/` stays `/`.
- It must not contain `.` or `..` segments, `?`, `#`, `%`, or empty segments.
- Its first segment must not be one the server owns at the root: `v1`, `healthz`, `bootstrap`, `assets`.
  Section 2 explains why a collision there would misroute real requests.

### 2. The server accepts requests with and without the prefix

A request whose path is `base_path` or starts with `base_path/` has the prefix removed before routing.
Any other request is routed as it is.

This is what lets the server work behind either kind of proxy without being told which:

| Proxy                                    | Public request                | Server receives  | After stripping |
| ---------------------------------------- | ----------------------------- | ---------------- | --------------- |
| passes the path through (Caddy `handle`) | `/axon/v1/rooms`              | `/axon/v1/rooms` | `/v1/rooms`     |
| strips the prefix (Caddy `handle_path`)  | `/axon/v1/rooms`              | `/v1/rooms`      | `/v1/rooms`     |
| none, on the server itself               | `http://127.0.0.1:8080/axon/` | `/axon/`         | `/`             |

It also keeps every root path that a script already depends on.
`/healthz` still answers at the root, so the compose healthcheck and any monitor keep working when an operator adds a prefix.
The TUI configured with `http://127.0.0.1:8080` before the change still works.

**The server never redirects between the two forms.**
Under a stripping proxy, a redirect from `/` to `/axon/` arrives back as `/`, and the browser loops.
Every answer is served in place.
The client tidies its own address bar (section 4) with `history.replaceState`, which is not a navigation.

**The stripping happens before routing, outside the router.**
axum runs `Router::layer` middleware after the route is matched, so a URI rewritten there would not change which handler runs.
The rewrite is a `map_request` layer wrapped around the finished `Router`, in `axon-server` where it is served, the pattern the axum documentation gives for rewriting a request URI.

That placement has a security consequence, and it is the reason the alternative, `Router::nest(base_path, ...)`, is rejected.
The request log redacts stream tickets by testing `path.starts_with("/v1/media-stream")` (ADR 0139 § 5, `redacted_request_path`).
Under `nest`, the trace layer would see `/axon/v1/media-stream/<ticket>`, the test would fail, and every ticket would reach the log.
With the prefix gone before the trace layer runs, every handler, every route template and the redaction see the paths they see today.
A test pins this: a ticketed request under a `base_path` must log the route template.

The startup log states the base path once, and says that unprefixed requests are accepted too.
The access log records the path after stripping.

### 3. The web client is relocatable at run time

The bundle is built once and works under any base, so that the embedded copy, a Docker copy and a hand-deployed copy are the same bytes.

- `vite.config.ts` builds with a relative base (`base: './'`).
  Vite then writes `./assets/...` into `index.html`, resolves preloaded chunks against `import.meta.url`, and leaves CSS `url()`s relative to the stylesheet.
- `index.html` gains `<base href="/">` as the first element of `<head>`, before anything that loads a URL.
  Relative URLs resolve against it rather than against the page's own address, which is what makes `./assets/app.js` correct at `/axon/rooms/!x:y/` as well as at `/axon/`.
  The tag at `/` changes nothing for a root deployment.
- Relocating the bundle means replacing that one tag with `<base href="/axon/">`.
  It is the only per-deployment byte in the bundle.
  A build-time check (a Vite plugin hook) fails the build unless the emitted `index.html` has exactly one `<base href="/">`, so the marker cannot silently disappear.
- `public/manifest.webmanifest` uses relative URLs throughout (`"start_url": "./"`, `"scope": "./"`, `"id": "./"`, `"src": "icon-152.png"`, `"url": "./?matrixLink=%s"`).
  Manifest URLs resolve against the manifest's own URL, and `id` against `start_url`.
  At the root these resolve to exactly the values they have today, so an installed PWA keeps its identity and is not offered as a second app.
  Under `/axon/` they resolve to `/axon/`, so each path is its own installable app with its own scope.
- The client reads its base from `document.baseURI` in one module, `app-base.ts`, which exports the base path and `appUrl(path)` / `appPath(pathname)` to add and remove it.
  `version.json`, the emoji data, the OAuth `redirect_uri` and every in-app link go through it.
- The browser's default API base becomes the app base instead of `/`.
  A user-configured server URL and `VITE_AXON_SERVER_URL` keep precedence (ADR 0102 § 3).
- The packaged shells serve the bundle at their own root and keep `<base href="/">`.
  Their CSP already allows it (`base-uri 'self'`), and they are never relocated.

`<base>` has one well-known side effect: a fragment-only link, `<a href="#x">`, resolves against the base and navigates away from the page.
The client has none today; the ESLint rule below forbids adding one.
Inline SVG `url(#id)` references are same-document by the CSS Values specification regardless of `<base>`, and the client has none either; the prefixed e2e lane covers icons regardless.

### 4. Routing goes through the base

preact-iso is kept.
Its router reads the location from `LocationProvider.ctx`, so a small `BaseLocationProvider` sits inside the stock `LocationProvider` and re-provides that context:

- `path` and `url` with the base removed, so every `<Route path="/settings">` stays as it is;
- `route(url, replace)` with the base added;
- `scope` set to the base, so preact-iso does not intercept a click on a same-origin link outside this instance.

In-app links are written relative to the base (`href="settings"`), which `<base>` resolves to `/axon/settings` and preact-iso then intercepts as before.
If the page loads at an address outside the base (the server itself, browsed at `http://127.0.0.1:8080/` with `base_path = "/axon"`), the client replaces the address with the base, without navigating.

A new ESLint rule (`no-restricted-syntax` and `no-restricted-properties`) rejects, outside `app-base.ts`:

- reads of `location.pathname`;
- a string beginning with `/` passed to `route(...)` or `href`, other than `/v1/...` API paths, which the API client already joins onto its base;
- `href` values beginning with `#`;
- direct use of `localStorage`, `sessionStorage` and `indexedDB` (section 5).

The audit that this rule enforces is part of the client change, not a follow-up, because a single missed absolute link is a broken page only under a prefix, and the root deployments that all tests run today would never see it.

### 5. Browser storage is namespaced by base

Several instances under one origin are the case this record makes possible, and the case the storage layout breaks.
Each instance's state is therefore kept apart:

- At base `/`, every key and database name stays exactly as it is.
  No existing deployment migrates anything, and nobody is signed out.
- Under any other base, `localStorage` and `sessionStorage` keys get the suffix `@<base>` (`axon.token@/axon/`), and the IndexedDB database becomes `axon-cache@/axon/`.

The namespacing is one `Storage` adapter and one database-name function in `app-base.ts`, used at the few places the service graph obtains its stores.
Every module already takes its `Storage` as a parameter, so no call site changes; the ESLint rule keeps it that way.

The bootstrap handoff (ADR 0052, ADR 0116 § First sign-in) writes the token into `localStorage` from a server-rendered page, and its key is hard-coded on both sides today (`WEB_CLIENT_TOKEN_STORAGE_KEY`).
The server computes the same suffix from `base_path`.
Both sides gain a test against one shared fixture listing the root and prefixed key for a few bases, so the two implementations cannot drift.

**This is collision avoidance, not isolation.**
Two instances on one origin are one security principal to the browser: script from either can read the other's storage, and they share notification permission.
The documentation says plainly that separate hostnames are the way to give different people separate instances, and that a path is for putting one Axon beside other things on a host the operator already has.

### 6. The server's own pages and URLs honour the base

- The bootstrap page's form `action` and SSO links are built from `base_path`.
- The bootstrap URL that the server logs, and that the packages print at install, includes the base: `http://127.0.0.1:8080/axon/bootstrap/<code>`.
  Because the server accepts both forms, it works both on the server and through the proxy.
- When the server serves the web client, an unset `web_client_url` defaults to `base_path` with a trailing slash, instead of `/`.
  This removes the manual step that ADR 0116 § First sign-in asks of an operator behind a prefix.
  An explicit `web_client_url` still wins.
- The embedded `index.html` is rewritten once at startup: `<base href="/">` becomes `<base href="{base_path}/">`, and its ETag is recomputed from the new bytes.
  Nothing else in the bundle changes, so the hashed assets keep their immutable caching and their ETags.
  If `base_path` is not `/`, the server serves the web client, and the embedded `index.html` has no marker, startup fails with an error naming the cause and `web_client = false` as the way out.
  It cannot happen with a bundle built from the same commit; it guards a hand-built `AXON_WEB_DIST`.
- `oauth.external_base_url` is not derived from `base_path`, because it also carries the public scheme and host, which the server cannot know.
  At startup the server warns when `base_path` is not `/` and the path of `external_base_url` does not end with it, since that is almost always a forgotten prefix and the symptom is an SSO callback that 404s at the provider.
- The "no web interface" page and its `/healthz` and `/v1/` hints use the base.

### 7. A front door other than the server

The separately served bundle (the Docker `web` container, or any static host) is the same bundle and is relocated the same way.

- `scripts/relocate-web.sh <dist> <base>` replaces the marker, and refuses a `dist` without exactly one.
  It is for any static host, and it documents the contract in twenty lines of shell.
- The `deploy/web` image takes `AXON_WEB_BASE_PATH` at container start.
  Its entrypoint runs the same substitution on its copy of `index.html` and the Caddyfile adds `uri strip_prefix {$AXON_WEB_BASE_PATH}` ahead of its existing handlers, which is a no-op for requests without the prefix.
  The container therefore accepts both forms too, for the same reasons as section 2, and it needs no rebuild to move.
- The compose stack passes one variable, `AXON_BASE_PATH`, to both containers (`AXON_WEB_BASE_PATH` and `AXON_SERVER__BASE_PATH`), so the two cannot disagree.

Two relocators exist, one in Rust and one in shell, for one exact-string substitution.
They are kept honest by sharing the build-time marker check's contract and by the prefixed smoke and e2e lanes below exercising each, not by sharing code across a process boundary that would cost more than it saves.

Development gets the same choice: `AXON_WEB_BASE_PATH=/axon/ pnpm dev` sets Vite's development `base` and moves the `/v1` proxy under it.

### 8. Tests

- **Server.**
  Unit tests for `base_path` validation and normalisation.
  Router tests for each row of the table in section 2, for `/axon` without a trailing slash, for `/axonx` (not a prefix match), for `/healthz` at both paths, for the rewritten `index.html` and its ETag, and for ticket redaction under a prefix.
  Bootstrap page tests for prefixed form actions, links and the handoff key.
- **Web.**
  Vitest for `app-base.ts`, the location adapter, and the storage adapter against the shared key fixture.
  A Playwright project that serves the built bundle under `/axon/` from the e2e mock, relocated by `relocate-web.sh`, and runs the existing navigation, deep-link reload, OAuth callback, update-check and emoji picker specs.
  An e2e test that asserts the manifest's resolved `id` at the root is unchanged.
- **Embedded.**
  `scripts/embed-web-smoke.sh` gains a run with `AXON_SERVER__BASE_PATH=/axon` that fetches the index, an asset and `/axon/healthz` from the binary, in the existing `embed-web.yml` job.
- **Proxies.**
  A CI job is not worth a container per proxy.
  Each shipped site file's prefixed variant is checked by hand once, with both stripping and pass-through forms, and the PR's verification guide records the result.
- **TUI.**
  Its URL joining already keeps a prefix; a unit test pins the REST and WebSocket URLs for a base with a path.

## Sequence

One silo per pull request (`AGENTS.md`), server first.

1. **This record**, docs only.
2. **Server** (`crates/`).
   `base_path`, the stripping layer, the `index.html` rewrite and its startup error, the bootstrap URLs, the `web_client_url` default, the `external_base_url` warning, and their tests.
   In the same PR: `axon.toml.example` and the configuration table in `docs/technical-reference.md`.
   Until step 3 lands, a `base_path` other than `/` with the web client on fails at startup with the error from section 6, because the bundle has no marker yet; the setting is not documented for operators until then.
3. **Web client** (`clients/web`).
   The relative build and the marker check, `app-base.ts`, the location adapter and the link audit, the ESLint rule, the manifest, storage namespacing, `relocate-web.sh`, the dev-server variable, the prefixed Playwright project, and `clients/web/README.md`.
4. **Deployment and packaging docs.**
   `deploy/web` entrypoint and Caddyfile, the compose variable, the prefixed embed smoke run, the proxy site files, and the operator-facing documentation below.
   The operator documentation lands here, when the whole path works end to end.
5. **TUI**, test only, at any point after step 2.

## Documentation and example configuration

| File                                                                                          | Change                                                                                                                                                                                                                       |
| --------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `axon.toml.example`                                                                           | `base_path` under `[server]`, commented, with the `/axon` example; the `web_client_url` comment updated for the new default; an `[[oauth.clients]]` example for `axon-web` showing `https://example.com/axon/oauth/callback` |
| `docs/technical-reference.md`                                                                 | `server.base_path` / `AXON_SERVER__BASE_PATH` in the configuration and environment tables                                                                                                                                    |
| `docs/server-admin.md`                                                                        | A new section, "Serving Axon under a path": the setting, both proxy styles, `external_base_url` and OAuth `redirect_uris`, storage per path, and "use a hostname to separate people"                                         |
| `packaging/proxy/axon-server.{caddy,nginx.conf,apache.conf}`                                  | A commented prefixed variant beside the root one, including the WebSocket upgrade under the prefix and the stream-ticket log redaction rewritten to match `/axon/v1/media-stream/` as well                                   |
| `packaging/README.md`, `packaging/README.Debian`, `packaging/axon-server.1`, Homebrew caveats | One paragraph each pointing at the server-admin section; the man page lists the setting                                                                                                                                      |
| `deploy/README.md`, `docker-compose.yml`, `deploy/docker-compose.yml`                         | `AXON_BASE_PATH`, commented, and how it reaches both containers                                                                                                                                                              |
| `deploy/web/Caddyfile`, `deploy/web/Dockerfile`                                               | The entrypoint and `strip_prefix` from section 7                                                                                                                                                                             |
| `docs/install.md`                                                                             | A sentence in the remote-access part linking to the new section                                                                                                                                                              |
| `clients/web/README.md`                                                                       | The `<base>` contract, `relocate-web.sh`, `AXON_WEB_BASE_PATH` for `pnpm dev`, and the rule against root-absolute paths                                                                                                      |
| `clients/web/AGENTS.md`                                                                       | The same rule, stated for agents, naming the ESLint guard                                                                                                                                                                    |
| `AGENTS.md`                                                                                   | One sentence on `base_path` in the "Embedded web client (ADR 0116)" bullet                                                                                                                                                   |
| `docs/adr/0116-web-client-in-the-server-binary.md`                                            | A note under § First sign-in that ADR 0144 makes the base the default `web_client_url`                                                                                                                                       |

The stream-ticket redaction in the proxy site files deserves its own mention.
Those files exist partly to keep tickets out of proxy logs (ADR 0139 § 5).
A prefixed variant that redacted only `/v1/media-stream/` would quietly start logging every ticket under `/axon/`, so the prefixed variants carry their own pattern and the verification guide checks a proxy log line by hand.

## Consequences

- One bundle runs at any path, and nothing about a root deployment changes: not the bytes served at `/`, not a storage key, not a PWA's identity.
- An operator sets one value to move Axon under a path, and need not know whether the proxy strips the prefix.
- Every server path is reachable under two names when `base_path` is set.
  Nothing relies on the path for authorisation, so this widens nothing; the bootstrap's loopback check reads the peer address, not the path.
- The web client gains a rule that every URL goes through one module.
  It is the price of relocation and the ESLint rule is what keeps paying it.
- Two instances on one origin no longer collide, but they share a security boundary, and the docs say so.
- Changing `base_path` on a running deployment signs the browser out and starts a cold cache, since the storage keys move with it.
  That is documented beside the setting.

## Alternatives considered

**A build-time base (`vite build --base=/axon/`).**
It is the Vite default and the simplest client change.
It cannot serve the embedded bundle, which is built before any operator exists, and it would make the bundle differ by deployment, which ADR 0116 rejected for the embed itself.

**Pass-through only: the server serves the prefixed paths and nothing else.**
Simpler to explain, and one name per resource.
It breaks behind every stripping proxy with symptoms nowhere near the cause, and it moves `/healthz` out from under existing probes.

**Strip-only: the proxy must remove the prefix, and the server learns it only to write URLs.**
The mirror image: it breaks behind every pass-through proxy, and the server browsed directly would serve a page whose `<base>` points at a path it does not answer.

**`X-Forwarded-Prefix` instead of configuration.**
It needs no setting where the proxy sends it, which Traefik does and Caddy, nginx and Apache do only when configured to.
The server would trust a header from any peer to decide what `<base>` a page carries, and a shared cache that ignores the header could hand one visitor's page to another.
It also leaves the bootstrap URL logged at startup, when there is no request, without a base.
It could be added later as an override that is honoured only from configured trusted proxies.

**Injecting the base as a global (`window.__AXON_BASE__`) instead of `<base>`.**
The `index.html` that loads the bundle would still need its own URLs rewritten, which a `<base>` does with one tag, and every relative URL in CSS and the manifest would still need a base to resolve against.

**Replacing preact-iso with a router that has a base option.**
A larger change for the same result; the adapter is a few dozen lines against a context preact-iso exports.

**Leaving storage shared and documenting "one instance per origin".**
Rejected because it is exactly the deployment a path invites, and the failure is one person's client signing in as another's instance's token.
