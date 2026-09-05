# axon-smoke-local-stack

Reusable local test stack for Axon smoke tests and manual development. It starts
throwaway Postgres, Synapse, and `axon-server`, creates Matrix fixture users and
rooms, logs Axon into the fixture account, then writes a JSON manifest consumed
by smoke harnesses or by a human tester.

The stack is local-only and disposable. Do not expose it outside localhost and
do not reuse its credentials.

## Prerequisites

- Docker with Compose v2.
- Rust toolchain for this workspace.
- Free localhost ports, or override with environment variables:
  - `SMOKE_POSTGRES_PORT`
  - `SMOKE_SYNAPSE_PORT`
  - `SMOKE_AXON_PORT`
- A buildable `axon-server` binary. Set `AXON_SERVER_BIN=/path/to/axon-server` to skip the
  helper's `cargo build -p axon-server` step.

When ports are not explicitly overridden, the helper retries a small number of
times if another process claims one of the auto-selected ports before Docker or
Axon binds it. Explicit `SMOKE_*_PORT` values fail immediately on collision.

## Quick Start

```sh
cargo run -p axon-smoke-local-stack -- up --manifest /tmp/axon-smoke.json --keep-up
cargo run -p axon-smoke-local-stack -- info --manifest /tmp/axon-smoke.json
```

By default, `up` prints the same connection summary as `info` after the stack is
ready. Pass `--quiet` to suppress that launch summary when another harness is
reading the manifest directly:

```sh
cargo run -p axon-smoke-local-stack -- up --manifest /tmp/axon-smoke.json --quiet
```

The printed summary includes:

- the Axon URL and bearer token,
- an `axon-tui` command for a manual TUI session,
- Synapse homeserver URL,
- Matrix usernames/passwords for other Matrix clients,
- seeded rooms and jump-test dates,
- the cleanup command.

When finished:

```sh
cargo run -p axon-smoke-local-stack -- down --manifest /tmp/axon-smoke.json
```

## Running TUI Smoke Against the Stack

The TUI smoke harness can own the stack automatically:

```sh
cargo run -p axon-smoke-tui -- --profile true-local
```

For inspection after failures, leave the stack running:

```sh
KEEP_UP=1 cargo run -p axon-smoke-tui -- --profile true-local
```

Then inspect connection details:

```sh
cargo run -p axon-smoke-local-stack -- info --manifest smoke-artifacts/tui/<run-id>/local-stack.json
```

## Manual Interaction

Run your own TUI against Axon:

```sh
AXON_BASE_URL=http://127.0.0.1:<axon-port> AXON_TOKEN=<token> cargo run -p axon-tui
```

Or use any Matrix client with:

- homeserver: `http://127.0.0.1:<synapse-port>`
- username/password: one of the manifest accounts (`target`, `peer`,
  `observer`)

The `target` account is the one Axon logs in as. Logging into it from another
Matrix client creates another device on the same throwaway account.

## Fixture

The helper creates three rooms:

- `general`: short alternating conversation.
- `long_timeline`: 60 messages distributed across several dates.
  `jump_dates` in the manifest identify dates intended for `/jump` testing.
- `relations`: replies, edits, reactions, redaction, and formatted HTML.

Historical messages are seeded through Synapse's test-only application service
registration and timestamp massaging. The helper never hand-edits Synapse SQL.

### History Visibility Limitation

Axon does not currently have a "sync all room history" setting. The smoke stack
starts `axon-server` with `AXON_SYNC__TIMELINE_LIMIT=200`, which is enough for
the current 60-message `long_timeline` fixture to be ingested through the
bounded sliding-sync room window when the homeserver honors that request. The
fixture intentionally stays only modestly above 50 messages so the bounded
window still includes representative messages from every advertised `jump_dates`
entry.

This is a smoke-test workaround, not full historical backfill. If the fixture
grows beyond the bounded window, either raise the limit here, keep the per-date
message count low enough that every advertised date remains inside the bounded
window, or add a real backfill path before relying on older messages for TUI
navigation coverage. [#164](https://github.com/matrix-axon/matrix-axon/issues/164) is the tracking issue.

## Manifest

The manifest contains disposable secrets because manual clients need them. Smoke
failure artifacts should redact these values before upload.

Important fields:

- `axon_base_url`
- `axon_bearer_token`
- `homeserver_url`
- `accounts`
- `rooms`
- `paths.run_dir`
- `paths.axon_log`
- `paths.compose_project`

`down` uses the manifest to kill `axon-server`, drop the throwaway database, and
run `docker compose down` for the stack's unique Compose project.
