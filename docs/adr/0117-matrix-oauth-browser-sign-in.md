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

The method is offered only when the validated discovery metadata contains a `device_authorization_endpoint` and its `grant_types_supported` list includes `urn:ietf:params:oauth:grant-type:device_code`.
Matrix requires that list, so an endpoint advertised without the grant type is a server that does not offer it.
Otherwise the flow terminates with the existing stable `unsupported` classification, exactly as QR acquisition does, and password login, QR login, token import, recovery, and SAS are unaffected.

MAS remains the tested reference implementation and is not a server type in Axon's model.
The identity provider behind the authorization server is invisible to Axon by construction.

### 2. Implement the two grant requests in `axon-sync`, not through the SDK

`axon-sync` already hand-rolls authorization-server discovery and dynamic registration with a bounded `reqwest` client (`matrix_oauth.rs`).
The device authorization request and the token poll join them there, under the same rules: connect and total timeouts, redirects disabled, response bodies size-capped before parsing, HTTPS required unless the homeserver itself is plain-HTTP loopback, and stable log classifications with no response bodies.

The sequence is:

1. Build the staging SDK client, fetch the discovery metadata, apply the capability gate above, and only then resolve the client registration — a stored, static, or dynamic client ID — without installing it on the client.
   The gate comes first so that a server without the device grant is `unsupported` whatever else it would have refused, and Axon registers no client with a server it cannot use.
   _(Amended during implementation: the SDK accepts authentication data exactly once and panics on a second attempt, so the client ID cannot be installed here and again in step 7.
   The QR presentations still install it at this point, because the SDK's own login then adds the session.)_
2. Generate a fresh Matrix device ID locally.
3. `POST` the device authorization endpoint with the client ID and the same two scopes the pinned SDK requests for QR login: full client-server API access and the scope that binds the token to that device ID.
4. Validate the returned verification link and user code (below), publish them on the flow, and enter `waiting_for_authorization`.
5. Poll the token endpoint with the device code as RFC 8628 prescribes (below).
6. On a token response, enter `finalizing`, which clears the link and user code, and require a refresh token, as QR acquisition already does.
7. Assemble an `OAuthSession` from the client ID, the expected user ID, the generated device ID, and the tokens, and install it with the public `client.oauth().restore_session(...)`.
8. Continue into the existing `whoami` identity validation and finalization.
   For this method `whoami` must also report the device ID generated in step 2.
   A response that names another device, or omits the device, has not confirmed that the requested scope took effect, and the flow fails as `upstream`.

**One device ID.**
The staging client built in step 1 is unauthenticated and has no crypto identity; the SDK creates the device's Olm machine only when `restore_session` activates the client in step 7.
The device ID passed there is the one generated in step 2 and named in the step 3 scope, and no other device ID is ever derived.

**Validating what the authorization server returns.**
The verification link and user code are remote input that a client will render, so they are validated before they are published, and a response that fails validation ends the flow as `upstream` without exposing either value.
Both `verification_uri` and `verification_uri_complete` must parse as absolute URLs with a host, use `https` (or `http` only to a loopback host when the homeserver itself is plain-HTTP loopback), carry no userinfo, and fit a fixed length bound.
This is the rule `validate_qr_url` already applies to the grant flow's verification link, plus the length bound.
The user code must fit a short fixed length bound and contain only printable ASCII.
The link is not required to share an origin with the issuer, because RFC 8628 does not require it and the discovery metadata's own endpoints are not held to that either; clients show the link's host so the user can see where it leads.

**Polling.**
The first poll waits one interval after the device authorization response.
The interval is the server's `interval`, five seconds when absent, and never less than one second so a zero cannot hammer the endpoint.
It has no upper bound: RFC 8628 §3.5 makes `interval` the minimum wait, so polling sooner than the server asked invites throttling or a failed authorization.
A server that asks for a very long wait cannot stall the flow, because the deadline below ends it.
_(Amended after code review: the original decision clamped the interval to sixty seconds, which could poll sooner than the server required.)_
Token-endpoint responses map as follows:

| Response                                                         | Action                                                              |
| ---------------------------------------------------------------- | ------------------------------------------------------------------- |
| `authorization_pending`                                          | keep polling                                                        |
| `slow_down`                                                      | add five seconds to the interval, every time, with no ceiling       |
| `access_denied`                                                  | fail as `authorization_denied`                                      |
| `expired_token`                                                  | fail as `authorization_expired`                                     |
| transport error, timeout, `429`, or `5xx`                        | retry at the current interval; five consecutive fail as `upstream`  |
| any other OAuth error (`invalid_grant`, `server_error`, unknown) | fail as `upstream`                                                  |
| success without a refresh token                                  | fail as `upstream`, after best-effort revocation                    |

Only the stable classification and HTTP status are logged, never the error description.

**Deadline.**
The ten-minute flow TTL of ADR 0097 was sized for scanning a QR code and is too short for a sign-in that may pass through an upstream identity provider and a second factor on another device.
The browser presentation therefore has its own thirty-minute TTL, measured from flow creation.
The poll deadline is the earlier of that TTL and the server's `expires_in`, and reaching either while waiting fails as `authorization_expired`.
`timeout` keeps its existing meaning of an individual request that did not answer.
The QR presentations keep their ten-minute TTL.
The poll is cancellable at every wait.

If a later SDK release makes the grant public, the two requests can be replaced without changing the flow resource, storage, or clients.
The scope strings are kept identical to the pinned SDK's so both acquisition methods present the same request to the authorization server, and are revisited together when the SDK moves to the stable names.

### 3. Share the acquisition engine; add sibling routes

Browser sign-in is a third presentation of the existing `MatrixOAuthAcquireEngine` rather than a second engine.
It therefore inherits, without re-deciding them: the global concurrency and retention bounds, the durable non-secret breadcrumb and boot reconciliation, single-owner terminal transitions, and best-effort revocation of a session that was minted but not adopted.

The bound of one flow per expected Matrix user ID spans all three presentations.
Creating a browser flow while a QR flow for the same user is live, or the reverse, returns the existing `409` conflict, so two flows can never finalize against each other.

The existing routes say `qr` in their path, so the method gets sibling routes:

- `POST /v1/accounts/login/browser` — create a flow for an expected Matrix user ID.
- `GET /v1/accounts/login/browser/{flow_id}` — read the current stage and its presentation-safe data.
- `DELETE /v1/accounts/login/browser/{flow_id}` — cancel idempotently.

There is no input route: nothing is scanned and no check code exists.
There is no list route, and flows are not carried in WebSocket frames, as in ADR 0097.

Each route family sees only its own flows.
A browser flow's ID is `404` on every `/login/qr/{flow_id}` route, including `DELETE`, and a QR flow's ID is `404` on every `/login/browser/{flow_id}` route, so a client cannot drive or cancel a flow through the wrong surface.

The browser routes return their own flow DTO rather than the QR one, with their own stage enum:

- `starting`
- `waiting_for_authorization`
- `finalizing`
- `done`
- `failed`
- `cancelled`

`finalizing` covers everything after the token response: session installation, `whoami`, the wait for the lifecycle lock, and adoption.
A separate DTO keeps that stage out of the published QR stage enum, which existing clients match exhaustively, and omits the QR-only fields.

During `waiting_for_authorization`, and only then, the DTO carries the verification link and the user code.
The link is the authorization server's `verification_uri_complete` when it supplies one, so the user does not have to type the code, and the bare `verification_uri` otherwise.

Two failure codes are added for the browser presentation: `authorization_denied` when the user refuses at the authorization server, and `authorization_expired` when the device code or the flow TTL lapses before approval.
The remaining codes are reused as they are.
The QR presentations do not start emitting the new codes; their responses are unchanged by this decision.
`error_code` is an open string in the API, not an enum, and clients must present a code they do not recognize as a generic failure rather than reject the response.

The change is additive to `/v1/`.
The breadcrumb table's `presentation` check constraint gains the new value through a new forward-only migration; no published migration is edited.
The store is PostgreSQL, so the migration drops and re-adds the named check constraint in one transaction and does not rebuild the table.

Homeserver resolution matches the QR display presentation in what it takes: only the expected user ID, from whose server name the server discovers the homeserver.
It uses the resolver password login already has rather than the SDK's — the well-known document, then the server name itself, each confirmed as a homeserver, with plain HTTP only for a loopback name — so the method behaves like password login and can be tested against a loopback server.
_(Amended after code review: an account being reactivated uses the homeserver URL stored on its row instead, as password login does. Discovering it again would make an account reached only through an explicit URL unreachable by this method, or replace its stored URL at commit.)_

### 4. The account activates unverified, and says so

ADR 0097 fails a QR acquisition closed when the new device is not cross-signed, because QR login promises "sign in and verify".
Browser sign-in makes no such promise, so that gate does not apply to it; requiring it would make the method unusable.

A completed browser sign-in is therefore equivalent to a completed password login: the account is `active`, the device is not yet verified, and the API reports `verified: false`.
The user completes trust through the paths that already exist — recovery-key import or interactive SAS verification.
Finalization keeps every other ADR 0097 step: `whoami` must return the expected user ID, the per-identity lifecycle lock is held, a concurrent active login or deletion wins and the new session is revoked, and the session and staged store are adopted atomically before activation.

The trust check becomes a property of the acquisition method rather than of finalization as a whole, so the QR presentations keep failing closed.
Finalization takes the requirement as an explicit enum argument with no default — cross-signed or none — rather than a boolean or an optional value, so no call site can weaken the QR path by omission.

Clients must not describe a browser sign-in as verified, and should lead directly from a successful one into the recovery-key or verification step.

### 5. Treat the verification link and user code as secrets in transit

A device authorization can be approved by anyone who holds the link, so presenting it to the wrong person would sign that person's account in to this Axon.
Three properties bound that:

- The routes sit behind Axon's ordinary bearer-token gate, so only the Axon owner can create a flow or read its link.
- The expected user ID is fixed when the flow is created, and a token belonging to any other Matrix user fails identity validation and is revoked.
- The link and code are returned only while the flow is in `waiting_for_authorization`.
  The flow leaves that stage for `finalizing` the moment a token response arrives, which clears both, so neither is readable during finalization or from any terminal flow.

One risk remains and is accepted.
The link is a bearer capability, and the expected-user check catches only a _different_ Matrix user.
If the Axon owner passes the link to someone who can sign in as the same Matrix user, or it is read over their shoulder, that person can approve the device on the owner's behalf.
The result is still a session for the expected account on the device ID Axon generated, held only by this Axon, so nothing is disclosed to the approver; what is lost is the owner's own act of consent.
Clients should present the link as something to open, not something to share.

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

Unit and contract tests cover:

- the device authorization request shape and scopes, and that the staged store's device ID equals the one in the requested scope;
- the capability gate, including an endpoint advertised without the grant type, and that it is applied before any client is registered;
- rejection of an unsafe verification link in either field (`javascript:`, `data:`, plain HTTP on an HTTPS homeserver, userinfo, over-long) and of an over-long or non-printable user code, with nothing published;
- every row of the polling table, the wait before the first poll, a missing, zero, and long `interval`, repeated `slow_down` growing without a ceiling, a wait longer than the deadline, and the consecutive-failure budget;
- the thirty-minute TTL and a shorter `expires_in`, both ending as `authorization_expired`, and that the QR TTL is unchanged;
- oversized, non-JSON, and redirecting responses, and non-HTTPS endpoints on an HTTPS homeserver;
- cancellation during the poll, a Matrix user mismatch, a concurrent login, logout, or deletion, and crash reconciliation;
- that the link and user code are present in `waiting_for_authorization` and absent in `finalizing`, in every terminal stage, and from captured logs;
- that a browser flow is `404` on the QR routes and a QR flow is `404` on the browser routes, for reads and for cancellation; and
- that a second flow for the same expected user is a `409` across presentations.

A regression test pins that the QR presentations still fail closed on an unverified device.

The scope strings cannot be compared with the SDK's in a unit test, because the SDK's scope builder is private.
Drift is caught instead by the real lane below, which runs both acquisition methods against the same authorization server, and by a line in the matrix-sdk upgrade checklist to re-read those two constants.

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
It is rejected here because on an OAuth-delegating homeserver it exists only as a compatibility layer the operator may disable, it produces a legacy Matrix session on a code path separate from ADR 0097's OAuth session handling, and it needs the same redirect handling as the authorization-code flow.
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
- The acquisition engine serves three presentations, and the trust gate that was unconditional in ADR 0097 becomes a per-method property that tests must pin; ADR 0097 carries an amendment note saying so.
- Accounts acquired this way start unverified, so the clients' verification prompts become part of the sign-in experience for these users rather than an occasional follow-up.
- Homeservers that offer SSO without an OAuth authorization server remain unsupported except by token import.
- Steering a user to this method before they try a password is a separate change ([issue 691](https://github.com/matrix-axon/matrix-axon/issues/691)).
