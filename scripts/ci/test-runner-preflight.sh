#!/usr/bin/env bash
# Tests for runner-preflight.sh, the self-hosted runners' start-time check.
#
#   scripts/ci/test-runner-preflight.sh
#
# Runs unprivileged, so it cannot hand files to another account. The runner
# account is `nobody` when the tree must pass, and the current user when the
# test needs the "owned by the runner account" failure.
set -uo pipefail

here=$(CDPATH="" cd -- "$(dirname "$0")" && pwd)
preflight="$here/runner-preflight.sh"
failures=0

if ! id nobody >/dev/null 2>&1; then
  echo "skipped: no \`nobody\` account here"
  exit 0
fi
me=$(id -un)

# A fresh runner tree: install dir, writable _work and _diag, a hook in its own dir.
setup() {
  work=$(mktemp -d)
  chmod 755 "$work"
  # mktemp's parent must itself pass, or every case fails for the same reason.
  r="$work/runner"
  mkdir -p "$r/bin" "$r/_work/repo" "$r/_diag" "$work/hooks"
  printf 'x\n' >"$r/bin/Runner.Worker"
  printf '#!/bin/bash\n' >"$work/hooks/job-started.sh"
  printf 'LANG=C.UTF-8\nACTIONS_RUNNER_HOOK_JOB_STARTED=%s\n' "$work/hooks/job-started.sh" >"$r/.env"
  chmod -R go-w "$work"
  # The runner's own dirs may be anything; make them world-writable to prove it.
  chmod 777 "$r/_work" "$r/_diag"
}

check() { # pass|fail name user
  local rc
  bash "$preflight" "$r" "$3" >"$work/log" 2>&1
  rc=$?
  if [[ $1 == pass && $rc != 0 ]]; then
    echo "FAIL: $2: refused: $(cat "$work/log")" >&2
    failures=$((failures + 1))
  elif [[ $1 == fail && $rc == 0 ]]; then
    echo "FAIL: $2: passed" >&2
    failures=$((failures + 1))
  fi
  rm -rf "$work"
}

setup
if ! probe=$(bash "$preflight" "$r" nobody 2>&1) && grep -q "above" <<<"$probe"; then
  # A parent of the temporary directory fails the rule (for example a
  # non-sticky shared TMPDIR), so no tree made here can pass.
  echo "skipped: a parent of $(dirname "$work") is writable by others: $probe"
  rm -rf "$work"
  exit 0
fi
rm -rf "$work"

setup
check pass "clean tree" nobody

setup
check fail "tree owned by the runner account" "$me"

setup
chmod g+w "$r/bin/Runner.Worker"
check fail "group-writable binary" nobody

setup
chmod o+w "$r/.env"
check fail "world-writable .env" nobody

setup
chmod o+w "$r"
check fail "world-writable runner dir" nobody

setup
chmod o+w "$work/hooks"
check fail "world-writable hook dir" nobody

setup
chmod g+w "$work/hooks/job-started.sh"
check fail "group-writable hook" nobody

setup
chmod o+w "$work"
check fail "world-writable parent" nobody

setup
chmod o+wt "$work"
check pass "sticky world-writable parent, like /tmp" nobody

setup
printf 'LANG=C.UTF-8\n' >"$r/.env"
check fail "no hook in .env" nobody

setup
printf 'ACTIONS_RUNNER_HOOK_JOB_STARTED=hooks/job-started.sh\n' >"$r/.env"
check fail "relative hook path" nobody

setup
printf 'ACTIONS_RUNNER_HOOK_JOB_STARTED=%s\n' "$work/hooks/missing.sh" >"$r/.env"
check fail "missing hook" nobody

setup
rm "$r/.env"
check fail "no .env" nobody

setup
ln -s /etc "$r/bin/link"
check pass "symlink inside the tree" nobody

setup
if bash "$preflight" "$r" no-such-user-here >/dev/null 2>&1; then
  echo "FAIL: unknown user: passed" >&2
  failures=$((failures + 1))
fi
rm -rf "$work"

if ((failures > 0)); then
  echo "$failures failure(s)" >&2
  exit 1
fi
echo "test-runner-preflight: all passed"
