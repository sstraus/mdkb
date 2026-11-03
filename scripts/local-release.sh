#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$HOME/.local/bin/mdkb"

cd "$ROOT"

cargo build --release "$@"
mkdir -p "$(dirname "$BIN")"
install -m 755 "$ROOT/target/release/mdkb" "$BIN.new"
mv -f "$BIN.new" "$BIN"

find_runtime_pids() {
  ps -axo pid=,command= | awk '
    {
      pid = $1
      $1 = ""
      sub(/^[[:space:]]+/, "")
      count = split($0, argv, /[[:space:]]+/)
      executable = argv[1]
      if (executable !~ /(^|\/)mdkb$/) {
        next
      }
      if (argv[2] == "mcp") {
        print pid
        next
      }
      if (argv[2] == "serve") {
        for (i = 3; i <= count; i++) {
          if (argv[i] == "--daemon") {
            print pid
            next
          }
        }
      }
    }
  '
}

runtime_pids="$(find_runtime_pids | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
if [[ -n "$runtime_pids" ]]; then
  echo "Stopping every mdkb MCP and daemon process: $runtime_pids"
  # The PIDs come only from exact mdkb executable/argument matches above.
  # shellcheck disable=SC2086
  kill -TERM $runtime_pids 2>/dev/null || true

  for _ in {1..20}; do
    remaining="$(find_runtime_pids | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
    [[ -z "$remaining" ]] && break
    sleep 0.1
  done

  remaining="$(find_runtime_pids | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
  if [[ -n "$remaining" ]]; then
    echo "Force-stopping remaining mdkb processes: $remaining"
    # shellcheck disable=SC2086
    kill -KILL $remaining 2>/dev/null || true
  fi
else
  echo "No running mdkb MCP or daemon processes found."
fi

"$BIN" daemon restart
"$BIN" daemon status
"$BIN" --version

if command -v lsof >/dev/null 2>&1; then
  echo
  echo "Processes holding the rebuilt binary:"
  lsof "$BIN" 2>/dev/null || true
fi
