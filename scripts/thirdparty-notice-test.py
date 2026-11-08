#!/usr/bin/env python3
"""Tests for thirdparty-notice.py. Run: python3 scripts/thirdparty-notice-test.py

Stdlib only, like the script. The cases marked "regression" are bugs found while
verifying the first version against the real cargo-about output of this repo.
"""

import importlib.util
import json
import pathlib
import subprocess
import sys
import tempfile
import unittest

_path = pathlib.Path(__file__).with_name("thirdparty-notice.py")
_spec = importlib.util.spec_from_file_location("thirdparty_notice", _path)
tn = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tn)

MIT_BODY = """Permission is hereby granted, free of charge, to any person obtaining a copy
of this software, to deal in the Software without restriction.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND."""

BSD3_TEMPLATE = """Redistribution and use in source and binary forms are permitted.

3. Neither the name of {who} nor the names of its contributors may be used.

THIS SOFTWARE IS PROVIDED "AS IS"."""


def mit(*statements: str) -> str:
    return "MIT License\n\n" + "\n".join(statements) + "\n\n" + MIT_BODY + "\n"


def lic(spdx: str, text: str, *crates: tuple[str, str], name: str = "") -> dict:
    return {
        "id": spdx,
        "name": name or spdx,
        "text": text,
        "used_by": [{"crate": {"name": n, "version": v}, "path": None} for n, v in crates],
    }


def doc(*licenses: dict) -> dict:
    counts: dict[str, int] = {}
    for item in licenses:
        counts[item["id"]] = counts.get(item["id"], 0) + len(item["used_by"])
    return {
        "overview": [{"name": k, "id": k, "count": v} for k, v in counts.items()],
        "licenses": list(licenses),
        "crates": [],
    }


def blocks(*licenses: dict) -> list:
    return tn.merge(list(licenses))


class SplitStatements(unittest.TestCase):
    def test_header_statements_are_extracted(self):
        rest, found, inline = tn.split_statements(mit("Copyright (c) 2020 A", "Copyright 2021 B"))
        self.assertEqual(found, ["Copyright (c) 2020 A", "Copyright 2021 B"])
        self.assertFalse(inline)
        self.assertNotIn("Copyright", rest)
        self.assertIn("Permission is hereby granted", rest)

    def test_comment_markers_and_all_rights_reserved(self):
        text = "// Copyright 2015-2016 Brian Smith.\n// All rights reserved.\n//\n// Permission to use.\n"
        _, found, _ = tn.split_statements(text)
        self.assertEqual(found, ["Copyright 2015-2016 Brian Smith. All rights reserved."])

    def test_apache_wrapped_lines_are_not_statements(self):
        # Apache-2.0 wraps lines that start with the lowercase word "copyright",
        # and its appendix carries an unfilled template. Neither is anyone's attribution.
        text = (
            "grants to You a perpetual, worldwide,\n"
            "copyright license to reproduce, prepare Derivative Works of,\n"
            "copyright notice that is included in or attached to the work\n"
            "Copyright [yyyy] [name of copyright owner]\n"
            "COPYRIGHT HOLDERS BE LIABLE\n"
        )
        rest, found, inline = tn.split_statements(text)
        self.assertEqual(found, [])
        self.assertFalse(inline)
        self.assertIn("Copyright [yyyy]", rest)
        self.assertIn("copyright license to reproduce", rest)

    def test_a_stray_body_line_is_not_taken_for_a_statement(self):
        # A line that merely starts with the word is license text, not a holder.
        text = "MIT License\n\nPermission.\n\nCopyright Notice: keep this.\nCopyright Holder rights.\n\nAS IS.\n"
        rest, found, inline = tn.split_statements(text)
        self.assertEqual(found, [])
        self.assertTrue(inline)  # left in the text, and the label says to look there
        self.assertIn("Copyright Notice: keep this.", rest)
        self.assertIn("Copyright Holder rights.", rest)

    def test_unfilled_template_stays_in_the_text(self):
        _, found, inline = tn.split_statements("Copyright (c) <year> <owner>.\n\nPermission.\n")
        self.assertEqual(found, [])
        self.assertFalse(inline)  # boilerplate, not a statement the reader needs to find

    def test_a_holder_only_line_is_left_in_place_and_labelled(self):
        # utf8_iter: "Copyright Mozilla Foundation" has no year or (c).
        rest, found, inline = tn.split_statements("Copyright Mozilla Foundation\n\nPermission.\n")
        self.assertEqual(found, [])
        self.assertTrue(inline)
        self.assertIn("Copyright Mozilla Foundation", rest)

    def test_a_text_with_no_copyright_wording_is_not_labelled(self):
        self.assertEqual(tn.split_statements("Permission is hereby granted.\n")[2], False)

    def test_scattered_statements_are_left_in_place(self):
        # regression: atomic-waker's LICENSE-THIRD-PARTY repeats the same holders
        # once per embedded license section. Extracting them would print each
        # holder once and lose which section it belongs to.
        text = "Copyright (c) 2016 A\n\nFirst license text.\n\n====\n\nCopyright (c) 2016 A\n\nSecond.\n"
        rest, found, inline = tn.split_statements(text)
        self.assertEqual(found, [])
        self.assertTrue(inline)
        self.assertEqual(rest.count("Copyright (c) 2016 A"), 2)

    def test_blank_lines_between_statements_are_one_cluster(self):
        _, found, inline = tn.split_statements("Copyright (c) 1 A\n\nCopyright (c) 2 B\n\nPermission.\n")
        self.assertEqual(found, ["Copyright (c) 1 A", "Copyright (c) 2 B"])
        self.assertFalse(inline)


class Merge(unittest.TestCase):
    def test_texts_differing_only_in_the_copyright_line_merge(self):
        out = blocks(
            lic("MIT", mit("Copyright (c) 2020 Alice"), ("a", "1.0")),
            lic("MIT", mit("Copyright (c) 2018 Bob"), ("b", "2.0")),
            lic("MIT", mit("Copyright (c) 2018 Bob"), ("c", "3.0")),
        )
        self.assertEqual(len(out), 1)
        by_crate = out[0].crates
        self.assertEqual(by_crate[("a", "1.0")], ["Copyright (c) 2020 Alice"])
        self.assertEqual(by_crate[("b", "2.0")], ["Copyright (c) 2018 Bob"])
        self.assertEqual(by_crate[("c", "3.0")], ["Copyright (c) 2018 Bob"])
        groups = out[0].groups()
        self.assertEqual(len(groups), 2)  # Bob's two crates share a heading
        self.assertIn((("Copyright (c) 2018 Bob",), [("b", "2.0"), ("c", "3.0")]), groups)

    def test_whitespace_and_wrapping_differences_merge(self):
        wrapped = mit("Copyright (c) 1 A").replace("any person obtaining a copy\n", "any person obtaining a copy ")
        out = blocks(
            lic("MIT", mit("Copyright (c) 1 A"), ("a", "1")),
            lic("MIT", wrapped.replace("Copyright (c) 1 A", "Copyright (c) 2 B"), ("b", "1")),
        )
        self.assertEqual(len(out), 1)

    def test_different_clause_text_does_not_merge(self):
        # The Meta/zstd BSD-3 names Facebook in clause 3. Only the copyright line
        # may be normalized away.
        out = blocks(
            lic("BSD-3-Clause", "Copyright (c) 2026 A\n\n" + BSD3_TEMPLATE.format(who="the copyright holder"), ("a", "1")),
            lic("BSD-3-Clause", "Copyright (c) Meta\n\n" + BSD3_TEMPLATE.format(who="Facebook, nor Meta"), ("b", "1")),
        )
        self.assertEqual(len(out), 2)

    def test_same_text_under_different_spdx_ids_does_not_merge(self):
        out = blocks(
            lic("MIT", mit("Copyright (c) 1 A"), ("a", "1")),
            lic("ISC", mit("Copyright (c) 1 A"), ("b", "1")),
        )
        self.assertEqual(len(out), 2)

    def test_a_crate_with_two_files_in_one_block_keeps_both_statement_sets(self):
        # regression: the second file's statements used to overwrite the first's,
        # so the result depended on file order. Check both orders.
        bare = lic("MIT", mit(), ("dual", "1"))  # LICENSE-MIT with no statement
        held = lic("MIT", mit("Copyright (c) 2016 Alex", "Copyright (c) 2017 Tokio"), ("dual", "1"))
        for order in ((bare, held), (held, bare)):
            with self.subTest(first=order[0] is held):
                out = blocks(*order)
                self.assertEqual(len(out), 1)
                self.assertEqual(
                    out[0].crates[("dual", "1")],
                    ["Copyright (c) 2016 Alex", "Copyright (c) 2017 Tokio"],
                )

    def test_inline_statements_are_labelled(self):
        text = "Copyright (c) 1 A\n\nOne.\n\n====\n\nCopyright (c) 1 A\n\nTwo.\n"
        out = blocks(lic("MIT", text, ("x", "1")))
        self.assertEqual(out[0].crates[("x", "1")], [tn.INLINE])

    def test_nothing_is_lost(self):
        # merge() runs check_lossless on every entry; make sure it is wired to
        # the real text and not vacuously true.
        entry = lic("MIT", mit("Copyright (c) 2020 Alice"), ("a", "1"))
        real = tn.split_statements
        try:
            tn.split_statements = lambda t: (real(t)[0].replace("WARRANTY", ""), real(t)[1], False)
            with self.assertRaises(RuntimeError):
                tn.merge([entry])
        finally:
            tn.split_statements = real


class CheckLossless(unittest.TestCase):
    def test_accepts_statements_spliced_in_once(self):
        original = "Title\n\nCopyright (c) 1 A\n\nBody one. Body two.\n"
        tn.check_lossless(original, "Title\n\nBody one. Body two.", ["Copyright (c) 1 A"], "x")

    def test_accepts_a_statement_that_starts_like_the_text_after_it(self):
        # The original and the remainder agree through "Head Copyright (c) 1",
        # past the real splice point after "Head". A check that only tried the end
        # of the common prefix would reject this valid merge and abort the build.
        original = "Head Copyright (c) 1 A Copyright (c) 1 B"
        tn.check_lossless(original, "Head Copyright (c) 1 B", ["Copyright (c) 1 A"], "x")

    def test_rejects_a_dropped_word(self):
        with self.assertRaises(RuntimeError):
            tn.check_lossless("a b c", "a c", [], "x")

    def test_rejects_words_moved_out_of_order(self):
        # Same words, so a multiset comparison would pass this.
        with self.assertRaises(RuntimeError):
            tn.check_lossless("one two three four", "one three two four", [], "x")

    def test_rejects_body_text_relabelled_as_a_statement(self):
        # "Permission." moved from the body into the printed statements. The words
        # and their order are unchanged, so only the shape of the statement can
        # give it away.
        with self.assertRaises(RuntimeError):
            tn.check_lossless(
                "Copyright (c) 1 A\nPermission. Body.",
                "Body.",
                ["Copyright (c) 1 A", "Permission."],
                "x",
            )

    def test_still_raises_under_python_dash_O(self):
        code = (
            "import importlib.util,sys;"
            f"s=importlib.util.spec_from_file_location('tn',{str(_path)!r});"
            "m=importlib.util.module_from_spec(s);s.loader.exec_module(m);"
            "m.check_lossless('a b c','a c',[],'x')"
        )
        result = subprocess.run([sys.executable, "-I", "-O", "-c", code], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("RuntimeError", result.stderr)


class Render(unittest.TestCase):
    DATA = doc(
        lic("MIT", mit("Copyright (c) 2020 Alice"), ("a", "1.0"), name='MIT "License"'),
        lic("MIT", mit("Copyright (c) 2018 Bob"), ("b", "2.0"), name='MIT "License"'),
    )

    def test_plain_is_not_html_escaped(self):
        out = tn.render(self.DATA, "plain", "Title\n")
        self.assertIn('"AS IS"', out)
        self.assertNotIn("&quot;", out)

    def test_markdown_is_html_escaped(self):
        out = tn.render(self.DATA, "markdown", "# Title\n")
        self.assertIn("&quot;AS IS&quot;", out)
        self.assertIn("## LICENSE: MIT &quot;License&quot;", out)

    def test_each_text_is_printed_once_with_every_crate_and_statement(self):
        out = tn.render(self.DATA, "plain", "Title\n")
        self.assertEqual(out.count("Permission is hereby granted"), 1)
        for needle in ("Copyright (c) 2020 Alice", "Copyright (c) 2018 Bob", "a 1.0", "b 2.0"):
            self.assertEqual(out.count(needle), 1, needle)

    def test_overview_and_header(self):
        out = tn.render(self.DATA, "plain", "Title\n\nIntro.\n")
        self.assertTrue(out.startswith("Title\n\nIntro.\n\nOVERVIEW\n"))
        self.assertIn("- MIT (used by 2 crates)", out)


class Cli(unittest.TestCase):
    """The command line reads the JSON from a file argument or from stdin."""

    DATA = doc(lic("MIT", mit("Copyright (c) 2020 Alice"), ("a", "1.0")))

    def run_cli(self, *, as_file: bool) -> str:
        with tempfile.TemporaryDirectory() as tmp:
            header = pathlib.Path(tmp, "header.txt")
            header.write_text("Title\n", encoding="utf-8")
            argv = [sys.executable, "-I", str(_path), "--format", "plain", "--header", str(header)]
            payload = json.dumps(self.DATA)
            if as_file:
                data = pathlib.Path(tmp, "in.json")
                data.write_text(payload, encoding="utf-8")
                result = subprocess.run(argv + [str(data)], capture_output=True, text=True, check=True)
            else:
                result = subprocess.run(argv, input=payload, capture_output=True, text=True, check=True)
            return result.stdout

    def test_file_argument_and_stdin_agree(self):
        from_file = self.run_cli(as_file=True)
        self.assertIn("Copyright (c) 2020 Alice", from_file)
        self.assertEqual(from_file, self.run_cli(as_file=False))


if __name__ == "__main__":
    unittest.main()
