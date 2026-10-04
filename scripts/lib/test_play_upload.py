#!/usr/bin/env python3
"""Tests for play-upload.py.

    python3 scripts/lib/test_play_upload.py

Run as a subprocess against the local stand-in for Google and Play from
test_play_next_version_code.py, with a service-account key generated for the
occasion. Nothing touches the network or a real credential. What this cannot show is
that Play accepts the calls and returns the shapes the stand-in does; the first run
against the real thing, in `--mode check`, is what shows that.
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import test_play_next_version_code as base

HELPER = Path(__file__).with_name("play-upload.py")
PACKAGE = base.PACKAGE
EDITS = f"/androidpublisher/v3/applications/{PACKAGE}/edits"
UPLOAD = f"/upload/androidpublisher/v3/applications/{PACKAGE}/edits"

BUNDLE_BYTES = b"PK\x03\x04 pretend this is an app bundle " * 50
SYMBOLS_BYTES = b"PK\x03\x04 pretend this is a symbols zip " * 20
VERSION_CODE = 1001


class UploadTest(base.HelperTest):
    """Reuses the key generation and plumbing of the version-code tests."""

    # The inherited tests exercise the other helper; only the plumbing is wanted.
    test_names = ()

    def routes(self, *, track=None, track_status=200, bundle_status=200, bundle_sha=None,
               symbols_status=200, validate_status=200, commit_status=200, put_status=200):
        base_routes = self.play_routes()
        self.put_body = None

        def bundle(handler, body):
            if bundle_status != 200:
                return bundle_status, {"error": {"message": "The bundle is not valid"}}
            return 200, {
                "versionCode": VERSION_CODE,
                "sha256": bundle_sha or hashlib.sha256(body).hexdigest(),
                "sha1": "ignored",
            }

        def symbols(handler, body):
            if symbols_status != 200:
                return symbols_status, {"error": {"message": "bad symbols"}}
            return 200, {"deobfuscationFile": {"symbolType": "nativeCode"}}

        def track_get(handler, body):
            if track_status != 200:
                return track_status, {"error": {"message": "Track not found"}}
            return 200, track if track is not None else {"track": "internal", "releases": []}

        def track_put(handler, body):
            self.put_body = json.loads(body)
            if put_status != 200:
                return put_status, {"error": {"message": "invalid release"}}
            return 200, self.put_body

        base_routes.update({
            ("POST", f"{UPLOAD}/edit-1/bundles"): bundle,
            ("POST", f"{UPLOAD}/edit-1/apks/{VERSION_CODE}/deobfuscationFiles/nativeCode"): symbols,
            ("GET", f"{EDITS}/edit-1/tracks/internal"): track_get,
            ("PUT", f"{EDITS}/edit-1/tracks/internal"): track_put,
            ("POST", f"{EDITS}/edit-1:validate"): lambda h, b: (validate_status, {"id": "edit-1"} if validate_status == 200 else {"error": {"message": "validation failed"}}),
            ("POST", f"{EDITS}/edit-1:commit"): lambda h, b: (commit_status, {"id": "edit-1"} if commit_status == 200 else {"error": {"message": "cannot commit"}}),
        })
        return base_routes

    def files(self):
        directory = Path(tempfile.mkdtemp(dir=self.dir))
        (directory / "app.aab").write_bytes(BUNDLE_BYTES)
        (directory / "symbols.zip").write_bytes(SYMBOLS_BYTES)
        return directory / "app.aab", directory / "symbols.zip"

    def run_upload(self, server, *extra, symbols=True, bundle=None, env=None):
        aab, zipped = self.files()
        args = ["--package", PACKAGE, "--bundle", str(bundle or aab)]
        if symbols:
            args += ["--symbols", str(zipped)]
        args += list(extra)
        directory = Path(tempfile.mkdtemp(dir=self.dir))
        key = directory / "sa.json"
        key.write_text(json.dumps({
            "client_email": base.EMAIL,
            "private_key": self.pem,
            "token_uri": f"{server.base}/token",
        }))
        environment = {
            "PATH": os.environ["PATH"],
            "HOME": str(directory),
            "PLAY_SERVICE_ACCOUNT_JSON": str(key),
            "PLAY_API_BASE": server.base,
        }
        environment.update(env or {})
        return subprocess.run(
            [sys.executable, "-B", str(HELPER), *args],
            env=environment, capture_output=True, text=True, timeout=120,
        )

    def paths(self, server, method):
        return [p for m, p, _, _ in server.requests if m == method]

    def assertNothingCommitted(self, server):
        self.assertNotIn(f"{EDITS}/edit-1:commit", self.paths(server, "POST"))

    def assertEditDeleted(self, server):
        self.assertIn(f"{EDITS}/edit-1", self.paths(server, "DELETE"))

    # -- check mode: proves acceptance, changes nothing -----------------------

    def test_check_mode_uploads_and_validates_but_commits_nothing(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server)
            posts = self.paths(server, "POST")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), str(VERSION_CODE))
        self.assertIn(f"{UPLOAD}/edit-1/bundles", posts)
        self.assertIn(f"{EDITS}/edit-1:validate", posts)
        self.assertIn("nothing was committed", result.stderr)

    def test_check_is_the_default_mode(self):
        with base.Server(self.routes()) as server:
            self.run_upload(server)
            self.assertNothingCommitted(server)

    # -- what goes over the wire ---------------------------------------------

    def test_the_bytes_sent_are_the_files_and_the_calls_are_authorised(self):
        with base.Server(self.routes()) as server:
            self.run_upload(server)
            uploads = [(p, h, b) for m, p, h, b in server.requests if m == "POST" and p.startswith("/upload")]
        by_path = {p: (h, b) for p, h, b in uploads}
        headers, body = by_path[f"{UPLOAD}/edit-1/bundles"]
        self.assertEqual(body, BUNDLE_BYTES)
        self.assertEqual(headers["Content-Type"], "application/octet-stream")
        self.assertEqual(headers["Authorization"], f"Bearer {base.ACCESS_TOKEN}")
        headers, body = by_path[f"{UPLOAD}/edit-1/apks/{VERSION_CODE}/deobfuscationFiles/nativeCode"]
        self.assertEqual(body, SYMBOLS_BYTES)
        self.assertEqual(headers["Authorization"], f"Bearer {base.ACCESS_TOKEN}")

    def test_uploads_ask_for_media_upload(self):
        with base.Server(self.routes()) as server:
            self.run_upload(server)
            queries = dict(server.queries)
        self.assertEqual(queries[f"{UPLOAD}/edit-1/bundles"], "uploadType=media")
        self.assertEqual(
            queries[f"{UPLOAD}/edit-1/apks/{VERSION_CODE}/deobfuscationFiles/nativeCode"],
            "uploadType=media",
        )

    def test_no_symbols_means_no_symbols_upload(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, symbols=False)
            posts = self.paths(server, "POST")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse([p for p in posts if "deobfuscationFiles" in p])

    # -- draft mode -----------------------------------------------------------

    def test_draft_commits_a_draft_release_and_keeps_the_existing_ones(self):
        track = {"track": "internal", "releases": [
            {"name": "1000", "versionCodes": ["1000"], "status": "completed"},
        ]}
        with base.Server(self.routes(track=track)) as server:
            result = self.run_upload(server, "--mode", "draft", "--name", "0.1.0 (1001)")
            commits = [p for p in self.paths(server, "POST") if p.endswith(":commit")]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(commits), 1)
        self.assertEqual(self.put_body["track"], "internal")
        self.assertEqual(self.put_body["releases"], [
            {"name": "1000", "versionCodes": ["1000"], "status": "completed"},
            {"name": "0.1.0 (1001)", "versionCodes": [str(VERSION_CODE)], "status": "draft"},
        ])

    def test_draft_refuses_to_replace_someone_elses_draft(self):
        track = {"track": "internal", "releases": [
            {"name": "mine", "versionCodes": ["1000"], "status": "draft"},
        ]}
        with base.Server(self.routes(track=track)) as server:
            result = self.run_upload(server, "--mode", "draft")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("already has a draft", result.stderr)
        self.assertIsNone(self.put_body)

    def test_a_missing_track_is_treated_as_empty(self):
        with base.Server(self.routes(track_status=404)) as server:
            result = self.run_upload(server, "--mode", "draft")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual([r["status"] for r in self.put_body["releases"]], ["draft"])

    # -- release mode ---------------------------------------------------------

    def test_release_commits_a_completed_release_that_replaces_the_track(self):
        track = {"track": "internal", "releases": [
            {"name": "1000", "versionCodes": ["1000"], "status": "completed"},
        ]}
        with base.Server(self.routes(track=track)) as server:
            result = self.run_upload(server, "--mode", "release")
            commits = [p for p in self.paths(server, "POST") if p.endswith(":commit")]
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(commits), 1)
        self.assertEqual(self.put_body["releases"], [
            {"name": str(VERSION_CODE), "versionCodes": [str(VERSION_CODE)], "status": "completed"},
        ])

    def test_release_says_what_it_replaces(self):
        track = {"track": "internal", "releases": [
            {"name": "old build", "versionCodes": ["1000"], "status": "completed"},
        ]}
        with base.Server(self.routes(track=track)) as server:
            result = self.run_upload(server, "--mode", "release")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("replacing the completed release old build", result.stderr)

    def test_release_refuses_to_replace_a_draft_or_a_rollout(self):
        for status in ("draft", "inProgress", "halted"):
            with self.subTest(status=status):
                track = {"track": "internal", "releases": [
                    {"name": "someone's", "versionCodes": ["1000"], "status": status},
                ]}
                with base.Server(self.routes(track=track)) as server:
                    result = self.run_upload(server, "--mode", "release")
                    self.assertNothingCommitted(server)
                    self.assertEditDeleted(server)
                self.assertEqual(result.returncode, 1)
                self.assertEqual(result.stdout, "")
                self.assertIn(f"a {status} release (someone's)", result.stderr)
                self.assertIn("Play Console", result.stderr)
                self.assertIsNone(self.put_body)

    def test_release_may_replace_a_draft_of_the_same_version(self):
        track = {"track": "internal", "releases": [
            {"name": "mine", "versionCodes": [str(VERSION_CODE)], "status": "draft"},
        ]}
        with base.Server(self.routes(track=track)) as server:
            result = self.run_upload(server, "--mode", "release")
        self.assertEqual(result.returncode, 0, result.stderr)

    # -- the versionCode is checked before anything is committed --------------

    def test_a_matching_expected_version_code_goes_ahead(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, "--mode", "draft", "--expect-version-code", str(VERSION_CODE))
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_different_version_code_stops_before_symbols_and_commit(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, "--mode", "release", "--expect-version-code", str(VERSION_CODE + 1))
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
            symbols = [p for p in self.paths(server, "POST") if "deobfuscationFiles" in p]
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn(f"versionCode {VERSION_CODE}", result.stderr)
        self.assertIn(str(VERSION_CODE + 1), result.stderr)
        self.assertEqual(symbols, [])
        self.assertIsNone(self.put_body)

    # -- failures leave nothing behind ---------------------------------------

    def test_a_rejected_bundle_stops_everything_and_cleans_up(self):
        with base.Server(self.routes(bundle_status=400)) as server:
            result = self.run_upload(server, "--mode", "release")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("The bundle is not valid", result.stderr)

    def test_a_hash_that_does_not_match_the_file_stops_everything(self):
        with base.Server(self.routes(bundle_sha="0" * 64)) as server:
            result = self.run_upload(server, "--mode", "release")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 1)
        self.assertIn("sha256", result.stderr)

    def test_a_failed_validation_commits_nothing(self):
        with base.Server(self.routes(validate_status=400)) as server:
            result = self.run_upload(server, "--mode", "release")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 1)
        self.assertIn("validation failed", result.stderr)

    def test_a_failed_symbols_upload_commits_nothing(self):
        with base.Server(self.routes(symbols_status=400)) as server:
            result = self.run_upload(server, "--mode", "release")
            self.assertNothingCommitted(server)
            self.assertEditDeleted(server)
        self.assertEqual(result.returncode, 1)

    def test_a_failed_commit_reports_it_and_prints_no_version_code(self):
        with base.Server(self.routes(commit_status=400)) as server:
            result = self.run_upload(server, "--mode", "draft")
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("cannot commit", result.stderr)

    def test_a_permission_failure_names_the_service_account(self):
        with base.Server(self.routes(put_status=403)) as server:
            result = self.run_upload(server, "--mode", "draft")
        self.assertEqual(result.returncode, 1)
        self.assertIn(base.EMAIL, result.stderr)

    # -- inputs ---------------------------------------------------------------

    def test_a_missing_bundle_fails_before_any_network_call(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, bundle=self.dir / "does-not-exist.aab")
            self.assertEqual(server.requests, [])
        self.assertEqual(result.returncode, 1)
        self.assertIn("could not read the bundle", result.stderr)

    def test_no_credential_is_a_usage_error(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, env={"PLAY_SERVICE_ACCOUNT_JSON": ""})
        self.assertEqual(result.returncode, 2)

    def test_an_unknown_mode_is_rejected(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, "--mode", "everything")
            self.assertEqual(server.requests, [])
        self.assertEqual(result.returncode, 2)

    def test_no_secret_reaches_any_output(self):
        with base.Server(self.routes()) as server:
            result = self.run_upload(server, "--mode", "draft")
        self.assertNoSecrets(result)


# Keep the inherited plumbing but not the inherited tests: they would run the other
# helper's cases again against this class's routes.
for _name in [n for n in dir(base.HelperTest) if n.startswith("test_")]:
    if _name not in UploadTest.__dict__:
        setattr(UploadTest, _name, None)

if __name__ == "__main__":
    unittest.main(verbosity=2)
