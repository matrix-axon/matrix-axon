# ADR 0087 — Web client build identity and automatic refresh

## In brief

Emit `version.json` alongside the built bundle, poll it, and reload the page
when the origin serves a build other than the one running. Reload silently when
that costs the user nothing (a backgrounded tab returning after a minute away),
show a banner when it would interrupt, and recover from a chunk that a redeploy
deleted via `vite:preloadError`. Give builds a `release+hash` identity —
`0.1.0+ab12cd34ef56` — so a bug report can name what it was running.

Also: make every server that serves `dist/` — vite's preview server and the
Docker stack's Caddy — answer a missing `/assets/*` with `404` rather than
`200 text/html`, which is the specific defect that turns a redeploy into a hung
client; and stop a deploy from signing every OAuth client out, by no longer
treating an unreachable server as a revoked session.

Mostly `clients/`, plus `deploy/web/Caddyfile` by developer override of the
one-silo rule — the same defect in both places, reviewed once. No service
worker — see [Alternatives](#alternatives).

## Context

Pushing a new build to the test server hangs clients already running the
previous one. Test users recover by force-killing the PWA on mobile or reloading
on desktop. The deployment in question is `vite preview` serving `dist/` behind
a Caddy reverse proxy that does nothing but forward; a deploy is `pnpm build`
plus a restart of that process.

### The hang

Vite's preview server (measured on `vite@8.1.3`) stacks its middleware as proxy
→ static → HTML fallback → index. The HTML fallback rewrites any unmatched
`GET` whose `Accept` header includes the wildcard, and the wildcard is exactly
what a `<script type="module">` and a dynamic `import()` send. So a hashed chunk
that a redeploy has deleted comes back as `200 text/html`:

```console
$ curl -sI http://localhost:5175/assets/does-not-exist-abc123.js
HTTP/1.1 200 OK
Content-Type: text/html
Cache-Control: no-cache
```

The browser tries to parse HTML as a module, fails, and the app wedges. This is
reachable because the client is code-split: `src/code/languages.ts` lazy-imports
around forty highlight.js languages, and the PDF viewer and emoji picker are
separate chunks. Any session that outlives a deploy will eventually ask for one.

### What is *not* the cause

Worth recording, because both were the first guesses:

- **Caching.** Vite's static middleware runs sirv with `dev: true`, which sets
  `Cache-Control: no-cache` on every response. `index.html` is always
  revalidated, which is precisely why a manual reload reliably fixes the hang.
  (It also means no asset is ever cached across a load — a real cost of using
  `vite preview` as a test host, and not one this ADR addresses.)
- **The reverse proxy.** The deployed Caddyfile only does `reverse_proxy`. The
  `deploy/web/Caddyfile` in this repo *does* have the same `try_files` trap, and
  fixing it is worthwhile, but it is not what these users are hitting.

### The missing capability

The bundle has known its own build id since M-W1 — `__AXON_WEB_VERSION__` in
`vite.config.ts`, shown in the Settings footer. But nothing serves that id
out-of-band, so **a running tab has no way to learn what the origin now
serves**. Detection has to come from somewhere before any refresh policy can
exist.

Two existing facts shape the answer. The web bundle and the `/v1` API are served
through one front door, so a deploy restarts the process the WebSocket runs
through and every client's socket drops — a reconnect is near-instant evidence
that a deploy may have happened. And there is no version numbering anywhere:
`[workspace.package] version` has sat at `0.1.0` since the beginning and
`clients/web/package.json` at `0.0.0`, so the git short hash is the only real
build identity in the project.

## Decision

### 1. `version.json`

A build emits `dist/version.json`:

```json
{ "release": "0.1.0", "version": "ab12cd34ef56", "builtAt": "2026-08-02T14:03:11.204Z" }
```

`version` is the git short hash and the only field compared. `release` comes
from `clients/web/package.json` and is the human-readable number; `builtAt` is
display-only. The values are stamped once at module scope in `vite.config.ts` so
the `define` block and the emitted file cannot disagree.

**The manifest is emitted by `generateBundle` — only by a real build.** In
preview it is served out of `dist/` like any other asset. Synthesizing it at
preview time would stamp a fresh `builtAt` that disagrees with the value baked
into the already-built bundle, and the client would reload forever. Only the dev
server, which has no `dist/`, may synthesize it.

### 2. Version identity

`release+version`, formatted as semver build metadata: `0.1.0+ab12cd34ef56`.
`clients/web/package.json` moves to `0.1.0`, matching the workspace crates.

The release alone cannot identify a build — every test build of a release shares
it — so the hash always rides along, and it remains what the update check
compares. This is deliberately the smallest step that produces a quotable
version: no changelog, no release ritual, no cross-silo alignment. Those are a
separate decision, and this one does not block them.

### 3. Detection

`src/stores/update-check.ts` fetches the manifest with `cache: 'no-store'` and
latches an `available` signal when the origin's `version` differs. Triggers, in
order of how much work they do:

| Trigger | Why |
|---|---|
| Live socket reconnect | A deploy drops every socket. Seconds, no polling. |
| `visibilitychange` → visible | The mobile case: timers are frozen while backgrounded, so this is the workhorse. Throttled to 30 s. |
| 15-minute interval, visible only | Backstop for a focused desktop tab whose socket stays up. |
| `online` | Cheap; covers a laptop lid opening. |

**Production builds only.** The whole mechanism is gated off under `vite dev`,
because a dev stamp is not a deployment identity: `webClientVersion()` reads the
git hash and the `-dirty` flag once at dev-server start, so it moves whenever
the working tree does. Restarting the dev server after a commit or an edit would
otherwise make every open tab decide a new build had shipped and reload itself —
indistinguishable from a bug, and racing the HMR update arriving at the same
moment. HMR already owns reloading in dev. This was found by a developer asking
why their `pnpm dev` tab kept refreshing.

Every failure reads as "learned nothing", never as "the build changed". In
particular the fetch **requires a JSON content type**: an SPA server answering a
missing `/version.json` with `index.html` and a `200` is the normal case before
this ADR ships, and must not be mistaken for a new build.

The read is bounded, and the bound covers the **body** as well as the
connection. A hang here is not one lost check: `check()` de-duplicates every
trigger behind a single promise, so work that never settles pins that slot and
turns every later check into the same stuck promise — update detection off for
the life of the tab. Review of this PR caught it. Two layers now: the shipped
`fetchVersionManifest` aborts itself, and `check()` additionally releases its
slot on a watchdog, so the invariant does not depend on the manners of an
injected `fetchManifest`.

Freeing the slot does not cancel the run behind it — nothing can — so a second
review pass noted the abandoned run could still resolve much later and write its
answer over a newer one's. Each run therefore carries a generation and drops its
result if a later check has begun.

`available` latches. A poll that straddles a deploy restart can easily read the
old manifest again, and retracting a banner the user has already seen would
flicker it for no reason.

### 4. Policy

Reloading is free exactly when the user is not looking and has nothing unsent:

- **Hidden, or returning after ≥ 60 s away** → flush drafts, reload. This is the
  case the feature exists for. A shorter hide is a tab switch or a notification
  shade, and reloading through one is indistinguishable from a crash.
- **Unsent work** — any timeline store holding a `localEcho`, which includes an
  in-flight media upload since its echo carries the `File` — → never reload;
  show the banner. That echo lives only in memory.
- **Otherwise** → the banner, which is the entire interactive path.

Drafts are durable already (ADR 0048) but only once the `PUT` behind their 800 ms
debounce has gone out, so the automatic path calls a new
`DeviceStateStore.flushPending()` first. That flush is capped at two seconds:
the page we are fixing is already broken, and a server that never answers must
not be what prevents the fix.

"Flushed" has to mean *every* write for the scope has landed, not just the ones
this call started. Device state merges last-write-wins by arrival
(`ON CONFLICT … DO UPDATE SET value = EXCLUDED.value`, no ordering guard), so
two PUTs in flight at once are a lost update whenever the network reorders them.
Review of this PR caught that the first cut awaited only its own batches: a
stale PUT could still be in flight, and land *after* the reload had destroyed
the tab holding the newer text. Writes are therefore serialized per scope —
never two in flight for one scope — and `flushPending` awaits the chain tails.
The reordering window predates this ADR; the reload is what made it
unrecoverable.

### 5. Loop guard

A reload attempt is recorded in `sessionStorage` as the pair *(build we left,
thing we were reloading toward)*, and the same pair is never attempted twice. On
the next boot that record is either stale — we came back on a different build,
so it worked — or still current, in which case that attempt is spent.

This is not optional. A reload loop is a worse outage than the hang: it burns
battery, cannot be escaped by reloading, and on an installed PWA there is no
address bar to escape to.

Keying on the **pair** rather than on the departed build alone is a correction
made during implementation. Keying on the departed build alone does stop the
loop, but it also means one bad manifest disables automatic refresh for the rest
of that tab's session — the guard is never cleared (we keep coming back on the
build it names), so a later, genuinely new build is refused too. Since tabs and
installed PWAs stay open for days, that quietly defeats the feature. With the
pair, a new target is a new attempt.

That leaves the pathological case the pair rule cannot see: an origin handing
out a *different* build every time, so no pair ever repeats. Review of this PR
caught that the first cut of the budget did not bound it either — settling a
pair removed the whole record, refunding the budget on every boot that reached
a new bundle, so a manifest chain A→C→E→A reloaded without end.

So the two limits are now independent. The pair record settles on a successful
reload; `spent` does not, and counts reloads across every build the tab has been
through. Landing on a new bundle proves the last reload did *something*, not
that the deployment is consistent.

`spent` decays over a ten-minute window rather than accumulating for the tab's
lifetime. An installed PWA's session can outlast many legitimate deploys, and a
hard lifetime cap would silently stop updating a long-lived tab — the same
failure mode as the departed-build-only key, arrived at from the other side.
Bounding the *rate* is what separates thrash from ordinary use. Hitting the cap
is not silent: the banner takes over, so the update is still applicable by hand.

The chunk-failure path has no version to name and uses a `'chunk'` sentinel as
its target. That keeps it separate for **pair dedup** — a spent chunk reload
never blocks a later version reload, or the reverse — but the two deliberately
share the one `spent` budget. The budget's job is to bound how often a tab
reloads itself *at all*; giving each target kind its own would let a single
window spend `MAX_ATTEMPTS` on chunk failures **and** `MAX_ATTEMPTS` on version
updates, which is exactly the rate it exists to cap.

Automatic reloads fail closed — an unwritable or unreadable `sessionStorage`
blocks them — while a user clicking Reload is never guarded.

Every decision, to reload or to decline, is logged under `[axon:update]`. A page
that reloads itself leaves no evidence that it meant to: the reload wipes the
console, so by the time anyone looks there is nothing to distinguish a
deliberate refresh from a crash. The first question asked of this feature was
"why is my tab refreshing?", and it could not be answered from the console at
all.

### 6. Chunk-failure recovery

`window.addEventListener('vite:preloadError', …)` in `src/main.tsx`, before
`render`, reloads once through the same guard. Vite raises this for exactly the
new-deployment case, and the failure is not retryable — the document naming the
missing chunk is the stale thing.

### 7. Stop deploys from signing everyone out

Found while investigating the above, and part of the same complaint — testers
also had to re-authenticate after most pushes.

`refreshAccessToken` wrapped the token request in `try { … } catch { persist(null) }`,
and the request throws on a rejected `fetch` as readily as on `invalid_grant`.
So a refresh that never got an answer was treated as a dead session and a
30-day refresh token was discarded over a connection refused.

That is deploy-shaped, not random. Access tokens live one hour and refresh
lazily — only when `getToken` is called within a minute of expiry, with no
background timer — so any tab open longer than an hour carries a stale token.
Restarting the server drops the live socket; the reconnect calls `getToken` on
every attempt starting at a one-second backoff; and `/v1/oauth/token` is
proxied through the process that is still restarting. Push, restart, reconnect,
refresh, connection refused, sign-in screen.

The fix is to make the distinction the code was missing:

- `OAuthRejectedError` — the server answered and named `invalid_grant`
  (RFC 6749 §5.2): this refresh token is bad, expired, or revoked. The only
  error that ends a session.
- `OAuthTransportError` — no verdict: the request failed, the server failed to
  serve it, or it refused for a reason that says nothing about the grant. Never
  discards credentials; the caller gets no token and the next attempt tries
  again.

Classification is by **error code, not status class**. Review of this PR caught
that reading any 4xx as a refusal reintroduced the very bug through a narrower
door: `/v1/oauth/token` sits behind the OAuth rate limiter (30/min per IP,
`crates/axon-api/src/oauth/rate_limit.rs`), and a deploy is exactly when that
trips — every tab's socket drops at once and every reconnect asks for a token.
A 429 would have discarded a valid 30-day refresh token. A 429, a 5xx, an
unreadable body, and a 4xx naming no OAuth code are all "no verdict" now.

The cost of that choice is a session that is dead for some reason the server
never names as `invalid_grant`: the client keeps credentials it cannot use and
looks signed in while every request fails. That is the better failure — it is
visible, and it is recoverable by signing out — where the reverse loses a
working session to a transient blip.

A `401` on an ordinary request now attempts a refresh rather than signing out.
It only ever meant "this access token was not accepted", and we hold a refresh
token that outlives it thirty-fold; signing out threw away sessions a refresh
would have restored. Rate-limited to one refresh per five seconds so a server
answering `401` to everything cannot mint a token per request.

Deliberately unchanged: token-paste still discards on `401`. Those tokens never
expire (`expires_at IS NULL`), the verifier maps a store failure to `500` rather
than `401` (`crates/axon-api/src/auth.rs`), and there is no refresh to attempt —
so a `401` there really does mean the token is gone.

### 8. Honest 404s from every server that serves `dist/`

Assets are content-hashed, so a miss under `/assets/` is never a route and must
never reach an SPA history fallback. This is the root-cause fix; everything
above is detection and recovery. Two servers need it:

- **vite preview** (the test deployment) — a `configurePreviewServer`
  middleware, registered in the hook body so it runs ahead of vite's HTML
  fallback.
- **`deploy/web/Caddyfile`** (the Docker stack) — its
  `try_files {path} /index.html` has the identical defect. A `handle /assets/*`
  block ahead of the SPA fallback serves those paths with `file_server` alone,
  which 404s a missing file.

Caddy gets the cache headers too, since unlike vite preview it is a real
production server: `immutable` for hashed assets, `no-cache` for everything the
fallback serves (`index.html`, `version.json`, the manifest, icons — all mutable
under a fixed name). The `immutable` header is conditional on the file
existing; applying it to the 404 would be a year-long instruction to keep
not-finding a chunk, which an intermediary asking one moment too early during a
rollout would honor.

The Caddy half crosses from `clients/` into `deploy/`. That is a deliberate
override of the one-silo rule, made by the developer on the grounds that the
same defect is cheaper to review once in both places than twice in two PRs.

## Consequences

- A client notices a deploy within seconds of the socket reconnecting, and a
  backgrounded PWA comes back current with no force-kill.
- A stale chunk 404s honestly instead of returning HTML, so the failure is
  legible in a console and `vite:preloadError` fires reliably.
- Builds have a quotable identity in Settings and in `version.json`.
- **`version.json` is a public, unauthenticated endpoint** disclosing the build
  id and time. The bundle it describes is already public and its hashed asset
  names already leak the same information, so this adds no exposure. It contains
  nothing account-scoped.
- The polling cost is one conditional request per reconnect plus one per 15
  minutes per visible tab.
- A tab is allowed at most three automatic reloads per session. A client that
  hits that has an origin misreporting its build, and keeps the banner.
- A deploy no longer signs OAuth clients out. A genuinely revoked or expired
  grant still does, on the next refresh — the change narrows *when* we sign out,
  not whether.
- A dead server now leaves the client holding credentials it cannot use, where
  before it cleared them. That is the point, but it does mean "signed in" no
  longer implies "reachable"; the connection indicator remains the honest
  reading of that.
- Automatic refresh also repairs the force-kill path incidentally: killing an
  installed PWA ends the browser session and drops anything in
  `sessionStorage`, which is where the token lands if "Remember me" was ever
  unchecked. A reload keeps the session alive; a force-kill does not.
- `UpdateBanner` renders inside the signed-in shell, so a signed-out tab gets
  the automatic path but no visible affordance. Judged acceptable: the sign-in
  screen holds no state worth preserving, and a manual browser reload costs
  nothing there.
- `vite preview` remains an odd production host — no compression, and
  `Cache-Control: no-cache` on every asset. Unchanged here, and worth revisiting
  separately.
- The Docker stack additionally gains real asset caching, which it never had —
  no `Cache-Control` was set anywhere in `deploy/web/Caddyfile` before.

## Alternatives

**A service worker with Workbox `skipWaiting`.** The standard answer, and
unavailable: `clients/web/AGENTS.md` bans service workers permanently because
M-W12 wraps this same `dist` in a Tauri shell. ADR 0085 independently rejected a
service-worker precache. Polling a small file is the alternative, and the
socket-reconnect trigger makes it nearly as prompt.

**Push the new version over the WebSocket.** The frame protocol is additive and
would carry it. But the server knows its *own* build, not the web bundle's —
they are separate images, and `deploy/publish.sh --web-only` can ship one
without the other. The origin serving the bundle is the only authority on what
bundle it serves.

**Compare against `/v1/status`.** Same objection, plus it is authenticated, so
it could not check on the sign-in screen.

**Always auto-reload.** Simplest, and rejected: it takes the page out from under
someone mid-sentence. The banner exists for that case.

**Never auto-reload.** The most conservative, and it fails the actual
requirement — a backgrounded mobile PWA would still need a deliberate tap, which
is close to the force-kill we are replacing.
