#!/usr/bin/env bash
# Rasterise the browser client's icons from clients/web/public/favicon.svg.
#
# The Tauri desktop set has `tauri icon` to do this; the browser set has no
# equivalent, and these four files were previously unexplained binaries with no
# recorded provenance. Run this after changing the master, alongside
# `pnpm exec tauri icon` — the `icons-regenerated` pre-push hook requires both.
#
# Two conventions, both load-bearing and both pre-existing:
#
#   favicon.png     transparent. A browser tab composites it onto whatever the
#                   chrome is, light or dark.
#   icon-*.png      opaque. iOS renders an apple-touch-icon with transparency
#                   against black, so the glyph would sit in a dark square on
#                   a home screen. White matches the manifest's
#                   background_color and the background `tauri icon` gives the
#                   iOS set, so the mark looks the same on every home screen.
set -euo pipefail

cd "$(dirname "$0")/.."
src=clients/web/public/favicon.svg
out=clients/web/public

command -v rsvg-convert >/dev/null || {
  echo "rsvg-convert is required (apt install librsvg2-bin)" >&2
  exit 1
}

# Transparent, at the size the manifest declares for it.
rsvg-convert -w 406 -h 406 "$src" -o "$out/favicon.png"

# Opaque; see above.
for size in 152 167 180; do
  rsvg-convert -w "$size" -h "$size" -b white "$src" -o "$out/icon-$size.png"
done

echo "regenerated $out/{favicon.png,icon-152.png,icon-167.png,icon-180.png}"
