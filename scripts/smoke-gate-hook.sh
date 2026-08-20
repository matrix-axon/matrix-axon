#!/usr/bin/env bash
#
# The pre-push hook's opt-in door to scripts/smoke-gate.sh.
#
# Smoke lanes need Docker and a true-local Synapse stack and take minutes, so
# they are never part of the default gate (ADR 0086, AGENTS.md § smoke gates).
# But they should be reachable from the same hook as everything else rather
# than through a second mechanism -- that is the whole point of ADR 0092.
#
# So: a no-op unless RUN_SMOKE names a lane.
#
#   RUN_SMOKE=tui jj push
#   RUN_SMOKE=all git push
#
# Valid lanes are whatever smoke-gate.sh accepts (tui, tui-true-local, server,
# all); it prints its own usage on anything else. This wrapper deliberately
# does not validate the value -- one list of lanes, in one place.
set -euo pipefail

if [ -z "${RUN_SMOKE:-}" ]; then
  exit 0
fi

echo "RUN_SMOKE=$RUN_SMOKE -- running the smoke gate (Docker; minutes)"
exec ./scripts/smoke-gate.sh "$RUN_SMOKE"
