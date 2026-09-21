# Apple browser OAuth internal review follow-up

Response to the internal review of the three local Apple browser OAuth commits.
All changes remain in the server silo except documentation and the mechanically generated web API schema.

## Findings

1. **Bind URL reloads and prefetches:** generate the immutable nonce when inserting the CLI bind request, before printing its URL.
   GET and HEAD only read and redirect; repeated requests return the same state and nonce.
   Completion and cancellation remain single-use.
   Legacy pending rows without a nonce require a fresh CLI attempt, not a migration.
2. **Apple cancellation:** recognize `user_cancelled_authorize` for Apple, as documented in [Apple's authorization endpoint reference](https://developer.apple.com/documentation/signinwithapplerestapi/request-an-authorization-to-the-sign-in-with-apple-server.).
   Unknown provider errors receive neutral retry guidance, not an outage diagnosis.
3. **Callback hardening:** the HTML/no-store/no-referrer response layer now wraps the rate limiter as well as the handler.
   Regression tests cover body-size rejection, per-state and per-IP throttling, and a stalled request body.
4. **Bearer warning amplification:** one fixed-size, process-wide warning slot is shared by HTTP and WebSocket rejection paths.
   It emits at most one warning per 30 seconds; additional events are debug-only.
   A concurrent test covers contention and reopening the warning window.
5. **OpenAPI:** document GET query parameters and POST form input separately, both with HTML failure responses.
   Regenerate the spec and web schema; a dedicated test asserts both methods and media types.
   ADR 0054 records the existing GET error-envelope change.
6. **Retry messaging:** the CLI reports cancellation, failure, or expiry without guessing which occurred, and explicitly requests a new bind attempt.
   Provider-error text also specifies a new attempt; an upstream exchange cannot safely be resumed after its code may have been consumed.

## Smaller findings

- **Key-file diagnostics:** include `io::ErrorKind` and a relative-path/working-directory hint.
  Keep paths redacted deliberately: configuration can accidentally put sensitive material in a path, and paths are not needed to distinguish missing files from permission errors.
- **Callback URL construction:** use one pure URL helper shared by startup validation and `OAuthRuntime`, without allocating a temporary runtime or limiters.
- **Bind nonce invariant:** carry the validated nonce in the `CallbackFlow::Bind` variant; remove the empty-string fallback.
- **Diagnostic tests:** assert every constructed OIDC error category, not a subset.
- **Cross-crate fixture:** move the shared in-memory EC key generator to the dev-only `axon-test-support` crate; remove the cross-crate test-file include.
- **Transaction cleanup:** explicitly roll back the missing-identity branch of unbind.
- **Bootstrap separation:** reject the reserved bootstrap client in ordinary login even if operator configuration registers it; add a regression test.

## Verification

Use the commands in [the verification guide](apple-oauth-browser.md#verification-guide), plus:

```sh
cargo test -p axon-api --lib auth::tests
cargo test -p axon-api --lib routes::oauth
pnpm --dir clients/web check:api
```

Live cancellation and bootstrap acceptance remain operator follow-ups; unit and mock-provider tests do not replace those checks.
