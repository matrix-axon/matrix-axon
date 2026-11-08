#!/usr/bin/env bash
#
# Generates the third-party license notices.
#
# Usage:
#   scripts/generate-thirdparty.sh                 # root workspace and desktop shell
#   scripts/generate-thirdparty.sh --root-only     # build/THIRDPARTY + THIRDPARTY.md
#   scripts/generate-thirdparty.sh --desktop-only  # the shell's two notices only
#
# The two workspaces are separable on purpose. `cross-build.yml` builds the
# server and the TUI and needs only the root notice, so it passes --root-only:
# without it, resolving and license-checking the shell's dependency graph would
# be on the critical path of a server release, and a crate in the shell with a
# license outside `build/about-desktop.toml`'s accepted list could block a
# release that ships none of it. `desktop-build.yml` passes --desktop-only for
# the same reason in the other direction. Running with no flag is the local
# "regenerate everything" case.
#
# Two stages. cargo-about resolves the dependency graph and emits JSON;
# `scripts/thirdparty-notice.py` renders the notices from it. The old
# Handlebars templates are gone because cargo-about prints one block per
# distinct license *text*, so crates whose MIT texts differ only in the
# copyright line each got a full block (272 of them, 73% of the root notice),
# and a template cannot merge them. The renderer prints each distinct text once
# and keeps every crate's own copyright statement. See its docstring, and #855.
# The static title and preamble of each notice live in
# `build/notice-header-<workspace>-<format>.txt`.
#
# One pinned cargo-about, here and in CI — `.github/workflows/desktop-build.yml`
# runs this script and keys its cache on this file, so bumping the pin here is
# the whole procedure. The output format changes between versions (regenerating
# the root notice on a newer one came back 1,150 lines different with no crate
# added or removed), so with a floating version "is the committed notice
# stale?" has no answer, and the desktop job asks exactly that. Note this means
# the script *replaces* whatever cargo-about is already on PATH with the pinned
# version, which is deliberate but will surprise anyone who keeps their own.
set -euo pipefail
CARGO_ABOUT_VERSION=0.9.2

root=true
desktop=true
for arg in "$@"; do
  case "$arg" in
    --desktop-only) root=false ;;
    --root-only) desktop=false ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done
if [ "$root" = false ] && [ "$desktop" = false ]; then
  echo "--root-only and --desktop-only are mutually exclusive" >&2
  exit 2
fi

if [ "$(cargo-about --version 2> /dev/null || true)" = "cargo-about $CARGO_ABOUT_VERSION" ]; then
  echo "cargo-about $CARGO_ABOUT_VERSION already installed."
else
  echo "Installing cargo-about $CARGO_ABOUT_VERSION..."
  cargo install "cargo-about@$CARGO_ABOUT_VERSION" --locked --features=cli
  echo "Success."
fi

command -v python3 > /dev/null || { echo "python3 is required to render the notices" >&2; exit 1; }
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
scratch="$(mktemp -d)"
trap 'rm -rf "$scratch"' EXIT

# render JSON FORMAT HEADER OUT
render() {
  "$here/thirdparty-notice.py" --format "$2" --header "$3" "$1" > "$4"
}

if [ "$root" = true ]; then
  echo "Resolving the root dependency graph..."
  cargo-about generate --format json --config ./build/about.toml > "$scratch/root.json"
  echo "Generating plaintext build/THIRDPARTY notice..."
  render "$scratch/root.json" plain build/notice-header-root-plain.txt build/THIRDPARTY
  echo "Success."
  echo "Generating markdown THIRDPARTY.md notice..."
  render "$scratch/root.json" markdown build/notice-header-root-markdown.txt ./THIRDPARTY.md
  echo "Success."
fi

# The desktop shell is a separate cargo workspace (ADR 0102), so the runs above
# cannot see it: `cargo-about` walks one dependency graph, and `src-tauri` is
# not in the root one. Until this existed, every crate compiled into the desktop
# binary was absent from both notices above.
#
# `--manifest-path` rather than a second config wholesale: the accepted-license
# list is deliberately the same, and duplicating policy is how two lists drift.
# `build/about-desktop.toml` differs only in the targets it names and in saying
# why it exists.
#
# The plaintext notice is what the installers ship: `bundle.resources` in
# `clients/web/src-tauri/tauri.conf.json` puts it inside the .deb, .dmg and
# .exe as `THIRDPARTY.txt`. It is committed for that reason, and CI regenerates
# it and fails on any difference, so a dependency change that forgets this
# script cannot merge.
if [ "$desktop" = true ]; then
  DESKTOP_MANIFEST=clients/web/src-tauri/Cargo.toml
  echo "Resolving the desktop dependency graph..."
  cargo-about generate \
    --format json \
    --manifest-path "$DESKTOP_MANIFEST" \
    --config ./build/about-desktop.toml > "$scratch/desktop.json"
  echo "Generating plaintext build/THIRDPARTY-desktop notice..."
  render "$scratch/desktop.json" plain build/notice-header-desktop-plain.txt build/THIRDPARTY-desktop
  echo "Success."
  echo "Generating markdown clients/web/src-tauri/THIRDPARTY.md notice..."
  render "$scratch/desktop.json" markdown build/notice-header-desktop-markdown.txt clients/web/src-tauri/THIRDPARTY.md
  echo "Success."
fi
