#!/usr/bin/env python3
"""Fail if a link reachable from the built Pages site's homepage does not work.

Crawls the Jekyll output (``_site``) starting at ``index.html``, follows every
link that stays on the site, and reports the ones that would not give a reader a
working, rendered page:

* the target file does not exist in the site;
* the target is a directory with no ``index.html`` (GitHub Pages does not list
  directories, so it 404s);
* the target is a raw ``.md`` file: Jekyll copied it verbatim instead of
  rendering it, so the reader gets markdown source (or a download) instead of a
  page. This is the failure that ``jekyll-relative-links`` leaves silent;
* the link carries a ``#fragment`` and the target page has no such id.

Only pages reachable from the homepage are crawled, on purpose: Jekyll renders
every markdown file in the repository, but only the ones a reader can reach need
their links to work. Reachability is what the README promises, so it is what is
checked.

Usage: check-site-links.py SITE_DIR --site-url URL

``--site-url`` is the public root of the site (for example
``https://matrix-axon.github.io/matrix-axon/``). Absolute links beneath it, and
root-relative links beneath its path (``/matrix-axon/docs/x.html``), are mapped
back to files in SITE_DIR. Links to anything else are external and not checked.
"""

import argparse
import os
import posixpath
import sys
from collections import deque
from html.parser import HTMLParser
from urllib.parse import unquote, urldefrag, urlparse

# (tag, attribute) pairs that name something the reader navigates to or loads.
LINK_ATTRS = {("a", "href"), ("img", "src"), ("source", "src"), ("video", "src")}


class PageParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.links = []  # (tag, url)
        self.ids = set()
        self.base = None

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        for key in ("id", "name"):
            if attrs.get(key):
                self.ids.add(attrs[key])
        if tag == "base" and attrs.get("href"):
            self.base = attrs["href"]
        for (t, attr) in LINK_ATTRS:
            if tag == t and attrs.get(attr):
                self.links.append((tag, attrs[attr]))


def parse(path):
    parser = PageParser()
    with open(path, encoding="utf-8", errors="replace") as f:
        parser.feed(f.read())
    return parser


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("site_dir")
    ap.add_argument("--site-url", required=True)
    args = ap.parse_args()

    site = args.site_dir.rstrip("/")
    site_url = args.site_url.rstrip("/") + "/"
    site_path = urlparse(site_url).path  # e.g. /matrix-axon/

    def to_site_path(url, page):
        """Map a link found on `page` (a path relative to the site root) to a
        site-root-relative path, or None when the link is external."""
        url, _ = urldefrag(url)
        parsed = urlparse(url)
        if parsed.scheme in ("http", "https"):
            if not url.startswith(site_url) and url.rstrip("/") != site_url.rstrip("/"):
                return None
            return unquote(parsed.path[len(site_path):]) if parsed.path.startswith(site_path) else ""
        if parsed.scheme or url.startswith("//"):
            return None  # mailto:, tel:, data:, protocol-relative
        path = unquote(parsed.path)
        if path.startswith("/"):
            if path.startswith(site_path):
                return path[len(site_path):]
            return None  # root-relative but outside the site (e.g. /favicon.ico)
        if not path:
            return page  # pure "#fragment" link
        return posixpath.normpath(posixpath.join(posixpath.dirname(page), path))

    def resolve(rel):
        """Return (file, problem) for a site-root-relative path."""
        rel = posixpath.normpath(rel) if rel else ""
        rel = "" if rel == "." else rel
        full = posixpath.join(site, rel)
        if os.path.isdir(full):
            index = posixpath.join(full, "index.html")
            if os.path.isfile(index):
                return posixpath.join(rel, "index.html") if rel else "index.html", None
            return None, "is a directory with no index.html (404 on Pages)"
        if os.path.isfile(full):
            if full.endswith(".md"):
                return None, "is raw markdown (Jekyll did not render it to .html)"
            return rel, None
        return None, "does not exist in the built site"

    problems = []  # (page, url, why)
    seen = set()
    queue = deque(["index.html"])
    parsed_pages = {}

    while queue:
        page = queue.popleft()
        if page in seen:
            continue
        seen.add(page)
        info = parse(posixpath.join(site, page))
        parsed_pages[page] = info
        for _tag, url in info.links:
            rel = to_site_path(url, page)
            if rel is None:
                continue
            target, why = resolve(rel)
            if why:
                problems.append((page, url, why))
                continue
            _, frag = urldefrag(url)
            if target.endswith((".html", ".htm")):
                if target not in parsed_pages and target not in seen:
                    queue.append(target)
                if frag:
                    tinfo = parsed_pages.get(target) or parse(posixpath.join(site, target))
                    parsed_pages[target] = tinfo
                    if unquote(frag) not in tinfo.ids:
                        problems.append((page, url, f"has no #{frag} anchor on {target}"))

    print(f"checked {len(seen)} page(s) reachable from index.html")
    if problems:
        print(f"\n{len(problems)} broken link(s):\n", file=sys.stderr)
        for page, url, why in sorted(set(problems)):
            print(f"  {page}: {url}\n      {why}", file=sys.stderr)
            print(f"::error file={page}::broken site link {url} — {why}")
        return 1
    print("all links resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main())
