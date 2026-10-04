#!/usr/bin/env python3
"""Print the next Android versionCode for a package, from Google Play.

Play rejects an upload whose versionCode it has already seen for the app, so
`--version-code auto` in `scripts/package-android.sh` asks it what the highest
one is instead of leaving the caller to remember. The iOS and Mac scripts do the
same against App Store Connect (`--build-number auto`).

    play-next-version-code.py <package-name>

Reads the path of a Google Cloud service-account key (JSON) from
PLAY_SERVICE_ACCOUNT_JSON. The service account has to be invited in Play
Console (Users and permissions) with permission to view the app and its
releases, and the Google Play Android Developer API has to be enabled in the
key's Cloud project. The key is read here only to sign a short-lived token for
Google's token endpoint; it is never printed or sent anywhere else.

The answer is the highest versionCode Play lists for the package, on any track
or among the uploaded bundles, plus one, or 1 when there are none. Two things
it cannot see: a versionCode used by a bundle that was uploaded and later
discarded, which Play still refuses to accept again, and a bundle that was
uploaded a moment ago and is still being processed. Either way the upload fails
with Play's own message and the next try needs a higher number.

It opens a Play "edit" to read with and deletes it afterwards; it changes
nothing in the app.

Standard library only, with `openssl` to sign: PyJWT and `cryptography` are not
installed on a stock machine, and a packaging script is the wrong place to ask
for them.

Silence on stdout with a non-zero exit means "could not tell"; the reason is on
stderr. It never prints a guess.
"""

from __future__ import annotations

import base64
import json
import os
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

# Overridable so the tests can point it at a local server. Nothing else sets it.
API = os.environ.get("PLAY_API_BASE", "https://androidpublisher.googleapis.com")

SCOPE = "https://www.googleapis.com/auth/androidpublisher"
DEFAULT_TOKEN_URI = "https://oauth2.googleapis.com/token"

# Every request is bounded: a packaging script that hangs on a dead network is
# worse than one that says so.
TIMEOUT_SECONDS = 30

# Play refuses a versionCode above this.
MAX_VERSION_CODE = 2_100_000_000


class PlayError(Exception):
    """Something to tell the person running the script, without a traceback."""


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def load_service_account(path: str) -> dict:
    try:
        with open(path, encoding="utf-8") as handle:
            account = json.load(handle)
    except OSError as err:
        raise PlayError(f"could not read the service-account key {path}: {err}") from err
    except json.JSONDecodeError as err:
        raise PlayError(f"{path} is not JSON ({err}); it should be the key file Google Cloud gave you") from err
    for field in ("client_email", "private_key"):
        if not isinstance(account.get(field), str) or not account[field]:
            raise PlayError(f"{path} has no {field}; it should be a service-account key file")
    return account


def make_assertion(account: dict, now: int | None = None) -> str:
    """A signed RS256 JWT that Google's token endpoint exchanges for an access token."""
    issued = int(time.time()) if now is None else now
    header = {"alg": "RS256", "typ": "JWT"}
    claims = {
        "iss": account["client_email"],
        "scope": SCOPE,
        "aud": account.get("token_uri") or DEFAULT_TOKEN_URI,
        "iat": issued,
        # The longest Google allows is an hour; this is used once, immediately.
        "exp": issued + 600,
    }
    signing_input = (
        b64url(json.dumps(header, separators=(",", ":")).encode())
        + "."
        + b64url(json.dumps(claims, separators=(",", ":")).encode())
    )
    # openssl signs from a key *file*. A private directory, removed straight
    # after, keeps the key off the disk for as long as that takes and out of
    # anyone else's reach meanwhile.
    with tempfile.TemporaryDirectory(prefix="play-key-") as directory:
        os.chmod(directory, 0o700)
        key_path = os.path.join(directory, "key.pem")
        descriptor = os.open(key_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, "w", encoding="ascii") as handle:
            handle.write(account["private_key"])
        try:
            signed = subprocess.run(
                ["openssl", "dgst", "-sha256", "-sign", key_path],
                input=signing_input.encode("ascii"),
                capture_output=True,
                check=False,
            )
        except OSError as err:
            raise PlayError(f"could not run openssl: {err}") from err
    if signed.returncode != 0:
        raise PlayError(
            "openssl could not sign with the service-account key: "
            f"{signed.stderr.decode(errors='replace').strip() or 'no message'}"
        )
    return signing_input + "." + b64url(signed.stdout)


def request(
    method: str,
    url: str,
    *,
    headers: dict | None = None,
    body: bytes | None = None,
    timeout: int = TIMEOUT_SECONDS,
) -> tuple[int, dict]:
    """One bounded HTTP request, returning (status, parsed JSON or {})."""
    req = urllib.request.Request(url, data=body, method=method, headers=headers or {})
    context = ssl.create_default_context()
    try:
        with urllib.request.urlopen(req, timeout=timeout, context=context) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as err:
        status, raw = err.code, err.read()
    except (urllib.error.URLError, TimeoutError, OSError) as err:
        raise PlayError(f"could not reach {urllib.parse.urlsplit(url).netloc}: {err}") from err
    if not raw:
        return status, {}
    try:
        parsed = json.loads(raw)
    except json.JSONDecodeError:
        return status, {"_unparsed": raw[:200].decode(errors="replace")}
    return status, parsed if isinstance(parsed, dict) else {"_value": parsed}


def api_message(parsed: dict) -> str:
    """Google's own explanation, which is usually the useful part."""
    error = parsed.get("error")
    if isinstance(error, dict):
        return str(error.get("message") or error.get("status") or error)
    if isinstance(error, str):
        return str(parsed.get("error_description") or error)
    return str(parsed.get("_unparsed") or "no message")


def fetch_token(account: dict) -> str:
    token_uri = account.get("token_uri") or DEFAULT_TOKEN_URI
    form = urllib.parse.urlencode(
        {
            "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer",
            "assertion": make_assertion(account),
        }
    ).encode("ascii")
    status, parsed = request(
        "POST", token_uri, headers={"Content-Type": "application/x-www-form-urlencoded"}, body=form
    )
    token = parsed.get("access_token")
    if status != 200 or not isinstance(token, str) or not token:
        raise PlayError(
            f"Google would not give a token for {account['client_email']} "
            f"(HTTP {status}: {api_message(parsed)}). Check that the key is current and "
            "that the Google Play Android Developer API is enabled in its Cloud project."
        )
    return token


def play(method: str, path: str, token: str, body: bytes | None = None) -> tuple[int, dict]:
    headers = {"Authorization": f"Bearer {token}", "Accept": "application/json"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    return request(method, f"{API}/androidpublisher/v3/applications/{path}", headers=headers, body=body)


def explain(status: int, parsed: dict, what: str, package: str, account: dict) -> PlayError:
    message = api_message(parsed)
    if status in (401, 403):
        hint = (
            f"Is {account['client_email']} invited in Play Console (Users and permissions) "
            "with permission to view this app and its releases?"
        )
    elif status == 404:
        hint = (
            f"Does the app {package} exist in Play Console, and was the service account "
            "invited for that app? The API only works on an app that has had a first "
            "upload through the Console."
        )
    else:
        hint = ""
    return PlayError(f"Play refused to {what} (HTTP {status}: {message}). {hint}".strip())


def parse_version_code(value: object) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value if value > 0 else None
    # ASCII only: `str.isdigit()` is also true for '\u00b2' and the like, which
    # `int()` then rejects with a traceback.
    if isinstance(value, str) and value.isascii() and value.isdigit():
        return int(value) if int(value) > 0 else None
    return None


def as_list(value: object, what: str) -> list:
    """`value` if it is a list. Play omits an empty field, so None is an empty
    list; anything else is not what the API documents, so it is ignored with a
    warning rather than iterated (a string would be split into characters)."""
    if value is None:
        return []
    if isinstance(value, list):
        return value
    print(f"warning: ignoring {what} that Play sent as {value!r}", file=sys.stderr)
    return []


def highest_version_code(package: str, account: dict, token: str) -> int | None:
    """The highest versionCode Play lists for `package`, or None when it lists none."""
    quoted = urllib.parse.quote(package, safe="")
    status, edit = play("POST", f"{quoted}/edits", token, b"{}")
    edit_id = edit.get("id")
    if status != 200 or not isinstance(edit_id, str) or not edit_id:
        raise explain(status, edit, "open an edit to read with", package, account)
    found: list[int] = []
    try:
        status, bundles = play("GET", f"{quoted}/edits/{edit_id}/bundles", token)
        if status != 200:
            raise explain(status, bundles, "list the uploaded bundles", package, account)
        values = [
            item.get("versionCode")
            for item in as_list(bundles.get("bundles"), "the bundle list")
            if isinstance(item, dict)
        ]

        status, tracks = play("GET", f"{quoted}/edits/{edit_id}/tracks", token)
        if status != 200:
            raise explain(status, tracks, "list the release tracks", package, account)
        for track in as_list(tracks.get("tracks"), "the track list"):
            if not isinstance(track, dict):
                continue
            for release in as_list(track.get("releases"), "a track's releases"):
                if isinstance(release, dict):
                    values.extend(as_list(release.get("versionCodes"), "a release's versionCodes"))

        for value in values:
            code = parse_version_code(value)
            if code is None:
                print(f"warning: ignoring a versionCode Play listed as {value!r}", file=sys.stderr)
            else:
                found.append(code)
    finally:
        # Best effort: an edit that is left open expires on its own.
        try:
            play("DELETE", f"{quoted}/edits/{edit_id}", token)
        except PlayError as err:
            print(f"warning: could not delete the read-only edit: {err}", file=sys.stderr)
    return max(found) if found else None


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] in ("-h", "--help"):
        print(__doc__.split("\n\n")[0], file=sys.stderr)
        print("usage: play-next-version-code.py <package-name>", file=sys.stderr)
        return 2
    package = argv[1]
    key_path = os.environ.get("PLAY_SERVICE_ACCOUNT_JSON", "")
    if not key_path:
        print(
            "error: set PLAY_SERVICE_ACCOUNT_JSON to the path of the service-account key (JSON)",
            file=sys.stderr,
        )
        return 2
    try:
        account = load_service_account(os.path.expanduser(key_path))
        token = fetch_token(account)
        highest = highest_version_code(package, account, token)
        if highest is None:
            print("note: Play lists no versionCode for this app yet, so starting at 1", file=sys.stderr)
        next_code = 1 if highest is None else highest + 1
        if next_code > MAX_VERSION_CODE:
            raise PlayError(f"the next versionCode would be {next_code}, above Play's limit of {MAX_VERSION_CODE}")
    except PlayError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    print(next_code)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
