#!/usr/bin/env python3
"""Tests for play-next-version-code.py.

    python3 scripts/lib/test_play_next_version_code.py

The helper is run as a subprocess against a local server that stands in for
Google's token endpoint and the Play Developer API, with a service-account key
generated for the occasion, so nothing here touches the network or a real
credential. What this cannot show is that Google accepts the token or that the
real API returns the shapes the server below does; that has to be checked once
against the real thing, with a real service account.
"""

from __future__ import annotations

import sys

# Before the helper is imported: importing it would otherwise leave a
# `__pycache__` next to it, which the repository does not ignore.
sys.dont_write_bytecode = True

import base64
import http.server
import importlib.util
import json
import os
import subprocess
import tempfile
import threading
import unittest
import urllib.parse
from pathlib import Path

HELPER = Path(__file__).with_name("play-next-version-code.py")

spec = importlib.util.spec_from_file_location("play_next_version_code", HELPER)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)

PACKAGE = "org.example.app"
EMAIL = "release-bot@example-project.iam.gserviceaccount.com"
ACCESS_TOKEN = "test-access-token-not-a-secret"


def b64url_decode(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


class Server:
    """A fake Google. `routes` maps (method, path) to a handler -> (status, body)."""

    def __init__(self, routes):
        self.requests: list[tuple[str, str, dict, bytes]] = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def handle_any(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                path = urllib.parse.urlsplit(self.path).path
                outer.requests.append((self.command, path, dict(self.headers), body))
                handler = routes.get((self.command, path))
                if handler is None:
                    status, payload = 404, {"error": {"message": f"no route for {self.command} {path}"}}
                else:
                    status, payload = handler(self, body)
                raw = json.dumps(payload).encode() if payload is not None else b""
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)

            do_GET = do_POST = do_DELETE = handle_any

            def log_message(self, *args):  # keep the test output clean
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    @property
    def base(self) -> str:
        return f"http://127.0.0.1:{self.httpd.server_address[1]}"

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()

    def calls(self, method: str) -> list[tuple[str, dict, bytes]]:
        return [(p, h, b) for m, p, h, b in self.requests if m == method]


def make_key(directory: Path) -> tuple[str, Path]:
    """A fresh RSA key as (PEM text, path of its public half)."""
    private = directory / "private.pem"
    public = directory / "public.pem"
    subprocess.run(["openssl", "genrsa", "-out", str(private), "2048"], check=True, capture_output=True)
    subprocess.run(
        ["openssl", "rsa", "-in", str(private), "-pubout", "-out", str(public)],
        check=True,
        capture_output=True,
    )
    return private.read_text(), public


class HelperTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.dir = Path(cls.tmp.name)
        cls.pem, cls.public = make_key(cls.dir)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    # -- plumbing -------------------------------------------------------------

    def play_routes(self, bundles=None, tracks=None, edit_status=200, bundles_status=200, token_status=200):
        """The routes of a healthy Play, each overridable."""
        base = f"/androidpublisher/v3/applications/{PACKAGE}/edits"

        def token(handler, body):
            if token_status != 200:
                return token_status, {"error": "invalid_grant", "error_description": "Invalid JWT signature."}
            self.assertion = urllib.parse.parse_qs(body.decode())["assertion"][0]
            return 200, {"access_token": ACCESS_TOKEN, "expires_in": 3600, "token_type": "Bearer"}

        def edit(handler, body):
            if edit_status != 200:
                return edit_status, {"error": {"message": "The caller does not have permission"}}
            return 200, {"id": "edit-1", "expiryTimeSeconds": "9999999999"}

        def bundles_list(handler, body):
            if bundles_status != 200:
                return bundles_status, {"error": {"message": "forbidden"}}
            return 200, {"bundles": [{"versionCode": v} for v in (bundles or [])]} if bundles else {}

        def tracks_list(handler, body):
            return 200, {"tracks": tracks} if tracks else {}

        return {
            ("POST", "/token"): token,
            ("POST", base): edit,
            ("GET", f"{base}/edit-1/bundles"): bundles_list,
            ("GET", f"{base}/edit-1/tracks"): tracks_list,
            ("DELETE", f"{base}/edit-1"): lambda handler, body: (204, None),
        }

    def run_helper(self, server, *, account=None, env=None, args=(PACKAGE,), tmpdir=None):
        directory = Path(tempfile.mkdtemp(dir=self.dir))
        key_file = directory / "sa.json"
        document = account if account is not None else {
            "type": "service_account",
            "client_email": EMAIL,
            "private_key": self.pem,
            "token_uri": f"{server.base}/token",
        }
        key_file.write_text(json.dumps(document))
        environment = {
            "PATH": os.environ["PATH"],
            "HOME": str(directory),
            "PLAY_SERVICE_ACCOUNT_JSON": str(key_file),
            "PLAY_API_BASE": server.base,
        }
        if tmpdir is not None:
            environment["TMPDIR"] = str(tmpdir)
        environment.update(env or {})
        return subprocess.run(
            [sys.executable, "-B", str(HELPER), *args],
            env=environment,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def assertNoSecrets(self, result):
        combined = result.stdout + result.stderr
        self.assertNotIn("BEGIN PRIVATE KEY", combined)
        self.assertNotIn(self.pem.splitlines()[1], combined)
        self.assertNotIn(ACCESS_TOKEN, combined)

    # -- the answer -----------------------------------------------------------

    def test_takes_the_highest_across_bundles_and_tracks_and_adds_one(self):
        tracks = [
            {"track": "internal", "releases": [{"status": "completed", "versionCodes": ["1000", "1004"]}]},
            {"track": "production", "releases": []},
            {"track": "beta", "releases": [{"status": "draft", "versionCodes": ["1002"]}]},
        ]
        with Server(self.play_routes(bundles=[1000, 1003], tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1005")
        self.assertNoSecrets(result)

    def test_a_bundle_on_no_track_still_counts(self):
        with Server(self.play_routes(bundles=[2500], tracks=[])) as server:
            result = self.run_helper(server)
        self.assertEqual(result.stdout.strip(), "2501")

    def test_numbers_compare_as_numbers_not_text(self):
        # "999" sorts after "1000" as text.
        tracks = [{"track": "internal", "releases": [{"versionCodes": ["999", "1000"]}]}]
        with Server(self.play_routes(tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.stdout.strip(), "1001")

    def test_no_versions_at_all_starts_at_one_and_says_so(self):
        with Server(self.play_routes()) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1")
        self.assertIn("starting at 1", result.stderr)

    def test_a_value_that_is_not_a_positive_integer_is_skipped_with_a_warning(self):
        tracks = [{"track": "internal", "releases": [{"versionCodes": ["abc", "0", "-3", "1000"]}]}]
        with Server(self.play_routes(tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.stdout.strip(), "1001")
        # Each is named. The answer alone would not show a zero or a negative
        # being accepted, since neither beats the real maximum.
        for ignored in ("'abc'", "'0'", "'-3'"):
            self.assertIn(ignored, result.stderr)

    def test_only_unusable_values_means_starting_at_one(self):
        tracks = [{"track": "internal", "releases": [{"versionCodes": ["0", "-3"]}]}]
        with Server(self.play_routes(tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.stdout.strip(), "1")

    def test_a_digit_that_int_cannot_read_is_skipped_not_a_traceback(self):
        # '\u00b2' (superscript two) is `isdigit()` but not an integer to `int()`.
        tracks = [{"track": "internal", "releases": [{"versionCodes": ["\u00b2", "1000"]}]}]
        with Server(self.play_routes(tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1001")
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn("'\u00b2'", result.stderr)

    def test_a_field_of_the_wrong_shape_is_ignored_not_a_traceback(self):
        tracks = [
            {"track": "internal", "releases": [
                {"versionCodes": None},
                {"versionCodes": "1234"},
                {"versionCodes": ["1000"]},
            ]},
            {"track": "beta", "releases": None},
            "not-a-track",
        ]
        with Server(self.play_routes(tracks=tracks)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        # "1234" is not split into characters or read as a code.
        self.assertEqual(result.stdout.strip(), "1001")
        self.assertNotIn("Traceback", result.stderr)
        self.assertIn("'1234'", result.stderr)

    def test_null_lists_mean_none_listed(self):
        routes = self.play_routes()
        edits = f"/androidpublisher/v3/applications/{PACKAGE}/edits/edit-1"
        routes[("GET", f"{edits}/bundles")] = lambda handler, body: (200, {"bundles": None})
        routes[("GET", f"{edits}/tracks")] = lambda handler, body: (200, {"tracks": None})
        with Server(routes) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1")
        # Play omitting an empty list is normal, not worth a warning.
        self.assertNotIn("ignoring", result.stderr)

    def test_the_limit_is_enforced(self):
        with Server(self.play_routes(bundles=[helper.MAX_VERSION_CODE])) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("limit", result.stderr)

    # -- what goes over the wire ---------------------------------------------

    def test_the_assertion_is_a_correctly_signed_rs256_jwt_for_the_right_audience(self):
        with Server(self.play_routes(bundles=[1])) as server:
            self.run_helper(server)
            audience = f"{server.base}/token"
        header_b64, claims_b64, signature_b64 = self.assertion.split(".")
        header = json.loads(b64url_decode(header_b64))
        claims = json.loads(b64url_decode(claims_b64))
        self.assertEqual(header, {"alg": "RS256", "typ": "JWT"})
        self.assertEqual(claims["iss"], EMAIL)
        self.assertEqual(claims["scope"], "https://www.googleapis.com/auth/androidpublisher")
        self.assertEqual(claims["aud"], audience)
        self.assertLessEqual(claims["exp"] - claims["iat"], 3600)
        # The signature has to verify against the key's public half, over the
        # first two segments exactly.
        signature = self.dir / "signature.bin"
        signature.write_bytes(b64url_decode(signature_b64))
        verified = subprocess.run(
            ["openssl", "dgst", "-sha256", "-verify", str(self.public), "-signature", str(signature)],
            input=f"{header_b64}.{claims_b64}".encode(),
            capture_output=True,
        )
        self.assertEqual(verified.returncode, 0, verified.stdout + verified.stderr)

    def test_play_calls_carry_the_token_and_the_edit_is_deleted_afterwards(self):
        with Server(self.play_routes(bundles=[1000])) as server:
            self.run_helper(server)
            gets = server.calls("GET")
            deletes = server.calls("DELETE")
        self.assertEqual(len(gets), 2)
        for _, headers, _ in gets + deletes:
            self.assertEqual(headers.get("Authorization"), f"Bearer {ACCESS_TOKEN}")
        self.assertEqual(len(deletes), 1)
        self.assertTrue(deletes[0][0].endswith("/edits/edit-1"))

    def test_the_edit_is_deleted_even_when_a_later_call_fails(self):
        with Server(self.play_routes(bundles_status=403)) as server:
            result = self.run_helper(server)
            deletes = server.calls("DELETE")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(len(deletes), 1)

    # -- failures say what to do ---------------------------------------------

    def test_a_refused_token_says_why_and_shows_no_secret(self):
        with Server(self.play_routes(token_status=400)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("HTTP 400", result.stderr)
        self.assertIn("Invalid JWT signature", result.stderr)
        self.assertIn(EMAIL, result.stderr)
        self.assertNoSecrets(result)

    def test_a_permission_failure_names_the_service_account(self):
        with Server(self.play_routes(edit_status=403)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 1)
        self.assertIn("HTTP 403", result.stderr)
        self.assertIn(EMAIL, result.stderr)
        self.assertIn("Users and permissions", result.stderr)

    def test_an_unknown_app_points_at_the_first_console_upload(self):
        with Server(self.play_routes(edit_status=404)) as server:
            result = self.run_helper(server)
        self.assertEqual(result.returncode, 1)
        self.assertIn(PACKAGE, result.stderr)
        self.assertIn("first upload", result.stderr)

    def test_an_unreachable_server_fails_cleanly(self):
        with Server(self.play_routes()) as server:
            dead = server.base
        # The server is gone by now.
        class Gone:
            base = dead

        result = self.run_helper(Gone, account={
            "client_email": EMAIL,
            "private_key": self.pem,
            "token_uri": f"{dead}/token",
        })
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("could not reach", result.stderr)

    # -- the credential -------------------------------------------------------

    def test_no_key_path_is_a_usage_error(self):
        with Server(self.play_routes()) as server:
            result = self.run_helper(server, env={"PLAY_SERVICE_ACCOUNT_JSON": ""})
        self.assertEqual(result.returncode, 2)
        self.assertIn("PLAY_SERVICE_ACCOUNT_JSON", result.stderr)

    def test_a_missing_key_file_is_reported(self):
        with Server(self.play_routes()) as server:
            result = self.run_helper(server, env={"PLAY_SERVICE_ACCOUNT_JSON": str(self.dir / "nope.json")})
        self.assertEqual(result.returncode, 1)
        self.assertIn("could not read", result.stderr)

    def test_a_key_file_without_the_fields_is_reported(self):
        with Server(self.play_routes()) as server:
            result = self.run_helper(server, account={"client_email": EMAIL})
        self.assertEqual(result.returncode, 1)
        self.assertIn("private_key", result.stderr)

    def test_a_bad_private_key_is_reported_without_printing_it(self):
        with Server(self.play_routes()) as server:
            result = self.run_helper(server, account={
                "client_email": EMAIL,
                "private_key": "-----BEGIN PRIVATE KEY-----\nnot a key\n-----END PRIVATE KEY-----\n",
                "token_uri": f"{server.base}/token",
            })
        self.assertEqual(result.returncode, 1)
        self.assertIn("openssl could not sign", result.stderr)

    def test_the_key_is_not_left_on_disk(self):
        scratch = Path(tempfile.mkdtemp(dir=self.dir))
        with Server(self.play_routes(bundles=[1])) as server:
            result = self.run_helper(server, tmpdir=scratch)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(scratch.iterdir()), [])


if __name__ == "__main__":
    unittest.main(verbosity=2)
