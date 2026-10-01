#!/usr/bin/env python3
"""Tests for asc-next-build-number.py.

    python3 scripts/lib/test_asc_next_build_number.py

The helper is run as a subprocess against a local server that stands in for
App Store Connect, with HOME pointed at a throwaway directory holding a freshly
generated key, so nothing here touches the network or a real credential. What
this cannot show is that Apple accepts the token or that the real API returns
the shapes the server below does; that has to be checked once against the real
thing.
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
import ssl
import subprocess
import tempfile
import threading
import unittest
from pathlib import Path

HELPER = Path(__file__).with_name("asc-next-build-number.py")

spec = importlib.util.spec_from_file_location("asc_next_build_number", HELPER)
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)

BUNDLE = "org.example.app"
KEY_ID = "TESTKEY123"
ISSUER = "11111111-2222-3333-4444-555555555555"


def b64url_decode(text: str) -> bytes:
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))


def raw_to_der(raw: bytes) -> bytes:
    """The inverse of der_to_raw, so a token's signature can go back to openssl."""

    def integer(chunk: bytes) -> bytes:
        body = chunk.lstrip(b"\0") or b"\0"
        if body[0] & 0x80:
            body = b"\0" + body
        return b"\x02" + bytes([len(body)]) + body

    body = integer(raw[:32]) + integer(raw[32:])
    return b"\x30" + bytes([len(body)]) + body


class Server:
    """A fake App Store Connect. `routes` maps a path to a handler -> (status, body)."""

    def __init__(self, routes):
        self.requests: list[tuple[str, str]] = []
        outer = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                path = self.path.split("?")[0]
                outer.requests.append((self.path, self.headers.get("Authorization", "")))
                status, body = routes.get(path, lambda: (404, {}))()
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        self.httpd = http.server.HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


class NextVersion(unittest.TestCase):
    def test_numbers_compare_as_numbers_not_text(self):
        self.assertEqual(helper.next_version(["1", "2", "10", "9"]), "11")

    def test_dotted_versions_raise_the_last_component(self):
        self.assertEqual(helper.next_version(["1.2", "1.10", "1.9"]), "1.11")

    def test_a_longer_version_beats_a_shorter_one_that_is_its_prefix(self):
        self.assertEqual(helper.next_version(["1", "1.0.4"]), "1.0.5")

    def test_no_builds_starts_at_one(self):
        self.assertEqual(helper.next_version([]), "1")

    def test_unparseable_values_are_skipped_not_guessed_at(self):
        self.assertEqual(helper.next_version(["3", "beta", "1.2.3.4"]), "4")

    def test_only_unparseable_values_starts_at_one(self):
        self.assertEqual(helper.next_version(["beta"]), "1")


class TlsContext(unittest.TestCase):
    @unittest.skipUnless(os.path.isfile(helper.SYSTEM_CA_BUNDLE), "no system CA bundle here")
    def test_the_system_ca_bundle_is_trusted_even_when_python_has_none(self):
        # A python.org Python has no default CA file, so a bare default context
        # trusts nothing and every HTTPS request fails verification. On a Python
        # that was set up properly this passes either way, so it only has teeth
        # on the broken one; that is the case it exists for.
        self.assertGreater(helper.ssl_context().cert_store_stats()["x509_ca"], 0)


class Signature(unittest.TestCase):
    def test_der_roundtrip_including_a_high_bit_and_short_integers(self):
        for r, s in [(1, 2), (2**255 + 5, 7), (2**256 - 1, 2**250)]:
            raw = r.to_bytes(32, "big") + s.to_bytes(32, "big")
            self.assertEqual(helper.der_to_raw(raw_to_der(raw)), raw)


class EndToEnd(unittest.TestCase):
    def setUp(self):
        self.home = tempfile.TemporaryDirectory()
        keydir = Path(self.home.name, ".appstoreconnect", "private_keys")
        keydir.mkdir(parents=True)
        self.key = keydir / f"AuthKey_{KEY_ID}.p8"
        ec = Path(self.home.name, "ec.pem")
        subprocess.run(["openssl", "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", str(ec)], check=True, capture_output=True)
        subprocess.run(["openssl", "pkcs8", "-topk8", "-nocrypt", "-in", str(ec), "-out", str(self.key)], check=True, capture_output=True)
        self.pub = Path(self.home.name, "pub.pem")
        subprocess.run(["openssl", "ec", "-in", str(ec), "-pubout", "-out", str(self.pub)], check=True, capture_output=True)
        self.server = None

    def tearDown(self):
        if self.server:
            self.server.close()
        self.home.cleanup()

    def run_helper(self, routes, env_extra=None, args=None):
        self.server = Server(routes)
        env = {
            **os.environ,
            "HOME": self.home.name,
            "ASC_API_BASE": self.server.url,
            "ASC_KEY_ID": KEY_ID,
            "ASC_ISSUER_ID": ISSUER,
            **(env_extra or {}),
        }
        return subprocess.run(
            [sys.executable, str(HELPER), *(args or [BUNDLE])],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )

    def apps(self, bundle=BUNDLE):
        return lambda: (200, {"data": [{"id": "APP1", "attributes": {"bundleId": bundle}}]})

    def test_picks_the_highest_number_across_pages_and_adds_one(self):
        def page1():
            return 200, {
                "data": [{"attributes": {"version": "2"}}, {"attributes": {"version": "9"}}],
                "links": {"next": f"{self.server.url}/v1/builds?cursor=2"},
            }

        def page2():
            return 200, {"data": [{"attributes": {"version": "10"}}]}

        # Both pages are served from the same path; the second is told apart by
        # call order, which is what a cursor is.
        calls = []

        def builds():
            calls.append(1)
            return page1() if len(calls) == 1 else page2()

        result = self.run_helper({"/v1/apps": self.apps(), "/v1/builds": builds})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "11")

    def test_an_app_with_no_builds_gets_one(self):
        result = self.run_helper({"/v1/apps": self.apps(), "/v1/builds": lambda: (200, {"data": []})})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), "1")

    def test_the_token_is_a_valid_es256_jwt_for_this_key(self):
        result = self.run_helper({"/v1/apps": self.apps(), "/v1/builds": lambda: (200, {"data": []})})
        self.assertEqual(result.returncode, 0, result.stderr)
        auth = next(a for _, a in self.server.requests if a)
        self.assertTrue(auth.startswith("Bearer "))
        head, claims, sig = auth.removeprefix("Bearer ").split(".")
        self.assertEqual(json.loads(b64url_decode(head)), {"alg": "ES256", "kid": KEY_ID, "typ": "JWT"})
        body = json.loads(b64url_decode(claims))
        self.assertEqual(body["iss"], ISSUER)
        self.assertEqual(body["aud"], "appstoreconnect-v1")
        self.assertLessEqual(body["exp"] - body["iat"], 1200)
        sigfile = Path(self.home.name, "sig.der")
        sigfile.write_bytes(raw_to_der(b64url_decode(sig)))
        verified = subprocess.run(
            ["openssl", "dgst", "-sha256", "-verify", str(self.pub), "-signature", str(sigfile)],
            input=f"{head}.{claims}".encode(),
            capture_output=True,
        )
        self.assertEqual(verified.returncode, 0, verified.stdout + verified.stderr)

    def test_an_unknown_bundle_id_is_an_error_with_nothing_on_stdout(self):
        result = self.run_helper({"/v1/apps": lambda: (200, {"data": []})})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("no app with bundle ID", result.stderr)

    def test_a_prefix_match_for_a_different_app_is_not_taken(self):
        result = self.run_helper({"/v1/apps": self.apps(bundle=BUNDLE + ".beta")})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")

    def test_a_401_names_the_credentials(self):
        result = self.run_helper({"/v1/apps": lambda: (401, {})})
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("rejected the credentials", result.stderr)

    def test_a_403_names_the_role(self):
        result = self.run_helper({"/v1/apps": self.apps(), "/v1/builds": lambda: (403, {})})
        self.assertEqual(result.returncode, 1)
        self.assertIn("App Manager", result.stderr)

    def test_a_missing_key_file_is_reported_before_any_request(self):
        self.key.unlink()
        result = self.run_helper({}, args=[BUNDLE])
        self.assertEqual(result.returncode, 2)
        self.assertIn("no key at", result.stderr)
        self.assertEqual(self.server.requests, [])

    def test_missing_credentials_are_reported_before_any_request(self):
        result = self.run_helper({}, env_extra={"ASC_KEY_ID": ""})
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.server.requests, [])


if __name__ == "__main__":
    unittest.main()
