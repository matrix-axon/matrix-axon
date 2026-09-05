# axon-smoke-server

Black-box smoke tests for the real `axon-server` binary. The harness talks only
to Axon's public HTTP API, `/v1/ws`, and a throwaway Synapse instance created by
`axon-smoke-local-stack`.

The package and executable are named `axon-smoke-server`.

## Prerequisites

- Docker with Compose v2.
- Rust toolchain for this workspace.
- Free localhost ports, or override local-stack ports with:
  - `SMOKE_POSTGRES_PORT`
  - `SMOKE_SYNAPSE_PORT`
  - `SMOKE_AXON_PORT`

## Running

Start a fresh true-local stack, run all server smoke scenarios, and tear it down:

```sh
cargo run -p axon-smoke-server -- --profile true-local
```

Run selected scenarios by substring:

```sh
cargo run -p axon-smoke-server -- --profile true-local --filter outbound
```

Keep the stack and artifacts for inspection:

```sh
KEEP_UP=1 cargo run -p axon-smoke-server -- --profile true-local
```

Attach to an already-running stack:

```sh
cargo run -p axon-smoke-server -- --profile true-local --manifest smoke-artifacts/server/<run-id>/local-stack.json
```

## Environment

- `AXON_SMOKE_LOCAL_STACK_BIN=/path/to/axon-smoke-local-stack`: skip building
  the local-stack helper.
- `SMOKE_TIMEOUT=30`: per-operation timeout in seconds. The default is 20.
- `KEEP_UP=1`: keep an owned local stack and failure artifacts after the run.
- `AXON_SERVER_BIN=/path/to/axon-server`: used by local-stack when it builds/runs Axon.

## Tested Scenarios

- `boot_health`: `/healthz` returns healthy JSON from the real server.
- `account_visible`: the local-stack Axon account appears active in the account
  list with the expected user, homeserver, and device id.
- `room_list`: the seeded general, long timeline, and relations rooms appear in
  `GET /v1/rooms`.
- `timeline_read`: seeded timeline data is readable, and a returned event can be
  fetched by event id.
- `inbound_timeline_ws`: a Matrix peer sends a run-marked message through
  Synapse; Axon emits the matching `timeline.event` frame and serves the event
  from the timeline.
- `outbound_send`: Axon sends a run-marked message; the peer observes the exact
  event id and body through Matrix `/sync`.
- `relation_reads`: seeded edit, reply, reaction, redaction, and thread fixtures
  are readable through Axon's public APIs.
- `graceful_stack_shutdown`: verifies the runner reaches the managed stack
  teardown path. In attach mode this is a no-op because the stack is owned by
  the caller.

## Not Yet Covered

- Destructive lifecycle flows such as logout, recovery, and delete.
- SAS verification and sender-trust flows.
- Media proxy behavior.
- Exhaustive pagination walks and restart persistence.
- E2EE re-decryption; that remains covered by `scripts/integration-test.sh`.

## Artifacts

On failure, artifacts are written under:

```text
smoke-artifacts/server/<run-id>/<scenario>/
```

Artifacts include redacted Axon and Matrix HTTP journals, WebSocket frames, a
redacted local-stack manifest, and a tail of the Axon log. Successful runs remove
their artifacts unless `KEEP_UP=1` or `--manifest` attach mode is used.
