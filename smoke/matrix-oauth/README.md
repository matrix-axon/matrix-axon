# Matrix OAuth QR interoperability

`axon-smoke-matrix-oauth` is the ADR 0097 black-box and real interoperability harness.
It drives the shipped `axon-server` only through `/v1/`; its separate matrix-rust-sdk client plays an existing trusted device or a fresh third-party device.
The harness depends on no Axon product crate.

## Run the lanes

Prerequisites are Rust, Docker with Compose v2, `curl`, Python 3, and `sed`.
Each command starts pinned Synapse 1.160.0, MAS 1.24.0, and Postgres containers on automatically selected localhost ports, then tears them down.

```sh
scripts/matrix-oauth-test.sh api
scripts/matrix-oauth-test.sh acquire
scripts/matrix-oauth-test.sh grant
scripts/matrix-oauth-test.sh unsupported
```

Set `TMPDIR` to a filesystem with enough room for the throwaway configuration and SDK stores when `/tmp` is constrained.

The lanes cover:

- `api`: authenticated acquisition boundaries, invalid identity, idempotent cancellation and bounded identity-release recovery, explicit authorization rejection, account-scoped grant isolation, malformed QR redaction, grant cancellation, and real rendezvous expiry;
- `acquire`: a trusted SDK device authorizes Axon, Axon becomes cross-signed, receives backup material, decrypts history created before login, then restores and refreshes its OAuth session after restart beyond the 60-second test access-token lifetime;
- `grant`: a trusted Axon account authorizes a fresh SDK device only after explicit MAS approval, and the new device becomes cross-signed, activates the transferred backup secret, and decrypts the seeded history; and
- `unsupported`: MAS omits the device-authorization capability and acquisition terminates with the stable `unsupported` classification.

The integration workflow exposes each lane separately and provides `matrix-oauth-all` to run all four manually.
The smoke workflow runs all four in a separate job after every push to `main` and on its nightly schedule.
Both multi-lane jobs attempt every lane after a failure and report the aggregate result, so one run exposes all broken scenarios it reaches within the job budget.
These expensive, unstable-protocol lanes remain outside the pull-request path.

## Secret handling

Runtime access tokens, refresh tokens, QR payloads, check codes, authorization user codes and URLs, recovery material, and exported secret bundles must never enter diagnostics or retained artifacts.
The launcher passes its known compatibility and Axon bearer tokens without command-line arguments, and the harness retains known runtime protocol values only in memory for disclosure checks.

Axon's stdout and stderr are captured inside a randomly named throwaway run directory.
Every lane outcome is followed by a check that rejects secret-bearing field names or known runtime values in that log.
If both the lane and disclosure check fail, the harness reports both stable errors without printing the matched runtime value.
SQL statement text is excluded from the captured log so a column name cannot masquerade as a disclosure; a failing field-name check identifies the safe field name that matched.
The harness handles SIGINT and SIGTERM so its Axon child is killed and reaped before the launcher's cleanup trap removes the directory, SDK stores, containers, databases, and MAS configuration volume.
An intentional restart first gives Axon 45 seconds to complete its graceful shutdown, covering the server's 30-second sync-engine drain budget plus surrounding teardown, before the harness forcibly kills and reaps it.
This cleanup applies on success, failure, cancellation, and cooperative interruption; SIGKILL cannot run process cleanup.
The workflow deliberately uploads no failure artifacts for these lanes.

Harness errors name only a stable phase or classification.
Do not add raw HTTP bodies, SDK errors, OAuth errors, container configuration, or URLs to failure messages.
