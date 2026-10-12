# ADR 0117 — Matrix OAuth browser sign-in with the device authorization grant

**Status:** Proposed; implementation proceeds through [issue 690](https://github.com/matrix-axon/matrix-axon/issues/690).
This adds a third acquisition method beside the two QR presentations of [ADR 0097](0097-matrix-oauth-qr-login-and-device-authorization.md) and reuses that decision's session, registration, and finalization machinery.

## Context

A homeserver that delegates authentication to an OAuth 2.0 authorization server may offer no password login at all.
A tester's homeserver is one: Synapse with Matrix Authentication Service (MAS) in front of an Authentik identity provider, whose `GET /_matrix/client/v3/login` lists only `m.login.sso` and `m.login.token`.

Axon can add an account three ways today, and none of them works for that user from a client:

- **Password** (`POST /v1/accounts/login`) is refused by the homeserver.
- **QR login** (ADR 0097) needs another signed-in Matrix client that implements MSC4108, which the user may not have.
- **Token import** (`POST /v1/accounts/import`) works only if the user mints a token by hand, is not exposed in any client, and stores no refresh token.

What such a user expects is the ordinary delegated pattern: open a browser, sign in with the identity provider, approve the new device, and return.

ADR 0097 already built almost everything that pattern needs.
MSC4108 QR login _is_ the OAuth 2.0 Device Authorization Grant ([RFC 8628](https://www.rfc-editor.org/rfc/rfc8628)) wrapped in a QR-established secure channel that additionally transfers encryption secrets.
Authorization-server discovery and URL validation, dynamic and static client registration, encrypted refresh-token persistence and rotation, the staging SDK store, crash-safe adoption, and lifecycle finalization all exist, and Axon's registered client metadata already requests `urn:ietf:params:oauth:grant-type:device_code`.
The only QR-specific parts are the rendezvous channel and the secrets bundle.

Two facts constrain how the rest is built.

- **matrix-rust-sdk does not expose the grant on its own.**
  In 0.19.1, the newest release, `OAuth::request_device_authorization` and `OAuth::exchange_device_code` are private and reachable only through `login_with_qr_code()`.
  The public pieces are `restore_registered_client`, `restore_session`, `full_session`, and the authorization-code pair `login` / `finish_login`.
- **ADR 0097 rejected "treat ordinary Matrix OAuth login as verification".**
  A browser sign-in authenticates a Matrix device but does not cross-sign it or deliver the user's encryption secrets.
  That rejection stands: this decision adds an authentication method and makes no verification claim.

## Decision

### 1. Add browser sign-in as an acquisition method that uses the device authorization grant

Axon requests a device authorization from the homeserver's advertised authorization server, hands the client a verification link and a short user code, and polls the token endpoint until the user has signed in and approved in a browser of their choosing.
That browser may be on any device; it never needs to reach Axon.

The method is offered only when the validated discovery metadata contains a `device_authorization_endpoint`.
Without one the flow terminates with the existing stable `unsupported` classification, exactly as QR acquisition does, and password login, QR login, token import, recovery, and SAS are unaffected.

MAS remains the tested reference implementation and is not a server type in Axon's model.
The identity provider behind the authorization server is invisible to Axon by construction.

### 2. Implement the two grant requests in `axon-sync`, not through the SDK

`axon-sync` already hand-rolls authorization-server discovery and dynamic registration with a bounded `reqwest` client (`matrix_oauth.rs`).
The device authorization request and the token poll join them there, under the same rules: connect and total timeouts, redirects disabled, response bodies size-capped before parsing, HTTPS required unless the homeserver itself is plain-HTTP loopback, and stable log classifications with no response bodies.

The sequence is:

1. Build the staging SDK client and run `MatrixOAuthRegistrationManager::prepare`, unchanged.
2. Generate a fresh Matrix device ID locally.
3. `POST` the device authorization endpoint with the client ID and the same two scopes the pinned SDK requests for QR login: full client-server API access and the scope that binds the token to that device ID.
4. Publish the verification link and user code on the flow and enter `waiting_for_authorization`.
5. Poll the token endpoint with the device code, honouring RFC 8628: the server's `interval` (default five seconds), `slow_down` (add five seconds), `authorization_pending` (keep polling), `access_denied`, and `expired_token`.
6. Require a refresh token in the token response, as QR acquisition already does.
7. Assemble an `OAuthSession` from the client ID, the expected user ID, the generated device ID, and the tokens, and install it with the public `client.oauth().restore_session(...)`.
8. Continue into the existing `whoami` identity validation and finalization.

The poll deadline is the earlier of the server's `expires_in` and the engine's existing flow TTL, and the poll is cancellable at every wait.

If a later SDK release makes the grant public, the two requests can be replaced without changing the flow resource, storage, or clients.
The scope strings are kept identical to the pinned SDK's so both acquisition methods present the same request to the authorization server, and are revisited together when the SDK moves to the stable names.

### 3. Share the acquisition engine; add sibling routes

Browser sign-in is a third presentation of the existing `MatrixOAuthAcquireEngine` rather than a second engine.
It therefore inherits, without re-deciding them: one flow per expected Matrix user ID, the global concurrency and retention bounds, the overall TTL, the durable non-secret breadcrumb and boot reconciliation, single-owner terminal transitions, and best-effort revocation of a session that was minted but not adopted.

The existing routes say `qr` in their path, so the method gets sibling routes that return the same flow DTO:

- `POST /v1/accounts/login/browser` — create a flow for an expected Matrix user ID.
- `GET /v1/accounts/login/browser/{flow_id}` — read the current stage and its presentation-safe data.
- `DELETE /v1/accounts/login/browser/{flow_id}` — cancel idempotently.

There is no input route: nothing is scanned and no check code exists.
The flow uses a subset of the ADR 0097 stages — `starting`, `waiting_for_authorization`, `done`, `failed`, `cancelled` — and during `waiting_for_authorization` returns the verification link and the user code.
The link is the authorization server's `verification_uri_complete` when it supplies one, so the user does not have to type the code, and the bare `verification_uri` otherwise.

Two failure codes are added to the stable set: `authorization_denied` when the user refuses at the authorization server, and `authorization_expired` when the device code lapses before approval.
The remaining codes are reused as they are.

The change is additive to `/v1/`.
The breadcrumb table's `presentation` check constraint gains the new value through a new forward-only migration; no published migration is edited.

Homeserver resolution matches the QR display presentation: the server discovers it from the expected user ID's server name.

### 4. The account activates unverified, and says so

ADR 0097 fails a QR acquisition closed when the new device is not cross-signed, because QR login promises "sign in and verify".
Browser sign-in makes no such promise, so that gate does not apply to it; requiring it would make the method unusable.

A completed browser sign-in is therefore equivalent to a completed password login: the account is `active`, the device is not yet verified, and the API reports `verified: false`.
The user completes trust through the paths that already exist — recovery-key import or interactive SAS verification.
Finalization keeps every other ADR 0097 step: `whoami` must return the expected user ID, the per-identity lifecycle lock is held, a concurrent active login or deletion wins and the new session is revoked, and the session and staged store are adopted atomically before activation.

The trust check becomes a property of the acquisition method rather than of finalization as a whole, so the QR presentations keep failing closed.

Clients must not describe a browser sign-in as verified, and should lead directly from a successful one into the recovery-key or verification step.

### 5. Treat the verification link and user code as secrets in transit

A device authorization can be approved by anyone who holds the link, so presenting it to the wrong person would sign that person's account in to this Axon.
Three properties bound that:

- The routes sit behind Axon's ordinary bearer-token gate, so only the Axon owner can create a flow or read its link.
- The expected user ID is fixed when the flow is created, and a token belonging to any other Matrix user fails identity validation and is revoked.
- The link and code are returned only while the flow is in `waiting_for_authorization` and are cleared on every stage transition.

As ADR 0097 already requires, the user code, any URL containing it, the device code, and all tokens never enter logs, errors, the flow breadcrumb, or retained artifacts.
The device code never crosses `/v1/` at all.

### 6. Deliver as single-silo pull requests, server first

1. **ADR:** this docs-only decision, merged before implementation begins.
2. **Server:** the grant requests and browser presentation in `axon-sync`, the lifecycle change that makes the trust gate method-specific, the migration, the `/v1/` routes and DTOs, OpenAPI regeneration, and unit and contract tests.
3. **Smoke:** a `browser` lane in `smoke/matrix-oauth` that reuses the harness's existing scripted approval of an MAS verification link.
4. **Web:** a third sign-in method on the Accounts page — link, user code, polling, cancellation, failure recovery — leading into the existing recovery and verification UI.
5. **TUI:** the same method, which suits a terminal well because it needs neither a camera nor a QR renderer.

Directly related documentation ships with each code PR.

### 7. Verification required before calling the feature complete

Unit and contract tests cover: the device authorization request shape and scopes; every RFC 8628 poll outcome including `slow_down` and a missing `interval`; denial and expiry; a token response without a refresh token; oversized, non-JSON, and redirecting responses; non-HTTPS endpoints on an HTTPS homeserver; cancellation during the poll; the flow TTL; a Matrix user mismatch; a concurrent login, logout, or deletion; crash reconciliation; and that the link and user code appear only in `waiting_for_authorization` and nowhere in captured logs.
A regression test pins that the QR presentations still fail closed on an unverified device.

The real Synapse plus MAS lane proves that a user who approves in a browser ends with an active, unverified account whose OAuth session refreshes after a restart beyond the test access-token lifetime, that a recovery key then verifies the device and decrypts history created before sign-in, that explicit rejection yields `authorization_denied`, and that the existing `unsupported` configuration still reports `unsupported`.

That stack signs in to MAS with a local password rather than through an upstream identity provider, and serves plain HTTP on loopback.
Before the method is enabled by default it is also run once against a real HTTPS deployment whose authorization server fronts an upstream identity provider.

## Rejected alternatives

### The OAuth authorization-code flow

matrix-rust-sdk exposes it publicly, which would avoid hand-rolling the grant.
It is rejected as the first method because of the redirect URI.
Axon registers as a native client with a fixed project client URI, which restricts redirects to loopback or a custom scheme.
Loopback works only when the browser and the Axon server share a host; a custom scheme works only in the Tauri shells.
Supporting the plain web client would require registering each Axon deployment as its own web client under its own public HTTPS origin.
The device grant gives every client the same experience with no redirect at all.

### The legacy `m.login.sso` redirect

It would also reach older homeservers that offer SSO without an OAuth authorization server, which nothing in this decision can.
It is rejected here because on an OAuth-delegating homeserver it exists only as a compatibility layer the operator may disable, it yields a token with no refresh on a code path separate from ADR 0097, and it needs the same redirect handling as the authorization-code flow.
Support for those older homeservers would be a separate feature with its own decision.

### Driving the grant through the SDK's QR login

The private functions are reachable only inside `login_with_qr_code()`, which cannot proceed without a second device on the rendezvous channel.

### Patching or vendoring matrix-rust-sdk to make the functions public

Two HTTP requests do not justify carrying a second vendored SDK crate.
Raising the request upstream is worthwhile and does not block this work.

### Putting the method under the existing `qr` routes

Rejected because a `presentation` of `browser` under a path named `qr` would misdescribe the resource to every client author, and the browser flow has no scan or check-code input to share.

### Requiring a recovery key as part of the flow

Rejected because sign-in and trust are separate steps everywhere else in the API, and a user without their recovery key to hand should still be able to sign in and verify by SAS afterwards.

## Consequences

- A user on an OAuth-delegating homeserver can add their account with only a browser and no second Matrix client.
- Axon owns two small pieces of OAuth protocol code that the SDK also contains privately, and must keep their scopes in step with the SDK until it exposes them.
- The acquisition engine serves three presentations, and the trust gate that was unconditional in ADR 0097 becomes a per-method property that tests must pin.
- Accounts acquired this way start unverified, so the clients' verification prompts become part of the sign-in experience for these users rather than an occasional follow-up.
- Homeservers that offer SSO without an OAuth authorization server remain unsupported except by token import.
- Steering a user to this method before they try a password is a separate change ([issue 691](https://github.com/matrix-axon/matrix-axon/issues/691)).
