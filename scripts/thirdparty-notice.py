#!/usr/bin/env python3
"""Render the third-party license notices from `cargo-about generate --format json`.

Why this exists instead of a cargo-about Handlebars template
-----------------------------------------------------------

cargo-about groups crates by *exact* license text, so two MIT crates whose
LICENSE files differ only in the copyright line land in two blocks. In this
repository that was 272 MIT blocks (73% of the 12,300-line root notice), when
only 31 of them differ in anything but the copyright line. Handlebars has no
string-normalizing helpers, and cargo-about has no option to merge texts, so the
merge has to happen after cargo-about and before rendering. See #855.

What it does
------------

1. Pulls every copyright statement out of each license text.
2. Groups blocks whose remaining text is identical, ignoring whitespace.
3. Prints each distinct text once, with every crate that uses it listed under
   the copyright statement(s) taken from that crate's own license file.

The copyright statements are kept, not dropped: MIT, BSD and Apache-2.0 require
the notice to travel with redistributions, so a crate's holder stays attributed
to that crate. Nothing else in the text is normalized, so licenses that differ
in any other way (the Meta/Facebook zstd BSD-3 clause 3, a different title, an
extra paragraph) stay separate blocks. When in doubt this errs toward *not*
merging.

Every merged crate is checked to be lossless: its original words must equal the
printed shared text plus its own extracted copyright statements, word for word.
A violation aborts the run, so a bug here fails the build instead of silently
dropping license text.

Usage
-----

    cargo-about generate --format json --config build/about.toml \\
      | scripts/thirdparty-notice.py --format plain --header build/notice-header-root-plain.txt

`--header` is the static text printed before the OVERVIEW (title and any
preamble). Stdlib only.

Plain output is not HTML-escaped. The old Handlebars templates escaped it, so
the shipped `THIRDPARTY` files read `&quot;AS IS&quot;`. The markdown output
keeps the escaping (it is rendered as HTML, where a bare `<owner>` would
vanish).
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict

# A copyright *statement* line. Anchored and case-sensitive on purpose:
#   * Apache-2.0 wraps lines that begin "copyright license to reproduce" and
#     "copyright notice that is included"; a case-insensitive match would eat them.
#   * "COPYRIGHT HOLDERS BE LIABLE" (MIT disclaimer) and "COPYRIGHT AND PERMISSION
#     NOTICE" (Unicode) are upper-case boilerplate, not statements.
# A leading comment marker (`//`, `/*`, `*`, `#`) is allowed; ring's licenses use them.
_COMMENT = r"(?:(?://|/\*|\*|#)\s*)?"
_STATEMENT = re.compile(
    rf"^\s*{_COMMENT}Copyright\s*(?:\([cC]\)|©|[0-9]|[A-Z\[`])"
)
# The unfilled template that Apache-2.0's appendix and a few READMEs carry:
# "Copyright [yyyy] [name of copyright owner]". It is boilerplate shared by
# every crate, not anyone's attribution, so it stays in the shared text.
_PLACEHOLDER = re.compile(r"(?i)[\[{<]\s*(?:yyyy|year|name of|owner|copyright holders?)")
# "Copyright (c) 2016 Dropbox, Inc.\nAll rights reserved." is one statement.
_ALL_RIGHTS = re.compile(rf"^\s*{_COMMENT}All rights reserved\.?\s*$")
_MARKER = re.compile(rf"^\s*{_COMMENT}")

NO_STATEMENT = "(no copyright statement in the license text)"
INLINE = "(copyright statements are part of the license text below)"


def split_statements(text: str) -> tuple[str, list[str], bool]:
    """Return (text without copyright statements, the statements, inline).

    Statements are extracted only when they form a single cluster (a header or
    footer: statements separated by nothing but blank lines). A text that
    scatters them, like a file with one copyright line per embedded section,
    is left whole, because printing the statements apart from their sections
    would lose which holder goes with which text. `inline` reports that case:
    the text has statements but they were left in place.
    """
    lines = text.splitlines()
    found: list[tuple[int, int, str]] = []  # (first line, last line, statement)
    i = 0
    while i < len(lines):
        line = lines[i]
        if _STATEMENT.match(line) and not _PLACEHOLDER.search(line):
            statement = _MARKER.sub("", line, count=1).strip()
            last = i
            if i + 1 < len(lines) and _ALL_RIGHTS.match(lines[i + 1]):
                statement += " " + _MARKER.sub("", lines[i + 1], count=1).strip()
                last = i + 1
            found.append((i, last, statement))
            i = last
        i += 1
    if not found:
        return tidy(text), [], False
    taken = {n for first, last, _ in found for n in range(first, last + 1)}
    span = range(found[0][0], found[-1][1] + 1)
    if any(n not in taken and lines[n].strip() for n in span):
        return tidy(text), [], True
    kept = [ln for n, ln in enumerate(lines) if n not in taken]
    return tidy("\n".join(kept)), [st for _, _, st in found], False


def tidy(text: str) -> str:
    """Collapse the blank runs left behind by removed lines; trim the ends."""
    text = re.sub(r"\n[ \t]*(?:\n[ \t]*)+\n", "\n\n", text.strip("\n"))
    return text.strip("\n")


def key_of(text: str) -> str:
    return " ".join(text.split())


_BARE_MARKERS = {"//", "/*", "*", "#"}


def words(text: str) -> Counter:
    """Word multiset, ignoring bare comment markers.

    The markers are the one thing deliberately not carried over: a statement
    printed as "Copyright 2015 Brian Smith." needs no leading "//".
    """
    return Counter(w for w in text.split() if w not in _BARE_MARKERS)


class Block:
    """One distinct license text and the crates that use it."""

    def __init__(self, name: str, spdx: str, text: str):
        self.name = name
        self.spdx = spdx
        self.text = text
        self.crates: dict[tuple[str, str], list[str]] = {}  # (name, version) -> statements
        self.best = -1  # crate count of the entry that supplied `text`

    def groups(self) -> list[tuple[tuple[str, ...], list[tuple[str, str]]]]:
        """Crates grouped by identical statement set, in a stable order."""
        by_set: dict[tuple[str, ...], list[tuple[str, str]]] = defaultdict(list)
        for crate, statements in self.crates.items():
            by_set[tuple(dict.fromkeys(statements))].append(crate)
        out = [(s, sorted(c)) for s, c in by_set.items()]
        # Crates with no statement last; otherwise by first crate name.
        out.sort(key=lambda g: (not g[0], g[1][0]))
        return out


def merge(licenses: list[dict]) -> list[Block]:
    blocks: dict[tuple[str, str], Block] = {}
    for entry in licenses:
        remainder, statements, inline = split_statements(entry["text"])
        key = (entry["id"], key_of(remainder))
        block = blocks.get(key)
        if block is None:
            block = blocks[key] = Block(entry["name"], entry["id"], remainder)
        # The printed text comes from the entry with the most crates (ties keep
        # the first seen): it is the most representative formatting.
        if len(entry["used_by"]) > block.best:
            block.best = len(entry["used_by"])
            block.text = remainder
        for used in entry["used_by"]:
            crate = (used["crate"]["name"], used["crate"]["version"])
            # A crate can ship several license files that normalize to one
            # text (atomic-waker: LICENSE-MIT plus a LICENSE-THIRD-PARTY that
            # carries two holders). Accumulate, never overwrite.
            have = block.crates.setdefault(crate, [])
            shown = statements or ([INLINE] if inline else [])
            have.extend(st for st in shown if st not in have)
            # Lossless check, per crate: original words == shared words + own statements.
            assert words(entry["text"]) == words(remainder) + words("\n".join(statements)), (
                f"{crate[0]} {crate[1]}: license text changed by merging"
            )
    out = list(blocks.values())
    out.sort(key=lambda b: (b.spdx, min(b.crates)))
    return out


_HTML = {
    "&": "&amp;",
    "<": "&lt;",
    ">": "&gt;",
    '"': "&quot;",
    "'": "&#x27;",
    "`": "&#x60;",
    "=": "&#x3D;",
}


def html_escape(text: str) -> str:
    return "".join(_HTML.get(c, c) for c in text)


def render(data: dict, fmt: str, header: str) -> str:
    esc = html_escape if fmt == "markdown" else (lambda s: s)
    plural = lambda n: "" if n == 1 else "s"  # noqa: E731
    out: list[str] = [header.rstrip("\n"), ""]
    heading = "## " if fmt == "markdown" else ""
    sub = "### " if fmt == "markdown" else ""
    out.append(f"{heading}OVERVIEW")
    for item in data["overview"]:
        out.append(f"- {esc(item['name'])} (used by {item['count']} crate{plural(item['count'])})")
    out.append("")
    out.append(f"{heading}LICENSE DETAILS")

    rule = "=" * 80
    for block in merge(data["licenses"]):
        out += [
            "",
            rule,
            f"{heading}LICENSE: {esc(block.name)} (SPDX ID: {esc(block.spdx)})",
            f"{sub}USED BY:",
        ]
        for statements, crates in block.groups():
            lines = list(statements) or [NO_STATEMENT]
            out.append(f"  - {esc(lines[0])}")
            out += [f"    {esc(s)}" for s in lines[1:]]
            out += [f"      - {esc(n)} {esc(v)}" for n, v in crates]
        out += [rule, esc(block.text), ""]
        out.append("-----------------------" if fmt == "markdown" else "-" * 80)
    return "\n".join(out) + "\n"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--format", choices=("plain", "markdown"), required=True)
    ap.add_argument("--header", required=True, help="static text printed before the OVERVIEW")
    ap.add_argument("input", nargs="?", default="-", help="cargo-about JSON (default: stdin)")
    args = ap.parse_args()
    if args.input == "-":
        raw = sys.stdin.read()
    else:
        with open(args.input, encoding="utf-8") as fh:
            raw = fh.read()
    with open(args.header, encoding="utf-8") as fh:
        header = fh.read()
    sys.stdout.write(render(json.loads(raw), args.format, header))
    return 0


if __name__ == "__main__":
    sys.exit(main())
