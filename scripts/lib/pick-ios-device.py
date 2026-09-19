#!/usr/bin/env python3
"""Print the identifier of the one connected iOS device, or nothing.

`devicectl device install app` lists `--device` among options it requires and
has no "the only connected one" default, so `package-ios.sh --install` has to
resolve it. Reading `--json-output` rather than scraping the table: both the
device name and the model column contain spaces, so column positions do not
survive a second device or a renamed phone.

Silence means "you have to choose": nothing connected, or more than one, in
which case the candidates go to stderr so the caller can name one.
"""

from __future__ import annotations

import json
import sys


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: pick-ios-device.py <devicectl-json>", file=sys.stderr)
        return 2
    try:
        with open(argv[1], encoding="utf-8") as handle:
            devices = json.load(handle)["result"]["devices"]
    except (OSError, ValueError, KeyError):
        # No file, or a shape this does not recognise. The caller's own error
        # covers it; guessing here would be worse than saying nothing.
        return 0

    connected = [
        device
        for device in devices
        if device.get("connectionProperties", {}).get("tunnelState")
        != "disconnected"
    ]
    if len(connected) == 1:
        print(connected[0]["identifier"])
        return 0
    for device in connected:
        name = device.get("deviceProperties", {}).get("name", "?")
        print(f"  {device['identifier']}  {name}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
