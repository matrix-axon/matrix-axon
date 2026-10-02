#!/usr/bin/env python3
"""Print the next CFBundleVersion for a bundle ID and platform, from App Store Connect.

App Store Connect rejects an upload whose build number it has already seen for the
app, so `--build-number auto` in the packaging scripts asks it what the highest one
is instead of leaving the caller to remember.

    asc-next-build-number.py <bundle-id> <platform>

<platform> is ios, macos, tvos or visionos, and is required. Build numbers run
separately per platform: an app with an iOS build 35 and a Mac build 2 has a next
Mac build of 3. An earlier version of this took the highest across every platform,
so the first Mac upload after thirty-odd iOS ones was handed 36; naming the platform
makes that mistake impossible to make by leaving something out.

Reads ASC_KEY_ID and ASC_ISSUER_ID from the environment and the key from
~/.appstoreconnect/private_keys/AuthKey_<ASC_KEY_ID>.p8 — the same three things
`--upload` already needs, and where altool looks for the key. The key is read
here only to sign a short-lived token; it is never printed or sent anywhere.

The answer is the highest build number App Store Connect lists for that platform,
with its last component raised by one, or 1 when there are none. Numbers are
compared as tuples of integers, not as text, so 10 follows 9 and 1.10 follows
1.9. Two things it cannot see: a build that has been uploaded and is still being
processed may not be listed yet, so two uploads minutes apart can be handed the
same number; and a build App Store Connect lists under a value that is not
dotted integers is skipped with a warning rather than guessed at.

Standard library only, with `openssl` to sign: PyJWT and `cryptography` are not
installed on a stock macOS, and a packaging script is the wrong place to ask
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
import time
import urllib.error
import urllib.parse
import urllib.request

# Overridable so the tests can point it at a local server. Nothing else sets it.
API = os.environ.get("ASC_API_BASE", "https://api.appstoreconnect.apple.com")

# What App Store Connect calls each platform, for `filter[preReleaseVersion.platform]`.
PLATFORMS = {"ios": "IOS", "macos": "MAC_OS", "tvos": "TV_OS", "visionos": "VISION_OS"}

# A page is at most 200 builds. 25 pages is 5000 builds, far beyond anything
# this app will have; the cap is there so a server that keeps returning a
# `next` link cannot loop this forever.
MAX_PAGES = 25


class AscError(Exception):
    """Something to tell the person running the script, without a traceback."""


def b64url(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


def der_to_raw(der: bytes) -> bytes:
    """Turn openssl's DER ECDSA signature into the 64 bytes JWT ES256 wants.

    DER is SEQUENCE { INTEGER r, INTEGER s }, each integer big-endian with a
    leading zero byte when its top bit is set and no padding otherwise; JWS
    wants r and s as fixed 32-byte big-endian values side by side.
    """

    def read_len(buf: bytes, i: int) -> tuple[int, int]:
        first = buf[i]
        if first < 0x80:
            return first, i + 1
        count = first & 0x7F
        return int.from_bytes(buf[i + 1 : i + 1 + count], "big"), i + 1 + count

    if not der or der[0] != 0x30:
        raise AscError("openssl produced a signature that is not a DER sequence")
    _, i = read_len(der, 1)
    parts = []
    for _ in range(2):
        if der[i] != 0x02:
            raise AscError("openssl produced a signature that is not two integers")
        length, i = read_len(der, i + 1)
        parts.append(int.from_bytes(der[i : i + length], "big"))
        i += length
    return b"".join(p.to_bytes(32, "big") for p in parts)


def make_token(key_id: str, issuer_id: str, key_path: str) -> str:
    now = int(time.time())
    header = {"alg": "ES256", "kid": key_id, "typ": "JWT"}
    # App Store Connect refuses a token that lives longer than 20 minutes.
    claims = {"iss": issuer_id, "iat": now, "exp": now + 600, "aud": "appstoreconnect-v1"}
    signing_input = (
        b64url(json.dumps(header, separators=(",", ":")).encode())
        + "."
        + b64url(json.dumps(claims, separators=(",", ":")).encode())
    )
    try:
        signed = subprocess.run(
            ["openssl", "dgst", "-sha256", "-sign", key_path],
            input=signing_input.encode("ascii"),
            capture_output=True,
            check=False,
        )
    except OSError as err:
        raise AscError(f"could not run openssl: {err}") from err
    if signed.returncode != 0:
        raise AscError(
            f"openssl could not sign with {key_path}: "
            f"{signed.stderr.decode(errors='replace').strip() or 'no message'}"
        )
    return signing_input + "." + b64url(der_to_raw(signed.stdout))


SYSTEM_CA_BUNDLE = "/etc/ssl/cert.pem"


def ssl_context() -> ssl.SSLContext:
    """The default context, plus macOS's own CA bundle when there is one.

    A python.org build of Python ships without a CA file of its own and needs its
    "Install Certificates" step run once; until then every HTTPS request fails
    with CERTIFICATE_VERIFY_FAILED, which looks like a problem with Apple's
    server and is not. macOS keeps the bundle it trusts at /etc/ssl/cert.pem, and
    this is a macOS-only script, so add it rather than ask anyone to fix their
    Python. Added to the defaults, not substituted for them, so a Python that was
    set up properly behaves as it always did.
    """
    context = ssl.create_default_context()
    if os.path.isfile(SYSTEM_CA_BUNDLE):
        context.load_verify_locations(cafile=SYSTEM_CA_BUNDLE)
    return context


def get(url: str, token: str) -> dict:
    request = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(request, timeout=30, context=ssl_context()) as response:
            return json.load(response)
    except urllib.error.HTTPError as err:
        if err.code == 401:
            raise AscError(
                "App Store Connect rejected the credentials (401). Check that "
                "ASC_KEY_ID and ASC_ISSUER_ID belong together and that the key "
                "has not been revoked."
            ) from err
        if err.code == 403:
            raise AscError(
                "App Store Connect says this key may not read builds (403). "
                "It needs the App Manager role or above."
            ) from err
        raise AscError(f"App Store Connect returned HTTP {err.code} for {url}") from err
    except (urllib.error.URLError, TimeoutError, ValueError) as err:
        raise AscError(f"could not reach App Store Connect: {err}") from err


def find_app_id(bundle_id: str, token: str) -> str:
    query = urllib.parse.urlencode(
        {"filter[bundleId]": bundle_id, "fields[apps]": "bundleId", "limit": "2"}
    )
    apps = get(f"{API}/v1/apps?{query}", token).get("data", [])
    # `filter[bundleId]` is a prefix-style match on some API versions, so an
    # exact comparison is what decides, not the length of the list.
    exact = [a for a in apps if a.get("attributes", {}).get("bundleId") == bundle_id]
    if not exact:
        raise AscError(
            f"App Store Connect has no app with bundle ID {bundle_id}. Create "
            "it there first, or pass an explicit --build-number."
        )
    return exact[0]["id"]


def build_versions(app_id: str, platform: str, token: str) -> list[str]:
    query = urllib.parse.urlencode(
        {
            "filter[app]": app_id,
            "filter[preReleaseVersion.platform]": PLATFORMS[platform],
            "fields[builds]": "version",
            "limit": "200",
        }
    )
    url: str | None = f"{API}/v1/builds?{query}"
    versions: list[str] = []
    for _ in range(MAX_PAGES):
        if not url:
            return versions
        page = get(url, token)
        versions += [
            b["attributes"]["version"]
            for b in page.get("data", [])
            if b.get("attributes", {}).get("version") is not None
        ]
        url = page.get("links", {}).get("next")
    raise AscError(f"App Store Connect kept paging after {MAX_PAGES} pages; not guessing")


def parse_version(text: str) -> tuple[int, ...] | None:
    parts = text.split(".")
    if 1 <= len(parts) <= 3 and all(p.isdigit() for p in parts):
        return tuple(int(p) for p in parts)
    return None


def next_version(listed: list[str]) -> str:
    parsed = []
    for text in listed:
        version = parse_version(text)
        if version is None:
            print(f"warning: ignoring build number {text!r}: not dotted integers", file=sys.stderr)
        else:
            parsed.append(version)
    if not parsed:
        return "1"
    top = max(parsed)
    return ".".join(str(n) for n in (*top[:-1], top[-1] + 1))


def main(argv: list[str]) -> int:
    if len(argv) != 3 or argv[2] not in PLATFORMS:
        print(
            "usage: asc-next-build-number.py <bundle-id> <platform>\n"
            f"       platform is one of: {', '.join(PLATFORMS)}",
            file=sys.stderr,
        )
        return 2
    key_id = os.environ.get("ASC_KEY_ID", "")
    issuer_id = os.environ.get("ASC_ISSUER_ID", "")
    if not key_id or not issuer_id:
        print("error: set ASC_KEY_ID and ASC_ISSUER_ID", file=sys.stderr)
        return 2
    key_path = os.path.expanduser(f"~/.appstoreconnect/private_keys/AuthKey_{key_id}.p8")
    if not os.path.isfile(key_path):
        print(f"error: no key at {key_path}", file=sys.stderr)
        return 2
    try:
        token = make_token(key_id, issuer_id, key_path)
        app_id = find_app_id(argv[1], token)
        print(next_version(build_versions(app_id, argv[2], token)))
    except AscError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
