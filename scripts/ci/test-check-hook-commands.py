#!/usr/bin/env python3
"""Tests for scripts/check-hook-commands.py and scripts/ci/run-hook-entry.py.

    python3 scripts/ci/test-check-hook-commands.py

The cases are synthetic. The real tree is the last test, so a workflow edit
that stops running the hook entry fails this file as well as the hook.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import os
import re
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check-hook-commands.py"


def load_checker():
    spec = importlib.util.spec_from_file_location("check_hook_commands", SCRIPT)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load {SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    # dataclasses looks the class's module up in sys.modules while the class
    # body is executing. Register it before exec, or that lookup is None.
    sys.modules["check_hook_commands"] = module
    spec.loader.exec_module(module)
    return module


checker = load_checker()


def workflow(body: str) -> str:
    return textwrap.dedent(body).lstrip("\n")


def write_repo(root: Path, config: str, files: dict[str, str]) -> None:
    (root / ".pre-commit-config.yaml").write_text(textwrap.dedent(config).lstrip("\n"), encoding="utf-8")
    for relative, body in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")


class Runner(unittest.TestCase):
    def test_env_entry_splits_without_a_shell(self) -> None:
        argv = checker.runner.argv_from_entry(
            "env AXON_WEB_AUTOBUILD=0 cargo clippy --all-features --all-targets -- -D warnings"
        )
        self.assertEqual(argv[0], "env")
        self.assertEqual(argv[1], "AXON_WEB_AUTOBUILD=0")
        self.assertEqual(argv[2], "cargo")
        self.assertIn("--", argv)

    def test_entry_rejects_shell_composition(self) -> None:
        for entry in (
            "cargo test --all;true",
            "cargo clippy -- -D warnings||true",
            "cargo test --all > /dev/null",
            "echo x || scripts/check-migrations-immutable.sh",
        ):
            with self.assertRaises(ValueError, msg=entry):
                checker.runner.argv_from_entry(entry)

    def test_scanner_reads_one_hook_and_stops_at_the_next(self) -> None:
        text = textwrap.dedent(
            """\
            repos:
              - repo: local
                hooks:
                  - id: rustfmt
                    entry: cargo fmt --all -- --check
                  - id: rust-clippy
                    entry: env AXON_WEB_AUTOBUILD=0 cargo clippy --all-features
            """
        )
        scanned = checker.runner.scan_entries(text)
        self.assertEqual(scanned["rustfmt"], "cargo fmt --all -- --check")
        self.assertEqual(scanned["rust-clippy"], "env AXON_WEB_AUTOBUILD=0 cargo clippy --all-features")
        self.assertNotIn("actionlint", scanned)

    def test_scanner_keeps_a_trailing_comment_off_the_id(self) -> None:
        text = "      - id: rustfmt  # gates the expensive hooks\n        entry: cargo fmt --all -- --check\n"
        self.assertEqual(
            checker.runner.scan_entries(text)["rustfmt"],
            "cargo fmt --all -- --check",
        )
        glued = checker.runner.scan_entries("      - id: foo#bar\n        entry: true\n")
        self.assertEqual(glued["foo#bar"], "true")

    def test_scanner_keeps_a_hash_inside_a_plain_token(self) -> None:
        text = "      - id: sample\n        entry: cargo test -- a#b  # note\n"
        self.assertEqual(checker.runner.scan_entries(text)["sample"], "cargo test -- a#b")
        quoted = checker.runner.scan_entries('      - id: quoted\n        entry: "a # b"  # note\n')
        self.assertEqual(quoted["quoted"], "a # b")

    def test_windows_returns_the_child_status(self) -> None:
        with (
            mock.patch.object(checker.runner.os, "name", "nt"),
            mock.patch.object(
                checker.runner.subprocess,
                "run",
                return_value=mock.Mock(returncode=2),
            ) as run,
            mock.patch.object(checker.runner.os, "execvp") as execvp,
        ):
            code = checker.runner._exec_entry(["cargo", "test"])
        self.assertEqual(code, 2)
        run.assert_called_once_with(["cargo", "test"], check=False)
        execvp.assert_not_called()

    def test_windows_reports_a_missing_command(self) -> None:
        with (
            mock.patch.object(checker.runner.os, "name", "nt"),
            mock.patch.object(
                checker.runner.subprocess,
                "run",
                side_effect=FileNotFoundError("missing"),
            ),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            code = checker.runner._exec_entry(["missing-tool"])
        self.assertEqual(code, 127)

    def test_posix_replaces_the_process(self) -> None:
        with (
            mock.patch.object(checker.runner.os, "name", "posix"),
            mock.patch.object(
                checker.runner.os,
                "execvp",
                side_effect=AssertionError("replaced"),
            ) as execvp,
            mock.patch.object(checker.runner.subprocess, "run") as run,
        ):
            with self.assertRaises(AssertionError):
                checker.runner._exec_entry(["cargo", "test"])
        execvp.assert_called_once_with("cargo", ["cargo", "test"])
        run.assert_not_called()

    def test_scanner_rejects_a_folded_entry(self) -> None:
        text = "      - id: folded\n        entry: >\n          cargo test --all\n"
        self.assertIsNone(checker.runner.scan_entries(text)["folded"])

    def test_real_clippy_entry_scans_as_the_yaml_command(self) -> None:
        text = (ROOT / ".pre-commit-config.yaml").read_text(encoding="utf-8")
        scanned = checker.runner.scan_entries(text)
        self.assertEqual(
            scanned["rust-clippy"],
            "env AXON_WEB_AUTOBUILD=0 cargo clippy --all-features --all-targets -- -D warnings",
        )
        self.assertEqual(scanned["migrations-immutable"], "scripts/check-migrations-immutable.sh")
        self.assertNotIn("actionlint", scanned)

    def test_base_ref_prefers_the_live_tip(self) -> None:
        seen = []

        def rev_parse(ref: str) -> bool:
            seen.append(ref)
            return ref == "origin/main"

        self.assertEqual(
            checker.runner.resolve_pull_request_base("main", "abc123", rev_parse),
            "origin/main",
        )
        self.assertEqual(seen, ["origin/main"])

    def test_base_ref_falls_back_to_the_event_sha(self) -> None:
        self.assertEqual(
            checker.runner.resolve_pull_request_base("main", "abc123", lambda _ref: False),
            "abc123",
        )
        self.assertEqual(
            checker.runner.resolve_pull_request_base("", "abc123", lambda _ref: True),
            "abc123",
        )


class Agreement(unittest.TestCase):
    def spec_for(self, **kwargs):
        base = dict(
            via_pre_commit={},
            runner={},
            local_only={},
            fixed={},
            exempt={},
        )
        base.update(kwargs)
        return checker.Spec(**base)

    def test_runner_line_passes_and_a_retyped_command_fails(self) -> None:
        step = checker.Step("ci.yml", "lint", "Check formatting")
        spec = self.spec_for(runner={"rustfmt": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: rustfmt
                entry: cargo fmt --all -- --check
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                config,
                {
                    "ci.yml": workflow(
                        """\
                        jobs:
                          lint:
                            steps:
                              - name: Check formatting
                                run: python3 scripts/ci/run-hook-entry.py rustfmt
                        """
                    )
                },
            )
            self.assertEqual(checker.check(root, spec), [])
            drifted = (root / "ci.yml").read_text(encoding="utf-8").replace(
                "python3 scripts/ci/run-hook-entry.py rustfmt",
                "cargo fmt --all -- --check",
            )
            (root / "ci.yml").write_text(drifted, encoding="utf-8")
            found = checker.check(root, spec)
        self.assertTrue(any("must be `python3 scripts/ci/run-hook-entry.py rustfmt`" in p for p in found), found)

    def test_shell_tricks_around_the_runner_fail(self) -> None:
        step = checker.Step("ci.yml", "lint", "Test")
        spec = self.spec_for(runner={"cargo-test": checker.Run((step,))})
        for run in (
            "python3 scripts/ci/run-hook-entry.py cargo-test; true",
            "python3 scripts/ci/run-hook-entry.py cargo-test > /dev/null",
            "python3 scripts/ci/run-hook-entry.py cargo-test && true",
        ):
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write_repo(
                    root,
                    """\
                    repos:
                      - repo: local
                        hooks:
                          - id: cargo-test
                            entry: cargo test --all
                    """,
                    {
                        "ci.yml": workflow(
                            f"""\
                            jobs:
                              lint:
                                steps:
                                  - name: Test
                                    run: {run}
                            """
                        )
                    },
                )
                found = checker.check(root, spec)
            self.assertTrue(any("must be `" in problem for problem in found), (run, found))

    def test_continue_on_error_and_if_false_fail(self) -> None:
        step = checker.Step("ci.yml", "lint", "Clippy")
        spec = self.spec_for(runner={"rust-clippy": checker.Run((step,))})
        bodies = (
            """\
            jobs:
              lint:
                steps:
                  - name: Clippy
                    continue-on-error: true
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """,
            """\
            jobs:
              lint:
                steps:
                  - name: Clippy
                    if: false
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """,
            """\
            jobs:
              lint:
                defaults:
                  run:
                    shell: pwsh
                steps:
                  - name: Clippy
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """,
        )
        for body in bodies:
            with tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                write_repo(
                    root,
                    """\
                    repos:
                      - repo: local
                        hooks:
                          - id: rust-clippy
                            entry: env A=1 cargo clippy -- -D warnings
                    """,
                    {"ci.yml": workflow(body)},
                )
                found = checker.check(root, spec)
            self.assertTrue(found, body)
            self.assertFalse(any("must be `" in problem for problem in found), found)

    def test_rust_changed_if_is_allowed_only_on_the_lint_job(self) -> None:
        step = checker.Step(checker.LINT, checker.LINT_JOB, "Check formatting")
        spec = self.spec_for(runner={"rustfmt": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: rustfmt
                entry: cargo fmt --all -- --check
        """
        allowed = workflow(
            """\
            jobs:
              lint-and-test:
                if: ${{ !cancelled() }}
                steps:
                  - name: Check formatting
                    if: steps.rust-changed.outputs.changed == 'true'
                    run: python3 scripts/ci/run-hook-entry.py rustfmt
            """
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {checker.LINT: allowed})
            self.assertEqual(checker.check(root, spec), [])
            skipped = allowed.replace(
                "steps.rust-changed.outputs.changed == 'true'",
                "false",
            )
            (root / checker.LINT).write_text(skipped, encoding="utf-8")
            found = checker.check(root, spec)
        self.assertTrue(any("if: must be" in problem for problem in found), found)

        dispatch = checker.Step(checker.DISPATCH, "lint-and-test", "Check formatting")
        spec = self.spec_for(runner={"rustfmt": checker.Run((dispatch,))})
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {checker.DISPATCH: allowed.replace(checker.LINT_JOB, "lint-and-test")})
            # The allowed text names lint-and-test already. Give the dispatch
            # job the rust-changed condition, which that job must not carry.
            found = checker.check(root, spec)
        self.assertTrue(any("sets if:" in problem for problem in found), found)

    def test_web_job_directory_requires_the_workspace_root(self) -> None:
        step = checker.Step("web.yml", "web", "Lint")
        spec = self.spec_for(runner={"web-lint": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: web-lint
                entry: pnpm --dir clients/web lint
        """
        missing = workflow(
            """\
            jobs:
              web:
                defaults:
                  run:
                    working-directory: clients/web
                steps:
                  - name: Lint
                    run: python3 scripts/ci/run-hook-entry.py web-lint
            """
        )
        present = missing.replace(
            "name: Lint\n",
            'name: Lint\n        working-directory: "${{ github.workspace }}"\n',
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {"web.yml": missing})
            found = checker.check(root, spec)
            self.assertTrue(any("github.workspace" in problem for problem in found), found)
            (root / "web.yml").write_text(present, encoding="utf-8")
            self.assertEqual(checker.check(root, spec), [])

    def test_workflow_shell_and_directory_count(self) -> None:
        step = checker.Step("ci.yml", "lint", "Clippy")
        spec = self.spec_for(runner={"rust-clippy": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: rust-clippy
                entry: env A=1 cargo clippy -- -D warnings
        """
        workflow_shell = workflow(
            """\
            defaults:
              run:
                shell: pwsh
            jobs:
              lint:
                steps:
                  - name: Clippy
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """
        )
        workflow_dir = workflow(
            """\
            defaults:
              run:
                working-directory: clients/web
            jobs:
              lint:
                steps:
                  - name: Clippy
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """
        )
        job_overrides = workflow(
            """\
            defaults:
              run:
                shell: pwsh
                working-directory: clients/web
            jobs:
              lint:
                defaults:
                  run:
                    shell: bash
                steps:
                  - name: Clippy
                    working-directory: ${{ github.workspace }}
                    run: python3 scripts/ci/run-hook-entry.py rust-clippy
            """
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {"ci.yml": workflow_shell})
            found = checker.check(root, spec)
            self.assertTrue(any("inherits shell" in problem for problem in found), found)
            (root / "ci.yml").write_text(workflow_dir, encoding="utf-8")
            found = checker.check(root, spec)
            self.assertTrue(any("github.workspace" in problem for problem in found), found)
            (root / "ci.yml").write_text(job_overrides, encoding="utf-8")
            self.assertEqual(checker.check(root, spec), [])

    def test_pinned_format_must_inherit_clients_web(self) -> None:
        step = checker.Step("web.yml", "web", "Check formatting")
        spec = self.spec_for(
            local_only={"prettier": "the package check is pinned instead"},
            fixed={
                step: checker.Pinned(
                    "pnpm format:check",
                    "package prettier",
                    directory="clients/web",
                )
            },
        )
        config = """\
        repos:
          - repo: local
            hooks:
              - id: prettier
                entry: prettier --check .
        """
        good = workflow(
            """\
            jobs:
              web:
                defaults:
                  run:
                    working-directory: clients/web
                steps:
                  - name: Check formatting
                    run: pnpm format:check
            """
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {"web.yml": good})
            self.assertEqual(checker.check(root, spec), [])
            dropped = good.replace(
                "    defaults:\n      run:\n        working-directory: clients/web\n",
                "",
            )
            (root / "web.yml").write_text(dropped, encoding="utf-8")
            found = checker.check(root, spec)
        self.assertTrue(any("must inherit working-directory" in problem for problem in found), found)

    def test_under_test_is_a_flag_not_an_environment_variable(self) -> None:
        with mock.patch.object(checker, "_run_unit_tests", return_value=0) as run:
            with mock.patch.object(checker, "check", return_value=[]) as checked:
                code = checker.main(["scripts/check-hook-commands.py", "--under-test"])
        run.assert_not_called()
        checked.assert_called_once()
        self.assertEqual(code, 0)
        with mock.patch.dict(os.environ, {"HOOK_COMMANDS_UNDER_TEST": "1"}):
            with mock.patch.object(checker, "_run_unit_tests", return_value=0) as run:
                with mock.patch.object(checker, "check", return_value=[]):
                    code = checker.main(["scripts/check-hook-commands.py"])
        run.assert_called_once()
        self.assertEqual(code, 0)

    def test_base_ref_step_requires_the_two_env_keys(self) -> None:
        step = checker.Step("mig.yml", "migrations-immutable", "Migrations are forward-only")
        spec = self.spec_for(
            runner={"migrations-immutable": checker.Run((step,), pull_request_base=True)}
        )
        good = workflow(
            """\
            jobs:
              migrations-immutable:
                steps:
                  - name: Migrations are forward-only
                    env:
                      BASE_REF: ${{ github.event.pull_request.base.ref }}
                      BASE_SHA: ${{ github.event.pull_request.base.sha }}
                    run: python3 scripts/ci/run-hook-entry.py migrations-immutable --pull-request-base
            """
        )
        config = """\
        repos:
          - repo: local
            hooks:
              - id: migrations-immutable
                entry: scripts/check-migrations-immutable.sh
        """
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {"mig.yml": good})
            self.assertEqual(checker.check(root, spec), [])
            mentioned = good.replace(
                "python3 scripts/ci/run-hook-entry.py migrations-immutable --pull-request-base",
                "echo x || scripts/check-migrations-immutable.sh",
            )
            (root / "mig.yml").write_text(mentioned, encoding="utf-8")
            found = checker.check(root, spec)
            self.assertTrue(any("must be `" in problem for problem in found), found)
            (root / "mig.yml").write_text(good.replace("BASE_SHA", "OTHER"), encoding="utf-8")
            found = checker.check(root, spec)
        self.assertTrue(any("env must be" in problem for problem in found), found)

    def test_cross_job_if_is_the_checks_input_only(self) -> None:
        step = checker.Step(checker.CROSS, checker.CROSS_JOB, "Test")
        spec = self.spec_for(runner={"cargo-test": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: cargo-test
                entry: cargo test --all
        """
        body = workflow(
            """\
            jobs:
              lint-fmt-test:
                if: ${{ github.event.inputs.checks != 'false' }}
                steps:
                  - name: Test
                    run: python3 scripts/ci/run-hook-entry.py cargo-test
            """
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {checker.CROSS: body})
            self.assertEqual(checker.check(root, spec), [])
            (root / checker.CROSS).write_text(body.replace(checker.CROSS_JOB_IF, "false"), encoding="utf-8")
            found = checker.check(root, spec)
        self.assertTrue(any("if: must be" in problem for problem in found), found)

    def test_lint_job_if_must_not_skip_the_gate(self) -> None:
        # lint-and-test needs the runner picker (ADR 0141). Only `!cancelled()`
        # is allowed: no `if` at all would skip the gate whenever the picker job
        # fails, and a skipped required check passes.
        step = checker.Step(checker.LINT, checker.LINT_JOB, "Test")
        spec = self.spec_for(runner={"cargo-test": checker.Run((step,))})
        config = """\
        repos:
          - repo: local
            hooks:
              - id: cargo-test
                entry: cargo test --all
        """
        body = workflow(
            """\
            jobs:
              lint-and-test:
                if: ${{ !cancelled() }}
                steps:
                  - name: Test
                    if: steps.rust-changed.outputs.changed == 'true'
                    run: python3 scripts/ci/run-hook-entry.py cargo-test
            """
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(root, config, {checker.LINT: body})
            self.assertEqual(checker.check(root, spec), [])
            for replacement in ("${{ false }}", "${{ success() }}"):
                (root / checker.LINT).write_text(
                    body.replace(checker.LINT_JOB_IF, replacement), encoding="utf-8"
                )
                found = checker.check(root, spec)
                self.assertTrue(any("if: must be" in problem for problem in found), found)
            (root / checker.LINT).write_text(
                body.replace("    if: ${{ !cancelled() }}\n", ""), encoding="utf-8"
            )
            found = checker.check(root, spec)
        self.assertTrue(any("if: must be" in problem for problem in found), found)

    def test_unregistered_run_in_a_watched_job_fails(self) -> None:
        spec = self.spec_for(
            runner={"cargo-test": checker.Run((checker.Step("ci.yml", "lint", "Test"),))}
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                """\
                repos:
                  - repo: local
                    hooks:
                      - id: cargo-test
                        entry: cargo test --all
                """,
                {
                    "ci.yml": workflow(
                        """\
                        jobs:
                          lint:
                            steps:
                              - name: Test
                                run: python3 scripts/ci/run-hook-entry.py cargo-test
                              - name: Clippy again
                                run: cargo clippy --all-features --all-targets -- -D warnings
                        """
                    )
                },
            )
            found = checker.check(root, spec)
        self.assertTrue(any("Clippy again" in problem for problem in found), found)

    def test_pre_commit_step_must_name_the_hook_stage(self) -> None:
        step = checker.Step("ci.yml", "lint", "actionlint")
        spec = self.spec_for(via_pre_commit={"actionlint": step})
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                """\
                repos:
                  - repo: https://example.invalid/actionlint
                    hooks:
                      - id: actionlint
                """,
                {
                    "ci.yml": workflow(
                        """\
                        jobs:
                          lint:
                            steps:
                              - name: actionlint
                                run: pre-commit run actionlint --all-files
                        """
                    )
                },
            )
            found = checker.check(root, spec)
        self.assertTrue(any("hook-stage pre-push" in problem for problem in found), found)

    def test_unclassified_hook_fails(self) -> None:
        spec = self.spec_for(local_only={"kept": "there is a reason"})
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                """\
                repos:
                  - repo: local
                    hooks:
                      - id: kept
                        entry: "true"
                      - id: brand-new
                        entry: "true"
                """,
                {},
            )
            found = checker.check(root, spec)
        self.assertTrue(any("brand-new" in problem for problem in found), found)

    def test_pinned_command_is_not_silently_a_hook(self) -> None:
        step = checker.Step("web.yml", "web", "Check formatting")
        spec = self.spec_for(
            local_only={"prettier": "vendored, and this step is the package script"},
            fixed={step: checker.Pinned("prettier --check", "package prettier")},
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                """\
                repos:
                  - repo: local
                    hooks:
                      - id: prettier
                        entry: prettier --check
                """,
                {
                    "web.yml": workflow(
                        """\
                        jobs:
                          web:
                            steps:
                              - name: Check formatting
                                run: prettier --check
                        """
                    )
                },
            )
            found = checker.check(root, spec)
        self.assertTrue(any("prettier hook entry" in problem for problem in found), found)

    def test_unit_tests_are_not_a_workflow_step(self) -> None:
        self.assertNotIn("test hook commands", {step.name for step in checker.EXEMPT})

    def test_files_regex_is_stable_across_re_escape(self) -> None:
        pattern = checker.expected_files_regex()
        self.assertNotIn(r"\-", pattern)
        self.assertIn(r"\.yml", pattern)
        for path in checker.watched_paths():
            self.assertIsNotNone(re.search(pattern, path), path)
        self.assertIsNone(re.search(pattern, ".github/workflows/smoke.yml"))

    def test_files_filter_must_equal_the_generated_regex(self) -> None:
        step = checker.Step("ci.yml", "lint", "Check formatting")
        spec = self.spec_for(
            runner={"rustfmt": checker.Run((step,))},
            local_only={"hook-commands": "the check itself, for this fixture"},
        )
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            write_repo(
                root,
                """\
                repos:
                  - repo: local
                    hooks:
                      - id: rustfmt
                        entry: cargo fmt --all -- --check
                      - id: hook-commands
                        entry: scripts/check-hook-commands.py
                        files: '^ci\\.yml$'
                """,
                {
                    "ci.yml": workflow(
                        """\
                        jobs:
                          lint:
                            steps:
                              - name: Check formatting
                                run: python3 scripts/ci/run-hook-entry.py rustfmt
                        """
                    )
                },
            )
            found = checker.check(root, spec, enforce_files_filter=True)
        self.assertTrue(any("files filter" in problem for problem in found), found)
        self.assertTrue(any(checker.expected_files_regex(spec) in problem for problem in found), found)


class RealTree(unittest.TestCase):
    def test_repository_agrees(self) -> None:
        found = checker.check(ROOT)
        self.assertEqual(found, [], "\n\n".join(found))


if __name__ == "__main__":
    unittest.main()
