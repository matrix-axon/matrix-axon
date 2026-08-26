#!/usr/bin/env bash
#
# One-shot pipeline for the ADR 0086 web demo recordings: bring the demo
# world up, drive the Playwright demo lane against it, stitch the per-scene
# clips into the single ordered "tour" video each platform needs, and
# optionally publish them to the demo release.
#
# `clients/web/e2e/demo/desktop.spec.ts` and `mobile.spec.ts` deliberately
# record one clip per scene rather than a single continuous take (see that
# file's module docstring) — "the output is a set of per-scene clips an
# editor can cut". This script is that editor. `demo.html` and
# `.github/workflows/api-docs.yml` both expect exactly one
# web-demo-desktop.mp4 and one web-demo-mobile.mp4, walking the scenes in the
# order `demo.html` lists them — the SCENE arrays below encode that order and
# must be kept in sync with it (and with the `test()` order in the spec
# files, which currently matches).
#
# Usage:
#   scripts/demo-web.sh up                    # boot the stack (delegates to demo-stack.sh)
#   scripts/demo-web.sh record [-- <args>]    # run the Playwright demo lane
#   scripts/demo-web.sh assemble              # concat + transcode into web-demo-*.mp4
#   scripts/demo-web.sh upload                # attach both MP4s to the demo release
#   scripts/demo-web.sh down                  # tear the stack down (delegates to demo-stack.sh)
#   scripts/demo-web.sh all [--upload] [--keep-up] [-- <args>]
#                                              # up -> record -> assemble -> [upload] -> [down]
#
# `record` and `all` pass anything after `--` straight through to
# `pnpm demo`, e.g. `-- --grep rooms` or `-- --project demo-mobile` while
# authoring a single scene.
#
# Environment:
#   AXON_DEMO_MANIFEST   manifest path, shared with demo-stack.sh (default /tmp/axon-demo.json)
#   DEMO_RELEASE_TAG     release to upload to (default: the repo's latest release —
#                         matches api-docs.yml's own default)
#   DEMO_RELEASE_REPO    owner/repo to upload to (default: the `upstream` remote, falling back to `origin`)
#   DEMO_PACE, DEMO_PORT read directly by the demo lane itself — see clients/web/README.md
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

manifest="${AXON_DEMO_MANIFEST:-/tmp/axon-demo.json}"
web_dir="$repo_root/clients/web"
artifacts_dir="$web_dir/demo-artifacts"
out_desktop="$artifacts_dir/web-demo-desktop.mp4"
out_mobile="$artifacts_dir/web-demo-mobile.mp4"

# The tour order from demo.html's <ul> under each of #web-desktop and
# #web-mobile — not an independent choice. If a scene is added, renamed, or
# reordered, update demo.html first and mirror the change here.
DESKTOP_SCENES=(rooms spaces timeline threads media search send shortcuts)
MOBILE_SCENES=(rooms timeline threads media search send)

usage() {
  cat >&2 <<'EOF'
usage: scripts/demo-web.sh <up|record|assemble|upload|down|all> [options]

  up        boot Synapse + axon and render the demo corpus (via demo-stack.sh up)
  record    run the Playwright demo lane against the running stack
            (pass extra args after `--`, e.g. `-- --grep rooms`)
  assemble  concatenate each platform's scene clips, in demo.html's order,
            and transcode to demo-artifacts/web-demo-{desktop,mobile}.mp4
  upload    attach both MP4s to the demo release with `gh release upload`
  down      tear the stack down (via demo-stack.sh down)
  all       up -> record -> assemble -> [upload] -> [down]
              --upload   also run `upload` after assembling
              --keep-up  skip the final `down` (e.g. to re-record a scene)

Manifest path: $AXON_DEMO_MANIFEST, default /tmp/axon-demo.json
EOF
}

say() { printf '\ndemo-web: %s\n' "$1"; }
die() {
  printf 'demo-web: %s\n' "$1" >&2
  exit 1
}

assemble_list=""
cleanup_assemble_list() {
  [ -z "$assemble_list" ] || rm -f -- "$assemble_list"
}
trap cleanup_assemble_list EXIT

cmd_up() {
  AXON_DEMO_MANIFEST="$manifest" "$repo_root/scripts/demo-stack.sh" up
}

cmd_down() {
  AXON_DEMO_MANIFEST="$manifest" "$repo_root/scripts/demo-stack.sh" down
}

cmd_record() {
  [ -f "$manifest" ] || die "no manifest at $manifest — run 'scripts/demo-web.sh up' first"
  # Clear stale clips before recording: a scene rename or removal leaves an
  # old directory behind that `assemble`'s glob would otherwise still find,
  # silently stitching yesterday's clip into today's tour.
  if [ -d "$artifacts_dir" ]; then
    say "clearing $artifacts_dir"
    rm -rf "${artifacts_dir:?}"/*
  fi
  say "recording the demo lane (this takes a few minutes)"
  ( cd "$web_dir" && DEMO_MANIFEST="$manifest" pnpm demo "$@" )
}

# find_scene_dir desktop rooms demo-desktop  ->  prints the one matching
# demo-artifacts/desktop-rooms*demo-desktop/, or dies loudly.
#
# Playwright's directory names are the sanitized test title (truncated with a
# content hash when long) joined to the project name, not something this
# script controls — so this matches on the scene keyword as a prefix rather
# than assuming an exact name, and refuses to guess if that is ambiguous.
find_scene_dir() {
  local form="$1" scene="$2" project="$3"
  local -a matches=()
  shopt -s nullglob
  matches=("$artifacts_dir/${form}-${scene}"*"${project}")
  shopt -u nullglob
  case "${#matches[@]}" in
    1) printf '%s\n' "${matches[0]}" ;;
    0)
      die "no clip matches ${form}-${scene}*${project} in $artifacts_dir — did 'record' run, or did the scene get renamed? Check e2e/demo/${form}.spec.ts against the SCENE list in this script."
      ;;
    *)
      die "${#matches[@]} clips match ${form}-${scene}*${project} in $artifacts_dir — expected exactly one: ${matches[*]}"
      ;;
  esac
}

# assemble_platform desktop demo-desktop web-demo-desktop.mp4 "${DESKTOP_SCENES[@]}"
assemble_platform() {
  local form="$1" project="$2" out="$3"
  shift 3
  local scenes=("$@")

  # Keep the list in a shell-global variable so the EXIT trap also removes it
  # when find_scene_dir or the input validation below calls die().
  local list
  list="$(mktemp)"
  assemble_list="$list"
  local scene dir
  for scene in "${scenes[@]}"; do
    dir="$(find_scene_dir "$form" "$scene" "$project")"
    [ -s "$dir/video.webm" ] || die "$dir/video.webm is missing or empty"
    printf "file '%s'\n" "$dir/video.webm" >>"$list"
  done

  say "assembling $out from ${#scenes[@]} scenes (${scenes[*]})"
  ffmpeg -y -f concat -safe 0 -i "$list" \
    -c:v libx264 -pix_fmt yuv420p -movflags +faststart \
    "$out" </dev/null -loglevel error -stats
  rm -f "$list"
  assemble_list=""
  test -s "$out" || die "$out was not produced"
}

cmd_assemble() {
  command -v ffmpeg >/dev/null 2>&1 || die "ffmpeg is required — see clients/web/README.md § Demo recording"
  [ -d "$artifacts_dir" ] || die "no $artifacts_dir — run 'scripts/demo-web.sh record' first"
  assemble_platform desktop demo-desktop "$out_desktop" "${DESKTOP_SCENES[@]}"
  assemble_platform mobile demo-mobile "$out_mobile" "${MOBILE_SCENES[@]}"
  say "done:"
  ls -lh "$out_desktop" "$out_mobile"
}

# shellcheck source=scripts/lib/demo-release.sh
source "$repo_root/scripts/lib/demo-release.sh"

cmd_upload() {
  command -v gh >/dev/null 2>&1 || die "gh (GitHub CLI) is required to upload"
  [ -s "$out_desktop" ] || die "$out_desktop is missing — run 'scripts/demo-web.sh assemble' first"
  [ -s "$out_mobile" ] || die "$out_mobile is missing — run 'scripts/demo-web.sh assemble' first"
  local repo tag
  repo="$(demo_release_repo)"
  tag="$(demo_release_tag "$repo")"
  say "uploading to $repo release $tag (overwriting any existing web-demo-*.mp4 assets)"
  gh release upload "$tag" "$out_desktop" "$out_mobile" --repo "$repo" --clobber
  cat <<EOF

demo-web: uploaded. Re-run the api-docs workflow by hand to publish it —
re-uploading a release asset changes no path in the repo, so nothing in the
workflow's own 'paths:' trigger fires on its own:

  gh workflow run api-docs.yml --repo $repo
EOF
}

do_upload=0
keep_up=0
extra_args=()

cmd="${1:-}"
[ "$#" -gt 0 ] && shift || true

if [ "$cmd" = "all" ]; then
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --upload) do_upload=1 ;;
      --keep-up) keep_up=1 ;;
      --)
        shift
        extra_args=("$@")
        break
        ;;
      *) die "unknown option to 'all': $1" ;;
    esac
    shift
  done
fi
[ "${1:-}" = "--" ] && shift || true

case "$cmd" in
  up) cmd_up ;;
  record) cmd_record "$@" ;;
  assemble) cmd_assemble ;;
  upload) cmd_upload ;;
  down) cmd_down ;;
  all)
    cmd_up
    cmd_record "${extra_args[@]}"
    cmd_assemble
    [ "$do_upload" -eq 1 ] && cmd_upload
    if [ "$keep_up" -eq 1 ]; then
      say "leaving the stack up (--keep-up) — tear it down with: scripts/demo-web.sh down"
    else
      cmd_down
    fi
    ;;
  ""|-h|--help|help)
    usage
    exit 0
    ;;
  *)
    printf 'demo-web: unknown command %s\n\n' "$cmd" >&2
    usage
    exit 2
    ;;
esac
