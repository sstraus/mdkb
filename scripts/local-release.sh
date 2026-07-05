#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/mdkb"

cd "$ROOT"

cargo build --release "$@"

find_mcp_pids() {
  ps -axo pid=,command= | awk '$0 ~ /(^|\/)mdkb mcp($| )/ { print $1 }'
}

mcp_pids="$(find_mcp_pids | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
if [[ -n "$mcp_pids" ]]; then
  echo "Stopping stale mdkb mcp processes: $mcp_pids"
  # shellcheck disable=SC2086
  kill -TERM $mcp_pids 2>/dev/null || true
  sleep 1

  remaining="$(find_mcp_pids | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
  if [[ -n "$remaining" ]]; then
    echo "Force-stopping stale mdkb mcp processes: $remaining"
    # shellcheck disable=SC2086
    kill -KILL $remaining 2>/dev/null || true
  fi
else
  echo "No stale mdkb mcp processes found."
fi

"$BIN" daemon restart
"$BIN" daemon status

if command -v lsof >/dev/null 2>&1; then
  echo
  echo "Processes holding the rebuilt binary:"
  lsof "$BIN" 2>/dev/null || true
fi
