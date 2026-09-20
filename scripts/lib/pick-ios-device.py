#!/usr/bin/env python3
"""Print the identifier of the one connected iOS device, or nothing.

`devicectl device install app` lists `--device` among options it requires and
has no "the only connected one" default, so `package-ios.sh --install` has to
resolve it. Reading `--json-output` rather than scraping the table: both the
device name and the model column contain spaces, so column positions do not
survive a second device or a renamed phone.

Silence means "you have to choose": nothing connected, or more than one. Either
way the devices `devicectl` knows go to stderr with the tunnel state each one
reported, so the caller can name one.
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

    # An allow-list, not "anything but disconnected". `devicectl` knows several
    # states a device can be in without being usable — a paired phone that is
    # not attached reports `unavailable`, and a tunnel being established
    # reports `connecting` — and counting those as present is how one attached
    # phone plus one old paired one becomes "several are connected", leaving
    # the caller to pass `--device` for a choice it does not actually have.
    # Being wrong the other way costs nothing: an unrecognised state means the
    # list below is printed, which names every device and the state it is in.
    connected = [
        device
        for device in devices
        if device.get("connectionProperties", {}).get("tunnelState") == "connected"
    ]
    if len(connected) == 1:
        print(connected[0]["identifier"])
        return 0

    # Silence would be a dead end here. Print every device `devicectl` knows,
    # with the state it reported, so the caller can name one — and so an
    # unexpected value for a usable device shows up as a readable line rather
    # than as "nothing is connected".
    for device in connected or devices:
        name = device.get("deviceProperties", {}).get("name", "?")
        state = device.get("connectionProperties", {}).get("tunnelState", "?")
        print(f"  {device['identifier']}  {name}  ({state})", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
