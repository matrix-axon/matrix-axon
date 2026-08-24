# ADR 0097 — Matrix OAuth QR login and device authorization

## Context

Axon can currently add a Matrix account with a password or imported access token, then make its Matrix device trusted through recovery-key import or interactive SAS verification.
That split is awkward for accounts whose homeserver delegates authentication to an OAuth 2.0 authorization server such as Matrix Authentication Service (MAS): a password may not exist, an imported access token is a poor long-lived session, and ordinary OAuth login authenticates a device without cross-signing it or transferring the user's end-to-end encryption secrets.

[MSC4108](https://github.com/matrix-org/matrix-spec-proposals/pull/4108) combines the OAuth 2.0 Device Authorization Grant with a QR-established secure channel.
It lets an existing trusted Matrix device participate in the new device's login, transfer the private cross-signing and backup secrets, and leave the new device verified.
At the time of this decision MSC4108 remains an open Matrix proposal, so this is an intentionally capability-gated interoperability feature rather than a protocol Axon may assume every OAuth deployment or Matrix client supports.

matrix-rust-sdk 0.18 already exposes both sides of this flow:

- `OAuth::login_with_qr_code()` lets a new Axon Matrix device acquire a session and trust from an existing device.
- `OAuth::grant_login_with_qr_code()` lets an already trusted Axon Matrix device authorize and provision a new device, including another client such as Element X.

The second direction matters independently of Axon's own onboarding.
Axon is persistent and may become the user's most reliably available trusted Matrix device, so it should eventually be able to bootstrap another Matrix client rather than only consume trust from one.

Three existing decisions constrain the design.

- **ADR 0054 uses OAuth in the opposite direction.**
  There Axon is an authorization server to Axon clients and a relying party to Apple, Google, or Microsoft.
  Here Axon is a Matrix OAuth client to the homeserver's advertised authorization server.
- **ADR 0022 owns account lifecycle and serialization.**
  A completed login must activate exactly one account identity under the existing per-identity lock, and must not race logout, deletion, or another login.
- **ADR 0027 owns SDK-facing verification.**
  Matrix OAuth QR login and grant logic belongs in `axon-sync` beside `ClientManager`, `IdentityLocks`, and supervised account tasks.
  `axon-crypto` remains a reserved stub.

## Decision

### 1. Name the three different trust boundaries explicitly

Project-owned APIs, types, documentation, and logs use these terms consistently:

- **Axon OAuth** means an Axon client authenticating to Axon's `/v1/` API under ADR 0054.
- **Matrix OAuth** means Axon authenticating a Matrix device to a homeserver's advertised OAuth 2.0 authorization server.
- **Device verification** means Axon's Matrix device is cross-signed by the account owner and possesses the secrets needed to participate as a trusted encrypted Matrix client.

“MAS login” may be used in operator-facing examples because MAS is the reference implementation Axon will test against, but MAS is not hard-coded as a server type or API dependency.
Axon discovers and validates the standard Matrix authentication metadata advertised by the homeserver and capability-gates the QR option on the MSC4108 information required by matrix-rust-sdk.

This feature verifies a Matrix **device**, not the human's account as a separate protocol operation.
The user-visible outcome “sign in and verify this account” means that Axon has authenticated a new session for that Matrix account and confirmed that its new device is cross-signed.

### 2. Support both MSC4108 roles at the server boundary

The server design supports two separate roles.

#### Acquire trust: another device authorizes Axon

Axon is the new Matrix device and drives matrix-rust-sdk's QR login flow.
A trusted existing client, such as Element X, participates in the secure rendezvous, approves the authorization server's device request, and transfers the encryption secrets.
On success Axon persists the OAuth session and starts the account only after independently deriving that its own device is verified.

This is a new account-login path, not an extension of the existing SAS endpoints.
It performs authentication and key acquisition as one flow and therefore does not require a Matrix password, imported access token, or recovery key.

#### Grant trust: Axon authorizes another device

Axon is the existing trusted Matrix device and drives matrix-rust-sdk's grant flow.
The new device may belong to another Matrix client, including Element X.
Axon participates in the secure rendezvous, directs the user to the authorization server's verification URI for explicit authorization, and transfers the secrets only after the protocol has confirmed the channel and authorization.

The Axon **server** performs this role because it owns the Matrix SDK session, crypto store, and exportable secrets.
An Axon web or terminal client is only a presentation and input surface; it never receives the exported secrets or a Matrix access or refresh token.

Granting login is a distinct implementation slice after Axon can acquire and restore Matrix OAuth sessions.
It is not part of the first acquire-flow server PR because it has different preconditions, threat consequences, and interoperability tests.

### 3. QR presentation is symmetric; a camera is not a server requirement

Both roles support the two presentations exposed by matrix-rust-sdk:

- **Display:** Axon generates opaque QR data for the other device to scan, then asks the user to enter the short check code displayed by that device.
- **Scan:** an Axon client scans QR data generated by the other device and submits the decoded, bounded payload to the server; Axon returns the short check code that the client must display to the user.

The normal first-device-to-second-device experience is expected to use the first device's camera to scan a QR code displayed by the new device.
That makes the web client the useful Axon grant surface: it can access a camera or accept an image while the Axon server retains all Matrix credentials and secrets.

Terminal support for **acquiring** trust remains planned because a terminal can render a QR code and accept a check code without a camera.
A TUI surface for **granting** trust is deferred with no display-only follow-up in the initial plan.
The server API will not forbid a future TUI consumer, but no TUI grant UI or parity claim is part of this decision's delivery sequence.

### 4. Expose replayable HTTP flow resources

QR login begins before an Axon Matrix `account_id` exists, while every current `/v1/ws` frame requires an `account_id`.
The authoritative interface is therefore an HTTP flow resource that clients poll, not a new exception in the WebSocket envelope.
The routes remain behind Axon's ordinary bearer-token gate; “pre-account” refers to the Matrix account row, not to Axon owner authentication.

The acquire surface is:

- `POST /v1/accounts/login/qr` — create a flow with `presentation = display | scan` and the expected Matrix user ID.
- `GET /v1/accounts/login/qr/{flow_id}` — read the current stage and its presentation-safe data.
- `POST /v1/accounts/login/qr/{flow_id}/scan` — provide one decoded QR payload for a scan flow.
- `POST /v1/accounts/login/qr/{flow_id}/check-code` — provide one check code for a display flow.
- `DELETE /v1/accounts/login/qr/{flow_id}` — cancel idempotently.

The later account-scoped grant surface mirrors it:

- `POST /v1/accounts/{account_id}/login-grants/qr` — create a grant flow with `presentation = display | scan`.
- `GET /v1/accounts/{account_id}/login-grants/qr/{flow_id}` — read the current stage and its presentation-safe data.
- `POST /v1/accounts/{account_id}/login-grants/qr/{flow_id}/scan` — provide one decoded QR payload for a scan flow.
- `POST /v1/accounts/{account_id}/login-grants/qr/{flow_id}/check-code` — provide one check code for a display flow.
- `DELETE /v1/accounts/{account_id}/login-grants/qr/{flow_id}` — cancel idempotently.

The flow DTO uses stable Axon stages rather than exposing matrix-rust-sdk enums directly:

- `starting`
- `qr_ready`
- `check_code_to_display`
- `check_code_required`
- `waiting_for_authorization`
- `syncing_secrets`
- `done`
- `failed`
- `cancelled`

Only the stage-appropriate field is returned: QR data, a check code to display, an authorization user code, a verification URI, the completed account, or a stable error code.
QR and check-code inputs are single-use, size- and shape-validated before parsing, and rejected when they do not match the flow's presentation or current stage.
Terminal states remain readable for a short bounded grace period, then the flow expires.

The grant flow never silently approves a new device.
It returns the authorization server's verification URI and waits for the user to open it and explicitly authorize the device there.

### 5. Make Matrix authentication kind part of the stored session

The current account store can restore only a legacy Matrix access-token session and deliberately sets `refresh_token: None`.
Matrix OAuth access tokens are short-lived, so OAuth session support must land before a QR login can be considered successful.

A forward-only migration will extend account session storage with:

- an authentication kind that distinguishes legacy Matrix authentication from Matrix OAuth;
- an encrypted OAuth refresh token alongside the encrypted access token;
- the OAuth client ID needed to restore the SDK session; and
- enough non-secret session metadata to reconstruct matrix-rust-sdk's `OAuthSession` for the existing Matrix user and device.

Existing account rows default to the legacy authentication kind and continue to restore exactly as they do today.
No published migration is edited.

OAuth client registration is stored separately from a user session and keyed by the discovered issuer or homeserver identity.
Dynamic client registration is the normal path when advertised.
Operator-provided static client IDs are the fallback for deployments that do not permit dynamic registration.
Registration metadata is shared safely across accounts, while user access and refresh tokens remain account-scoped and encrypted under Axon's store key.

`ClientManager` restores OAuth accounts with `client.oauth().restore_session(...)` and legacy accounts with `client.matrix_auth().restore_session(...)`.
Each OAuth account run supervises a session-change persister that subscribes before the session can refresh, reads `client.oauth().full_session()` after a token-change signal, and writes the replacement access token, refresh token, and client ID in one encrypted database update.
Persistence is serialized per account, retries the newest full snapshot with bounded backoff, reports degraded session durability, and gets a bounded shutdown flush so an older snapshot cannot overwrite a newer rotation.
A provider may invalidate a rotated refresh token before any local process can durably record its replacement, so a crash in that remote/local gap can still require a fresh QR login; the implementation and tests must make that residual failure explicit rather than silently restoring a known-stale token forever.
Logout uses the matching authentication implementation and makes upstream revocation best-effort, bounded, and observable without leaking token material.

### 6. Finalize acquisition under the existing lifecycle lock

An acquire flow uses a new matrix-rust-sdk client and a staging SDK store until the remote protocol has completed.
Nothing writes an active Axon account row merely because the authorization server issued tokens.

After matrix-rust-sdk reports success, Axon:

1. calls `whoami` with a timeout and requires its canonical Matrix user ID to equal the flow's expected user ID;
2. acquires that identity's existing lifecycle lock;
3. re-reads account state under the lock and rejects a concurrent active login or deletion according to ADR 0022;
4. derives the device's cross-signing state from `get_own_device().is_cross_signed_by_owner()` rather than trusting a protocol stage name;
5. persists the OAuth session and atomically adopts the staged SDK store into the account's permanent location; and
6. activates and supervises the account through the existing lifecycle path.

If the identity became active concurrently, Axon revokes or discards the new OAuth session, removes the staging store, and returns a stable conflict rather than replacing the live device.
If the QR protocol completes but the device is not actually cross-signed, the flow fails closed and does not claim the combined “sign in and verify” outcome.

The staged-store adoption and account activation form a crash-safe multistep operation.
A durable, non-secret cleanup breadcrumb names the flow and staging location before work starts; boot reconciliation removes abandoned staging state or completes an idempotent finalization whose database state already committed.
No access token, refresh token, QR payload, check code, secrets bundle, or verification URI is written to the flow record.

### 7. Grant only from a currently trusted account

Creating a grant flow requires an `active` account whose own Matrix device is actually cross-signed and whose SDK crypto store can export the required secrets bundle.
The persisted `verified` field is a useful API signal but is not sufficient authorization by itself; `axon-sync` re-derives the SDK state before creating the flow.

The flow holds the account's current supervised client without blocking sync for its whole lifetime.
Immediately before secrets are released, it rechecks under the per-identity lock that the account is still active, the client still belongs to the current run, and the device is still trusted.
Logout, deletion, client eviction, trust loss, or cancellation invalidates the grant and prevents secret transfer.

Only one acquire flow may target an expected Matrix user ID at a time, and only one grant flow may target an active account at a time.
The registry names its owner and cancellation token explicitly, and every terminal transition consumes the handle so concurrent completion, timeout, and cancellation cannot each act on it.

### 8. Treat every flow boundary as hostile and failure-prone

All rendezvous, homeserver discovery, OAuth registration, token, `whoami`, revocation, and device-creation calls have connect and total timeouts.
The server bounds concurrent flows globally and per identity, QR payload bytes, decoded fields, registration metadata, response bodies, retries, and terminal retention.
A flow has an overall TTL and cancellation propagates into its matrix-rust-sdk driver.

One account or flow failing is logged and isolated rather than terminating sync or another account's flow.
Logs include `flow_id`, role, presentation, `account_id` when one exists, and stable stage/error classifications.
Logs and errors never include access tokens, refresh tokens, QR payloads, check codes, authorization user codes, authorization URLs containing user codes, or encryption secrets.

MSC4108 compatibility is enabled only when discovery and the pinned SDK expose the required capability.
Before enabling it by default, Axon must pass the real interoperability lanes below against the supported MAS, matrix-rust-sdk, and Element X versions.
An unsupported or incompatible deployment reports that the QR method is unavailable and leaves password, token import, recovery, and SAS behavior unchanged.

### 9. Deliver this as reviewable, single-silo pull requests

The implementation sequence is:

1. **ADR:** this docs-only decision, merged before implementation begins.
2. **Server session foundation:** schema, encrypted OAuth refresh state, client registration, restoration, refresh rotation, logout, and migration/backward-compatibility tests.
3. **Server acquire flow:** `axon-sync` driver, lifecycle finalization, `/v1/` acquire routes and DTOs, OpenAPI regeneration, and focused tests.
4. **Web acquire UI:** camera/image scan and QR display paths, polling, check-code handling, cancellation, and failure recovery.
5. **TUI acquire UI:** QR display, check-code entry/display, polling, and cancellation without assuming a camera.
6. **Server grant flow:** the separate account-scoped driver and `/v1/` grant routes, including trust rechecks and explicit authorization-server approval.
7. **Web grant UI:** camera/image scan and QR display paths for authorizing clients such as Element X.
8. **Smoke and integration coverage:** black-box API coverage plus real Matrix OAuth interoperability lanes.

Each code PR stays within its server, web, TUI, or smoke silo, with directly related documentation in the same PR.
The server grant PR is deliberately separate and later than acquire because authorizing another device exports the user's highest-value encryption material and deserves an independent security review.
TUI grant UI, including a display-only variant, is not in this sequence and remains deferred until a concrete terminal use case justifies it.

### 10. Verification required before calling the feature complete

The server session foundation tests:

- legacy account migration and restart with no behavior change;
- OAuth discovery, issuer validation, dynamic and static client registration, session restore, access-token refresh, refresh-token rotation, restart after rotation, and logout/revocation failure; and
- atomic persistence under concurrent refresh and lifecycle operations.

The acquire and grant unit/contract tests cover every stage, both QR presentations, one-shot QR/check-code consumption, malformed and oversized input, timeout, cancellation, terminal retention, unsupported capability, Matrix user mismatch, trust derivation failure, concurrent login, logout/delete races, client eviction, and crash reconciliation.

A real Synapse plus MAS integration lane proves that a trusted existing device can authorize Axon, Axon becomes cross-signed, encryption secrets arrive, encrypted history decrypts, and the OAuth session refreshes after restart.
A separate interoperability lane proves that a trusted Axon account can authorize a fresh Element X or SDK device, the new device becomes trusted, receives the secrets bundle, and cannot complete without the user's explicit authorization.

## Rejected alternatives

### Treat ordinary Matrix OAuth login as verification

Rejected because OAuth authentication establishes a Matrix session but does not by itself cross-sign the device or transfer the user's encryption secrets.
It would leave Axon authenticated yet unable to claim the user-visible “verified” outcome.

### Make MAS a first-class server type

Rejected because the Matrix authentication protocol defines discovery and OAuth behavior without requiring clients to depend on MAS-specific APIs.
MAS remains the tested reference implementation, not a branch in Axon's domain model.

### Run the grant protocol in the Axon web or TUI process

Rejected because Axon clients authenticate to Axon, not to the homeserver, and do not own the Matrix crypto store or exportable cross-signing secrets.
Moving the protocol client-side would either duplicate Matrix state or expose secrets across the `/v1/` boundary.

### Extend the SAS verification endpoints

Rejected because SAS verifies an already authenticated Matrix device, while MSC4108 couples login, device authorization, a secure rendezvous, and secret transfer.
Sharing the SDK ownership and lifecycle machinery does not make them the same API resource.

### Add WebSocket commands or account-less WebSocket frames

Rejected because HTTP polling already provides a replayable source of truth, while an account-less frame would weaken the existing envelope invariant and a client-to-server WebSocket command channel would duplicate the HTTP operations.

### Ship server acquire and grant in one PR

Rejected because the acquire path creates Axon's own session while the grant path releases the account's encryption secrets to a new device.
Their preconditions, failure modes, and review focus differ enough to warrant separate server PRs.

### Plan a TUI grant surface now

Rejected for the initial delivery sequence because the typical trusted-device experience uses a camera, which a terminal usually cannot access directly, while the web client can provide both camera scan and QR display.
The API remains presentation-neutral if a real display-only terminal use case appears later.

## Consequences

- Axon can eventually use any compatible Matrix OAuth authorization server, with MAS as the supported test target, to sign in and become a verified Matrix device without handling a password or recovery key.
- A trusted Axon installation can eventually authorize a different Matrix client such as Element X, but only through the separately reviewed grant slice and an explicit user authorization step.
- OAuth session restoration and refresh become account-lifecycle infrastructure used by both directions, rather than QR-specific token handling.
- The HTTP flow resource gives disconnected clients a replayable state without changing the WebSocket envelope.
- Supporting an unstable MSC creates a compatibility burden, so capability detection and pinned real-world interoperability tests are release gates rather than optional follow-ups.
- TUI acquisition remains useful without a camera, while TUI grant presentation is intentionally absent from the initial plan.
