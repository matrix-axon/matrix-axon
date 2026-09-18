#!/usr/bin/env python3
"""Assert the icon sets were regenerated when their shared source changed.

`clients/web/public/favicon.svg` is the one master for both clients. The
browser loads it directly as a favicon; everything else is rasterised from it
and committed rather than built:

- `clients/web/src-tauri/icons/` — 52 files, by `pnpm exec tauri icon`
- `clients/web/public/{favicon,icon-152,icon-167,icon-180}.png` and the
  `docs/brand/` marks — by `scripts/build-brand-assets.py`

Changing the master without rerunning both ships the old artwork, and nothing
would notice: no build reads the SVG except as a static asset, and the five
files `tauri.conf.json` bundles all live under `icons/`. That has already
happened once here — the committed desktop set had been generated from a
source two commits behind the one beside it, so all 52 disagreed with it.

## Why this checks the coupling instead of the content

The obvious hook regenerates into a temp directory and diffs. That does not
work, and the reason is worth recording so nobody tries it again.

`tauri icon` is not reproducible. Two consecutive runs over one source produce
51 byte-identical files and one that differs: `icon.icns` packs its members in
a nondeterministic order (`icns....ic10` one run, `icns....ic07` the next --
same size, same images, different order). A content check would be flaky on
that file forever.

Regenerating during the *build* is worse for the same reason: every desktop
build would ship a different `.icns` than the last, which is noise now and a
problem once macOS notarization signs over those bytes.

So this asserts only what is cheap and certain -- that they changed together --
and leaves the artwork itself to review, where a wrong icon is visible anyway.
The inverse (regenerating without touching the source) is fine and is not
flagged: a Tauri CLI upgrade legitimately produces that.

## Why this reads the range itself

`pass_filenames` is off, deliberately. pre-commit shards a hook's file list
across parallel invocations, so a hook given filenames sees a *subset* of the
push -- fine for a linter, whose answer is per-file, and useless here, where
the question is whether two groups of files moved *together*. A shard holding
the master and none of its outputs is indistinguishable from a push that forgot
to regenerate. That shipped once, and rejected a correct push.

So pre-commit decides only *whether* to run this (the `files:` filter), and the
range comes from `PRE_COMMIT_FROM_REF`/`PRE_COMMIT_TO_REF`, which it sets for
pre-push hooks. Arguments still work for running it by hand.

That also means this is a *local* gate only: it is not in the CI whitelist in
`.github/workflows/lint-and-clippy.yml`, so `--no-verify` and GitHub's merge
button both bypass it. Wiring it into CI needs a range that a `--all-files` run
does not have; tracked in #424.

Usage: scripts/check-icons-regenerated.py [<changed file>...]
"""

from __future__ import annotations

import os
import subprocess
import sys

SOURCE = "clients/web/public/favicon.svg"

# Named exactly, not matched by shape. "any .png under public/" was the first
# attempt and it is a false negative waiting to happen: an unrelated static
# asset added in the same commit as a favicon change satisfies the check while
# the generator was never run — the precise drift this exists to catch.
BROWSER_ICONS = frozenset(
    f"clients/web/public/{name}"
    for name in ("favicon.png", "icon-152.png", "icon-167.png", "icon-180.png")
)

# Each entry is (what to call it, how to recognise it, how to rebuild it).
DERIVED = (
    (
        "the desktop set",
        lambda name: name.startswith("clients/web/src-tauri/icons/"),
        "cd clients/web && pnpm exec tauri icon public/favicon.svg -o src-tauri/icons",
    ),
    (
        "the browser set",
        lambda name: name in BROWSER_ICONS,
        "scripts/build-brand-assets.py",
    ),
    (
        "the brand marks",
        lambda name: name.startswith("docs/brand/axon-mark-"),
        "scripts/build-brand-assets.py",
    ),
)


def changed_files(argv: list[str]) -> set[str]:
    """Everything the push touches, or the paths named on the command line."""
    if len(argv) > 1:
        return set(argv[1:])
    before = os.environ.get("PRE_COMMIT_FROM_REF")
    after = os.environ.get("PRE_COMMIT_TO_REF")
    if not before or not after:
        # Not a pre-push run and nothing named: compare against the previous
        # commit, which is what someone poking at this by hand likely means.
        before, after = "HEAD~1", "HEAD"
    result = subprocess.run(
        ["git", "diff", "--name-only", f"{before}...{after}"],
        capture_output=True,
        text=True,
        check=False,
    )
    if result.returncode != 0:
        print(
            f"could not diff {before}...{after}; treating the push as unchanged",
            file=sys.stderr,
        )
        return set()
    return {line for line in result.stdout.splitlines() if line}


def main(argv: list[str]) -> int:
    changed = changed_files(argv)
    if SOURCE not in changed:
        return 0
    stale = [
        (label, command)
        for label, matches, command in DERIVED
        if not any(matches(name) for name in changed)
    ]
    if not stale:
        return 0
    missing = [label for label, _ in stale]
    if len(missing) > 1:
        missing = [", ".join(missing[:-1]), missing[-1]]
    print(f"{SOURCE} changed but {' and '.join(missing)} did not.", file=sys.stderr)
    print(file=sys.stderr)
    print("The icons are generated and committed, not built. Regenerate in the", file=sys.stderr)
    print("same commit:", file=sys.stderr)
    print(file=sys.stderr)
    seen: list[str] = []
    for _, command in stale:
        if command not in seen:
            seen.append(command)
            print(f"    {command}", file=sys.stderr)
    print(file=sys.stderr)
    print(
        "If you meant to change only the source -- to record better artwork for\n"
        "later, say -- commit the regenerated icons anyway. There is no supported\n"
        "state in which they disagree.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
