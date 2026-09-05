# ADR 0031 — Multi-platform client strategy

## Status

**Superseded in part by ADR 0102.** The "native per platform" decision
below no longer holds: a single Tauri shell around the `clients/web` dist
targets macOS, Windows, Linux, iOS and Android, and `clients/apple/` and
`clients/android/` are withdrawn. The framing that remains current is the
single `/v1/` contract, the bearer/OAuth auth stance, and the media-proxy
rule. Individual paragraphs are annotated inline below.

(See also ADR 0053, which corrects this ADR's push-notification and
Swift-stub claims, and ADR 0046, which resolved its web-framework question.)

## Context

`axon-tui` proved the `/v1/` API surface during the MVP. The post-MVP roadmap
(tech-spec §Roadmap signposts) calls for additional clients targeting web
browsers, iOS, Android, and desktop (macOS, Windows, Linux). No per-client
plan existed; this ADR records the approach before implementation begins.

Several constraints shape the decision:

- **Single API surface.** Clients consume `/v1/` HTTP REST + the WebSocket at
  `/v1/ws`, not the Matrix homeserver directly. The server is the deliverable;
  clients are consumers of a stable contract.
- **Bearer token auth today; OAuth 2.0 + PKCE later.** The wire protocol
  (`Authorization: Bearer`) won't change (ADR 0029), but the mint flow will.
  Clients must not hard-wire the token-paste bootstrap path. OAuth 2.0+SSO 
  with common providers (Google, Apple, Microsoft) is a near-term implementation 
  goal.
- **Browser WebSocket auth.** Browsers cannot set `Authorization` on a
  WebSocket upgrade, so the server accepts `Sec-WebSocket-Protocol:
  bearer.<token>` from browser clients (ADR 0029). Web and Tauri clients must
  use this path.
- **Push notifications (APNs/FCM/web push) are out of scope for the iOS
  MVP**, not a day-one client concern — see ADR 0053, which corrects this
  bullet's original framing. No device-token endpoint or push router is
  commissioned until a dedicated future ADR scopes it.
- **OpenAPI spec is the contract.** The spec is checked into the repo and is
  the source of truth for every `/v1/` operation. No Swift SDK stubs exist
  yet as of ADR 0053 — this bullet's original claim that they "already ship
  as part of the MVP build" was inaccurate; standing up Swift codegen is
  part of the iOS client's own (separate, later) roadmap ADR.
- **Media proxy contract.** The in-flight media proxy work (`matrix-api-media-proxy`
  branch) fixes the media URL shape. Clients must not assume direct homeserver
  media URLs; all media is served through the axon `/v1/` surface.

See ADR 0053 for the current inventory of server-side prerequisites for the
iOS client (OAuth 2.0 + PKCE, a device-listing endpoint, and the ADR 0030
`sync_state` implementation), which corrects the push and Swift-stub
assumptions above. A separate iOS client MVP roadmap ADR, covering the
client-side milestone sequence, is still to come.

## Decision

**Approach: native per platform.** Each client uses the idiomatic toolkit for
its target rather than a shared cross-platform framework. This gives
best-in-class UX, full access to platform push and credential APIs, and no
lowest-common-denominator abstractions. The trade-off is three separate
codebases rather than one; the OpenAPI-generated stubs are the mechanism that
keeps them consistent with the server contract without hand-rolling HTTP calls.

**iOS client: SwiftUI, targeting iOS 17+.** OpenAPI-generated Swift stubs
form the networking layer (codegen tooling does not exist yet; standing it
up is part of the iOS roadmap, not something already shipped). Push-token
registration is deferred, not a day-one concern — see ADR 0053. Directory:
`clients/apple/` (see macOS entry below).
*(Superseded by ADR 0102: no Swift client is planned; iOS ships as a Tauri
target in M-W13.)*

**macOS (desktop): SwiftUI multiplatform, sharing the iOS Swift Package.** The
iOS project is structured as a Swift Package with a shared `axon-core` library
(networking, models, business logic) and platform-specific UI targets. The
macOS target reuses `axon-core` with a native macOS SwiftUI UI — not
Mac Catalyst. Both live under `clients/apple/` as targets within the same
package.
*(Superseded by ADR 0102: macOS ships as a Tauri target in M-W12.)*

**Android client: Kotlin + Jetpack Compose, targeting Android 10 (API 29)+.**
FCM push-token registration is a day-one concern in the client architecture,
mirroring the iOS stance above. Directory: `clients/android/`.
*(Superseded by ADR 0102: no Kotlin client is planned; Android ships as a
Tauri target in M-W13.)*

**Windows / Linux desktop: Tauri, delivered alongside the web client.** A
Tauri shell (`src-tauri/` config directory) wraps the web SPA in a native
desktop app using the OS's own WebView — Edge WebView2 on Windows 10+,
WebKitGTK on Linux. This produces a ~5–10 MB installer with no bundled
Chromium. The server is already Rust, so Tauri uses the same toolchain
(`cargo tauri build`). Tauri support lives inside `clients/web/` alongside the
SPA; no separate directory is needed. Target: ship Windows and Linux desktop
builds as soon as the web SPA stabilizes, at near-zero marginal cost.
*(Widened by ADR 0102 to all five targets. The webview inventory here is
incomplete as a result — add WKWebView for macOS/iOS and Android's System
WebView — and none of the four has been verified against a real build.)*

**Web client: TypeScript SPA with Vite — framework to be decided.** The SPA
consumes `/v1/` over `fetch` and the native `WebSocket` API (using the
`Sec-WebSocket-Protocol: bearer.<token>` path). It is hosted separately from
the server; no SSR is required. The web client is the design reference: its
component library and screen flows inform the mobile clients. Directory:
`clients/web/`.

> **Amended by ADR 0102 § 2.** That transport is still exactly right for a
> browser. The packaged shell does not use it: it routes `/v1` through Rust,
> where an `Authorization` header is available and the subprotocol workaround
> is unnecessary. And per ADR 0102 the web client is not a reference the mobile
> clients imitate — it *is* them.

The JavaScript framework is an open question that must be resolved before the
web client milestone begins:

|                            | React                            | Vue 3                       | Svelte                    | Preact                              |
| -------------------------- | -------------------------------- | --------------------------- | ------------------------- | ----------------------------------- |
| **Ecosystem / components** | Largest (shadcn/ui, Radix, etc.) | Large                       | Smaller                   | React-compatible (via compat layer) |
| **TypeScript**             | Excellent                        | Excellent (Composition API) | Good, less mature         | Excellent (mirrors React)           |
| **OpenAPI code-gen**       | Most mature tooling              | Good                        | Less mature               | Same as React tooling               |
| **Tauri integration**      | Best-documented                  | Good                        | Less-documented           | Good (same as React)                |
| **Boilerplate**            | Moderate                         | Low–moderate                | Very low                  | Moderate                            |
| **Bundle size**            | Moderate                         | Moderate                    | Very small (compile-time) | Very small (~3 KB)                  |
| **Developer availability** | Highest                          | High                        | Lower                     | High (React devs transfer easily)   |

React is the lowest-risk default; Vue 3 is a legitimate alternative with a
cleaner API; Svelte is compelling for performance and simplicity but carries
ecosystem and tooling risk; Preact offers near-identical React API with a
fraction of the bundle size via its compatibility layer.
**Team should discuss and decide before the web client milestone is started.**
*(Resolved: ADR 0046 selects Preact and records the web client roadmap.)*

**Sequencing: Web (+Tauri desktop) → iOS → Android → macOS.** Web ships
first: no app-store approval, fastest feedback loop, validates design patterns
the other clients follow. Tauri Windows/Linux desktop ships alongside it at
near-zero marginal cost. iOS ships second, because APNs is the P0 push target
and the Swift stubs already exist. Android third, because FCM follows APNs.
macOS last, because it depends on the iOS Swift Package being stable.
*(Superseded by ADR 0102 § 7: desktop (M-W12) then mobile (M-W13). The
rationale given here for iOS-before-Android — that APNs is the P0 push target
and Swift stubs exist — was already wrong on both counts per ADR 0053.)*

## Consequences

- Three client codebases: `clients/web/` (SPA + Tauri shell), `clients/apple/`
  (shared Swift Package with iOS and macOS targets), `clients/android/`.
  *(Superseded by ADR 0102: one codebase, `clients/web/`, with the shell in
  `clients/web/src-tauri/`.)*
- ADR 0046 keeps `clients/web/` in this monorepo through the basic browser
  client and parity audit; revisit a separate repo after the SPA is stable.
- The OpenAPI spec becomes a first-class contract artifact. Breaking changes to
  `/v1/` require coordinated updates across all generated SDKs.
- OAuth 2.0 + PKCE (post-MVP) will replace the bearer-token paste flow for web
  and mobile clients. Bearer-token paste is acceptable alpha onboarding only
  for `axon-tui`; mobile and web clients should implement proper login UX once
  OAuth lands.
- Push notification support requires server-side additions (APNs/FCM
  integration, device-token registration endpoint) before mobile clients can
  deliver notifications. Client code should stub the registration path and
  activate it when the server ships the feature.
- Media URLs are axon-proxied; clients must not construct homeserver media URLs
  directly. The `matrix-api-media-proxy` branch establishes this contract.
- The web-framework choice is the one unsettled decision. It must be resolved —
  and recorded as a follow-on ADR or amendment here — before `clients/web/`
  work begins. *(Resolved by ADR 0046: Preact.)*
