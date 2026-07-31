#!/usr/bin/env bash
# Stop the WHI-715 continuous shadow supervisor tree.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SHADOW_ROOT="${SHADOW_ROOT:-$ROOT/evidence/shadow/continuous}"
RUN_DIR="$SHADOW_ROOT/run"
PID_DIR="$RUN_DIR/pids"

mkdir -p "$RUN_DIR" "$PID_DIR"
touch "$RUN_DIR/STOP"

stopped=0
if [[ -d "$PID_DIR" ]]; then
  for f in "$PID_DIR"/*.pid "$PID_DIR"/*.supervisor.pid; do
    [[ -f "$f" ]] || continue
    pid="$(cat "$f" 2>/dev/null || true)"
    if [[ -n "${pid:-}" ]] && kill -0 "$pid" 2>/dev/null; then
      echo "sending TERM to pid $pid ($f)"
      kill -TERM "$pid" 2>/dev/null || true
      stopped=$((stopped + 1))
    fi
    rm -f "$f"
  done
fi

# Also kill any lingering cargo-run children bound to our ledgers (best-effort).
if command -v pgrep >/dev/null 2>&1; then
  # Escape SHADOW_ROOT for basic regex match under pgrep -f.
  local_pattern="$(printf '%s' "$SHADOW_ROOT" | sed 's/[.[\*^$()+?{|]/g')/.*/ledger\\.jsonl"
  while read -r pid; do
    [[ -z "$pid" ]] && continue
    if kill -0 "$pid" 2>/dev/null; then
      echo "sending TERM to leftover pid $pid"
      kill -TERM "$pid" 2>/dev/null || true
      stopped=$((stopped + 1))
    fi
  done < <(pgrep -f "$local_pattern" || true)
fi

echo "stop signal written to $RUN_DIR/STOP (signaled ~$stopped process(es))"
