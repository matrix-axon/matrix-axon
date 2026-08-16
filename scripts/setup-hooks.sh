#!/usr/bin/env bash
#
# Install the pre-push gate for git. Run once per clone.
#
# The list of checks is .pre-commit-config.yaml and nothing else (ADR 0092).
# jj users do not need this script -- `jj push` reaches the same file through
# jj-hooks; see AGENTS.md for that alias.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

if ! command -v pre-commit >/dev/null 2>&1; then
  cat >&2 <<'EOF'
pre-commit is not installed. Pick one:

  pipx install pre-commit
  uv tool install pre-commit
  pip install --user pre-commit
  sudo apt install pre-commit

Then re-run ./scripts/setup-hooks.sh
EOF
  exit 1
fi

# This script used to point git at .githooks/ instead. That directory is gone,
# and a stale core.hooksPath silently disables every hook -- git neither runs
# them nor complains. Clear it, but only when it is still the value we set, so
# a genuinely custom hooksPath is left alone.
hooks_path="$(git config --get core.hooksPath || true)"
if [ "$hooks_path" = ".githooks" ]; then
  git config --unset core.hooksPath
  echo "cleared core.hooksPath (was .githooks, which no longer exists)"
elif [ -n "$hooks_path" ]; then
  echo "core.hooksPath is set to '$hooks_path'; pre-commit refuses to install" >&2
  echo "while it is set. Unset it, or install the hook into that directory by" >&2
  echo "hand." >&2
  exit 1
fi

pre-commit install --hook-type pre-push

cat <<'EOF'

Pre-push gate installed. It runs .pre-commit-config.yaml, path-filtered, so a
web-only push does not build rust and a rust-only push does not run pnpm.

  SKIP=cargo-test git push     skip one hook
  git push --no-verify         skip all of them
  RUN_SMOKE=tui git push       additionally run a Docker smoke lane
EOF
