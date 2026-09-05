# ADR 0102 — Tauri packaging and store distribution

## Status

Supersedes ADR 0031's per-platform-native client strategy. Amends ADR 0046's
M-W12 scope and its "out of scope" list, and reverses the premise ADR 0101
reasoned from ("no store channel is planned for any target").

Decided: the shell technology for all five targets, the transport, runtime
server configuration, the callback scheme and the bundle identifier it derives
from, the milestone split, and the macOS architecture policy. Not decided here:
push notifications (deliberately deferred, see § "What this does not decide"),
and the HEIC question ADR 0101 left open — this ADR only constrains where a
decoder may not live.

**Dependencies, both now landed.** This ADR was opened standing on two
unmerged PRs, and said so. ADR 0101 arrived on `main` in #328, so § 6's
reversal of its premise now has something recorded to reverse; the OAuth
hand-off page arrived in #331, so the Context section below describes shipped
behaviour rather than a proposal. Nothing here is prospective any more.

## In brief

Wrap the existing `clients/web` `dist` in a single Tauri v2 shell and ship it
to **macOS, Windows, Linux, iOS and Android**, replacing ADR 0031's plan of
three separate native codebases (`clients/apple/` SwiftUI, `clients/android/`
Kotlin). The shell reaches the Axon server through Tauri's Rust-side HTTP and
WebSocket plugins rather than the webview's own `fetch`/`WebSocket`, which
retires the never-built CORS milestone (M-W1.5) as a dependency and lets a
packaged app talk to a plain-`http` LAN axon. The server URL becomes a runtime
setting rather than a build-time constant, because a binary distributed through
a store cannot have one baked in. Desktop ships first as direct-download
installers; mobile follows through the App Store and Play.

## Context

ADR 0031 chose "native per platform": SwiftUI for iOS and macOS under
`clients/apple/`, Kotlin + Compose for Android under `clients/android/`, and
Tauri only for Windows and Linux desktop, delivered "alongside the web client
at near-zero marginal cost". ADR 0046 turned that last item into milestone
**M-W12** — "`clients/web/src-tauri/`, Windows + Linux builds, Tauri
`AuthProvider` (OS keychain), CI build lane" — and listed "macOS/mobile Tauri
targets" as explicitly out of scope.

Two things have changed since.

**The native codebases were never started, and the web client kept going.**
There is no Swift or Kotlin file anywhere in this repository — ADR 0053 already
had to correct ADR 0031's claim that "generated SDK stubs for Swift already
ship as part of the MVP build". Meanwhile `clients/web` grew past the roadmap
that described it: media send, inline preview, syntax highlighting, threads,
search, room actions, an offline-first cache, keyboard shortcuts, an on-device
performance readout, and a mobile-web posture that ADR 0046 had called "a
stopgap, not a target". Three native rewrites of that surface is a much larger
commitment now than it was when ADR 0031 was written, and it would not produce
a better client for a good while.

**The web client was built to be wrapped.** ADR 0046 made "Tauri compatibility
a standing constraint from day one", and it held: `clients/web/AGENTS.md:607`
records "no service workers, `document.cookie`, or `window.open` anywhere,
ever", and the ban has been honoured through every subsequent web ADR — ADR
0064 hardened it into a rule for media, ADR 0072 declined a service worker for
inline preview, ADR 0085 declined one for the offline cache, and ADR 0087
declined Workbox for auto-refresh, each citing M-W12 by name. ADR 0046's own
addendum went further and pre-built the seams: `apiBaseUrl()` in
`clients/web/src/services.ts` is the single server-base accessor,
`OAuthAuthOptions.redirectUriBase` and its injectable `navigate` parameterize
the OAuth redirect, and `AuthProvider.getToken()` in
`clients/web/src/auth/provider.ts` already permits a `Promise` return so an
OS-keychain provider can drop in.

So the cheap path is the one the codebase has been kept ready for, and the
expensive path is the one that was written down. This ADR takes the cheap one
and extends it to the two platform families ADR 0031 reserved for native.

### What actually blocks a packaged build today

An audit of `clients/web` against a custom-scheme origin found six gaps, four
of which already have a seam:

1. **Transport.** `createApiClient` (`src/api/client.ts`), the media service's
   two raw `fetch` calls (`src/media/media-service.ts`), the OAuth token
   exchange (`src/auth/oauth.tsx`), and `openLiveSocket` (`src/api/ws.ts`) all
   use the webview's own globals. A packaged app is cross-origin against the
   user's server, and **no CORS exists**: ADR 0046 designed `cors_allowed_origins`
   for M-W1.5, ADR 0052 § 5 chose a same-origin front door *instead* of
   building it, and there is no `CorsLayer` anywhere in `crates/axon-api`.
2. **The server URL is a build-time constant.** `apiBaseUrl()` reads
   `import.meta.env.VITE_AXON_SERVER_URL`, and there is no server field in
   `src/stores/settings.ts`. A store binary cannot ship with one operator's
   hostname compiled in.
3. **`wsUrl()` derives the socket protocol from the page.**
   `src/api/ws.ts` maps `https:`/`wss:` to `wss:` and coerces *everything else*
   to `ws:`. Against a shell origin that fails two ways and reports neither:
   `tauri://localhost` (macOS, iOS, Linux) comes back unchanged, because the
   URL spec ignores a `.protocol` assignment on a non-special scheme, and the
   socket constructor then throws; `http://tauri.localhost` (Windows, Android)
   yields a perfectly valid `ws://tauri.localhost/v1/ws` aimed at the app's own
   origin. Both are swallowed by `socketFactory`'s catch in
   `stores/live-connection.ts` and become a permanent "reconnecting" with
   nothing to diagnose. A latent bug in its own right, not only a packaging
   one.
4. **Deep-link cold load.** History routing is the signed-off deep-link
   contract (ADR 0046 open question 5), and `deploy/web/Caddyfile` implements
   the required `try_files {path} /index.html`. Tauri's asset protocol has no
   such fallback. `src/app.tsx:134` has flagged this as "M-W12's problem" since
   M-W3.
5. **File save.** `src/media/download-media.ts:96` sets `anchor.download` —
   the only sanctioned path, precisely because `window.open` is banned. A
   custom-scheme origin has no download manager to honour it.
6. **OAuth callback.** `src/app.tsx:197` detects the callback by comparing
   `window.location.pathname` to `/oauth/callback`, which a custom-scheme
   redirect never sets, and `src/auth/oauth.tsx` navigates the app webview
   itself to the authorization endpoint.

Registering the client needs no server code. `OAuthClients::redirect_uri_allowed`
(`crates/axon-api/src/oauth/mod.rs:127`) already allow-lists pre-registered
public clients by exact redirect URI, and ADR 0054's own configuration example
registers `axon-ios` with `redirect_uris = ["axon://oauth/callback"]`. A
desktop or mobile client is an `axon.toml` entry.

*Delivering the authorization code* turned out to need server code, and this
paragraph originally claimed otherwise. `callback` ended Path A with a bare
redirect to the client's `redirect_uri`, which is correct for a browser client
and wrong for a private scheme: the OS takes the URL and the tab is left with
no document to render, so it spins forever on a sign-in that has already
succeeded. Reported on Windows against Edge, reproducible every time. The
server therefore answers a private-scheme `redirect_uri` with a small hand-off
page that performs the same delivery from script and says the tab can be
closed (#331). http and https are untouched. The lesson generalises past this
one bug: "the client is native" is not purely a client-side fact, and the parts
of the API that hand something *back* to a client are where it surfaces.

## Decision

### 1. One Tauri v2 shell for all five targets

`clients/web/src-tauri/` wraps the same `dist` for macOS, Windows, Linux, iOS
and Android. ADR 0031's `clients/apple/` and `clients/android/` are
**withdrawn, not deferred** — no Swift or Kotlin client is planned. ADR 0031's
sequencing ("Web → iOS → Android → macOS") is replaced by § 6 below.

The webviews are the OS's own: WKWebView on macOS and iOS, WebView2 on
Windows, WebKitGTK on Linux, and Android's System WebView. Two consequences
the repo has assumed but never tested, and which the first builds must confirm
(ADR 0101 flags them as unverified): WebView2 and Android WebView are Chromium
and decode no HEIC; WebKitGTK is WebKit but lacks Apple's Image I/O and so
decodes none either; WKWebView decodes HEIC natively. That means the macOS and
iOS builds fix ADR 0101's placeholder for free, while Windows, Linux and
Android keep it.

### 2. Rust-side transport, not server CORS

All `/v1` traffic — HTTP and the WebSocket — goes through Tauri's `http` and
`websocket` plugins, i.e. through `reqwest` in the shell process, not through
the webview. Four reasons, in order of weight:

- **Self-hosters change nothing.** The alternative requires every operator to
  add the app's origins to a `cors_allowed_origins` list that does not exist
  yet, and to keep it correct across platforms whose custom-scheme origins
  differ.
- **A plain-`http` LAN axon keeps working.** `deploy/web/Caddyfile` says
  outright that plain HTTP on `:8080` is "fine on a trusted LAN", and ADR 0052
  makes Tailscale the recommended remote path for a home box. A webview at a
  secure custom-scheme origin would refuse those requests as mixed content no
  matter what the server's CORS policy said.
- **iOS App Transport Security does not apply.** `reqwest` is not
  `NSURLSession`, so reaching a LAN server does not need an ATS exception
  argued at review.
- **It is the seam the OS keychain will need.** Credential storage can only
  move into the shell once transport is there — the "first payoff of the M-W2
  seam" ADR 0046 promised. Note the tense: M-W12 moves transport and *not*
  credentials. See the authority note below.

#### What this grants the webview, and what it does not

Routing transport through Rust widens the app's authority, and the widening
should be recorded rather than discovered. Tauri's `http` plugin takes a static
URL scope, and the server is chosen at runtime by the user (§ 3), so there is
no host to narrow it to: the scope is effectively `http(s)://*:*`. The
`websocket` plugin's `allow-connect` takes no URL scope at all. A browser
build, by contrast, is held to `connect-src 'self'` by the CSP.

So a webview compromise — a DOMPurify bypass in `FormattedBody` or
`MediaPreview`, a dependency with a malicious update — reaches arbitrary hosts
on the Internet and the user's LAN, which the same compromise in a browser tab
would not. And because the guest calls the plugins directly, the bearer token
is in JavaScript's reach: the shell carries the credential on the wire, but it
does not *hold* it.

**M-W12 accepts this**, with the mitigations that exist: the webview loads one
document from its own scheme under a CSP admitting no other origin, it runs no
third-party script, and both `dangerouslySetInnerHTML` sites are sanitised.
`minimumReleaseAge` and the `cargo-about` gate are what stand between a
supply-chain update and this authority.

**The target design is narrower**, and is recorded here so it is not
re-litigated: custom Rust commands that accept a path rather than a URL, resolve
it against the confirmed server, and inject a credential the shell holds. That
removes the wildcard scope and the token from JavaScript in one move, which is
why it is the same piece of work as the OS keychain and is sequenced with it.
It is deliberately not M-W12: it means reimplementing enough of `fetch` in Rust
to carry the media pipeline's uploads and blob reads, and doing it badly would
be worse than the wildcard.

**M-W1.5 is therefore not a dependency of this work.** Server-side CORS
remains unbuilt and remains owed to any *separately-hosted browser*
deployment; it is simply no longer on the critical path to a packaged client.

The client keeps one `dist`. A `Platform` seam (`clients/web/src/platform/`)
defaults to the browser globals and is swapped for the Tauri implementation by
runtime feature detection, so the same bundle still runs unmodified in a
browser — ADR 0046's stated M-W12 exit criterion, preserved.

### 3. The server URL becomes a runtime setting

`apiBaseUrl()` resolves in order: a persisted user setting, then
`VITE_AXON_SERVER_URL` as a build-time default, then `/` for the same-origin
browser deployment. A packaged build with nothing configured shows a "connect
to your Axon server" screen before sign-in, validating the entry against
`GET /healthz` — which lives outside `/v1`, is unauthenticated, and is already
proxied by `deploy/web/Caddyfile` for exactly this kind of probe.

Cache correctness needs no new work: ADR 0085 already keys every record by
`(apiBaseUrl, reader, …)`, and `cacheNamespace()` in
`src/stores/cache-store.ts` drops non-matching keys on write, so repointing the
app at a different server evicts immediately rather than ageing out.

Note what this does *not* reach: the zero-paste first-credential bootstrap
(ADR 0052 § "Sign-in path", implemented at
`crates/axon-api/src/routes/bootstrap.rs`) writes the minted token into the
SPA's `localStorage` from a same-origin page. That is structurally impossible
from a packaged build, which uses OAuth or token paste instead.

### 4. The callback scheme is the bundle identifier, not `axon:`

RFC 8252 § 7.1 asks a native client's private-use scheme to be a reverse
domain name under the publisher's control, and § 8.4 and § 8.6 give the reason:
a scheme is claimed first-come on every desktop OS and is not authenticated, so
a short generic one is both easy to collide with and easy to impersonate. Any
application registering `axon` could receive an authorization code intended for
this one. ADR 0054's configuration *example* uses `axon://oauth/callback`; an
example is not a decision, and this is the decision.

The callback is therefore `org.matrixaxon.axon:/oauth/callback`. Single slash:
there is no authority component, and `://` would make the first path segment
look like a host.

Two consequences worth stating rather than discovering.

**It couples the callback to the bundle identifier**, which this ADR therefore
also settles: `org.matrixaxon.axon`, previously carried as provisional in
`tauri.conf.json`. It could not stay provisional once the callback derives from
it — changing it after any build ships would mean re-registering the redirect
URI on every operator's server, and on the stores it is permanent from the
first submission.

**It is not the scheme the bundle is served from.** The shell serves its own
assets from `axon://localhost/`, which is an in-webview protocol handler that
is never registered with the OS and is not an OAuth participant. Sharing the
short name was confusing enough to be worth this paragraph.

Claimed HTTPS links (universal links / App Links) are the stronger option and
are deliberately not taken here: they need a verified HTTPS domain serving an
association file, which a self-hoster does not have, and the redirect must
reach *this* installation rather than whatever axon the domain names.

### 5. Deep-link routing keeps history mode

Resolve ADR 0046 open question 5 in favour of history routing on every target,
implemented by a Rust URI-scheme handler in the shell that serves `index.html`
for any non-asset path — mirroring `deploy/web/Caddyfile`, including its
deliberate 404 for a missing `/assets/*` (ADR 0087's "honest 404s": a hashed
chunk a redeploy deleted must not come back as HTML).

Hash routing is rejected: the URL shape `/:accountId/rooms/:roomId` plus
`?thread=`/`?event=` is a signed-off contract that search results, `matrix:`
links and the room-entry flow all address, and forking it per platform would
make the deep-link shape a platform detail. This retires the "M-W12's problem"
comment at `src/app.tsx:134`.

### 6. Store distribution is now a real channel, with one licensing consequence

ADR 0101 reasoned from "ADR 0031 targets direct-download installers for
Windows and Linux and no store channel is planned for any target". That is no
longer true, which sharpens exactly the risk that ADR 0101 anticipated:

**No LGPL component may enter the shared bundle.** This is a conservative
project policy, adopted because nobody here is in a position to run the
analysis that would justify anything narrower — not a statement of settled law,
and the ADR should not be read as one.

The tension is real: LGPL § 4 permits conveying a combined work under terms
that allow the recipient to relink, and store distribution makes that awkward.
But § 4 offers several compliance routes, and the case usually cited in this
argument does not support the strong reading. VLC's App Store removal was a
*GPL* dispute; VideoLAN's own statement describes relicensing the iOS client to
MPLv2 precisely so store distribution would work. Anyone wanting to revisit
this should start there and take advice, rather than from this paragraph.
libheif and libde265 are LGPL-3.0, and a wasm decoder minified into an
application chunk is the worst posture of the three ADR 0101 weighed. Since
WKWebView decodes HEIC natively, a bundled decoder would buy nothing on
precisely the two platforms where it would cost the most. ADR 0101's open
question stays open, but the browser-bundle option is now foreclosed for any
build that reaches a store, and any future decoder must be a
per-platform-excluded resource or a server-side transcode in `axon-media`.

This does not implicate the webviews themselves. WebKitGTK is a system shared
library the distribution provides — dynamic linkage against the OS's own copy,
which ADR 0101 calls "the arrangement the LGPL was written for" — so it never
enters the crate graph that `build/about.toml` gates. WebView2 is proprietary
but redistributable. Both need attribution, neither needs an `accepted` entry.

### 7. Milestone split and sequencing

- **M-W12 — desktop shell.** As ADR 0046 scoped it, widened to include macOS:
  `clients/web/src-tauri/`, the platform seam, runtime server config, OAuth
  under a deep-link scheme, OS-keychain credentials, and a CI build lane.
  Exit: signed installers for macOS, Windows and Linux built from the same
  `dist`, reaching a remote axon.
- **M-W13 — store submission, mobile and desktop.** iOS and Android builds,
  the platform work a native webview needs (camera permissions for the ADR
  0097 QR flow, safe-area insets, app icons, local-network declarations), and
  the review process for the App Store, Play **and the Mac App Store**. The
  macOS store submission sits here rather than in M-W12 because it is review
  work rather than build work, and it shares M-W13's constraints — sandboxing,
  Apple 4.2 and 4.8, and the demo server a reviewer needs. Exit: builds
  installed from TestFlight and the Play internal track, and a Mac App Store
  submission accepted for review.

Desktop first: no gatekeeper, fastest feedback, and it proves the shell before
the review runway starts. M-W13 depends on M-W12; neither depends on M-W11,
whose parity and a11y audit is a gate for *public release*, not for test
builds.

### 8. Auto-update

ADR 0087's `version.json` polling is meaningless once `dist` is compiled into
the binary — the client would compare a bundled file against itself forever.
Gate the poller off under the shell — done, along with the banner and the
auto-reload it drove, since all three rest on the origin being able to serve a
different build. Note the interval state that leaves: a packaged build has *no*
update path at all until the Tauri updater lands, and the updater needs the
signing key that M-W12's own release infrastructure produces, so this is
sequenced behind that rather than beside it. Desktop direct-download builds get
the Tauri updater; store builds get the store's own update channel. Service
workers remain banned, which as a side effect forecloses issue #23's media
service worker for packaged builds.

### 9. macOS ships one universal bundle, unlike the server and TUI

`tauri build` targets the host architecture, so a build on an Apple Silicon
machine produces an arm64 bundle that will not open on an Intel Mac at all.
The reverse is softer — an x86_64 bundle runs under Rosetta 2 — but neither is
an answer for a download page. macOS builds therefore use
`--target universal-apple-darwin`, which lipos both slices into one bundle;
`rustup target add aarch64-apple-darwin x86_64-apple-darwin` is the only
prerequisite, and cross-compiling the Intel slice from Apple Silicon is
already how `cross-build.yml` produces its macOS binaries.

This **diverges from an existing, deliberate policy**, which is why it is
recorded rather than assumed. `.github/workflows/cross-build.yml` states that
the project "intentionally ship[s] per-arch macOS binaries rather than a lipo'd
universal binary", because it matches the one-arch-per-file Windows and Linux
zips and "avoids a fat binary silently masking a broken osxcross target for one
architecture".

That reasoning holds for what it was written about and does not travel to a
desktop app. Those binaries are fetched by an operator, often by a script that
knows its own architecture; a `.dmg` is chosen by a person who frequently does
not know which Mac they own, and choosing wrong yields an application that
simply refuses to launch. The Mac App Store (M-W13) expects native Apple
Silicon support besides. The masking concern is real and is answered by
building each slice as its own CI step and lipo'ing only the artifact, so a
broken architecture still fails a job rather than disappearing into a fat
binary.

Windows and Linux stay one-arch-per-file: neither has an equivalent of a
universal binary, and arm64 builds of either are not in M-W12's scope.

### 10. Platform behaviour the shell has to absorb

Found while getting M-W12 running on real hardware rather than reasoned about
up front, and recorded here because each one is a property of the *engines*, not
of this codebase — anything built on this shell inherits all three, and M-W13
inherits them again on two more webviews.

**A `<canvas>` can silently render nothing.** WebKitGTK's DMA-BUF renderer — how
the web process hands painted buffers to the UI process — can deliver an empty
buffer with no error anywhere. pdf.js reported a successful render of a page
that was blank. Confirmed on a GPU-backed desktop as well as a VNC session, so
it is not an artifact of software rendering, and the shell sets
`WEBKIT_DISABLE_DMABUF_RENDERER=1` on Linux. The consequence to carry forward is
that *any* canvas feature — PDF pages, QR rendering, future charts — is exposed
to this, and the failure mode is a blank rectangle rather than an exception.

**Drag-and-drop cannot be handled the same way on all three.** Windows requires
Tauri's drag-drop handler *disabled* for HTML5 drag-and-drop to work at all;
Linux requires it *enabled*, because WebKitGTK advertises `text/uri-list` and
puts no `File` behind it, so the page receives a path it cannot open. The shell
therefore runs two paths, and the client hit-tests the drop position against
the pane under the cursor, because the native event carries a point and no
target. Any future drop surface has to work in both.

**Camera permission defaults disagree three ways.** WebView2 prompts the user;
WKWebView asks a delegate that wry answers `Grant`; WebKitGTK denies unless the
application answers the `permission-request` signal itself, which wry's GTK
backend does not. macOS additionally hides the API entirely without
`NSCameraUsageDescription`, which presents as "unavailable in this browser"
rather than as a denial. M-W13 meets the same class of problem on iOS and
Android and should budget for it rather than discover it.

## What this does not decide

**Push notifications stay out of scope**, as ADR 0031 § push and ADR 0053 have
it: no device-token endpoint, no push router, no APNs or FCM integration. The
consequence is concrete and should be stated in the store listings rather than
discovered at review — a backgrounded mobile client receives nothing, because
the WebSocket does not survive backgrounding. This is the largest known
functional gap in the mobile builds and it needs its own ADR and a server
silo; it is not a prerequisite for shipping M-W13.

## Consequences

- ADR 0031's client strategy is superseded; `clients/apple/` and
  `clients/android/` will not exist. ADR 0031 gains an amendment pointer here,
  as it did for ADR 0046.
- ADR 0046's roadmap table gains M-W13, and "macOS/mobile Tauri targets" leaves
  its out-of-scope list.
- `README.md` already advertises a Mac Tauri client and a "(soon to be
  packaged) Tauri desktop client"; this ADR makes those claims true rather
  than requiring their removal.
- `docs/client-parity.md` gains no column and *loses* one. Its `iOS (future)`
  column is removed in this ADR's own change: the shell runs the same `dist`,
  so that column could only ever repeat `axon-web`'s, and leaving it filled
  with "Not started" described work nobody was going to do. Its `window.open`
  note and its HEIC row still want updating once the shell exists.
- `clients/web/src-tauri` is **not** a member of the root Cargo workspace. It
  carries its own `[workspace]` table and the root gains a matching `exclude`.
  Otherwise `cargo build --workspace` in `cross-build.yml` would require
  webkit2gtk on every existing Linux lane, run on Windows and both macOS
  arches, and `cargo clippy --all-targets` in the pre-push gate would too.
  The `.pre-commit-config.yaml` filters need the mirror-image fix: `src-tauri`
  Rust files currently fall outside the Rust filter and inside the web one.
- Signing infrastructure is entirely new. The repo has none: an Apple Developer
  ID plus notarytool credentials, a Windows Authenticode certificate, and a
  Tauri updater keypair all become required secrets, with no
  `environment:`-gated pattern in the repo to copy.
- Third-party disclosure forks. `scripts/generate-thirdparty.sh` runs
  `cargo-about` over the root workspace only, so the shell's separate workspace
  needs its own run folded into the release artifact.
- The first builds settle three untested claims ADR 0101 flagged (§ 1), and
  resolve ADR 0046 open question 5 (§ 4).

## Alternatives rejected

**Three native codebases, as ADR 0031 decided.** Best-in-class UX per platform
and full access to platform push and credential APIs — and the reason the
decision is being revisited rather than dismissed. Rejected on cost: it is
three reimplementations of a client surface that has roughly tripled since ADR
0031 was written, against a `/v1/` contract that only one of the three would
exercise first. ADR 0031's own justification for Tauri on desktop — "near-zero
marginal cost" from wrapping a dist that already exists — applies with equal
force to the other three platforms; ADR 0031 declined it there for UX reasons
that the web client's mobile work (ADR 0062 two-pane layout, ADR 0075 swipe-back,
ADR 0080 app badge, ADR 0077 on-device perf) has since substantially answered.
Nothing here forecloses a native client later; it declines to start with one.

**Server CORS (build M-W1.5) and use the webview's own transport.** One code
path, no transport seam, and the CORS layer is owed to browser deployments
anyway. Rejected as the *packaging* mechanism because it pushes configuration
onto every self-hoster, cannot reach a plain-`http` LAN server from a secure
custom-scheme origin at all, and would need an ATS exception on iOS. M-W1.5
remains a good idea on its own terms and is not built here.

**Hash routing under the shell**, ADR 0046's own "safe answer". Rejected in
§ 4: it would fork the deep-link contract per platform.

**A service worker to authenticate media requests** (issue #23), which would
let native `<img>` URLs and `Range` streaming replace the blob pipeline.
Rejected by inheritance — it has been rejected four times already on the
strength of this shell existing eventually, and the shell now exists.
