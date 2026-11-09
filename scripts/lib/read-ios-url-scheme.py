#!/usr/bin/env python3
"""Print the URL scheme the iOS build is configured to declare.

    read-ios-url-scheme.py TAURI_CONF [CONFIG_OVERRIDE_JSON ...]

package-ios.sh checks the exported .ipa against this, so the guard compares the
build with its own configuration instead of a literal. The scheme is
`plugins.deep-link.desktop.schemes[0]`; Tauri merges that into the bundle's
CFBundleURLTypes (see Info.ios.plist). Each override is a JSON string of the
kind passed to `tauri --config`, merged over the file as an RFC 7396 merge
patch, which is how Tauri merges it.

Accepts `--config`-style argument lists as well: a literal `--config` is
skipped, so the script can pass its `config_args` array through unchanged.
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import json


def merge_patch(target, patch):
    if not isinstance(patch, dict):
        return patch
    if not isinstance(target, dict):
        target = {}
    for key, value in patch.items():
        if value is None:
            target.pop(key, None)
        else:
            target[key] = merge_patch(target.get(key), value)
    return target


def configured_scheme(conf: dict) -> str:
    try:
        schemes = conf["plugins"]["deep-link"]["desktop"]["schemes"]
        scheme = schemes[0]
    except (KeyError, IndexError, TypeError):
        raise ValueError("plugins.deep-link.desktop.schemes is not set")
    if not isinstance(scheme, str) or not scheme:
        raise ValueError("plugins.deep-link.desktop.schemes[0] is not a non-empty string")
    return scheme


def main(argv: list[str]) -> int:
    if not argv:
        print("usage: read-ios-url-scheme.py TAURI_CONF [OVERRIDE_JSON ...]", file=sys.stderr)
        return 2
    path, overrides = argv[0], [a for a in argv[1:] if a != "--config"]
    try:
        with open(path, encoding="utf-8") as f:
            conf = json.load(f)
        for raw in overrides:
            conf = merge_patch(conf, json.loads(raw))
        print(configured_scheme(conf))
    except (OSError, ValueError) as e:
        print(f"error: cannot determine the iOS URL scheme from {path}: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
