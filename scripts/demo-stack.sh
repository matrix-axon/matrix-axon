#!/usr/bin/env bash
#
# Bring the ADR 0086 demo world up, and point the recording tools at it.
#
# This is not a test and not a gate: the stack it starts is meant to stay up
# while a human records a video against it. `scripts/smoke-gate.sh` is what CI
# and the pre-push hook run; this is not part of either.
#
# There is no gate-side counterpart yet — no `demo` target in smoke-gate.sh, so
# the corpus is not reachable from `RUN_SMOKE=<lane> git push`. That is ADR 0086
# phase 5b, tracked in #111, and deliberate rather than forgotten.
#
# Usage:
#   scripts/demo-stack.sh up       # generate placeholders if needed, then boot
#   scripts/demo-stack.sh info     # reprint the connection summary
#   scripts/demo-stack.sh record   # run the TUI pilot against the running stack
#   scripts/demo-stack.sh down     # tear it all down
#   scripts/demo-stack.sh photos   # (re)generate placeholder photos only
#
# `record --capture[=FILE]` records the take itself with ffmpeg, instead of a
# human starting and stopping their own screen recorder. X11/XWayland only —
# see the "ffmpeg window capture" section below for why, and what to do on
# Wayland-native or macOS. It also resizes this terminal's window to a fixed
# default (1280x800px, `--size WxH` or `--size=WxH` to override, `--no-resize`
# to skip) before recording, so takes come out a consistent size run to run —
# pass `--size`/`--no-resize` only alongside `--capture`.
#
# Environment:
#   AXON_DEMO_MANIFEST         manifest path (default /tmp/axon-demo.json)
#   AXON_DEMO_WINDOW_ID        skip auto-detecting this terminal's X window
#   AXON_DEMO_CAPTURE_FRAMERATE  --capture's framerate (default 30)
#   AXON_DEMO_CAPTURE_SIZE      --capture's default --size (default 1280x800)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

manifest="${AXON_DEMO_MANIFEST:-/tmp/axon-demo.json}"
corpus="smoke/local-stack/corpus/demo.toml"
photo_dir="smoke/local-stack/corpus/media/photos"
generator="smoke/local-stack/corpus/media/generate-placeholder-photos.mjs"
capture_default_out="$repo_root/demo-artifacts/tui-demo.mp4"
capture_default_size="${AXON_DEMO_CAPTURE_SIZE:-1280x800}"

usage() {
  cat >&2 <<'EOF'
usage: scripts/demo-stack.sh <up|info|record|down|photos> [-- <pilot args>]

  up      generate placeholder photos if missing, then boot Synapse + axon and
          render the demo corpus into it, leaving it running
  info    reprint the running stack's connection summary
  record  run axon-demo-tui against the running stack (needs a real terminal;
          start your screen recorder first, or pass --capture — see below).
          Extra args after `--` are passed through, e.g.
          `record --capture -- --scene media --pace 1.5`
  down    stop the stack and remove its containers and volumes
  photos  (re)generate the placeholder photographs and stop

  record --capture[=FILE] [--size WxH] [--no-resize]
          record this terminal's own window with ffmpeg instead of a human
          running a screen recorder (X11/XWayland only). FILE defaults to
          demo-artifacts/tui-demo.mp4. Do not move or resize the window
          while it's running.

          Resizes the window to 1280x800px first (via `xdotool windowsize`,
          not a terminal escape sequence — those depend on the terminal
          choosing to honor them, and most don't by default), so every take
          comes out the same size. --size WxH (or --size=WxH) picks a
          different target; --no-resize records at whatever size the window
          already is.

Manifest path: $AXON_DEMO_MANIFEST, default /tmp/axon-demo.json
EOF
}

say() { printf '\ndemo-stack: %s\n' "$1"; }

# The photographs are deliberately not committed (ADR 0086): the licensing
# decision is still open, so `photos/` is gitignored and the seeder fails on the
# missing path rather than seeding a room with no images.
#
# `up` therefore generates stand-ins, but only when they are actually missing.
# Once real photographs land, this must not be what silently overwrites them —
# which is why `up` never regenerates over existing files, and why replacing
# them on purpose needs the explicit `photos` subcommand.
photos_present() {
  [ -d "$photo_dir" ] && [ -n "$(ls -A "$photo_dir" 2>/dev/null || true)" ]
}

generate_photos() {
  if ! command -v node >/dev/null 2>&1; then
    cat >&2 <<EOF
demo-stack: node is required to generate the placeholder photographs.

Install Node, or drop the six images the corpus expects into
  $photo_dir
See smoke/local-stack/corpus/media/README.md for names and dimensions.
EOF
    exit 1
  fi

  # Node resolves ESM imports from the script's own directory, so the generator
  # has to run next to its node_modules rather than from the repo.
  local work
  work="$(mktemp -d)"
  trap 'rm -rf "$work"' RETURN

  say "generating placeholder photographs (sharp installs into $work)"
  ( cd "$work" && npm install --silent --no-audit --no-fund sharp@^0.34 )
  cp "$repo_root/$generator" "$work/"
  ( cd "$work" && node generate-placeholder-photos.mjs "$repo_root/$photo_dir" )

  cat <<EOF

  These are stand-ins, stamped PLACEHOLDER across the middle so one that
  survives into a take is obvious in the first frame. They must not reach a
  published recording.
EOF
}

require_stack() {
  if [ ! -f "$manifest" ]; then
    cat >&2 <<EOF
demo-stack: no manifest at $manifest — is the stack up?

  scripts/demo-stack.sh up
EOF
    exit 1
  fi
}

# --- ffmpeg window capture (X11/XWayland only) ------------------------------
#
# `record` normally hands the terminal to axon-demo-tui via `exec`, on the
# assumption a human is running their own screen recorder alongside it.
# `--capture` replaces that human step by finding *this terminal's own X
# window* and recording exactly it with ffmpeg's x11grab, for exactly the
# lifetime of the pilot run.
#
# This only works where an X window exists to find: a real X11 session, or a
# Wayland session running the terminal through XWayland. A native-Wayland
# terminal has no X window at all — x11grab cannot see it — and macOS has no
# x11grab. Both still want the pre-existing manual path: start your own
# screen recorder, then `record` with no --capture.
capture_pid=""

# Prints this terminal's X window id, or fails if none can be found.
capture_window_id() {
  if [ -n "${AXON_DEMO_WINDOW_ID:-}" ]; then
    printf '%s\n' "$AXON_DEMO_WINDOW_ID"
    return 0
  fi
  # xdotool asks the window manager for the focused window, which is right
  # even under tmux — the WM tracks focus on the outer terminal's real X
  # window regardless of how many multiplexer panes live inside it.
  if command -v xdotool >/dev/null 2>&1 && xdotool getactivewindow 2>/dev/null; then
    return 0
  fi
  # Many X terminal emulators (xterm, urxvt, st, alacritty, ...) export their
  # own window id; a plain fallback when xdotool is not installed.
  if [ -n "${WINDOWID:-}" ]; then
    printf '%s\n' "$WINDOWID"
    return 0
  fi
  return 1
}

# resize_window <window-id> <WxH>  ->  best-effort: warns and returns 1
# rather than dying. A resize failing (xdotool missing, a tiling window
# manager refusing the request) is not a reason to abandon the recording —
# only a reason it won't be a *consistent* size this time.
#
# This is `xdotool windowsize`, not a terminal escape sequence
# (`CSI 8;rows;colst`, the sibling of the XTWINOPS cell-size query
# capture_geometry's caller already uses). The escape sequence resizes the
# character grid and depends on the terminal emulator choosing to honor it —
# xterm gates it behind `allowWindowOps`, off by default in most distros for
# exactly the reason a script wants it: an unattended program resizing your
# terminal. `xdotool windowsize` instead resizes the X window directly at
# the server level, so it doesn't depend on the terminal cooperating at all
# — which does mean it is a target *pixel* size, not a target row/column
# count; how much of the TUI's content that pixel size fits still depends on
# the terminal's own font metrics.
resize_window() {
  local id="$1" size="$2" w h
  w="${size%x*}"
  h="${size#*x}"
  if ! command -v xdotool >/dev/null 2>&1; then
    printf 'demo-stack: xdotool not found — recording at this window'"'"'s current size, not %s (install xdotool for a consistent size every take)\n' "$size" >&2
    return 1
  fi
  if ! xdotool windowsize "$id" "$w" "$h" 2>/dev/null; then
    printf 'demo-stack: could not resize window %s to %s (a tiling window manager may refuse it) — recording at its current size instead\n' "$id" "$size" >&2
    return 1
  fi
  # Give the window manager a moment to actually apply it before geometry is
  # read back for the capture region.
  sleep 0.2
}

# capture_geometry <window-id>  ->  prints "W H X Y" (client area, no WM
# decorations — that is what we want, the content rather than its chrome).
capture_geometry() {
  local id="$1" info w h x y
  info="$(xwininfo -id "$id" 2>/dev/null)" || return 1
  w="$(printf '%s\n' "$info" | sed -n 's/^ *Width: *//p')"
  h="$(printf '%s\n' "$info" | sed -n 's/^ *Height: *//p')"
  x="$(printf '%s\n' "$info" | sed -n 's/^ *Absolute upper-left X: *//p')"
  y="$(printf '%s\n' "$info" | sed -n 's/^ *Absolute upper-left Y: *//p')"
  [ -n "$w" ] && [ -n "$h" ] && [ -n "$x" ] && [ -n "$y" ] || return 1
  printf '%s %s %s %s\n' "$w" "$h" "$x" "$y"
}

# Starts ffmpeg recording this terminal's window to $1 (target size $2,
# unless $3 is 1) in the background and sets $capture_pid. Exits loudly on
# anything that stops it from finding a window to record, rather than
# silently falling back to a full-screen or wrong-window capture.
start_capture() {
  local out="$1" target_size="$2" no_resize="$3"

  [ -n "${DISPLAY:-}" ] || {
    cat >&2 <<EOF
demo-stack: --capture needs an X11 (or XWayland) session — \$DISPLAY is unset.

Native Wayland and macOS are not supported by this flag (ffmpeg's x11grab is
X11-only). Start your own screen recorder and run 'record' without --capture.
EOF
    exit 1
  }
  command -v ffmpeg >/dev/null 2>&1 || {
    printf 'demo-stack: --capture needs ffmpeg, which is not on PATH.\n' >&2
    exit 1
  }
  command -v xwininfo >/dev/null 2>&1 || {
    printf 'demo-stack: --capture needs xwininfo (Debian/Ubuntu: x11-utils; Fedora: xorg-x11-utils).\n' >&2
    exit 1
  }

  local id
  id="$(capture_window_id)" || {
    cat >&2 <<EOF
demo-stack: could not find this terminal's X window to capture.

Install xdotool for automatic detection, or set AXON_DEMO_WINDOW_ID yourself
— click this terminal after running 'xwininfo' (no -id) to print its id.
EOF
    exit 1
  }

  [ "$no_resize" -eq 1 ] || resize_window "$id" "$target_size" || true

  local geom w h x y
  geom="$(capture_geometry "$id")" || {
    cat >&2 <<EOF
demo-stack: xwininfo could not read window $id.

If this terminal is a native-Wayland client (no XWayland), it has no X
window and cannot be captured this way — start your own screen recorder and
run 'record' without --capture.
EOF
    exit 1
  }
  read -r w h x y <<<"$geom"
  # libx264 with yuv420p (4:2:0 chroma subsampling) refuses to open the
  # encoder unless both dimensions are even — real window sizes are
  # frequently odd, so this isn't a rare case. Cropping the last row/column
  # instead of erroring loses at most one pixel of window chrome.
  w=$((w - w % 2))
  h=$((h - h % 2))

  local log="$out.log"
  say "capturing window $id (${w}x${h}+${x}+${y}) to $out — do not move or resize it while recording"
  # Clear now, before ffmpeg's first frame: axon-demo-tui itself never prints
  # anything to the terminal during the run (see smoke/tui/src/demo/main.rs),
  # so anything visible in the recording's opening frames is leftover
  # scrollback from this session — this script's own status line above, the
  # command that launched it, an earlier `cargo build`. Wiping it here means
  # the take starts on a blank screen instead of the shell that launched it;
  # the pilot's own startup still takes a moment, so a brief blank stretch
  # before the first drawn frame is expected, not a bug.
  printf '\033[2J\033[H'
  ffmpeg -y -f x11grab -video_size "${w}x${h}" \
    -framerate "${AXON_DEMO_CAPTURE_FRAMERATE:-30}" -i "${DISPLAY}+${x},${y}" \
    -pix_fmt yuv420p -c:v libx264 -movflags +faststart \
    "$out" >"$log" 2>&1 &
  capture_pid=$!
  # Armed immediately, not after the health check below: a Ctrl-C in that
  # narrow window would otherwise leave ffmpeg running with nothing left to
  # stop it.
  trap stop_capture EXIT INT TERM

  # Give ffmpeg a moment to actually start before the pilot's first frame —
  # a slow codec/driver init would otherwise clip the opening of the take.
  sleep 0.5
  kill -0 "$capture_pid" 2>/dev/null || {
    capture_pid=""
    printf 'demo-stack: ffmpeg exited immediately — see %s\n' "$log" >&2
    exit 1
  }
}

# Stops the running capture (if any) and waits for ffmpeg to finalize the
# file. Idempotent — safe to call from both the normal record path and the
# EXIT/INT/TERM trap that catches everything else (Ctrl-C mid-take, a pilot
# crash, this script erroring out).
stop_capture() {
  [ -n "$capture_pid" ] || return 0
  if kill -0 "$capture_pid" 2>/dev/null; then
    # SIGTERM, not SIGINT: this script is non-interactive and ffmpeg is a
    # background (`&`) job, and POSIX has the shell set SIGINT (and SIGQUIT)
    # to be *ignored* by such jobs precisely so a Ctrl-C at the terminal
    # doesn't also kill them — so SIGINT here would silently do nothing.
    # ffmpeg treats SIGTERM exactly like SIGINT otherwise: it finalizes the
    # output file instead of truncating it.
    kill -TERM "$capture_pid" 2>/dev/null || true
    wait "$capture_pid" 2>/dev/null || true
  fi
  capture_pid=""
}

cmd="${1:-}"
[ "$#" -gt 0 ] && shift || true
# Everything after `--` belongs to the pilot.
[ "${1:-}" = "--" ] && shift || true

case "$cmd" in
  up)
    if [ -f "$manifest" ]; then
      cat >&2 <<EOF
demo-stack: $manifest already exists — a stack may still be running.

Tear it down first, or point somewhere else:

  scripts/demo-stack.sh down
  AXON_DEMO_MANIFEST=/tmp/other.json scripts/demo-stack.sh up
EOF
      exit 1
    fi
    photos_present || generate_photos
    say "booting the stack and rendering $corpus (this takes a couple of minutes)"
    cargo run -p axon-smoke-local-stack -- up \
      --manifest "$manifest" --corpus "$corpus" --keep-up
    cat <<EOF

demo-stack: the stack is up and will stay up. To record:

  scripts/demo-stack.sh record --capture      # ffmpeg records the window (X11/XWayland)
  start your screen recorder, then scripts/demo-stack.sh record   # anywhere else

When you are done:

  scripts/demo-stack.sh down
EOF
    ;;
  info)
    require_stack
    cargo run -p axon-smoke-local-stack -- info --manifest "$manifest"
    ;;
  record)
    require_stack
    capture=0
    capture_out="$capture_default_out"
    capture_size="$capture_default_size"
    no_resize=0
    size_given=0
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --capture)
          capture=1
          shift
          ;;
        --capture=*)
          capture=1
          capture_out="${1#--capture=}"
          shift
          ;;
        --size=*)
          capture_size="${1#--size=}"
          size_given=1
          shift
          ;;
        --size)
          [ "$#" -ge 2 ] || {
            printf 'demo-stack: --size requires a value, e.g. --size 1920x1080\n' >&2
            exit 1
          }
          capture_size="$2"
          size_given=1
          shift 2
          ;;
        --no-resize)
          no_resize=1
          shift
          ;;
        --)
          shift
          break
          ;;
        *)
          break
          ;;
      esac
    done
    if [ "$capture" -eq 0 ] && { [ "$no_resize" -eq 1 ] || [ "$size_given" -eq 1 ]; }; then
      printf 'demo-stack: --size/--no-resize only apply together with --capture\n' >&2
      exit 1
    fi
    if [ "$capture" -eq 1 ]; then
      capture_dir="$(dirname "${capture_out}")"
      if [ ! -d "${capture_dir}" ]
      then
        if ! mkdir -p "${capture_dir}"
        then
          cat >&2 <<EOF
	Error: could not create output directory ${capture_dir}. Exiting.
EOF
          exit 1
        fi
      fi
    fi
    # Built first so cargo's own progress output cannot land in the recording.
    cargo build -p axon-smoke-tui --bin axon-demo-tui
    if [ "$capture" -eq 1 ]; then
      start_capture "$capture_out" "$capture_size" "$no_resize"
      # Not exec'd here (unlike the plain path below): stop_capture has to
      # run after the pilot exits, in this same shell, to stop ffmpeg and
      # finalize the file.
      set +e
      ./target/debug/axon-demo-tui --manifest "$manifest" "$@"
      pilot_status=$?
      set -e
      stop_capture
      if [ "$pilot_status" -eq 0 ]; then
        say "wrote $capture_out"
      else
        say "pilot exited with status $pilot_status — $capture_out may be a partial take"
      fi
      exit "$pilot_status"
    else
      # exec'd so the pilot inherits this terminal — it reads the terminal
      # size and enters raw mode, so it cannot run down a pipe.
      exec ./target/debug/axon-demo-tui --manifest "$manifest" "$@"
    fi
    ;;
  down)
    require_stack
    cargo run -p axon-smoke-local-stack -- down --manifest "$manifest"
    rm -f "$manifest"
    ;;
  photos)
    if photos_present; then
      say "photographs already present in $photo_dir — replacing them"
    fi
    generate_photos
    ;;
  ""|-h|--help|help)
    usage
    exit 0
    ;;
  *)
    printf 'demo-stack: unknown command %s\n\n' "$cmd" >&2
    usage
    exit 2
    ;;
esac
