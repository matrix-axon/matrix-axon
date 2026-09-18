#!/usr/bin/env python3
"""Rasterise everything derived from `clients/web/public/favicon.svg`.

That file is the one artwork master. The Tauri desktop set has `tauri icon` to
derive its 52 files; everything else is derived here, and before this script
existed those files were unexplained binaries with no recorded provenance.

Two groups, with different rules, all of which are load-bearing:

**The browser client's icons** (`clients/web/public/`). `favicon.png` keeps the
master's transparency, because a browser tab composites it onto its own chrome,
light or dark. The `icon-*.png` apple-touch icons are opaque, because iOS
renders a transparent one against black and the glyph would sit in a dark
square on a home screen. White matches the manifest's `background_color` and
the background `tauri icon` gives the iOS set, so the mark looks the same
wherever a phone shows it.

**Brand marks** (`docs/brand/`). For slides, documents and anywhere outside the
app, where a transparent PNG lands on an unknown background and a white one
leaves a visible box on a coloured slide. Two, because one is not enough: a
light variant for pages that are already white-ish, and an inverted one for
dark or coloured surfaces.

The light background is a pale tint of the brand colour rather than its true
complement. The complement of `#5142E6` is `#D7E642`, a yellow-green, and it is
both off-brand and the *worst* of the candidates on contrast — 4.7:1 against
the glyph, where white gives 6.4:1 and this tint 5.6:1. Complementary colours
maximise hue separation, which is not the same as being legible or looking
deliberate.

SVG as well as PNG for both, since a document or a slide scales them.

Usage: scripts/build-brand-assets.py
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MASTER = ROOT / "clients/web/public/favicon.svg"

BRAND = "#5142E6"
TINT = "#EEEDFD"
WHITE = "#FFFFFF"


def rasterise(source: Path, size: int, out: Path, background: str | None) -> None:
    command = ["rsvg-convert", "-w", str(size), "-h", str(size), str(source), "-o", str(out)]
    if background is not None:
        command[1:1] = ["-b", background]
    subprocess.run(command, check=True)


def on_background(svg: str, background: str, glyph: str) -> str:
    """The master with an opaque backdrop, and the glyph recoloured to suit.

    Anchored on the `<svg>` tag rather than the first `>` in the file. An XML
    declaration, a leading comment and a DOCTYPE are all legal before it, and
    any of them would put the backdrop outside the document — malformed markup
    that nothing would notice until somebody opened the result.
    """
    rect = f'<rect width="100%" height="100%" fill="{background}"/>'
    opening = re.search(r"<svg\b[^>]*>", svg)
    if opening is None:
        raise ValueError(f"no <svg> element in {MASTER}")
    head = opening.end()
    return svg[:head] + "\n  " + rect + svg[head:].replace(BRAND, glyph)


def main() -> int:
    if shutil.which("rsvg-convert") is None:
        print("rsvg-convert is required (apt install librsvg2-bin)", file=sys.stderr)
        return 1

    public = ROOT / "clients/web/public"
    # Transparent, at the size the manifest declares for it.
    rasterise(MASTER, 406, public / "favicon.png", None)
    # Opaque; see above.
    for size in (152, 167, 180):
        rasterise(MASTER, size, public / f"icon-{size}.png", WHITE)

    brand = ROOT / "docs/brand"
    brand.mkdir(parents=True, exist_ok=True)
    master = MASTER.read_text()
    for name, background, glyph in (
        ("axon-mark-light", TINT, BRAND),
        ("axon-mark-dark", BRAND, WHITE),
    ):
        svg = brand / f"{name}.svg"
        svg.write_text(on_background(master, background, glyph))
        rasterise(svg, 1024, brand / f"{name}.png", None)

    print("regenerated clients/web/public/ icons and docs/brand/ marks")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
