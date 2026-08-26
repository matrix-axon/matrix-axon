#!/usr/bin/env bash
#
# Single entry point for both ADR 0086 demo recordings (TUI and web): run
# either one on demand under one command namespace, or record and publish
# everything in one pass. It orchestrates `demo-stack.sh` and `demo-web.sh`
# rather than reimplementing them — this file owns no recording logic of its
# own, only sequencing and the combined upload.
#
# The two legs are not equally automatable, and `all` cannot paper over that:
#   - the web leg is genuinely headless — Playwright drives a built server,
#     nothing but Docker is required.
#   - the TUI leg needs a real terminal emulator window on a live X11 or
#     XWayland desktop session for `--capture` to find and record (that is
#     what makes Sixel/Kitty/iTerm2 graphics exist to capture at all — see
#     demo-stack.sh). There is no way to do this from a CI runner.
# So `all` still assumes it is being run *from* that kind of desktop
# session — same assumption `demo-stack.sh record --capture` already makes —
# and fails loudly rather than silently skipping the TUI video if it isn't
# one. Use --web-only there on purpose instead.
#
# Usage:
#   scripts/demo-all.sh up                       # boot the stack (delegates to demo-stack.sh)
#   scripts/demo-all.sh tui [--capture] [-- <args>]   # = demo-stack.sh record
#   scripts/demo-all.sh web [-- <args>]               # = demo-web.sh record
#   scripts/demo-all.sh assemble                 # = demo-web.sh assemble (web only; the TUI
#                                                 #   capture is already one finished file)
#   scripts/demo-all.sh upload [--tui-only|--web-only]
#                                                 # attach whichever of tui-demo.mp4 /
#                                                 # web-demo-{desktop,mobile}.mp4 exist
#   scripts/demo-all.sh down                     # tear the stack down (delegates to demo-stack.sh)
#   scripts/demo-all.sh all [--tui-only|--web-only] [--upload] [--keep-up]
#                                                 # up -> tui --capture -> web -> assemble
#                                                 #    -> [upload] -> [down]
#
# `all` runs the fixed full tour on both platforms and takes no pilot/
# Playwright passthrough — use `tui`/`web` directly for that (e.g. to
# re-record one scene while authoring).
#
# Environment: same as demo-stack.sh / demo-web.sh — AXON_DEMO_MANIFEST,
# AXON_DEMO_WINDOW_ID, AXON_DEMO_CAPTURE_FRAMERATE, DEMO_RELEASE_TAG,
# DEMO_RELEASE_REPO. See those scripts' own headers.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

stack="$repo_root/scripts/demo-stack.sh"
web="$repo_root/scripts/demo-web.sh"
tui_out="$repo_root/demo-artifacts/tui-demo.mp4"
web_out_desktop="$repo_root/clients/web/demo-artifacts/web-demo-desktop.mp4"
web_out_mobile="$repo_root/clients/web/demo-artifacts/web-demo-mobile.mp4"

usage() {
  cat >&2 <<'EOF'
usage: scripts/demo-all.sh <up|tui|web|assemble|upload|down|all> [options]

  up        boot Synapse + axon and render the demo corpus (= demo-stack.sh up)
  tui [--capture] [-- <args>]
            run the TUI pilot (= demo-stack.sh record)
  web [-- <args>]
            run the Playwright demo lane (= demo-web.sh record)
  assemble  stitch the web scene clips into web-demo-{desktop,mobile}.mp4
            (= demo-web.sh assemble; the TUI capture needs no assembly)
  upload [--tui-only|--web-only]
            attach whichever of tui-demo.mp4 / web-demo-{desktop,mobile}.mp4
            exist to the resolved release
  down      tear the stack down (= demo-stack.sh down)
  all [--tui-only|--web-only] [--upload] [--keep-up]
            up -> tui --capture -> web -> assemble -> [upload] -> [down]
            --tui-only   just the TUI leg: up -> tui --capture -> down. A
                         single batch run of the TUI demo alone — this is
                         the answer to "do I need 'up' before 'tui'?"
            --web-only   just the web leg: up -> web -> assemble -> down.
                         No X11/XWayland needed at all this way.
            --upload     also run 'upload' once the selected leg(s) are done
            --keep-up    skip the final 'down' (e.g. to inspect, or chain
                         into 'tui'/'web' again without waiting on a reboot)

`tui`/`web` pass anything after `--` straight through to the underlying
pilot/Playwright run. `all` does not — it is the fixed full tour on both
platforms; use `tui`/`web` directly for single-scene work.
EOF
}

say() { printf '\ndemo-all: %s\n' "$1"; }
die() {
  printf 'demo-all: %s\n' "$1" >&2
  exit 1
}

# shellcheck source=scripts/lib/demo-release.sh
source "$repo_root/scripts/lib/demo-release.sh"

cmd_upload() {
  local scope="${1:-both}"
  command -v gh >/dev/null 2>&1 || die "gh (GitHub CLI) is required to upload"

  local files=()
  if [ "$scope" != "web" ] && [ -s "$tui_out" ]; then
    files+=("$tui_out")
  fi
  if [ "$scope" != "tui" ]; then
    [ -s "$web_out_desktop" ] && files+=("$web_out_desktop")
    [ -s "$web_out_mobile" ] && files+=("$web_out_mobile")
  fi
  [ "${#files[@]}" -gt 0 ] ||
    die "nothing to upload — run 'tui --capture' and/or 'web' + 'assemble' first"

  local repo tag
  repo="$(demo_release_repo)"
  tag="$(demo_release_tag "$repo")"
  say "uploading to $repo release $tag:"
  printf '  %s\n' "${files[@]}"
  gh release upload "$tag" "${files[@]}" --repo "$repo" --clobber
  cat <<EOF

demo-all: uploaded. Re-run the api-docs workflow by hand to publish it —
re-uploading a release asset changes no path in the repo, so nothing in the
workflow's own 'paths:' trigger fires on its own:

  gh workflow run api-docs.yml --repo $repo
EOF
}

upload_for_scope() {
  if [ "$tui_only" -eq 1 ]; then
    cmd_upload tui
  elif [ "$web_only" -eq 1 ]; then
    cmd_upload web
  else
    cmd_upload both
  fi
}

cmd="${1:-}"
[ "$#" -gt 0 ] && shift || true

tui_only=0
web_only=0
do_upload=0
keep_up=0

if [ "$cmd" = "all" ] || [ "$cmd" = "upload" ]; then
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --tui-only) tui_only=1 ;;
      --web-only) web_only=1 ;;
      --upload) [ "$cmd" = "all" ] || die "--upload only applies to 'all'"; do_upload=1 ;;
      --keep-up) [ "$cmd" = "all" ] || die "--keep-up only applies to 'all'"; keep_up=1 ;;
      *) die "unknown option to '$cmd': $1" ;;
    esac
    shift
  done
  [ "$tui_only" -eq 1 ] && [ "$web_only" -eq 1 ] && die "--tui-only and --web-only are mutually exclusive"
fi

case "$cmd" in
  up) exec "$stack" up ;;
  down) exec "$stack" down ;;
  tui) exec "$stack" record "$@" ;;
  web) exec "$web" record "$@" ;;
  assemble) exec "$web" assemble ;;
  upload) upload_for_scope ;;
  all)
    "$stack" up
    [ "$web_only" -eq 1 ] || "$stack" record --capture
    if [ "$tui_only" -eq 0 ]; then
      "$web" record
      "$web" assemble
    fi
    if [ "$do_upload" -eq 1 ]; then
      upload_for_scope
    fi
    if [ "$keep_up" -eq 1 ]; then
      say "leaving the stack up (--keep-up) — tear it down with: scripts/demo-all.sh down"
    else
      "$stack" down
    fi
    ;;
  ""|-h|--help|help)
    usage
    exit 0
    ;;
  *)
    printf 'demo-all: unknown command %s\n\n' "$cmd" >&2
    usage
    exit 2
    ;;
esac
