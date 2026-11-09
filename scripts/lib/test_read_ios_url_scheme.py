#!/usr/bin/env python3
"""Tests for read-ios-url-scheme.py.

    python3 scripts/lib/test_read_ios_url_scheme.py
"""

from __future__ import annotations

import sys

sys.dont_write_bytecode = True

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

HELPER = Path(__file__).with_name("read-ios-url-scheme.py")
REAL_CONF = Path(__file__).resolve().parents[2] / "clients/web/src-tauri/tauri.conf.json"


def conf(scheme="org.example.app"):
    return {"plugins": {"deep-link": {"desktop": {"schemes": [scheme]}}}}


class ReadSchemeTest(unittest.TestCase):
    def run_helper(self, config, *overrides):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "tauri.conf.json"
            path.write_text(config if isinstance(config, str) else json.dumps(config))
            return subprocess.run(
                [sys.executable, "-I", str(HELPER), str(path), *overrides],
                capture_output=True,
                text=True,
            )

    def test_reads_the_configured_scheme(self):
        r = self.run_helper(conf("org.fork.axon"))
        self.assertEqual((r.returncode, r.stdout), (0, "org.fork.axon\n"), r.stderr)

    def test_the_shipped_config_declares_a_scheme(self):
        r = subprocess.run(
            [sys.executable, "-I", str(HELPER), str(REAL_CONF)], capture_output=True, text=True
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout, "org.matrixaxon.axon\n")

    def test_an_override_wins_over_the_file(self):
        override = json.dumps(conf("org.test.bundle"))
        r = self.run_helper(conf(), override)
        self.assertEqual(r.stdout, "org.test.bundle\n", r.stderr)

    def test_an_unrelated_override_leaves_the_scheme_alone(self):
        # What package-ios.sh passes today: `--config '{"bundle":...}'`.
        override = '{"bundle":{"iOS":{"bundleVersion":"26"}}}'
        r = self.run_helper(conf("org.fork.axon"), "--config", override)
        self.assertEqual((r.returncode, r.stdout), (0, "org.fork.axon\n"), r.stderr)

    def test_a_missing_scheme_fails(self):
        for bad in ({}, {"plugins": {}}, conf(""), {"plugins": {"deep-link": {"desktop": {"schemes": []}}}}):
            with self.subTest(bad=bad):
                r = self.run_helper(bad)
                self.assertEqual(r.returncode, 1)
                self.assertEqual(r.stdout, "")
                self.assertIn("iOS URL scheme", r.stderr)

    def test_an_override_can_remove_the_scheme(self):
        r = self.run_helper(conf(), '{"plugins":{"deep-link":null}}')
        self.assertEqual(r.returncode, 1)

    def test_unreadable_or_invalid_input_fails(self):
        self.assertEqual(self.run_helper("{not json").returncode, 1)
        self.assertEqual(self.run_helper(conf(), "{not json").returncode, 1)


if __name__ == "__main__":
    unittest.main()
