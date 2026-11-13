#!/usr/bin/env python3
"""Tests for scripts/ci/changed-by-hook.py.

    python -I scripts/ci/test-changed-by-hook.py

That script decides whether the expensive steps of two *required* checks run, so
a wrong answer either merges unreviewed code (a false `changed=false`) or quietly
throws away the saving (a false `changed=true`). Each case builds a throwaway git
repository, commits the files a pull request would change, and runs the script
against `HEAD^1`, which is what the workflows pass.

Two kinds of case:

- the script's own semantics, against a small synthetic config so they do not
  move when `.pre-commit-config.yaml` does;
- the real `rust-clippy` hook in the repository's config, with a handful of
  paths that must and must not select it. The full selection matrix is pinned by
  scripts/check-hook-filters.py; this only proves the script reads that hook.

Needs PyYAML (the script does). Stdlib otherwise.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "changed-by-hook.py"
REPO_CONFIG = HERE.parent.parent / ".pre-commit-config.yaml"

SYNTHETIC = """\
repos:
  - repo: local
    hooks:
      - id: code
        name: code
        entry: "true"
        language: system
        files: '^(src/.*\\.rs|Cargo\\.lock)$'
        exclude: '^src/generated/'
      - id: unfiltered
        name: unfiltered
        entry: "true"
        language: system
"""


def git(cwd: Path, *args: str) -> str:
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "t",
        "GIT_AUTHOR_EMAIL": "t@example.invalid",
        "GIT_COMMITTER_NAME": "t",
        "GIT_COMMITTER_EMAIL": "t@example.invalid",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_CONFIG_SYSTEM": os.devnull,
    }
    return subprocess.run(
        ["git", *args], cwd=cwd, env=env, check=True, capture_output=True, text=True
    ).stdout


class Repo:
    """A scratch repository whose `HEAD^1..HEAD` diff is the 'pull request'."""

    def __init__(self, root: Path, config: str) -> None:
        self.root = root
        git(root, "init", "-q", "-b", "main")
        (root / ".pre-commit-config.yaml").write_text(config)
        (root / "README.md").write_text("base\n")
        self.commit("base")

    def commit(self, msg: str, files: dict[str, str] | None = None) -> None:
        for rel, body in (files or {}).items():
            path = self.root / rel
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(body)
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "--allow-empty", "-m", msg)

    def run(self, *args: str, env: dict[str, str] | None = None) -> tuple[str, str]:
        full_env = {k: v for k, v in os.environ.items() if k != "GITHUB_OUTPUT"}
        full_env.update(env or {})
        proc = subprocess.run(
            [sys.executable, str(SCRIPT), *args],
            cwd=self.root,
            env=full_env,
            capture_output=True,
            text=True,
        )
        # The script must never fail the step: every doubt is `changed=true`.
        assert proc.returncode == 0, f"script exited {proc.returncode}: {proc.stderr}"
        return proc.stdout.strip(), proc.stderr


class Base(unittest.TestCase):
    config = SYNTHETIC

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.repo = Repo(Path(self._tmp.name), self.config)

    def decide(self, hook: str, *args: str, **kw) -> str:
        out, _ = self.repo.run(hook, "--base", "HEAD^1", *args, **kw)
        return out

    def pr(self, *paths: str) -> None:
        """Commit `paths` as the pull request (HEAD^1..HEAD)."""
        self.repo.commit("the pull request", {p: "x\n" for p in paths})


class Semantics(Base):
    def test_no_matching_file_skips(self) -> None:
        self.pr("docs/a.md", "web/a.ts")
        self.assertEqual(self.decide("code"), "changed=false")

    def test_matching_file_runs(self) -> None:
        self.pr("src/lib.rs")
        self.assertEqual(self.decide("code"), "changed=true")

    def test_one_matching_file_among_many_runs(self) -> None:
        self.pr("docs/a.md", "web/a.ts", "Cargo.lock")
        self.assertEqual(self.decide("code"), "changed=true")

    def test_exclude_wins_over_files(self) -> None:
        self.pr("src/generated/x.rs")
        self.assertEqual(self.decide("code"), "changed=false")

    def test_hook_without_files_selects_everything(self) -> None:
        self.pr("docs/a.md")
        self.assertEqual(self.decide("unfiltered"), "changed=true")

    def test_also_adds_inputs_the_hook_does_not_know(self) -> None:
        self.pr("scripts/lane.sh")
        self.assertEqual(self.decide("code"), "changed=false")
        self.assertEqual(
            self.decide("code", "--also", r"^scripts/lane\.sh$"), "changed=true"
        )

    def test_empty_pull_request_skips(self) -> None:
        self.repo.commit("the pull request")
        self.assertEqual(self.decide("code"), "changed=false")

    def test_deleted_file_counts(self) -> None:
        self.repo.commit("add", {"src/lib.rs": "x\n"})
        git(self.repo.root, "rm", "-q", "src/lib.rs")
        git(self.repo.root, "commit", "-q", "-m", "the pull request")
        self.assertEqual(self.decide("code"), "changed=true")

    def merge(self, pr_file: str, main_file: str) -> None:
        """What Actions checks out: the PR merged into a base that moved on."""
        root = self.repo.root
        git(root, "checkout", "-q", "-b", "pr")
        self.repo.commit("pr", {pr_file: "x\n"})
        git(root, "checkout", "-q", "main")
        self.repo.commit("main moves on", {main_file: "x\n"})
        git(root, "checkout", "-q", "-b", "merge", "main")
        git(root, "merge", "-q", "--no-ff", "pr", "-m", "merge")

    def test_base_that_moved_ahead_is_not_the_pull_request(self) -> None:
        # Diffing against the PR's fork point would see main's Rust change.
        self.merge(pr_file="docs/only.md", main_file="src/lib.rs")
        self.assertEqual(self.decide("code"), "changed=false")

    def test_rust_change_in_a_merge_commit_runs(self) -> None:
        self.merge(pr_file="src/lib.rs", main_file="docs/a.md")
        self.assertEqual(self.decide("code"), "changed=true")


class FailSafe(Base):
    """Every doubt must resolve to running everything."""

    def setUp(self) -> None:
        super().setUp()
        # A pull request that, decided correctly, would skip.
        self.pr("docs/a.md")
        self.assertEqual(self.decide("code"), "changed=false")

    def undecided(self, *args: str, env: dict[str, str] | None = None) -> tuple[str, str]:
        return self.repo.run(*args, env=env)

    def test_no_base_is_not_a_pull_request(self) -> None:
        self.assertEqual(self.undecided("code")[0], "changed=true")

    def test_empty_base_is_not_a_pull_request(self) -> None:
        self.assertEqual(self.undecided("code", "--base", "")[0], "changed=true")

    def test_unresolvable_base(self) -> None:
        self.assertEqual(self.undecided("code", "--base", "deadbeef")[0], "changed=true")

    def test_base_before_the_start_of_a_shallow_history(self) -> None:
        self.assertEqual(self.undecided("code", "--base", "HEAD~50")[0], "changed=true")

    def test_unknown_hook_id(self) -> None:
        self.assertEqual(self.undecided("no-such-hook", "--base", "HEAD^1")[0], "changed=true")

    def test_missing_config(self) -> None:
        out, _ = self.undecided("code", "--base", "HEAD^1", "--config", "nope.yaml")
        self.assertEqual(out, "changed=true")

    def test_unparseable_config(self) -> None:
        (self.repo.root / ".pre-commit-config.yaml").write_text("repos: [unclosed\n")
        self.assertEqual(self.undecided("code", "--base", "HEAD^1")[0], "changed=true")

    def test_bad_regex_in_also(self) -> None:
        out, _ = self.undecided("code", "--base", "HEAD^1", "--also", "(")
        self.assertEqual(out, "changed=true")

    def test_undecided_warns_on_actions_and_a_plain_skip_does_not(self) -> None:
        actions = {"GITHUB_ACTIONS": "true"}
        out, _ = self.undecided("no-such-hook", "--base", "HEAD^1", env=actions)
        self.assertIn("::warning", out)
        out, _ = self.undecided("no-such-hook", "--base", "HEAD^1")
        self.assertNotIn("::warning", out)
        out, _ = self.undecided("code", "--base", "HEAD^1", env=actions)
        self.assertEqual(out, "changed=false")

    def test_writes_github_output(self) -> None:
        sink = self.repo.root / "out.txt"
        env = {"GITHUB_OUTPUT": str(sink)}
        self.undecided("code", "--base", "HEAD^1", env=env)
        self.undecided("no-such-hook", "--base", "HEAD^1", env=env)
        self.assertEqual(sink.read_text().split(), ["changed=false", "changed=true"])


class RealConfig(Base):
    """The repository's own `rust-clippy` hook, which is what the workflows name."""

    config = REPO_CONFIG.read_text()

    def test_rust_inputs_run(self) -> None:
        for path in (
            "crates/axon-store/src/lib.rs",
            "Cargo.lock",
            ".github/workflows/lint-and-clippy.yml",
        ):
            with self.subTest(path=path):
                self.repo.commit("the pull request", {path: "x\n"})
                self.assertEqual(self.decide("rust-clippy"), "changed=true")

    def test_docs_and_web_skip(self) -> None:
        self.pr("docs/guide.md", "clients/web/src/app.tsx", "scripts/ci/x.py")
        self.assertEqual(self.decide("rust-clippy"), "changed=false")


if __name__ == "__main__":
    unittest.main(verbosity=2)
