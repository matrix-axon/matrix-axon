#!/usr/bin/env bash
#
# Run black-box smoke gates consistently from local shells, hooks, and the
# manual GitHub Actions smoke workflow.
#
# Usage:
#   scripts/smoke-gate.sh tui
#   scripts/smoke-gate.sh tui-true-local
#   scripts/smoke-gate.sh server
#   scripts/smoke-gate.sh all
set -euo pipefail

mode="${1:-}"

usage() {
  cat >&2 <<'EOF'
usage: scripts/smoke-gate.sh <tui|tui-true-local|server|all>

  tui             TUI PTY smoke against the in-process stub
  tui-true-local  TUI PTY smoke against axon-smoke-local-stack
  server          Server smoke against axon-smoke-local-stack
  all             S1 gate: TUI stub + TUI true-local + server true-local smoke
EOF
}

if [ -z "$mode" ]; then
  usage
  exit 2
fi

target_bin="target/debug"
if [ "${CARGO_TARGET_DIR:-}" ]; then
  target_bin="${CARGO_TARGET_DIR%/}/debug"
fi

run_tui_stub() {
  ./scripts/check-smoke-isolation.sh axon-smoke-tui
  cargo build -p axon-tui -p axon-smoke-tui
  AXON_TUI_BIN="$target_bin/axon-tui" \
    cargo run -p axon-smoke-tui -- --profile stub
}

run_tui_true_local() {
  ./scripts/check-smoke-isolation.sh axon-smoke-local-stack axon-smoke-tui
  cargo build -p axon-server -p axon-tui -p axon-smoke-local-stack -p axon-smoke-tui
  AXON_SERVER_BIN="$target_bin/axon-server" \
    AXON_TUI_BIN="$target_bin/axon-tui" \
    AXON_SMOKE_LOCAL_STACK_BIN="$target_bin/axon-smoke-local-stack" \
    cargo run -p axon-smoke-tui -- --profile true-local
}

run_server() {
  ./scripts/check-smoke-isolation.sh axon-smoke-local-stack axon-smoke-server
  cargo build -p axon-server -p axon-smoke-local-stack -p axon-smoke-server
  AXON_SERVER_BIN="$target_bin/axon-server" \
    AXON_SMOKE_LOCAL_STACK_BIN="$target_bin/axon-smoke-local-stack" \
    cargo run -p axon-smoke-server -- --profile true-local
}

case "$mode" in
  tui)
    run_tui_stub
    ;;
  tui-true-local)
    run_tui_true_local
    ;;
  server)
    run_server
    ;;
  all)
    run_tui_stub
    run_tui_true_local
    run_server
    ;;
  *)
    usage
    exit 2
    ;;
esac
