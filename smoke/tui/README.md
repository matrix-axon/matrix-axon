# axon-smoke-tui

Black-box PTY smoke tests for the real `axon-tui` binary. The harness drives the
terminal UI as a user would: it starts `axon-tui`, types commands into a pseudo
terminal, and asserts on the rendered screen.

The package and executable are named `axon-smoke-tui`. All new smoke-test
packages and executables should use the `axon-smoke-*` naming pattern.

The package ships a **second binary**, `axon-demo-tui` — the ADR 0086 demo
pilot. It is not a test; see [Recording a demo](#recording-a-demo-axon-demo-tui)
below. It lives here because it drives the TUI through the same `PtyDriver`, and
a separate `smoke/demo-tui` package could only reach that type through a path
dependency on `axon-smoke-tui`, which `scripts/check-smoke-isolation.sh` rejects
(it forbids every `axon-*` edge but the package's own). `src/lib.rs` is what the
two binaries share: `pty`, `env`, and `local_stack`.

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

The photographs the corpus references are committed (Unsplash License,
provenance per file in `corpus/media/README.md`), so a fresh checkout seeds
images without any extra step. `corpus/media/generate-placeholder-photos.mjs`
still exists for working without them; it overwrites the real files in place, so
run it deliberately, not by habit.

One artifact is worth knowing about: `/createRoom` ignores the appservice `ts`
parameter, so each room's creation events carry the current time even though
everything after them — membership, name, topic, avatar, and every message — is
backdated. Only the creator's own join is visible at the default
`stateEvents: important` tier, as a single "joined" line dated today at the end
of each room. A demo driver that wants it gone can set the client's state-event
visibility to `none`.

## Recording a demo (`axon-demo-tui`)

The pilot spawns the real `axon-tui` under a PTY and copies that PTY to its own
stdout **verbatim**, so the terminal you are sitting at draws real
Sixel/Kitty/iTerm2 graphics and a screen recorder captures what the client
actually renders. That is the whole reason it exists: `agg` and xterm.js-based
recorders (VHS) render none of those protocols, and would reduce the TUI's most
distinctive output to halfblock approximations (ADR 0086).

It is **not a test and not a CI gate** — it needs Docker and a real terminal.
`scripts/smoke-gate.sh` does not run it.

```sh
scripts/demo-stack.sh up               # placeholder photos if needed, then boot + seed
scripts/demo-stack.sh record --capture # ffmpeg records the window itself — see below
scripts/demo-stack.sh down
```

`--capture[=FILE]` records this terminal's own window with ffmpeg instead of you starting and stopping a screen recorder by hand —
no more racing the pilot to hit record in time, or trimming a take's dead air at the edges afterward.
It works on X11 and XWayland (ffmpeg's `x11grab`, pointed at exactly this window's geometry via `xdotool`/`xwininfo`);
it does nothing for a native-Wayland terminal or macOS, both of which have no X window for `x11grab` to find.
There, keep using your own screen recorder:

```sh
scripts/demo-stack.sh up
# start your screen recorder, then:
scripts/demo-stack.sh record    # -- --scene media --pace 1.5 to pass flags through
scripts/demo-stack.sh down
```

`FILE` defaults to `demo-artifacts/tui-demo.mp4` (gitignored — it belongs on a release, not in git, same as the web recordings; see `clients/web/README.md` § Demo recording).
Don't move or resize the window while it's running: the captured region is fixed at start.
`AXON_DEMO_WINDOW_ID` overrides the auto-detected window if it ever picks the wrong one (e.g. multiple terminals open);
`AXON_DEMO_CAPTURE_FRAMERATE` overrides the default 30fps.

`--capture` also resizes the window to a fixed 1280x800px first (`--size WxH` to pick a different target, `--no-resize` to keep whatever size the window already is),
so takes come out a consistent size regardless of however the terminal happened to be sized that day.
This is `xdotool windowsize` — the actual X window, at the server level — not a terminal escape sequence:
those depend on the terminal choosing to honor a resize request (xterm gates it behind `allowWindowOps`, off by default in most distros; several common terminals don't implement it at all),
so they are not a reliable way to get this.

`scripts/demo-stack.sh` is a thin wrapper; the underlying commands are worth
knowing when something goes wrong:

```sh
# 1. bring a demo world up and leave it running
cargo run -p axon-smoke-local-stack -- up --manifest /tmp/axon-demo.json \
  --corpus smoke/local-stack/corpus/demo.toml --keep-up

# 2. start your screen recorder, then drive the TUI through it
cargo run -p axon-smoke-tui --bin axon-demo-tui -- --manifest /tmp/axon-demo.json

# 3. when you are done recording
cargo run -p axon-smoke-local-stack -- down --manifest /tmp/axon-demo.json
```

On a fresh checkout the corpus photographs are missing — they are gitignored
until the licensing decision lands (ADR 0086), and the seeder fails on the
missing path rather than seeding a room with no images. `demo-stack.sh up`
generates stand-ins when the directory is empty and **never** regenerates over
existing files, so it cannot become the thing that quietly overwrites the real
photographs later; `demo-stack.sh photos` replaces them on purpose. Generating
them needs Node, and installs `sharp` into a temp directory.

`--manifest` must point at a stack brought up with `--corpus`; the pilot
attaches to it and neither starts nor stops anything. Options:

- `--scene NAME` — one scene instead of the whole `tour`. `--list-scenes` prints
  them. Scenes are listed against the capabilities they cover in
  `docs/demo-coverage.md`.
- `--pace FACTOR` — multiplies every scripted dwell (default 1.0; `0` removes
  them). Waits are unaffected: they are correctness gates, not pacing, so a low
  pace runs the script faster but never skips ahead of the client.
- `--image-protocol sixel|kitty|iterm2|halfblocks` — defaults to what this
  terminal's environment implies, else Sixel. Use `halfblocks` if graphics come
  out as garbage.
- `--font-size WxH` — cell size in pixels. By default the pilot **asks the
  terminal** with XTWINOPS (`CSI 16 t`) and falls back to `TIOCGWINSZ`, in that
  order, because plenty of terminals answer the query accurately while leaving
  the ioctl's pixel fields zero. Graphics protocols encode an image at
  `cells × cell size` and ask the terminal to draw it at exactly those pixels,
  so an under-guessed cell size renders the picture **smaller than the frame
  drawn around it** — most visible in the full-size preview. The run log always
  says which source was used, and warns when it had to guess.
- `--timeout SECONDS` (default 30), `--keep-dirs`.

Two ADR 0086 details are load-bearing and easy to undo by accident:

- The pilot **declares** `AXON_IMAGE_PROTOCOL` and `AXON_FONT_SIZE`, because a
  pilot-owned PTY has nobody on the far end to answer the TUI's capability
  probes. Which is also why the pilot runs those probes itself, against the real
  terminal, and hands the answers over — run by hand, `axon-tui` asks the
  terminal directly and gets the truth, so a pilot that only guessed would
  render differently from a manual session.
- It must **not** set `AXON_NO_IMAGE_QUERY=1`. The smoke harness does, for the
  opposite reason — it wants determinism and does not care about images — which
  is why `env::child_env` and `env::base_child_env` are separate.

`TERM` and the tmux variables are passed through rather than pinned: the child's
bytes end up on your real terminal, so it has to be described truthfully.

The pilot holds the terminal in raw mode and forwards your keystrokes to the
child, so Ctrl-C reaches `axon-tui` and quits it cleanly rather than killing the
pilot and leaving you in the alternate screen. Nothing is printed while the
child owns the screen — the run log is held and printed after the terminal is
restored. A failing scene writes the rendered screen and the raw transcript to
`smoke-artifacts/demo-tui/<scene>/`.

### Writing a scene

Scenes live in `src/demo/scenes.rs`. Three rules, all learned the hard way:

- **Address content by corpus name, never by Matrix id** — `manifest.demo` maps
  the stable names onto the per-run ids.
- **Every step that changes state must be followed by a wait that only the new
  state satisfies.** A needle the _previous_ frame already satisfies is not a
  wait at all: the script runs ahead of the client and the next keystroke lands
  somewhere unintended. Prefer state-unique chrome (a popup title, the
  `[in thread]` marker, `result 1 of`) over message text that is on screen
  either way, and use `WaitGone` to prove a narrowing step actually narrowed.
- **Date anything from `demo.seeded_at`, never from the clock.** The corpus
  resolves its relative `at = "-6d 09:12"` offsets against the instant it was
  seeded, and `demo-stack.sh up` is designed to leave a stack running for a
  recording made later — so `up` and `record` can straddle midnight. A scene
  that computed its own `Utc::now() - 14d` jumped to a date the corpus never
  wrote to, and hung waiting for a status line that never came.

Scenes are coupled to the corpus prose on purpose: rewording a message in
`demo.toml` fails the scene loudly, with the screen attached, rather than
quietly demonstrating nothing.

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
- Media rendering is not _asserted_. The demo pilot renders it for real
  (`axon-demo-tui --scene media`, above), which proves the path end to end by
  eye, but no smoke scenario asserts on terminal graphics: the `vt100` screen
  model the harness reads discards them by design, so an assertion would have to
  work on the raw transcript instead. Tracked in #111, with the rest of the
  assertions the demo corpus made possible (ADR 0086 phase 5a).
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
