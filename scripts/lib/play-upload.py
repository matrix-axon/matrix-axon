#!/usr/bin/env python3
"""Upload a signed Android App Bundle (and its native debug symbols) to a Play track.

    play-upload.py --package <name> --bundle <file.aab> [--symbols <zip>]
                   [--track internal] [--mode check|draft|release] [--name <release name>]
                   [--expect-version-code <n>] [--preflight]

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
--mode release   Commit it as a completed release, i.e. rolled out to the track. Prints
                 each release it replaces, and refuses if one of them is a draft or an
                 in-progress or halted rollout: finishing or discarding that is a decision
                 for Play Console, not for a script.

The existing releases on the track are kept when a draft is added to them. A completed
release replaces what the track had, because that is what completing one means.

--preflight      Do not upload. Open an edit, read the track, and apply the same refusals as
                 above (an existing draft; for release, also an in-progress or halted
                 rollout), then discard the edit. `package-android.sh` runs this before the
                 build, so a track that would refuse the upload says so in seconds, not after
                 the build. Needs no --bundle.

--expect-version-code N   Stop, before anything is attached or committed, if Play gives the
                 uploaded bundle a different versionCode than the build said it would have.

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

# A release in one of these states is someone's work in progress. A completed
# release in `release` mode is replaced by design; these are not.
IN_FLIGHT = ("draft", "inProgress", "halted")

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


def describe(release: dict) -> str:
    return str(release.get("name") or release.get("versionCodes"))


def refuse_in_flight(existing: list[dict], track: str, mode: str, version_code: int | None = None) -> None:
    """Raise if committing in `mode` would throw away someone's work on the track.

    `draft` keeps what is there and only refuses another draft. `release` replaces the
    track, so it refuses anything unfinished. A release of the same versionCode is not
    in the way (`version_code` is unknown before the upload, so then nothing is exempt)."""
    mine = [str(version_code)] if version_code is not None else None
    other = [r for r in existing if [str(v) for v in r.get("versionCodes", [])] != mine]
    if mode == "release":
        blocking = [r for r in other if r.get("status") in IN_FLIGHT]
        if blocking:
            raise PlayError(
                f"the {track} track has "
                + ", ".join(f"a {r.get('status')} release ({describe(r)})" for r in blocking)
                + "; a completed release would replace it. Complete or discard it in Play Console first"
            )
    else:
        for r in other:
            if r.get("status") == "draft":
                raise PlayError(
                    f"the {track} track already has a draft release ({describe(r)}); complete or discard it in "
                    "Play Console first, so it is not overwritten"
                )


def read_track(package: str, edit_id: str, track: str, token: str, name: str, account: dict) -> list[dict]:
    """The releases on `track` in this edit; a track that does not exist yet has none."""
    status, current = play_api.play("GET", f"{package}/edits/{edit_id}/tracks/{track}", token)
    if status == 404:
        return []
    if status != 200:
        raise play_api.explain(status, current, f"read the {track} track", name, account)
    return [r for r in current.get("releases", []) if isinstance(r, dict)]


def preflight(args: argparse.Namespace) -> int:
    """Read the track and say whether this mode would be refused. Changes nothing."""
    account = play_api.load_service_account(os.path.expanduser(os.environ["PLAY_SERVICE_ACCOUNT_JSON"]))
    token = play_api.fetch_token(account)
    package = urllib.parse.quote(args.package, safe="")
    status, edit = play_api.play("POST", f"{package}/edits", token, b"{}")
    edit_id = edit.get("id")
    if status != 200 or not isinstance(edit_id, str) or not edit_id:
        raise play_api.explain(status, edit, "open an edit", args.package, account)
    try:
        refuse_in_flight(read_track(package, edit_id, args.track, token, args.package, account), args.track, args.mode)
    finally:
        try:
            play_api.play("DELETE", f"{package}/edits/{edit_id}", token)
        except PlayError as err:
            print(f"warning: could not delete the read-only edit: {err}", file=sys.stderr)
    print(f"the {args.track} track has nothing a {args.mode} upload would overwrite", file=sys.stderr)
    return 0


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
        if args.expect_version_code is not None and version_code != args.expect_version_code:
            raise PlayError(
                f"Play gave the bundle versionCode {version_code}, but the build said "
                f"{args.expect_version_code}; nothing was attached or committed"
            )

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
        existing = read_track(package, edit_id, args.track, token, args.package, account)

        wanted = "completed" if args.mode == "release" else "draft"
        release = release_for(version_code, args.name, wanted)
        refuse_in_flight(existing, args.track, args.mode, version_code)
        if wanted == "draft":
            # Keep what is there.
            releases = [r for r in existing if r.get("status") != "draft"] + [release]
        else:
            # Replaced by design, but never silently.
            for other in existing:
                print(f"replacing the {other.get('status')} release {describe(other)} on the {args.track} track", file=sys.stderr)
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
    parser.add_argument("--bundle")
    parser.add_argument("--symbols")
    parser.add_argument("--track", choices=TRACKS, default="internal")
    parser.add_argument("--mode", choices=MODES, default="check")
    parser.add_argument("--name")
    parser.add_argument("--expect-version-code", type=int)
    parser.add_argument("--preflight", action="store_true")
    args = parser.parse_args(argv[1:])
    if not args.preflight and not args.bundle:
        parser.error("--bundle is required unless --preflight is given")
    if not os.environ.get("PLAY_SERVICE_ACCOUNT_JSON"):
        print("error: set PLAY_SERVICE_ACCOUNT_JSON to the path of the service-account key (JSON)", file=sys.stderr)
        return 2
    try:
        return preflight(args) if args.preflight else run(args)
    except PlayError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
