# axon-smoke-tui

Black-box PTY smoke tests for the real `axon-tui` binary. The harness drives the
terminal UI as a user would: it starts `axon-tui`, types commands into a pseudo
terminal, and asserts on the rendered screen.

The package and executable are named `axon-smoke-tui`. All new smoke-test
packages and executables should use the `axon-smoke-*` naming pattern.

## Running

Fast stub profile:

```sh
cargo run -p axon-smoke-tui -- --profile stub
```

True local stack profile:

```sh
cargo run -p axon-smoke-tui -- --profile true-local
```

Attach to an already-running local stack manifest:

```sh
cargo run -p axon-smoke-tui -- --profile true-local --manifest smoke-artifacts/tui/<run-id>/local-stack.json
```

Run a subset by name:

```sh
cargo run -p axon-smoke-tui -- --profile true-local --filter relations
```

Keep a failing true-local stack running for manual inspection:

```sh
KEEP_UP=1 cargo run -p axon-smoke-tui -- --profile true-local --filter thread
cargo run -p axon-smoke-local-stack -- info --manifest smoke-artifacts/tui/<run-id>/local-stack.json
```

Useful environment variables:

- `AXON_TUI_BIN=/path/to/axon-tui`: skip building the TUI binary.
- `AXON_SMOKE_LOCAL_STACK_BIN=/path/to/axon-smoke-local-stack`: skip building
  the local-stack helper.
- `SMOKE_TIMEOUT=30`: raise per-step wait timeout.
- `KEEP_UP=1`: keep the true-local stack and artifacts after a failure.

When `--manifest` is provided, the TUI harness reuses the existing stack and
does not tear it down.

Artifacts for failing scenarios are written under
`smoke-artifacts/tui/<run-id>/<scenario>/`.

## Profiles

### `stub`

Runs against an in-process Axum stub of Axon's `/v1` API. This profile is fast
and deterministic, and is best for rendering, PTY, terminal-restoration, and
request-shape regressions.

Current stub scenarios:

- `launch_and_quit`: first paint and clean `/quit`.
- `ctrl_c_exit`: clean Ctrl-C exit.
- `send_round_trip`: send path, stub request journal, and WebSocket echo.
- `border_integrity`: wide/ambiguous Unicode does not overflow the right border.
- `scroll_pin_on_relation_refresh`: relation refresh does not move the
  materialized tail viewport.
- `room_sort_filter_surface`: `/sort` and `/filter` commands plus the Alt-S /
  Alt-F cycle chords and the Alt-/ live name filter surface the expected status
  and input state (ADR 0042).

### `true-local`

Runs against `axon-smoke-local-stack`, which starts throwaway Postgres, Synapse,
and `axon-server`, seeds Matrix users/rooms/messages, logs Axon into the fixture
account, and hands the real TUI a bearer token.

Current true-local scenarios:

- `true_local_launch`: starts the real TUI against real Axon and renders the
  room list and input.
- `true_local_send_round_trip`: sends a real Matrix message and waits for it to
  render through Axon sync.
- `true_local_command_surfaces`: `/help` renders without breaking the terminal UI.
- `true_local_shortcuts_popup`: `/shortcuts` renders keyboard help.
- `true_local_status_commands`: `/whereami`, `/whoami`, and `/status` render
  expected account/room/server information.
- `true_local_room_navigation`: switches rooms by name and refreshes rooms.
- `true_local_send_variants`: sends `//` literal slash text, `/literal`,
  `/html`, and `/rainbow` messages.
- `true_local_relations_render`: verifies seeded edit/reply/reaction/formatted
  relation data renders in the relations room and `/event` can inspect a seeded
  event.
- `true_local_react`: sends a reaction from the TUI and verifies the command
  completes; seeded reaction badges are checked by `true_local_relations_render`.
- `true_local_thread_panel`: opens and closes the thread panel against seeded
  thread data.
- `true_local_jump_to_date`: jumps to an older seeded date in the long timeline.
- `true_local_room_sort`: drives sort modes via `/sort` and the Alt-S cycle;
  rooms stay visible across recent/oldest/A–Z (ADR 0042).
- `true_local_room_filter`: drives filter modes — groups, DMs (named rooms drop
  out), all, favorites (after `/pin`), and the Alt-/ live name filter (ADR 0042).

## Fixture Expectations

The true-local profile assumes `axon-smoke-local-stack` provides:

- `Smoke General`: ordinary chat and send target.
- `Smoke Timeline`: 60 timestamped messages over several dates for jump and
  history navigation.
- `Smoke Relations`: edit, reply, reaction, own reaction, formatted HTML,
  redaction, and thread fixtures.

The local stack publishes fixture event IDs in its JSON manifest so scenarios can
test event-specific commands without depending on generated Matrix IDs.

### The demo corpus (opt-in)

`axon-smoke-local-stack up --corpus <path>` additionally renders a declarative
corpus — `smoke/local-stack/corpus/demo.toml` — into the same stack: personas
with display names and avatars, two spaces, six rooms and a DM, images, a
thread, an edit, a redaction and reactions, all backdated over the preceding
five weeks (ADR 0086).

No scenario passes `--corpus` today and the flag defaults to unset, so every
profile above gets exactly the stack it always got. What it unblocks is the
"Not Yet Covered" list below: the corpus is the first fixture in `smoke/` with
uploaded media and with a DM.

Two things to know before writing a scenario against it:

- **The viewer changes.** With `--corpus`, axon logs in as the corpus viewer
  (`@alex:localhost`) rather than `accounts.target`, so the `Smoke *` rooms
  exist on the homeserver but are not in the room list. That is deliberate —
  a demo recording must not have smoke fixtures in shot.
- **The TUI shows the two spaces as ordinary rooms**, because it has no space
  support (`docs/client-parity.md`, "Room-state reads"). Expected, not a
  seeding bug: a space is a room, and nothing filters it out client-side yet.
  A TUI scenario asserting on the room list has to account for the two extra
  entries until that lands.
- **Addresses come from the manifest.** `manifest.demo` maps corpus names to
  real ids: `demo.rooms["trip-photos"].room_id`, `demo.events["party-plan"]`,
  `demo.media["media/photos/party-crew.jpg"]`. Never scrape them off the
  screen, and never hard-code a Matrix id.

```sh
cargo run -p axon-smoke-local-stack -- up --manifest /tmp/axon-demo.json \
  --corpus smoke/local-stack/corpus/demo.toml --keep-up
cargo run -p axon-smoke-local-stack -- info --manifest /tmp/axon-demo.json
```

The photographs the corpus references are not committed; generate stand-ins
first with `corpus/media/generate-placeholder-photos.mjs` (see that directory's
README). The seeder fails with the missing path rather than seeding a room with
no images.

One artifact is worth knowing about: `/createRoom` ignores the appservice `ts`
parameter, so each room's creation events carry the current time even though
everything after them — membership, name, topic, avatar, and every message — is
backdated. Only the creator's own join is visible at the default
`stateEvents: important` tier, as a single "joined" line dated today at the end
of each room. A demo driver that wants it gone can set the client's state-event
visibility to `none`.

## Not Yet Covered

The suite does not yet exercise every TUI capability end to end. Omitted or
partial areas:

- Full account lifecycle mutation flows (`/login`, `/logout`, `/recover`,
  `/delete`) are not run destructively in the shared fixture account. Prompt and
  cancellation paths should be added with disposable accounts.
- Full SAS verification (`/verify`) is not covered; it requires a deterministic
  second trusted device.
- Sender-trust `/bundle` is not covered beyond the lower-level TUI tests; a
  stable fixture should be added once trust data is available in the local stack.
- Media rendering is not covered. The media fixtures now exist — the demo
  corpus uploads real images (`--corpus`, above) — but terminal-protocol
  assertions do not, so nothing asserts on them yet.
- `/unreact` withdrawal is not asserted end to end yet. The suite covers the
  `/react` command path and rendering seeded own reactions.
- Config editor launching (`/editconfig`) is not covered because it would spawn
  a host editor; config parsing and serialization remain unit-test territory.
- Full historical backfill is not covered. The local stack uses a bounded
  Sliding Sync window large enough for its current timeline fixture, not a true
  "ingest all history" mode.
- Room-list DM filtering is covered only negatively (named group rooms drop out
  of the DMs filter). The demo corpus now seeds an unnamed DM with `m.direct`
  account data, so the positive case for the interim `is_likely_dm` heuristic
  (ADR 0042) is finally fixturable; no scenario claims it yet because no
  scenario runs with `--corpus`.
