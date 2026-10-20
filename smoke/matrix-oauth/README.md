# Matrix OAuth QR and browser sign-in interoperability

`axon-smoke-matrix-oauth` is the ADR 0097 and ADR 0117 black-box and real interoperability harness.
It drives the shipped `axon-server` only through `/v1/`; its separate matrix-rust-sdk client plays an existing trusted device or a fresh third-party device.
The harness depends on no Axon product crate.

## Run the lanes

Prerequisites are Rust, Docker with Compose v2, `curl`, Python 3, and `sed`.
Each command starts pinned Synapse 1.160.0, MAS 1.24.0, and Postgres containers on automatically selected localhost ports, then tears them down.
The trusted fixture signs in through MAS's Matrix password-login endpoint, then restores the resulting session in the independent SDK client pointed at Synapse.
Synapse's delegated-auth listener does not serve password login itself.
MAS 1.24.0 serializes that path with device reconciliation and commits the session before creating the Synapse device.
The `issue-compatibility-token` CLI creates the device before committing its session, allowing concurrent user provisioning to remove it and leave the fixture returning HTTP 401.

```sh
scripts/matrix-oauth-test.sh api
scripts/matrix-oauth-test.sh acquire
scripts/matrix-oauth-test.sh grant
scripts/matrix-oauth-test.sh browser
scripts/matrix-oauth-test.sh unsupported
```

Synapse's server name is `localhost:<published port>`, so the test user is `@alice:localhost:<port>`.
Browser sign-in finds the homeserver from the user ID alone, and a bare `localhost` would send that lookup to a port where nothing listens.

Set `TMPDIR` to a filesystem with enough room for the throwaway configuration and SDK stores when `/tmp` is constrained.

In CI the lanes run after every push to `main` and nightly from `smoke.yml`, and can be dispatched by hand from either `integration.yml` (no notification) or `smoke.yml` (posts the result to the axon-developer room).

The lanes cover:

- `api`: authenticated acquisition boundaries, invalid identity, idempotent cancellation and bounded identity-release recovery, explicit authorization rejection, account-scoped grant isolation, malformed QR redaction, grant cancellation, and real rendezvous expiry;
- `acquire`: a trusted SDK device authorizes Axon, Axon becomes cross-signed, receives backup material, decrypts history created before login, then restores and refreshes its OAuth session after restart beyond the 60-second test access-token lifetime;
- `grant`: a trusted Axon account authorizes a fresh SDK device only after explicit MAS approval, and the new device becomes cross-signed, activates the transferred backup secret, and decrypts the seeded history;
- `browser`: a browser sign-in (ADR 0117) that MAS's device page rejects ends as `authorization_denied` and is unreachable through the QR routes; an approved one leaves an active account that is not yet verified, the recovery key then verifies it and Axon decrypts history created before sign-in, and the session restores and refreshes after a restart beyond the 60-second test access-token lifetime; and
- `unsupported`: MAS omits the device-authorization capability, and both QR acquisition and browser sign-in terminate with the stable `unsupported` classification.

The integration workflow exposes each lane separately and provides `matrix-oauth-all` to run all five manually.
The smoke workflow runs all five in a separate job after every push to `main` and on its nightly schedule.
Both multi-lane jobs attempt every lane after a failure and report the aggregate result, so one run exposes all broken scenarios it reaches within the job budget.
These expensive, unstable-protocol lanes remain outside the pull-request path.

## Secret handling

Runtime access tokens, refresh tokens, QR payloads, check codes, authorization user codes and URLs (including browser sign-in's link and code), recovery material, and exported secret bundles must never enter diagnostics or retained artifacts.
The launcher passes the fixture password through the environment, and the harness retains the resulting trusted-device access token and other known runtime protocol values in memory for disclosure checks.
The Axon bearer token is passed without command-line arguments.

Axon's stdout and stderr are captured inside a randomly named throwaway run directory.
Every lane outcome is followed by a check that rejects secret-bearing field names or known runtime values in that log.
If both the lane and disclosure check fail, the harness reports both stable errors without printing the matched runtime value.
SQL statement text is excluded from the captured log so a column name cannot masquerade as a disclosure; a failing field-name check identifies the safe field name that matched.
The harness handles SIGINT and SIGTERM so its Axon child is killed and reaped before the launcher's cleanup trap removes the directory, SDK stores, containers, databases, and MAS configuration volume.
An intentional restart first gives Axon 45 seconds to complete its graceful shutdown, covering the server's 30-second sync-engine drain budget plus surrounding teardown, before the harness forcibly kills and reaps it.
This cleanup applies on success, failure, cancellation, and cooperative interruption; SIGKILL cannot run process cleanup.
The workflow deliberately uploads no failure artifacts for these lanes.

Harness errors name only a stable phase or classification.
The encrypted-history wait logs changes between fixed classifications: request failure, HTTP status, invalid response, missing event, encrypted event, redacted event, unexpected event type, body mismatch, and decrypted history.
It matches the seeded event ID as well as its expected plaintext, and reports poll and API-failure counts plus whether the event was ever observed or encrypted when its existing deadline expires.
These observations distinguish an unavailable timeline from a missing event or stalled decryption without printing event IDs, bodies, or raw errors.
Do not add raw HTTP bodies, SDK errors, OAuth errors, container configuration, or URLs to failure messages.
