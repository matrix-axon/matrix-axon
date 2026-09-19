# ADR 0054 — OAuth 2.0 authorization server (Apple / Google / Microsoft sign-in)

## Context

ADR 0053 named OAuth 2.0 + PKCE + Sign-in-with-Apple as a hard blocker before
an iOS client ships, and explicitly deferred the design to a follow-on ADR:

> This ADR does not design the OAuth flow — implementation should be gated on
> a dedicated follow-on ADR (authorization-code + PKCE flow, Apple
> identity-token verification, session/refresh-token model) before any code
> is written.

This is that ADR. It depends on ADR 0053's decision that OAuth is required at
all; 0053 is still only open PR #214 at the time of writing, and should merge
before this one, or this ADR reads as gated on a decision not yet in `main`.

Apple is a real (not just UX) requirement here, not merely
a preference: App Store Review Guideline 4.8 requires that any app offering a
third-party/social login (Google, Microsoft, etc.) also offer Sign-in-with-Apple
as an equivalent option. Since this ADR adds Google and Microsoft sign-in
alongside Apple, Apple stops being optional the moment those ship on iOS —
this is an App Store compliance requirement, not only a design preference. A
client offering no social login at all (pure CLI-token-paste) would be
exempt from Guideline 4.8, but that is not the scope commissioned here.

Three prior decisions constrain the design:

- **ADR 0029** carved out a `TokenVerifier` port specifically so "OAuth
  replaces the `TokenVerifier` impl and the mint path; the `Authorization:
  Bearer` contract, the middleware, and every route stay unchanged." This ADR
  is bound by that constraint: `verify()` keeps its `(token) -> Result<bool,
  ApiError>` signature; expiry becomes a `WHERE` clause, not a new method.
- **`docs/mvp/implementation.md`** ("What not to build") ruled out a full
  OAuth server for the MVP. That MVP is complete; this ADR is explicitly
  post-MVP scope, gated by ADR 0053.
- **AGENTS.md**: "One human per Axon process." There is no multi-tenant or
  per-account authorization model here, and this ADR does not introduce one.

Scope decisions taken before writing this ADR:

- All three identity providers — Apple, Google, Microsoft — behind one
  generic abstraction, so provider #2 and #3 are thin. Apple ships first (the
  actual iOS blocker, and the hardest).
- Owner binding is an explicit CLI verb (`axon oauth bind`), not
  trust-on-first-use and not a config-pinned subject — matching the existing
  `axon token issue` CLI-bootstrap precedent (ADR 0029).
- This ADR is server-side only. No axon-owned client exists to drive the
  browser-redirect leg (`clients/tui` has no OAuth UI; there is no web or iOS
  client in the repo yet). The CLI bind command is the only client-shaped
  surface this work delivers end-to-end.

## Decision

### Two protocol roles, one process

Axon plays two different OAuth/OIDC roles simultaneously, and keeping them
conceptually separate is the key to the rest of this design:

1. **Axon is an OAuth 2.0 Authorization Server (RFC 6749) to its own
   clients** — `axon-tui`, a future web client, a future iOS client. It mints
   *axon's own* access and refresh tokens. Every axon-registered client is a
   **public client** (RFC 8252): no client secret, PKCE (S256 only, `plain`
   rejected) mandatory on any flow that has a redirect leg.
2. **Axon is an OpenID Connect Relying Party to the upstream IdP** (Apple /
   Google / Microsoft) purely to answer one question: is the human on the
   other end of this request the bound owner of this axon instance? Axon
   never hands an upstream (Apple/Google/Microsoft) token to a client to use
   against `/v1/`. Upstream tokens are consumed internally and never leave
   the server process.

These compose into two independent entry paths that both terminate in "mint
an axon access + refresh token," corresponding to the two paths ADR 0053
called out:

- **Path A — web-redirect (the browser/system-webview flow).** An axon
  client starts an authorization-code+PKCE flow against axon's own
  `/v1/oauth/authorize`. Axon, acting as the RP, redirects the browser again
  — to the *upstream* provider's own authorize endpoint. The provider
  redirects back to axon's `/v1/oauth/{provider}/callback` with an upstream
  authorization code. Axon exchanges that code at the provider's token
  endpoint (using its own client credential — see Apple below), verifies the
  returned `id_token`, matches its `sub` against a bound `oauth_identities`
  row, and only then mints axon's own authorization code and redirects back
  to the *original* axon client's `redirect_uri`. The client redeems axon's
  code via `POST /v1/oauth/token` (`grant_type=authorization_code`) with its
  PKCE `code_verifier`.
- **Path B — native identity token (the mobile-SDK flow).** The native
  Sign-in-with-Apple / Google / Microsoft SDK on the device hands the *app* a
  provider-signed identity token directly — no redirect through axon at all.
  The app forwards that token in one `POST /v1/oauth/token` call
  (`grant_type=urn:axon:identity_token`, carrying `provider` +
  `identity_token`). Axon verifies it against the provider's JWKS/claims,
  matches `sub`, and mints tokens directly. No PKCE here — there is no
  redirect leg to protect. But "the identity token is the proof of
  possession" is not by itself a replay defense: it is a bearer artifact
  valid for its whole `exp` window, and anything that captures one (a
  malicious SDK, a logging sink, a TLS-terminating middlebox) can replay it
  to mint axon tokens as many times as it likes before expiry. The explicit
  mitigation is **single-use consumption**: axon records each verified
  token's `jti` (falling back to a hash of the token if the provider omits
  `jti`) in a small short-TTL table keyed by `(provider, jti)`, and a second
  presentation of the same token is rejected outright — the same
  "conditional-update, not read-then-write" discipline used for Path A's
  nonce and code consumption. This bounds replay to the (short) window
  between legitimate first use and the reject taking effect, not the token's
  full `exp` lifetime.

Axon's own token response is plain OAuth 2.0
(`access_token`/`refresh_token`/`token_type`/`expires_in`) — not an OIDC
`id_token`. Axon is not an OIDC provider to its own clients; there is only
one owner, so there is no profile/claims surface a client needs beyond "here
is your bearer token."

### `OidcProvider` abstraction

```rust
#[async_trait]
pub trait OidcProvider: Send + Sync {
    fn name(&self) -> &'static str; // "apple" | "google" | "microsoft"

    /// Build the upstream authorize URL for Path A.
    fn authorize_url(&self, state: &str, nonce: &str, redirect_uri: &str) -> String;

    /// Exchange an upstream authorization code for upstream tokens (Path A).
    async fn exchange_code(&self, code: &str, redirect_uri: &str)
        -> Result<UpstreamTokens, OidcError>;

    /// Verify a raw JWT — either the id_token from exchange_code (Path A) or
    /// a native-SDK identity token handed straight to the client (Path B) —
    /// against this provider's JWKS + expected claims (iss/aud/exp/nonce).
    async fn verify_identity_token(&self, token: &str, nonce: Option<&str>)
        -> Result<VerifiedIdentity, OidcError>;
}

pub struct VerifiedIdentity {
    pub subject: String,
    pub email: Option<String>,
    /// The token's `jti` claim, or a hash of the token if the provider
    /// omits `jti`. Used by the Path B handler for single-use replay
    /// consumption — see "Schema" and "Robustness at this boundary" below.
    pub replay_key: String,
}
```

- **`GenericOidcProvider`** is config-driven and covers Google and Microsoft
  directly: fetch the provider's `.well-known/openid-configuration` once
  (cached, refreshed on a timer / on `kid` miss) to learn the
  authorization/token endpoints and JWKS URI, so config only needs `issuer` +
  `client_id` + `client_secret`.

  Microsoft's multi-tenant endpoints (`common` / `organizations` /
  `consumers`) are the one case where "`issuer` is just a config value" and
  "`iss` exact-match" (Robustness, below) are in tension: a token issued
  through the `common` endpoint carries a **tenant-specific** `iss`
  (`https://login.microsoftonline.com/{tenantid}/v2.0`), never the literal
  string `common` — so a naive exact-match against the configured
  `.../common/v2.0` value rejects every genuine token. `GenericOidcProvider`
  resolves this by treating a discovery-doc issuer containing the literal
  `{tenantid}` placeholder (which Microsoft's discovery document actually
  returns verbatim in multi-tenant mode) as a **template**: validation
  substitutes the token's own `tid` claim into the template and requires the
  result to exactly match the token's `iss`, rather than comparing `issuer`
  as a literal string. Single-tenant configurations (a specific tenant GUID
  in the issuer URL) need no special case — the literal exact-match applies
  as originally stated, and Google (no tenancy concept) is unaffected
  either way.
- **`AppleProvider`** wraps the same JWKS-verification core but overrides two
  things that are genuinely Apple-specific and stay isolated to this module:
  1. **Client-secret generation.** Apple has no static client secret; axon
     signs an ES256 JWT (`iss`=team_id, `sub`=client_id/Services ID,
     `aud`=`https://appleid.apple.com`, `kid`=key_id, `exp`≤6 months) from an
     Apple-issued private key, and regenerates it on a schedule (e.g. every
     24h) well inside the 6-month ceiling — fully automatic, no operator
     action, as long as the underlying private key stays valid. Rotating the
     *underlying key itself* is an Apple-Developer-console action outside
     axon's control; axon can only detect its failure at exchange time
     (provider returns an auth error) and surface it clearly in logs/metrics,
     not prevent it.
  2. **Native audience mismatch.** Apple's native-SDK identity token's `aud`
     is the app's bundle ID / App ID, while the web-redirect flow's `aud` is
     the Services ID configured as `client_id`. `AppleProvider` therefore
     takes an explicit `native_audiences: Vec<String>` alongside `client_id`,
     and `verify_identity_token` accepts either.

### Schema

Four new tables, one extension to an existing table:

- **`tokens` (extend in place).** New nullable columns: `expires_at
  TIMESTAMPTZ`, `provider TEXT`, `oauth_identity_id UUID REFERENCES
  oauth_identities(id)`, `client_id TEXT`. All default `NULL`, which is
  exactly today's CLI-token semantics (never expires, no provenance) —
  existing CLI-minted tokens and `axon token issue` are untouched, zero
  behavior change. `Store::verify_token`'s query gains one clause:

  ```sql
  UPDATE tokens SET last_used_at = now()
  WHERE hash = $1 AND revoked_at IS NULL
    AND (expires_at IS NULL OR expires_at > now())
  RETURNING id
  ```

  `TokenVerifier::verify` does not change its signature or its caller
  (`require_bearer`, the WS upgrade check, and the WS 30s revalidation loop
  all keep working unmodified — an OAuth-minted token that expires mid-socket
  gets dropped by the existing revalidation path with no new code).

- **`oauth_identities (id, provider, subject, email NULL, linked_at, UNIQUE
  (provider, subject))`.** The thing `axon oauth bind` populates. No
  `owner_id`/`account_id` column — like `tokens`, this table is global to the
  single-owner instance; any row *is* a recognized owner identity. The human
  can bind Apple **and** Google **and** Microsoft simultaneously; all three
  authenticate the same one owner.

- **`oauth_refresh_tokens (id, hash UNIQUE, oauth_identity_id FK, client_id,
  created_at, expires_at, revoked_at, replaced_by UUID NULL FK to self)`.** A
  separate table from `tokens` — deliberately: a refresh token's lifetime and
  rotation are independent of any one access token it mints, and keeping it
  out of `tokens` means `TokenVerifier` (and everything that depends on ADR
  0029's "port stays unchanged" promise) never has to know refresh tokens
  exist. Refresh is one-time-use with rotation: each redemption revokes the
  presented row (`revoked_at`, `replaced_by` = the new row) and mints a fresh
  refresh token alongside the new access token — standard reuse-detection
  hygiene for public clients. Hashed like access tokens (SHA-256), not
  `pgp_sym_encrypt`'d (ADR 0008's model) — same reasoning as ADR 0029: a
  high-entropy secret has nothing to recover, and there is no need to ever
  read one back in plaintext.

- **`oauth_bind_requests (device_code, user_code, provider, status,
  created_at, expires_at, oauth_identity_id NULL FK)`.** Ephemeral
  scaffolding for the CLI bind flow (below); short TTL (10 minutes),
  single-use, pruned on next insert or a cheap boot-time sweep.

- **`oauth_authorization_requests (id/state, client_id, redirect_uri,
  code_challenge, code_challenge_method, provider, upstream_state,
  upstream_nonce, created_at, expires_at, status, oauth_identity_id NULL,
  axon_code_hash NULL)`.** The in-flight bookkeeping for Path A's
  double-redirect: axon can't rely on a client-side cookie/session across a
  hop through the upstream IdP's own domain, so the pending request lives
  server-side, keyed by an opaque `state`/id, single-use, short TTL.

- **`oauth_consumed_identity_tokens (provider, replay_key, consumed_at,
  UNIQUE (provider, replay_key))`.** Backs Path B's single-use replay
  defense. A verified identity token's `replay_key` (its `jti`, or a hash of
  the raw token if the provider omits `jti`) is inserted here in the same
  transaction that mints the resulting axon tokens; the insert is the
  concurrency control (`ON CONFLICT DO NOTHING` + check rows-affected, not
  read-then-write), so two concurrent replays of the same token can mint at
  most once. Rows are pruned once `consumed_at` is older than the provider's
  own token `exp` ever gets (short-lived, bounded growth).

Provider client secrets/Apple private keys live in config only (see below),
consistent with `store_key`'s existing precedent of "plain `Option<String>`,
validated lazily at use, no secrets-manager integration."

### Router boundary

`/v1/oauth/*` must be reachable with no bearer token — that's the entire
point. `crates/axon-api/src/lib.rs`'s `router()` builds a gated `authed`
sub-router (`route_layer(require_bearer)`) and merges `/healthz` and
`/v1/ws` outside it. This adds a **third** sibling: an un-gated `oauth`
sub-router merged at the same level, carrying:

- `GET /v1/oauth/authorize` (Path A, start)
- `GET /v1/oauth/{provider}/callback` (Path A, upstream redirect target)
- `POST /v1/oauth/token` (all three grants: `authorization_code`,
  `refresh_token`, `urn:axon:identity_token`)
- `GET /v1/oauth/bind` (CLI-bind browser landing page/redirect —
  `?user_code=`)

Each handler checks `oauth.enabled` (and the specific provider's `enabled`)
in config and returns `404` when off — unlike `GET /v1/search`, which
returns `503` for the same "feature disabled" case. The two routes are
deliberately different: `/v1/search` is a route behind the bearer gate,
so its existence is already known to any authenticated client, and `503`
("exists, temporarily/config unavailable") is the honest signal. `/v1/oauth/*`
is unauthenticated surface, where not confirming the route exists at all is
the more defensible default; `404` is chosen for that reason, not because it
mirrors `/v1/search`. Either way, no conditional router construction is
needed — the handler itself checks config and returns early.

`/v1/ws` is already proof, in the live router, that a specific route beats
the authed sub-router's `/v1/{*path}` catch-all regardless of merge order —
it is merged as an un-gated sibling today and axum resolves it correctly.
The same guarantee applies to `/v1/oauth/*`. A regression test must still
assert this explicitly for `/v1/oauth/token`, rather than relying on the
`/v1/ws` precedent by inference.

### Robustness at this boundary

This is now the only unauthenticated surface in the whole API capable of
minting credentials, which raises the bar per AGENTS.md's "robustness at
boundaries" checklist:

- **Timeouts.** Every outbound call (provider token endpoint, JWKS fetch,
  discovery-doc fetch) on a bounded `reqwest::Client` (connect + total
  timeout); a hung provider fails the one login attempt with a `502`/`503`,
  never blocks the request indefinitely.
- **Bounded resources.** JWKS responses cached with a TTL. A cache-miss on an
  unrecognized `kid` triggers at most one refresh per provider per
  short minimum interval (e.g. 60s), independent of how many requests arrive
  with unknown `kid`s in that window — otherwise an attacker submitting
  tokens with random `kid`s could turn every miss into an outbound fetch,
  amplifying one inbound request into one outbound request against the
  provider. A fixed-size response cap applies to fetched JWKS/discovery
  docs.
- **Hostile input.** Request-body size caps on `/v1/oauth/token` and the
  callback route; strict JWT validation — `iss` exact-match against the
  configured issuer *for that request's tenant*, `aud` matched against
  `client_id` (or `native_audiences` for Apple), `exp`/`iat`/`nbf` checked
  with a small clock-skew allowance, algorithm allow-list (RS256/ES256 only,
  `none` rejected outright), signature verified against the JWKS key
  selected by `kid`. "Exact-match" needs a caveat for Microsoft's
  multi-tenant issuers: see below. Nonce is single-use, matched against
  `oauth_authorization_requests`, and the row is consumed on use (replay
  protection for Path A); Path B's equivalent is `jti`-based single-use
  consumption (`oauth_consumed_identity_tokens`, see Schema).
- **`client_id` is not a security boundary.** Axon's clients are public
  (no secret) and Path B carries no PKCE verifier, so a valid
  provider-signed identity token redeemed at `POST /v1/oauth/token` mints
  tokens regardless of which registered `client_id` is presented alongside
  it. `client_id` is a label for `tokens.client_id`/telemetry, not an
  authorization control — Path B's entire trust rests on the provider's
  signature over the identity token plus the bound `oauth_identities.sub`
  match. (Path A's `client_id` *does* participate in the boundary, via
  redirect-URI allow-listing below — the two paths are not symmetric here.)
- **Redirect-URI allow-listing.** A client's requested `redirect_uri` must
  exactly match one of that `client_id`'s pre-registered URIs (no
  prefix/substring match) — this is the open-redirect / code-interception
  defense for Path A.
- **Rate limiting.** No rate-limiting infrastructure exists anywhere in this
  codebase today; this is new. An in-process per-IP + per-`state` token
  bucket (e.g. the `governor` crate) over `/v1/oauth/*`, sized to blunt
  RFC 8628 §5.4-style `user_code` guessing on the bind flow and general
  brute-forcing of codes/tokens.
- **Concurrency.** Redeeming an authorization code or refreshing a token must
  be single-use under concurrent duplicate requests — the row's
  `status`/`revoked_at` transition needs a `WHERE status = 'pending'`-style
  conditional update (matching `revoke_token`'s existing idempotent-update
  pattern), not a read-then-write race.
- **Partial failure.** A JWKS/provider outage fails *that* login attempt
  cleanly (never a panic, never a hang); it does not affect any other
  in-flight request or existing sessions.

### CLI bind command (`axon oauth bind`)

The one client-shaped deliverable in this ADR. The central design problem:
axon supports both "on the same box as your browser" and "home server behind
a VPN, admin's browser is elsewhere" deployments (M13). A naive
loopback-listener design (`gh auth login`-style: CLI spins up a local HTTP
listener, browser redirects to it) only works for the first case. Instead,
this piggybacks on axon's **already-running** `/v1/` HTTP surface — the same
connectivity path every other axon client already uses — rather than
inventing a new listener/port:

1. `axon oauth bind --provider apple` connects directly to the `Store` (same
   pattern as `axon token issue` — no dependency on the sync engine). It
   generates a `device_code` (opaque, server-side) and a short human-typeable
   `user_code` (8 chars, RFC 8628-style), INSERTs a pending
   `oauth_bind_requests` row (10-minute TTL), and prints: "Open
   `https://<oauth.external_base_url>/v1/oauth/bind?user_code=XXXX-XXXX` in
   any browser and sign in with Apple."
2. The CLI then polls the **same row directly via `Store`** (not over HTTP)
   until `status` becomes `completed`/`expired` — this deliberately keeps the
   CLI's own dependency surface identical to `axon token`'s (DB-only), while
   the *browser's* leg depends on the long-running `axon` server process
   already being up (a new, explicit, documented precondition —
   `axon token issue` has no such requirement; `axon oauth bind` does).
3. The admin's browser hits `GET /v1/oauth/bind?user_code=...` on the running
   server, which looks up the pending row and redirects into the normal Path
   A upstream flow (state carries a reference to the pending
   `oauth_bind_requests` row). On successful upstream verification, the
   callback handler UPSERTs `oauth_identities` and marks the bind request
   `completed`.
4. The CLI's poll loop reports success and exits.

This works identically for localhost and remote/VPN deployments, since it
reuses whatever reachability the deployment already has — no new port, no
new firewall rule. `axon oauth identities list` / `axon oauth identities
unbind <id>` round out the management surface; `unbind` revokes the
`oauth_identities` row **and** every `tokens`/`oauth_refresh_tokens` row
carrying that `oauth_identity_id` — the "sign out everywhere for this
identity" operation.

### Config shape

A new `oauth` section, following the existing `sync`/`search`/`media`
sectioning convention (`#[serde(default)]`, defaults off):

```toml
[oauth]
enabled = false                     # default off; CLI-token flow unaffected
external_base_url = "https://myaxon.example.com"   # what `oauth bind` prints
access_token_ttl_secs = 3600
refresh_token_ttl_secs = 2592000

[[oauth.clients]]                   # axon's own pre-registered public clients
client_id = "axon-ios"
redirect_uris = ["axon://oauth/callback"]

[[oauth.clients]]
client_id = "axon-web"
redirect_uris = ["https://myaxon.example.com/callback"]

[oauth.providers.apple]
enabled = true
client_id = "com.example.axon.service"   # Apple Services ID
native_audiences = ["com.example.axon.ios"]
team_id = "ABCDE12345"
key_id = "XYZ98765"
private_key = "-----BEGIN PRIVATE KEY-----..."   # or private_key_path
redirect_uri = "https://myaxon.example.com/v1/oauth/apple/callback"

[oauth.providers.google]
enabled = false
issuer = "https://accounts.google.com"
client_id = "..."
client_secret = "..."

[oauth.providers.microsoft]
enabled = false
issuer = "https://login.microsoftonline.com/common/v2.0"
client_id = "..."
client_secret = "..."
```

Secrets (`private_key`, `client_secret`) are plain `Option<String>`,
validated lazily at first use, matching `sync.store_key`'s precedent — no
secrets-manager integration exists in this codebase and this ADR does not
introduce one.

The `axon-ios` client's `redirect_uris = ["axon://oauth/callback"]` uses a
custom URL scheme, which any app on the device is free to register — a
second app claiming `axon://` could intercept the redirect. PKCE-S256 is the
stated mitigation (the intercepting app gets a code it cannot redeem without
the original app's in-memory `code_verifier`), which is sufficient. Whether
M14c's iOS client should prefer a universal link (`https://` redirect,
which iOS uniquely routes to the registered app, no scheme-squatting
possible) or `ASWebAuthenticationSession` with the custom scheme is a client
implementation choice, not a wire-contract change either way — left for
M14c to decide.

### Explicitly out of scope

- No client implementation (iOS/web/Android) — none exists yet; this ADR
  only specifies the wire contract they'll consume.
- No multi-tenant/multi-owner model. `oauth_identities` has no owner-scoping
  column by design.
- No dynamic client registration (RFC 7591). Axon's own clients are
  statically pre-registered in `[[oauth.clients]]`.
- No `scope` enforcement. `scope` is accepted and echoed for spec
  compliance, never enforced — axon-minted tokens remain all-or-nothing
  bearer tokens, matching the existing "per-account authorization is a
  non-goal" stance.
- No changes to any existing `/v1/*` route or its wire contract (ADR 0029's
  constraint, honored).
- No CORS design for a browser-based web client — deferred to whichever PR
  actually builds a web client (none exists today); only `/v1/oauth/token`
  would ever need it, not the redirect legs (top-level navigations).

## Consequences

- `TokenVerifier` and every existing route are provably unchanged — one new
  `WHERE` clause in `Store::verify_token`, nothing else. This is the
  ADR 0029 forward-compat promise cashed in.
- `axon token issue`/`list`/`revoke` continue to work unmodified, and now
  transparently double as the management surface for OAuth-minted access
  tokens too (they show up with `provider`/`client_id` populated).
- Apple's client-secret JWT needs periodic regeneration (automatic, in
  process) but the underlying private key's rotation is an Apple-console
  action axon cannot detect except by exchange failure at runtime — an
  accepted operational gap, not something this design can close.
- New rate-limiting infrastructure is required where none existed before.
- The web-redirect flow (Path A) has zero consumers until a web or iOS
  client exists; its verification story for now is a mocked-JWKS
  integration test plus a documented manual walkthrough against a real
  provider sandbox app, not an automated end-to-end client test.
- With `access_token_ttl_secs = 3600`, the existing WS 30s revalidation loop
  (ADR 0029) drops every OAuth-authenticated socket roughly once an hour, by
  design — no new server code is needed to enforce this, since expiry is
  just the existing `expires_at IS NULL OR expires_at > now()` clause going
  false. But it does mean any client on Path A/B must implement
  refresh-then-reconnect, not just refresh: a client that refreshes its
  access token but never re-establishes the WebSocket will silently stop
  receiving events an hour after login.
- Project docs (`docs/mvp/implementation.md` and any milestone tracker) need
  a one-line update to record M14's existence and status once this ADR
  lands — not done as part of this PR.

## Open Questions

- **Token/refresh TTL policy.** `3600s` / `30d` above are starting points,
  not a considered final recommendation — revisit once real usage patterns
  (mobile background refresh behavior, etc.) are observed.
- **Rate-limiting implementation.** `governor` is a plausible in-process
  choice but this is a real new architectural surface, not a copy-paste of
  an existing pattern.
- **Apple/Google/Microsoft developer-console ownership.** Registering a real
  Apple Services ID + private key (paid Apple Developer account, verified
  domain), and Google/Microsoft OAuth app registrations, are
  operational/business dependencies, not engineering — these must exist
  before the corresponding provider PR's manual verification step can run.
- **M14 vs. ADR 0053's other two prerequisite items.** If the device-listing
  endpoint or the ADR 0030 `sync_state` implementation are worked in
  parallel, they should claim `M15`/`M16` rather than interleave with this
  milestone's lettered sub-PRs (M14a–M14e).

## Implementation addendum: Apple rollout

This addendum records the Apple implementation sequence as of September 2026 and supersedes conflicting Apple-specific details above.
Google and Microsoft sign-in, Axon access/refresh tokens, provider discovery, owner binding, browser clients, and the Tauri shell now exist.
Apple remains unavailable to operators until the callback and binding integration below lands.
This authenticates the owner of an Axon instance; it is separate from Matrix homeserver OAuth in ADR 0097.

### Deployment and trust

Axon remains a single-owner service.
Ordinary sign-in accepts only an explicitly bound `(provider, subject)` identity.
An email address, including an Apple private relay address, never establishes ownership or links identities across providers.
Binding requires either existing-owner authorization or the existing explicit bootstrap capability; no trust-on-first-use is introduced.

Apple's browser flow needs a Services ID associated with a primary App ID, registered domains/return URLs, and a developer-controlled signing key.
Apple limits registered website URLs and instructs developers not to share signing keys outside their team.
Therefore, the distributed Axon application must not give its developer-team signing key to self-hosters or embed it in the client.
Browser Apple OAuth is for deployments with their own appropriately registered credentials.
The distributed iOS app will instead use native Sign in with Apple and forward its identity token to the selected Axon instance for public-key verification.
The native-only verifier must be constructible without a Services ID or private key when that runtime integration lands; this first provider constructor is for the credentialed browser flow.
No central authentication broker is introduced by this decision.
See Apple's [web registration guide](https://developer.apple.com/help/account/capabilities/configure-sign-in-with-apple-for-the-web) and [environment/key guidance](https://developer.apple.com/documentation/signinwithapple/configuring-your-environment-for-sign-in-with-apple).

Guideline 4.8 currently requires an equivalent login option with specified privacy properties, rather than mandating Apple by name in every case.
Sign in with Apple is the chosen way to meet that requirement for Axon's mobile social-login experience.
Shipping a provider library alone does not establish App Store readiness.
See [App Review Guidelines, section 4.8](https://developer.apple.com/app-store/review/guidelines/#login-services).

### PR 1: provider foundation and this addendum

The server library now implements `AppleProvider` alongside `GenericOidcProvider`.
Both use shared signature/claims verification and authorization-code exchange with the existing bounded HTTP/JWKS infrastructure.
The extraction preserves Google's issuer and Microsoft's tenant-template handling, with explicit bounds on JWT size, overflow-safe timestamp comparisons, nonempty subjects, and sanitized errors that do not echo token claims.

Apple's authorization request uses `response_type=code`, `response_mode=form_post`, `scope=email`, state, and nonce.
Axon does not request names or depend on the unsigned, first-login-only `user` object.
Apple's token response is bounded and only its identity token leaves the provider adapter; upstream access and refresh tokens are discarded.
Errors include fixed categories and HTTP status, never response bodies, authorization codes, generated client secrets, or key material.
The existing error boundary can log those categories; no secret-bearing developer mode is added.

Client authentication uses an ES256 JWT with the configured key ID, team ID as issuer, Services ID as subject, and `https://appleid.apple.com` as audience.
Generate a fresh five-minute JWT for each token exchange instead of maintaining a renewal timer or secret cache.
This automatically renews credentials after idle periods and restarts, stays well within Apple's six-month ceiling, and needs no background-task ownership or lock.
Construction validates required fields and proves the PEM key can sign ES256 before accepting login attempts.
Underlying key revocation still needs operator action.

Web verification accepts only the configured Services ID and requires the stored upstream nonce.
The explicit native verification method accepts only configured bundle IDs and requires a nonce supplied by the future server challenge handler.
Unlike the original union-of-audiences proposal, one path cannot substitute the other's audience.
The Apple implementation rejects the existing nonce-free identity-token grant; enabling the browser provider later must not silently enable an unbound native flow.

The binary startup and CLI binding gates remain in place in this PR.
Removing them before POST callbacks land would advertise a provider whose login cannot finish.
No HTTP API, migration, or client-rendering change is introduced here, so no OpenAPI regeneration or demo scene is needed.

Verification needs Rust and the repository's normal build dependencies, but no Apple account, database, or external service for these focused tests:

```bash
cargo test -p axon-api --lib oauth::
```

Apple tests generate ephemeral keys in memory using a test-only dependency on the existing AWS-LC backend, with real ES256 signing and RS256 verification and local HTTP endpoints.
No private signing material is committed or persisted.
They exercise authorization encoding, client-secret renewal, code exchange, separate web/native audiences, issuer/nonce/time/signature rejection, relay email, absent repeat-login profile data, deterministic replay keys, body limits, transport timeouts, and secret-safe errors.
The shared Google/Microsoft verifier also needs signed-token regression coverage, not only issuer predicate tests.

### PR 2: browser callbacks, runtime configuration, and owner binding

Add bounded form POST handling beside GET on `/v1/oauth/{provider}/callback`, with one shared completion implementation for login, CLI binding, and first-run bootstrap.
Decode success and cancellation/error responses explicitly.
Validate state, provider, expiry, and flow purpose before dispatch, and preserve atomic single-use completion.
Do not require a browser cookie to survive Apple's cross-site POST.
Errors must lead to a retryable UI without reflecting upstream descriptions or leaving the client waiting indefinitely.
See Apple's [callback contract](https://developer.apple.com/documentation/signinwithapple/configuring-your-webpage-for-sign-in-with-apple).

Load the signing key from protected configuration or a bounded private-key file, redact config diagnostics, and use one validated callback resolver across authorize, exchange, binding, and bootstrap.
Require the exact registered HTTPS callback for Apple; reject conflicting configured and derived callback URLs.
Wire the provider into startup and discovery, then remove both enablement gates together.
Update configuration examples, README, OpenAPI where applicable, and the implementation tracker in the same server PR.
Validate real Apple browser login and CLI/bootstrap binding on a registered test deployment before calling browser support complete.

### PR 3: native server contract and owner binding

Add a short-lived, server-issued challenge for Apple native login, bound to an explicit flow purpose and selected Axon instance.
Specify precisely whether the native SDK receives the raw nonce or its digest and verify the corresponding signed claim.
The expected nonce comes from server state, never from an untrusted token or request field.
Keep the public API additive for existing Google/Microsoft clients.
Construct native verification independently of browser credentials and expose its availability accurately in provider discovery.

Consume the challenge and replay key in the transaction that mints Axon tokens.
The existing Path B implementation consumes the replay record before a separate mint operation; close that crash/failure window here.
Concurrent redemption may mint at most once, and an interrupted attempt must be recoverable by starting a fresh challenge.
Authorize native binding through an existing owner session or the explicit bootstrap capability, so self-hosters need no Apple browser credentials to establish ownership.
Verify App ID/Services ID grouping and subject behavior with real Apple credentials rather than linking by matching emails.

### Subsequent client, lifecycle, and smoke PRs

After server stabilization, integrate native Sign in with Apple into the Tauri iOS shell, with the Apple entitlement and compliant button, secure Axon-token persistence, cancellation/retry, restart recovery, and refresh/reconnect.
Keep client behavior in its own PR and update client parity and demo coverage there.
Retain the existing browser PKCE flow for registered browser deployments.

Resolve Apple authorization revocation and applicable account-deletion requirements before App Store submission.
Local Axon logout/unbinding does not revoke Apple's authorization.
The current provider discards upstream tokens, so it cannot promise unattended Apple revocation; a follow-up decision must specify a fresh-authorization revocation flow or explicitly revisit credential retention and key custody.
Do not persist upstream credentials or introduce a credential broker as an incidental implementation detail.
Deletion must define its Axon-data scope, local token invalidation, retries after partial failure, and recovery after restart.
See Apple's [account-deletion guidance](https://developer.apple.com/support/offering-account-deletion-in-your-app) and [TN3194](https://developer.apple.com/documentation/technotes/tn3194-handling-account-deletions-and-revoking-tokens-for-sign-in-with-apple).

Keep black-box smoke additions in their own smoke-silo PR.
The release gate includes real-device first and repeat sign-in, Hide My Email, binding, cancellation/retry, restart, refresh, logout, and deletion/revocation, plus Google/Microsoft regression checks.
Logs and persisted test artifacts must contain no real tokens, authorization codes, private keys, or client-secret JWTs.
App Review needs a working review deployment and instructions that exercise the actual supported flow.
