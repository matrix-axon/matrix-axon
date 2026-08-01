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
# Environment:
#   AXON_DEMO_MANIFEST  manifest path (default /tmp/axon-demo.json)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

manifest="${AXON_DEMO_MANIFEST:-/tmp/axon-demo.json}"
corpus="smoke/local-stack/corpus/demo.toml"
photo_dir="smoke/local-stack/corpus/media/photos"
generator="smoke/local-stack/corpus/media/generate-placeholder-photos.mjs"

usage() {
  cat >&2 <<'EOF'
usage: scripts/demo-stack.sh <up|info|record|down|photos> [-- <pilot args>]

  up      generate placeholder photos if missing, then boot Synapse + axon and
          render the demo corpus into it, leaving it running
  info    reprint the running stack's connection summary
  record  run axon-demo-tui against the running stack (needs a real terminal;
          start your screen recorder first). Extra args after `--` are passed
          through, e.g. `-- --scene media --pace 1.5`
  down    stop the stack and remove its containers and volumes
  photos  (re)generate the placeholder photographs and stop

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

  start your screen recorder, then
  scripts/demo-stack.sh record

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
    # Built first so cargo's own progress output cannot land in the recording,
    # and exec'd so the pilot inherits this terminal — it reads the terminal
    # size and enters raw mode, so it cannot run down a pipe.
    cargo build -p axon-smoke-tui --bin axon-demo-tui
    exec ./target/debug/axon-demo-tui --manifest "$manifest" "$@"
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
