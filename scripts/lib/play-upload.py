#!/usr/bin/env python3
"""Upload a signed Android App Bundle (and its native debug symbols) to a Play track.

    play-upload.py --package <name> --bundle <file.aab> [--symbols <zip>]
                   [--track internal] [--mode check|draft|release] [--name <release name>]

Uses the same service account as `play-next-version-code.py` (PLAY_SERVICE_ACCOUNT_JSON),
which now also needs permission to release to testing tracks. Everything happens inside
one Play "edit", which is how the API is shaped: nothing is visible until it is committed.

--mode check     Upload and validate, then throw the edit away. Play inspects the bundle
                 on upload, so this proves it would be accepted, and changes nothing: no
                 release exists afterwards and the versionCode is not used up. The default,
                 because the first real run of a pipeline should be this one.
--mode draft     Commit the bundle as a DRAFT release on the track. Testers see nothing
                 until someone completes it in Play Console. Refuses to proceed if the
                 track already has a draft, rather than replace someone's work.
--mode release   Commit it as a completed release, i.e. rolled out to the track.

The existing releases on the track are kept when a draft is added to them. A completed
release replaces what the track had, because that is what completing one means.

Prints the uploaded versionCode on stdout. Silence with a non-zero exit means nothing was
changed (or, if it failed after the commit started, the reason is on stderr).

Standard library only, with the auth and request code of play-next-version-code.py.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import sys
import urllib.parse
from pathlib import Path

_spec = importlib.util.spec_from_file_location(
    "play_next_version_code", Path(__file__).with_name("play-next-version-code.py")
)
play_api = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(play_api)

PlayError = play_api.PlayError

# A bundle is at most 200 MB and this link may be slow. Reads keep their 30 seconds.
UPLOAD_TIMEOUT_SECONDS = 900

MODES = ("check", "draft", "release")
TRACKS = ("internal", "alpha", "beta", "production")


def upload(path: str, token: str, body: bytes) -> tuple[int, dict]:
    return play_api.request(
        "POST",
        f"{play_api.API}/upload/androidpublisher/v3/applications/{path}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/json",
            "Content-Type": "application/octet-stream",
        },
        body=body,
        timeout=UPLOAD_TIMEOUT_SECONDS,
    )


def read_file(path: str, what: str) -> bytes:
    try:
        return Path(path).read_bytes()
    except OSError as err:
        raise PlayError(f"could not read the {what} {path}: {err}") from err


def release_for(version_code: int, name: str | None, status: str) -> dict:
    return {"name": name or str(version_code), "versionCodes": [str(version_code)], "status": status}


def run(args: argparse.Namespace) -> int:
    bundle = read_file(args.bundle, "bundle")
    symbols = read_file(args.symbols, "symbols zip") if args.symbols else None
    account = play_api.load_service_account(os.path.expanduser(os.environ["PLAY_SERVICE_ACCOUNT_JSON"]))
    token = play_api.fetch_token(account)

    package = urllib.parse.quote(args.package, safe="")
    status, edit = play_api.play("POST", f"{package}/edits", token, b"{}")
    edit_id = edit.get("id")
    if status != 200 or not isinstance(edit_id, str) or not edit_id:
        raise play_api.explain(status, edit, "open an edit", args.package, account)

    committed = False
    try:
        status, uploaded = upload(f"{package}/edits/{edit_id}/bundles?uploadType=media", token, bundle)
        if status != 200 or not isinstance(uploaded.get("versionCode"), int):
            raise play_api.explain(status, uploaded, "accept the bundle", args.package, account)
        version_code = uploaded["versionCode"]
        # What Play stored should be what was sent.
        local = hashlib.sha256(bundle).hexdigest()
        remote = str(uploaded.get("sha256", "")).lower().replace(":", "")
        if remote and remote != local:
            raise PlayError(f"Play stored a bundle with sha256 {remote}, but the file sent is {local}")
        print(f"uploaded the bundle as versionCode {version_code}", file=sys.stderr)

        if symbols is not None:
            status, result = upload(
                f"{package}/edits/{edit_id}/apks/{version_code}/deobfuscationFiles/nativeCode?uploadType=media",
                token,
                symbols,
            )
            if status != 200:
                raise play_api.explain(status, result, "accept the native debug symbols", args.package, account)
            print("uploaded the native debug symbols", file=sys.stderr)

        # What the track holds now.
        status, current = play_api.play("GET", f"{package}/edits/{edit_id}/tracks/{args.track}", token)
        if status == 404:
            current = {}
        elif status != 200:
            raise play_api.explain(status, current, f"read the {args.track} track", args.package, account)
        existing = [r for r in current.get("releases", []) if isinstance(r, dict)]

        wanted = "completed" if args.mode == "release" else "draft"
        release = release_for(version_code, args.name, wanted)
        if wanted == "draft":
            # Keep what is there. Replacing a draft would throw someone's work
            # away, so that is refused, not done.
            for other in existing:
                if other.get("status") == "draft" and [str(v) for v in other.get("versionCodes", [])] != [str(version_code)]:
                    raise PlayError(
                        f"the {args.track} track already has a draft release "
                        f"({other.get('name') or other.get('versionCodes')}); complete or discard it in "
                        "Play Console first, so it is not overwritten"
                    )
            releases = [r for r in existing if r.get("status") != "draft"] + [release]
        else:
            releases = [release]

        status, result = play_api.play(
            "PUT",
            f"{package}/edits/{edit_id}/tracks/{args.track}",
            token,
            json.dumps({"track": args.track, "releases": releases}).encode(),
        )
        if status != 200:
            raise play_api.explain(status, result, f"put the release on the {args.track} track", args.package, account)

        status, result = play_api.play("POST", f"{package}/edits/{edit_id}:validate", token, b"{}")
        if status != 200:
            raise play_api.explain(status, result, "validate the edit", args.package, account)
        print("Play validated the edit", file=sys.stderr)

        if args.mode == "check":
            print(
                "check only: nothing was committed, so no release exists and versionCode "
                f"{version_code} is still free",
                file=sys.stderr,
            )
        else:
            status, result = play_api.play("POST", f"{package}/edits/{edit_id}:commit", token, b"{}")
            if status != 200:
                raise play_api.explain(status, result, "commit the edit", args.package, account)
            committed = True
            print(f"committed: versionCode {version_code} is a {wanted} release on the {args.track} track", file=sys.stderr)
    finally:
        if not committed:
            # Nothing was published; the edit would expire on its own, but
            # leaving it open is untidy and counts against a per-app limit.
            try:
                play_api.play("DELETE", f"{package}/edits/{edit_id}", token)
            except PlayError as err:
                print(f"warning: could not delete the unused edit: {err}", file=sys.stderr)
    print(version_code)
    return 0


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0], add_help=True)
    parser.add_argument("--package", required=True)
    parser.add_argument("--bundle", required=True)
    parser.add_argument("--symbols")
    parser.add_argument("--track", choices=TRACKS, default="internal")
    parser.add_argument("--mode", choices=MODES, default="check")
    parser.add_argument("--name")
    args = parser.parse_args(argv[1:])
    if not os.environ.get("PLAY_SERVICE_ACCOUNT_JSON"):
        print("error: set PLAY_SERVICE_ACCOUNT_JSON to the path of the service-account key (JSON)", file=sys.stderr)
        return 2
    try:
        return run(args)
    except PlayError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
