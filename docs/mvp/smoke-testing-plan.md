# Smoke Testing Plan

> **Status:** S1 has shipped, and the stub lane is now the required PR job this plan originally called for — `smoke.yml` runs `scripts/smoke-gate.sh tui` on every pull request (no Docker, ~90s). The Docker-backed lanes remain off the PR path and run on push to `main` and nightly; `scripts/smoke-gate.sh all` also runs locally via the pre-push hook's `RUN_SMOKE=` opt-in. The lanes were split once measured: keeping all of them manual meant nothing ran automatically, and `scroll_pin_on_relation_refresh` sat broken on `main` undetected (#190). Most of S2's journey coverage (login, the full mutation set, live updates/resilience, the live-stack journey) is also built out under the same harness. S3 (dedicated Windows/macOS PTY runners) and S4 (external-homeserver profile) have not started. This plan predates 7b bearer-token auth landing — see the note under "Assumptions" below, which is now stale in one respect.

## Summary

Stand up black-box smoke coverage for the two shipped binaries:

- **`smoke/server`** (package `axon-smoke-server`): boots the real
  `axon-server` binary against real Postgres and Synapse, then exercises its
  public HTTP API and `/v1/ws` stream end to end.
- **`smoke/tui`** (package `axon-smoke-tui`): runs the real `axon-tui` binary
  inside a pseudo-terminal and asserts on what is actually rendered and what
  actually crosses the wire.

Both harnesses are Rust workspace members that depend on **zero `axon-*`
crates** — they interact with the binaries the way a user or operator does:
process spawn, HTTP, WebSocket, keystrokes. The work is sequenced in four
milestones (S1–S4) so a minimal PR-blocking smoke job lands quickly and the
heavier infrastructure follows only after the basic suite has proven its value.

## What exists today, and the gap smoke fills

We are not starting from zero. Current coverage:

- **TUI unit tests (~150)** drive key handling, command parsing, completion,
  config, HTML conversion, and wrapping against `App` state. Fast and
  thorough at the logic level — but they never start the binary, never draw to
  a terminal, and never touch a socket.
- **`axon-api` integration tests** (DB-gated, `--ignored` by default) run the
  real router against real Postgres via `tower::oneshot`, with stubbed account
  lifecycle and message sender. They pin the read API envelope, mutation
  contracts, login/logout responses, and WS fan-out — but in-process: no real
  socket, no real process boot, no real Matrix homeserver.
- **`scripts/integration-test.sh`** is the deepest test we have: compose
  Postgres + Synapse, the `axon-itest` seeder as verified device A, axon as
  unverified device B, asserting UTD persistence and then recovery-key
  re-decryption across a restart. It covers the E2EE prize path — but only
  that path, and it asserts through direct SQL, not the public API.

What none of that covers is the smoke target:

- **The binaries as processes**: config loading, migrations on boot, graceful
  shutdown, restart behavior — observed through the public API, not SQL.
- **Real network round-trips**: HTTP over an actual socket, a genuine
  WebSocket upgrade, the live tail under churn.
- **The TUI's real terminal path**: ratatui rendering, the crossterm event
  loop, raw byte input, terminal setup/teardown. PR #53 (double input from
  key-release events on Windows) is exactly the class of bug the unit tests
  cannot see — it lived between the terminal and `handle_key`.
- **Client↔server integration**: the shipped TUI talking to the shipped
  server over real HTTP/WS.
- **Cross-platform behavior**, especially Windows ConPTY.

Smoke scenarios target only this delta. Logic permutations stay in unit
tests; contract details stay in the `axon-api` tests; deep E2EE stays in
`integration-test.sh`, which is kept as-is.

## Language and tooling: Rust

The first draft proposed Python (pytest + pexpect + pyte + pywinpty) on the
rationale that a non-Rust harness guarantees black-box discipline and survives
a future repo split. That rationale does not hold up: black-boxness is a
*dependency* property, not a language property. A Rust crate that depends on
no `axon-*` crate and only spawns released binaries is exactly as black-box as
a Python package, and each harness moves intact in a split (`smoke/server`
with the server, `smoke/tui` with the client).

Given that, Rust wins the remaining tradeoffs:

- **One toolchain.** Contributors and CI already have cargo. Python would add
  an interpreter version to pin, a venv and second lockfile ecosystem to
  maintain, a second Dependabot surface, and a second language reviewers must
  hold to the same standard as the product code.
- **The PTY problem — the original reason for pexpect/pywinpty — is solved in
  Rust.** [`portable-pty`](https://crates.io/crates/portable-pty) (from
  wezterm) abstracts POSIX PTYs and Windows ConPTY behind one API.
  [`vt100`](https://crates.io/crates/vt100) provides the terminal screen model
  `pyte` would have: feed it raw output bytes, assert on rendered rows instead
  of ANSI noise. [`expectrl`](https://crates.io/crates/expectrl) exists if
  expect-style interaction helpers prove useful.
- **The clients are already paid for.** `reqwest` and `tokio-tungstenite` are
  pinned workspace dependencies, and the TUI-side fake Axon server is a small
  axum app serving handwritten JSON (no `axon-api` import; the checked-in
  `openapi/` document is its contract reference). Drift between the stub and
  the real API is caught by the `live` profile (S2), not by stub runs — a
  green stub pass is process and rendering confidence, not contract conformance.
- **What we give up**: pytest's fixture ergonomics and zero-compile
  iteration. Acceptable — a harness crate this size rebuilds incrementally in
  seconds, and fixtures become plain structs with `Drop`-based cleanup.

**Harness shape.** Each smoke package is a binary, not a `#[test]` suite:

```sh
cargo run -p axon-smoke-server -- --profile true-local [--filter NAME]
cargo run -p axon-smoke-tui    -- --profile stub  [--filter NAME]
```

A sequential scenario runner owns the expensive shared environment (compose
stack, server process), controls ordering and teardown, captures artifacts on
failure, and exits nonzero with a per-scenario summary. This avoids fighting
test-framework process models (cargo test's parallelism, nextest's
process-per-test) for a suite whose scenarios deliberately share one stateful
environment, and it keeps reporting needs down to an exit code plus uploaded
artifacts — no JUnit machinery.

## Shared harness rules

- The smoke crates live at `smoke/server` and `smoke/tui`, are workspace
  members, and never depend on any `axon-*` crate or on each other. CI
  enforces this with a `cargo tree` check.
- Configuration comes from flags and environment variables, following the
  `integration-test.sh` conventions (`POSTGRES_PORT`, `SYNAPSE_PORT`, …).
  Secrets are environment-only and never land in committed files.
- Every run mints a run ID; mutating scenarios embed it in message bodies so
  observations match only their own writes.
- Waiting is condition-based polling with bounded deadlines. Eventually
  consistent *observations* are retried; failed *scenarios* are not.
- On failure the runner captures: server log tail, HTTP request/response
  journal, WS frames, PTY transcript and final rendered screen, with
  configured secrets redacted.

## Server smoke (`axon-smoke-server`)

### Profiles

- **`local`** (S1): the harness owns everything — the compose `integration`
  profile for Postgres + Synapse, a throwaway database, fresh Matrix accounts
  via Synapse shared-secret registration, and account activation through the
  real `POST /v1/accounts/login` (loopback-guarded, which is fine: the harness
  runs on the same host). Synapse-specific provisioning stays behind a small
  adapter so a Dendrite adapter can slot in later.
- **`attached`** (S4): an existing Axon URL plus a configured homeserver and
  dedicated accounts. Mutating scenarios require an explicit
  test-environment confirmation flag and expected-user-ID checks before
  touching the account. Process, restart, and login/logout scenarios are
  capability-skipped (lifecycle verbs are loopback-only).

### Scenarios

- **Boot and liveness** (S1): clean boot on an empty database (migrations
  run), `/healthz` responds, graceful shutdown on signal.
- **Account lifecycle** (S1 login; S2 full cycle): `POST /v1/accounts/login`
  activates an account that then appears in `GET /v1/accounts` /
  `GET /v1/accounts/{id}`; logout transitions state; re-login reactivates.
- **Inbound flow** (S1): a peer — a plain Matrix Client-Server API client
  over reqwest, in an *unencrypted* room — invites the Axon account and sends
  a marked message; assert the room appears in `/v1/rooms`, the event in the
  timeline and in event lookup, and a `timeline.event` frame arrives on
  `/v1/ws`.
- **Outbound flow** (S1 send; S2 the rest): send, edit, react, and redact
  through the API with run-marked bodies, each confirmed from the peer's side
  via CS-API sync, with response contracts asserted along the way.
- **Pagination and error contracts** (S2): cursor walk over a seeded
  timeline; representative `400` and `404` responses.
- **Restart persistence** (S2): stop the server, start it again, previously
  synced rooms and timelines are still served.
- **E2EE**: out of scope for the smoke harness. `scripts/integration-test.sh`
  remains the E2EE gate, and `axon-itest` remains its Rust seeder. Porting
  that flow into the harness is revisited in S4 only if it pays for itself.

## TUI smoke (`axon-smoke-tui`)

### Terminal driver

Spawn `axon-tui` under `portable-pty` with fixed dimensions and a controlled
environment (temporary config dir, pinned `TERM` and locale, isolated working
directory). All output feeds a `vt100` screen; assertions read rendered rows,
not raw ANSI. Driver surface: `spawn`, `send_keys`, `resize`,
`wait_for(predicate)`, `screen_text`, `terminate`.

### Profiles

- **`stub`** (S1): an in-crate axum fake of the Axon API — accounts, rooms,
  timeline, send/edit/react/redact, login, and `/v1/ws` — with a request
  journal and scriptable responses/pushed frames. Deterministic, no Docker,
  runs anywhere; this is what makes the cross-platform matrix cheap.
- **`live`** (S2): pointed at a real local Axon stack using only the public
  API, for one true client↔server journey.

### Scenarios

- **Launch and first paint** (S1): spawn against the stub; room list and
  status line render; `/quit` and Ctrl-C both exit cleanly with the terminal
  restored.
- **Send round-trip** (S1): compose and send a message via keystrokes; assert
  the stub's journal saw the `send` request and the echoed event renders.
- **Login flow** (S2): stub reports no active account; the TUI's
  username/password/homeserver prompts render; submitting drives a login
  request into the journal; cancel path returns to a sane state.
- **Navigation and commands** (S2): room switching, `/rooms`, `/switch`,
  `/help`, `/shortcuts`, `/whoami` popups render and dismiss.
- **Message actions** (S2): `/reply`, edit, `/react`, `/unreact`, redact via
  keystrokes, verified against the journal.
- **Live updates and resilience** (S2): stub pushes WS frames → event renders
  in the open room and marks another room unread; WS drop → reconnect and
  status handling; server `500` → error surfaced in the status line without a
  crash; resize mid-session does not panic.
- **Live journey** (S2): against the real local stack — launch, send a marked
  message, watch its WS echo render, exit.
- **Cross-platform** (S3): the stub suite runs natively on Windows and macOS
  runners. Windows gets a dedicated regression scenario for key-release
  double-input (PR #53).

## Milestones

- **S1 — first smoke signal.** Scaffold both crates, the scenario runner, and
  the no-`axon-*`-dependency CI guard. Server: boot/healthz, login, one
  inbound message visible via timeline + WS, one outbound send confirmed,
  graceful shutdown. TUI: stub launch/render, send round-trip, clean exit.
  One required Ubuntu PR job running both. Exit criterion: the job goes red
  for a server that fails migrations on boot and for a TUI that panics on
  first draw. S1 is a sizeable first PR (two crates, the runner, compose
  integration, Synapse registration, CI guard, and the axum stub) and will
  likely need sub-splitting to preserve incremental landing.
- **S2 — journey coverage.** Server: full mutation set, logout/re-login,
  pagination, error contracts, restart persistence. TUI: login flow,
  navigation and popups, message actions, live updates and resilience, and
  the live-stack journey. Failure-artifact capture and secret redaction
  mature here.
- **S3 — cross-platform TUI.** The stub suite as required PR jobs on
  `windows-latest` and `macos-latest`; ConPTY quirks documented; the PR #53
  regression scenario lands.
- **S4 — external environments.** The `attached` server profile with its
  safety rails; a nightly/manual workflow against an external homeserver
  using protected secrets (never exposed to PR jobs); runs serialized per
  environment to protect the dedicated accounts; a Dendrite compose matrix
  entry behind the provisioning adapter; revisit whether the E2EE script
  should fold into the harness.

## CI

- **S1**: one required Ubuntu PR job — build the binaries, run both
  harnesses. Existing jobs (unit tests, DB-gated `axon-api` tests,
  `integration-test.sh`) are unchanged.
- **S3**: Windows and macOS TUI-stub jobs become required.
- **S4**: nightly external job with environment-protected secrets.
- **Growth rule**: every new public endpoint gets a server smoke journey;
  every new user-visible TUI workflow gets a PTY journey; existing journeys
  remain as regression coverage.

## Assumptions

- **Stale as of 7b (ADR 0029):** this was written when the API had no
  authentication and lifecycle verbs were loopback-guarded. Bearer-token auth
  now gates all of `/v1/`, including the WebSocket, so the harness needs a
  token configuration knob (mint via the CLI, pass to the `attached` profile)
  rather than treating the API as open. Confirm current harness code reflects
  this before relying on this section.
- `/v1/ws` is a best-effort live tail with no replay, so scenarios connect
  before triggering events and fall back to HTTP reads for anything missed.
- Unencrypted rooms are sufficient for smoke; E2EE remains covered by
  `integration-test.sh`.
- External runs use dedicated test accounts, never production accounts.

## References

- [portable-pty](https://crates.io/crates/portable-pty) — cross-platform PTY
  (POSIX + ConPTY), maintained as part of wezterm
- [vt100](https://crates.io/crates/vt100) — terminal screen model for
  asserting on rendered output
- [expectrl](https://crates.io/crates/expectrl) — expect-style process
  interaction, if needed
- [Microsoft pseudoconsole (ConPTY) documentation](https://learn.microsoft.com/en-us/windows/console/pseudoconsoles)
- `openapi/` — the emitted API contract the TUI stub mirrors
