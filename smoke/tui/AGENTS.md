# axon-smoke-tui — Contributor Notes

Black-box PTY smoke harness for the shipped `axon-tui` binary. It spawns the
real binary under a pseudo-terminal, points it at an in-process Axum stub of the
Axon `/v1/` API, and asserts on the rendered terminal screen and the stub's
request journal. See ADR 0025 and `docs/mvp/smoke-testing-plan.md`.

## Hard rule: black-box boundary

This crate must depend on **no `axon-*` product crate** (CI enforces it via
`scripts/check-smoke-isolation.sh`). All wire types are handwritten in `wire.rs`
from the checked-in `openapi/` contract. The harness only ever interacts with
the binary through process spawn, HTTP, WebSocket, and terminal I/O — never by
importing product code. This keeps the harness independently movable across the
planned repository split, and means a green stub run proves process and
rendering behavior, not contract conformance (that is the `live` profile in S2).

## Running

```sh
cargo run -p axon-smoke-tui -- --profile stub [--filter NAME]
```

- `--profile stub` is the only S1 profile. `--filter` is a case-sensitive
  substring match over scenario names and fails if it matches nothing.
- `AXON_TUI_BIN` overrides the binary path (otherwise the runner builds
  `axon-tui` and resolves it under the target dir).
- `SMOKE_TIMEOUT` (seconds) bounds each wait; default 20.

## Layout

| File | Role |
|---|---|
| `lib.rs` | what the two binaries share: `pty`, `env`, `local_stack` |
| `pty.rs` | `PtyDriver` — spawn under `portable-pty`, model the screen with `vt100`, optional verbatim output tee |
| `env.rs` | binary resolution; `base_child_env` (shared) and `child_env` (smoke-only pins) |
| `local_stack.rs` | `axon-smoke-local-stack` manifest, including its `demo` section |
| `main.rs` | `axon-smoke-tui`: arg parsing, profile dispatch, exit code |
| `runner.rs` | sequential runner: run ID, per-scenario isolation, artifacts |
| `stub.rs` | in-process Axum stub + request journal + WS echo broadcast |
| `scenarios.rs` | the S1 scenarios |
| `wire.rs` | handwritten `/v1/` wire types |
| `demo/main.rs` | `axon-demo-tui`: arg parsing, terminal defaults (ADR 0086) |
| `demo/pilot.rs` | spawn with the tee, walk a scene, quit cleanly, failure artifacts |
| `demo/scenes.rs` | the demo scenes |
| `demo/term.rs` | the developer's real terminal: geometry, raw mode, repair |

## The demo pilot is not a test

`axon-demo-tui` shares this package but not its purpose: it drives the TUI so a
human can *record* it, needs Docker and a real terminal, and is not in any CI
gate. Do not add it to `scripts/smoke-gate.sh`. Two invariants it must keep,
both from ADR 0086 and both easy to break by "tidying":

- It declares `AXON_IMAGE_PROTOCOL` and `AXON_FONT_SIZE` and must never set
  `AXON_NO_IMAGE_QUERY=1` — that variable is a smoke-harness determinism choice
  and would switch off the graphics the TUI demo exists to show. This is why
  `env.rs` splits `base_child_env` from `child_env`.
- Nothing may write to stdout or stderr while the child owns the screen. The
  run log is buffered and printed after the terminal is restored.

Every scene is walked by screen predicates, never sleeps, and every state change
needs a wait that only the *new* state satisfies — see the README's "Writing a
scene". A scene added or changed here updates its row in `docs/demo-coverage.md`
in the same PR.

## Conventions

- Every wait is condition-based and bounded by a deadline. Eventually-consistent
  observations are polled; failed scenarios are not retried.
- Each scenario gets a fresh stub (ephemeral loopback port) and a fresh
  isolated config/home/working directory, so journals never bleed across
  scenarios and the developer's real `~/.config` is never touched.
- Exit scenarios assert on the alternate-screen leave sequence in the raw
  transcript, so a clean process exit cannot mask a terminal-restoration
  regression.
- On failure, the runner writes the PTY transcript and final rendered screen
  under `smoke-artifacts/tui/<run-id>/<scenario>/` (gitignored, removed after a
  passing run).

## Scenarios (S1)

- `launch_and_quit` — first paint renders the panes; `/quit` exits cleanly.
- `ctrl_c_exit` — the configured Ctrl-C shortcut exits cleanly.
- `send_round_trip` — keystrokes submit a run-marked message, the journal
  records the send, and the WebSocket echo renders in the open room.
- `border_integrity` — seeds messages with East Asian Ambiguous characters (`·` U+00B7, `■` U+25A0) and asserts that the rightmost terminal column contains only box-drawing characters after first paint, catching text-overflows-border regressions.

Login, navigation, message actions, resilience, and the live-stack journey are
S2 (see the plan).
