#!/usr/bin/env bash
#
# Generates third-party license notice files THIRDPARTY
#
# Usage:
#   scripts/generate-thirdparty.sh
set -euo pipefail
if command -v cargo-about > /dev/null 2>&1;
then
  echo "cargo-about already installed."
else
  echo "Installing cargo-about..."
  cargo install cargo-about --features="cli"
  echo "Success."
fi
echo "Generating plaintext build/THIRDPARTY notice..."
cargo-about generate --config ./build/about.toml ./build/about-plain.hbs > build/THIRDPARTY
echo "Success."
echo "Generating markdown THIRDPARTY.md notice..."
cargo-about generate --config ./build/about.toml ./build/about-markdown.hbs > ./THIRDPARTY.md
echo "Success."

# The desktop shell is a separate cargo workspace (ADR 0102), so the runs above
# cannot see it: `cargo-about` walks one dependency graph, and `src-tauri` is
# not in the root one. Until this existed, every crate compiled into the desktop
# binary was absent from both notices above.
#
# `--manifest-path` rather than a second config wholesale: the accepted-licence
# list is deliberately the same, and duplicating policy is how two lists drift.
# `build/about-desktop.toml` differs only in the targets it names and in saying
# why it exists.
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
