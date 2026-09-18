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

if [ "$root" = true ]; then
  echo "Generating plaintext build/THIRDPARTY notice..."
  cargo-about generate --config ./build/about.toml ./build/about-plain.hbs > build/THIRDPARTY
  echo "Success."
  echo "Generating markdown THIRDPARTY.md notice..."
  cargo-about generate --config ./build/about.toml ./build/about-markdown.hbs > ./THIRDPARTY.md
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
  echo "Generating plaintext build/THIRDPARTY-desktop notice..."
  cargo-about generate \
    --manifest-path "$DESKTOP_MANIFEST" \
    --config ./build/about-desktop.toml \
    ./build/about-desktop-plain.hbs > build/THIRDPARTY-desktop
  echo "Success."
  echo "Generating markdown clients/web/src-tauri/THIRDPARTY.md notice..."
  cargo-about generate \
    --manifest-path "$DESKTOP_MANIFEST" \
    --config ./build/about-desktop.toml \
    ./build/about-desktop-markdown.hbs > clients/web/src-tauri/THIRDPARTY.md
  echo "Success."
fi
