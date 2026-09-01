#!/usr/bin/env python3
"""Assert `src-tauri/icons/` was regenerated when its source artwork changed.

`icons/` is 52 files produced by `pnpm exec tauri icon` from
`src-tauri/icon-source.svg`, and they are committed rather than built. Changing
the source without rerunning the generator ships the old artwork: the five
files `tauri.conf.json` actually bundles are all under `icons/`, and nothing in
the build reads `icon-source.svg` at all. Nothing else would notice.

## Why this checks the coupling instead of the content

The obvious hook regenerates into a temp directory and diffs. That does not
work here, and the reason is worth recording so nobody tries it again.

`tauri icon` is not reproducible. Two consecutive runs over one source produce
51 byte-identical files and one that differs: `icon.icns` packs its members in
a nondeterministic order (`icns....ic10` one run, `icns....ic07` the next --
same size, same images, different order). So a content check would be flaky on
that file forever, and it would have to compare the `.icns` structurally to
avoid it.

Regenerating during the *build* is worse for the same reason: every desktop
build would ship a different `.icns` than the last, which is noise now and a
problem once macOS notarization signs over those bytes.

So this asserts only what is cheap and certain -- that the two changed
together -- and leaves the artwork itself to review, where a wrong icon is
visible anyway. The inverse (regenerating without touching the source) is fine
and is not flagged: a Tauri CLI upgrade legitimately produces that.

Usage: scripts/check-icons-regenerated.py <changed file>...
"""

from __future__ import annotations

import sys

SOURCE = "clients/web/src-tauri/icon-source.svg"
GENERATED = "clients/web/src-tauri/icons/"

REMEDY = f"""\
{SOURCE} changed but nothing under {GENERATED} did.

The icons are generated and committed, not built. Regenerate them in the same
commit:

    cd clients/web && pnpm exec tauri icon src-tauri/icon-source.svg -o src-tauri/icons

If you meant to change only the source -- to record better artwork for later,
say -- commit the regenerated icons anyway. There is no supported state in
which the two disagree.\
"""


def main(argv: list[str]) -> int:
    changed = set(argv[1:])
    if SOURCE not in changed:
        return 0
    if any(name.startswith(GENERATED) for name in changed):
        return 0
    print(REMEDY, file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
