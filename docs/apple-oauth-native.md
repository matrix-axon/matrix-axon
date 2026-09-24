# Apple native OAuth server contract

This is the final core-server layer of ADR 0054, stacked on the browser integration.
It does not ship native client UI, Apple authorization revocation, or account deletion, and does not by itself establish App Store readiness.

## Configuration and discovery

Enable OAuth, set the externally selected Axon HTTPS base URL, register the native public client in `[[oauth.clients]]`, and configure:

```toml
[oauth]
enabled = true
external_base_url = "https://axon.example"

[[oauth.clients]]
client_id = "axon-native"
redirect_uris = []

[oauth.providers.apple]
enabled = false
native_enabled = true
native_audiences = ["org.matrixaxon.axon"]
```

Use the actual bundle ID of the app with the Sign in with Apple entitlement, not a Services ID.
Native verification requires no team ID, key ID, private key, or browser callback registration.
Browser sign-in remains independently configured with `enabled = true`.
The native client must use the same selected Axon HTTPS instance throughout a flow and never forward credentials across redirects or server changes.

`GET /v1/oauth/providers` retains the browser-provider list for existing clients.
`GET /v1/oauth/providers?flow=native` lists native-capable providers, including native-only Apple.
Each provider includes additive `browser` and `native` capability booleans.
No Apple entry is returned for a disabled flow.
Only Apple currently advertises native SDK support.
The legacy Google/Microsoft identity-token grant still requires the configured browser audience; it does not establish native SDK audience support.
When both Apple flows are enabled, they share one JWKS cache while retaining independent audience validation and browser-only signing credentials.

## Challenge and redemption

Both POST endpoints accept URL-encoded forms and return JSON.
Responses have `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.
Do not log, save, attach to crash reports, or put credentials in URLs or command history.

1. POST `/v1/oauth/apple/native/challenge` with `client_id` and `purpose=login`.
   The client ID must be registered.
   The response is `{challenge, nonce, expires_in: 300}`.
2. Keep `challenge` in memory, separate from the Apple request.
   Set `ASAuthorizationAppleIDRequest.nonce` to the returned `nonce` **verbatim**.
   It is already a base64-encoded SHA-256 digest of independent server randomness; do not hash it again or substitute a client-generated expectation.
   Apple's [nonce property](https://developer.apple.com/documentation/authenticationservices/asauthorizationopenidrequest/nonce) is the string passed to the identity provider.
3. After Apple succeeds, POST `/v1/oauth/apple/native/token` with `client_id`, `challenge`, and `identity_token`.
   The response has the existing OAuth token shape: `{access_token, token_type, expires_in, refresh_token}`.
   Existing `/v1/oauth/token` refresh behavior is unchanged.
4. On cancellation, timeout, restart, or a lost redemption response, discard local flow state and start a new challenge and Apple authorization.
   No native flow needs a recovery journal or saved upstream token.

The server retrieves its expected nonce, purpose, client, and instance from durable flow state.
It checks Apple's signature, issuer, native audience, time claims, and that exact signed nonce.
Normal login only accepts an already-bound Apple subject.
Email, relay address, and unsigned profile data never establish or merge ownership.
The old nonce-free `urn:axon:identity_token` grant still refuses Apple.
Google/Microsoft keep their existing wire contract, with replay and token issuance now atomic.

## Explicit owner binding

Use `purpose=bind` and the existing owner's `Authorization: Bearer` header at both steps.
The same bearer must still be active when the transaction redeems the challenge.
Success binds the verified Apple subject and returns its new access/refresh pair.
There is no unauthenticated first-user claim.
As specified by ADR 0054, an active owner session includes an OAuth-issued bearer, not just a CLI-issued token.
These are full-owner credentials: a stolen short-lived bearer can authorize a persistent identity binding that survives its expiry or revocation.
Operators must revoke unauthorized identities as well as compromised sessions.
Requiring local approval, step-up authentication, or a separate binding privilege would be a separate authorization-policy change, not a restriction implemented by this endpoint.

For a genuinely empty instance, the operator may explicitly arm the existing first-run bootstrap capability.
Use `purpose=bootstrap` and `bootstrap_code` at both steps.
Existing loopback/allow-remote restrictions and wrong-code lockout apply.
The capability must match the one used to create the challenge and the instance must still have no accounts, credentials, or bound identities at commit.
Concurrent native/browser/bearer bootstrap attempts share the first-credential lock; only one can succeed.
Bootstrap is intentionally unavailable after credentials have existed, even if later revoked.
The short access code remains memory-only: the stored authority hash derives from an independent high-entropy bootstrap-session binding, never from the code.
Restarting bootstrap invalidates pending flows even if the same access code is reused.

## Failure, bounds, and diagnostics

Unknown clients or malformed forms are rejected before upstream verification.
Challenge creation uses the existing API error envelope; token validation failures use OAuth `error` and `error_description`, including retry guidance and unbound-identity feedback.
Invalid or revoked binding authority is a non-redeemable grant, never implicit authorization.
Logs use allowlisted verification categories; raw Apple tokens, challenge capabilities, bootstrap codes, and upstream responses are not diagnostics.
Database failures include an allowlisted kind distinguishing pool exhaustion, lock timeout, cancellation, deadlock, and constraint failures without SQL details.
Successful bind/bootstrap transactions emit an informational audit event identifying the provider and purpose, never credentials or subjects.
No new debug option is needed to expose secret-bearing data.

Flows expire after five minutes.
Creation prunes expired rows and serializes separate caps of 1,024 public login flows and 64 authorized bind/bootstrap flows.
Unauthenticated login traffic cannot consume the authorized reserve.
The store's TTL constant controls both inserted expiry and the advertised lifetime.
The existing OAuth IP/key limiter, bounded form buffering, ten-second body and upstream timeouts, and five-second database lock timeout bound each boundary.
Database transactions have ten-second statement timeouts.
Challenge claim, binding (where authorized), replay insertion, and both credential hashes commit together.
A failed mint rolls all of them back; concurrent redemption mints at most one pair.
Unbinding and bearer revocation serialize against relevant identity/token locks.
An aborted transaction may require retry, but cannot leave a partially issued pair.

## Verification guide

Prerequisites: Rust toolchain, Docker, and an isolated disposable Postgres 16 database.
Never run the DB suites on a deployed database: bootstrap tests clear credential tables.
Set `DATABASE_URL` to the disposable database, then run:

```sh
cargo test -p axon-api --lib oauth::apple
cargo test -p axon-api --test oauth -- --ignored --test-threads=1
cargo test -p axon-store --test oauth_native -- --ignored --test-threads=1
cargo test -p axon-store --test oauth_unbind -- --ignored --test-threads=1
cargo test -p axon-api --test openapi
cargo test -p axon-server native_only
```

The API suite exercises native-only discovery, nonce rejection, explicit binding, revoked/mismatched owners, bootstrap, replay, and usable issued bearers, alongside existing browser and Google/Microsoft behavior.
Store tests force a final-insert failure to check rollback, race legacy redemptions, and exercise flow bounds, context, and expiry.
Public-key tests use ephemeral signing fixtures, not real Apple credentials.
Run the existing `scripts/smoke-gate.sh server` lane as a regression check; it is not native-device acceptance.

Real Apple acceptance remains required with the subsequent native client: entitled device login, Hide My Email, repeat login with no profile object, cancellation, restart, refreshed Axon session, and App ID/Services ID grouping.
If Apple subjects differ between native and browser registrations, bind each explicitly; never repair it by email matching.

## Code review guide

Review the migration and `axon-store/src/oauth_native.rs` first, then the shared transactional mint helper in `tokens.rs`.
Next inspect the keyless Apple verifier, runtime construction, native HTTP handlers, and provider discovery.
Finish with the store/API tests, OpenAPI contract, and configuration examples.
Keep a close eye on authorization rechecks, rollback after the last insert fails, nonce provenance, instance/client/purpose binding, and sanitized failures.
The diff must not introduce native client behavior or retain upstream credentials.
