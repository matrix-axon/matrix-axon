# Apple browser OAuth: implementation review and verification

This is the second server step of ADR 0054.
It enables credentialed browser login, CLI owner binding, and first-run bootstrap; it does not enable native Apple identity-token grants or distribute a signing key to self-hosters.
The operator has verified live Apple binding, web login, repeat login, and Google/Microsoft SSO regression checks.
Cancellation, bootstrap, and the remaining registered-deployment cases below still need live verification; this is not an App Store readiness claim.

## Configuration

Use an Apple Services ID associated with the appropriate primary App ID and register the deployment's exact HTTPS return URL with Apple.
The return URL is `oauth.external_base_url`, with trailing slashes removed, plus `/v1/oauth/apple/callback`.
An optional `oauth.providers.apple.redirect_uri` must equal that URL exactly.
Userinfo, query strings, fragments, noncanonical URLs, and non-HTTPS callbacks are rejected at startup and by the binding CLI.

Set `client_id`, `team_id`, and `key_id` under `[oauth.providers.apple]`.
Supply exactly one of `private_key` (PEM in protected configuration) or `private_key_path` (a regular PEM file, at most 16 KiB).
On Unix the file must have owner-only permissions, for example `chmod 600`.
Relative paths are resolved from the process working directory; use an absolute path when the service and CLI run from different directories.
Key-file I/O errors report the error category and whether relative-path resolution applies, without printing the configured path or key.
Then set both `oauth.enabled` and `oauth.providers.apple.enabled` to true.
Keep keys and real callback payloads out of shell transcripts, logs, screenshots, and test artifacts.

## Verification guide

Run these from the repository root:

```sh
cargo test -p axon-core apple_config_debug
cargo test -p axon-server oauth::tests::apple_
cargo test -p axon-api --lib oauth::
cargo test -p axon-api --test openapi
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo test --all
```

For transaction and handler coverage, set `DATABASE_URL` to a disposable, isolated Postgres database, then run serially:

```sh
cargo test -p axon-api --test oauth -- --ignored --test-threads=1
cargo test -p axon-store --test tokens -- --ignored --test-threads=1
cargo test -p axon-store --test oauth_unbind -- --ignored --test-threads=1
TMPDIR=/opt/adam/tmp scripts/smoke-gate.sh server
```

The database tests delete bootstrap-related rows and must never target a live Axon database.
Tests cover GET regression behavior, cookie-free form POST, signed mock-provider exchanges, cancellation/retry, malformed and oversized input, provider/state/purpose checks, expiry, concurrent completion, per-state POST rate limiting, and transactional bootstrap token issuance.
The smoke command needs Docker and creates its own local stack.
The generated web schema changes mechanically; no client behavior or demo scene changes are included.
Server-rendered callback failure pages are covered by the HTTP tests, not a live-provider demo recording.

### Registered Apple deployment acceptance (still required)

1. Start a throwaway Axon deployment with explicit database, sync, search, and media paths and the protected Apple configuration above.
2. Confirm `/v1/oauth/providers` lists Apple only when enabled.
3. Run `axon-server oauth bind --provider apple` using the same configuration as the running service.
   Open the one-time URL privately and complete Apple sign-in, including Hide My Email.
   Confirm binding completes and ordinary browser PKCE login can now mint Axon tokens.
4. Repeat sign-in without a first-login profile object; verify an unbound Apple subject is rejected rather than linked by email.
5. Cancel login and binding; confirm the client can retry and the binding CLI stops waiting.
6. On a separate empty deployment with explicit bootstrap capability, verify Apple bootstrap, cancellation/retry, and rejection of repeated callbacks.
   Bootstrap peer restrictions still apply; enable remote bootstrap explicitly when testing a remote browser.
7. Exercise Google/Microsoft login and refresh again, then remove the throwaway deployment and locally issued tokens.

Do not enable unattended Apple authorization revocation or persist upstream access/refresh tokens as part of this check.
Those lifecycle decisions remain follow-up work in ADR 0054.

### Unbinding regression check

After signing in, run `axon-server oauth identities list` and then `axon-server oauth identities unbind <identity-id>` for the test identity.
Unbinding atomically invalidates its Axon access tokens, refresh tokens, and outstanding authorization codes, and removes the identity.
Access-token audit rows remain revoked, with their identity reference cleared; other identities are unaffected.
Ordinary sign-in must now fail until you explicitly bind that Apple account again.
This is local Axon credential invalidation, not revocation of consent at Apple.
No migration or manual database cleanup is required, including after a previously failed unbind attempt.

### Authentication failure feedback

An unbound account now receives an instruction to ask the instance owner to bind it or use another account.
Cancellation, provider unavailability, and credential verification failures have distinct, application-owned descriptions.
The current web client already displays the returned OAuth `error_description`; no web change is required for these messages.
Invalid authorization codes and refresh tokens instruct the user to start sign-in again.
Rejected bearer tokens return the existing HTTP 401 and `invalid_token` challenge with instructions to sign in again or obtain a new token.
Unknown, expired, and revoked bearer tokens deliberately share one message.
The manual token-paste web form currently discards a rejected token without displaying the API error; displaying that feedback requires a separate web-client change.

At the normal warning log level, OAuth failures report controlled reasons and, for validated callback flows, the provider and flow type.
Verification diagnostics distinguish signature, issuer, audience, nonce, and token-time failures without printing attached upstream values.
Bearer rejections share a process-wide warning budget across HTTP and WebSocket authentication: at most one warning per 30 seconds, with further rejections available at debug level.
No credentials, callback state, authorization codes, email addresses, or subjects are included in these diagnostic events.
To verify, attempt ordinary login after unbinding, cancel a fresh sign-in, and submit an invalid test bearer token; check both the user-facing response and the server warning.
Do not capture real tokens or full callback URLs in test transcripts.

## Code review guide

1. `crates/axon-core/src/config.rs`: additive key-file configuration and redacted Debug output, including nested config.
2. `crates/axon-server/src/oauth.rs` and `main.rs`: shared startup/CLI construction, exact callback validation, bounded key reads, and provider registration.
3. `crates/axon-store/src/oauth_authorization_requests.rs`, `oauth_bind_requests.rs`, and `tokens.rs`: conditional cancellation, CLI-created immutable bind nonce, and atomic bootstrap flow/token transaction.
4. `crates/axon-api/src/oauth/mod.rs` and `rate_limit.rs`: shared callback resolver and bounded POST-state throttling.
5. `crates/axon-api/src/routes/oauth.rs`, `bootstrap.rs`, and router wiring: one exchange/verification path, flow-purpose validation before dispatch, sanitized failure delivery, and no-cache/no-referrer callback responses.
6. OAuth HTTP/store tests, startup/config tests, OpenAPI, generated schema, and operator docs.

For the unbinding follow-up, review `crates/axon-store/src/oauth_identities.rs` before its CLI caller and `crates/axon-store/tests/oauth_unbind.rs`.
Check transaction rollback, the identity row lock against concurrent credential insertion, and invalidation of both issued codes and refresh-token rotation chains.

Keep a close eye on the boundaries between server-stored state and browser-supplied fields, the single-use transaction guards, the absence of secrets in diagnostics, and the continued refusal of Apple's nonce-free native grant.
No migration is needed: canceled flows use the existing terminal `expired` status.
Pending bind URLs can be reloaded or prefetched without changing their nonce; successful completion or cancellation remains single-use.
Apple's `user_cancelled_authorize` error is treated as cancellation, while unknown provider errors receive neutral retry guidance.
An exchange failure terminates the current flow because its upstream code may already have been consumed; rerun `oauth bind` to obtain a new URL.
If the process stops before completion, no credential is committed; start a new flow after restart.
If completion committed but the response was lost, the old flow remains single-use and the owner must start a fresh attempt.
