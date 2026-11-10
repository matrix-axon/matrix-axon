#!/usr/bin/env python3
"""Print the URL scheme the iOS build is configured to declare.

    read-ios-url-scheme.py TAURI_CONF [LAYER ...]

package-ios.sh compares the exported .ipa against this, so the guard checks the
build against its own configuration instead of a literal. The scheme is
`plugins.deep-link.desktop.schemes[0]` in the merged configuration.

Each LAYER is merged over the one before it, as an RFC 7396 merge patch, which
is how Tauri layers its configuration: a platform file such as
tauri.ios.conf.json is a path, and a `tauri --config` override is a JSON string
(an argument starting with `{`). A literal `--config` is skipped, so the script
can pass its `config_args` array through unchanged.

What this reads is the declared scheme, not the one that ships. On iOS the
bundle's CFBundleURLTypes comes from Info.ios.plist, not from
`desktop.schemes`, so the two must be kept in step by hand. The .ipa check
exists to catch them disagreeing.
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
        print("usage: read-ios-url-scheme.py TAURI_CONF [LAYER ...]", file=sys.stderr)
        return 2
    path, layers = argv[0], [a for a in argv[1:] if a != "--config"]
    try:
        with open(path, encoding="utf-8") as f:
            conf = json.load(f)
        for layer in layers:
            if layer.lstrip().startswith("{"):
                conf = merge_patch(conf, json.loads(layer))
            else:
                with open(layer, encoding="utf-8") as f:
                    conf = merge_patch(conf, json.load(f))
        print(configured_scheme(conf))
    except (OSError, ValueError) as e:
        print(f"error: cannot determine the iOS URL scheme from {path}: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
