#!/usr/bin/env bash
# Status dump for the WHI-715 continuous shadow run.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SHADOW_ROOT="${SHADOW_ROOT:-$ROOT/evidence/shadow/continuous}"
PID_DIR="$SHADOW_ROOT/run/pids"

echo "SHADOW_ROOT=$SHADOW_ROOT"
if [[ -f "$SHADOW_ROOT/STATUS.md" ]]; then
  echo "--- STATUS.md ---"
  cat "$SHADOW_ROOT/STATUS.md"
  echo "-----------------"
fi

for key in v2 v3 v3-1559 moe; do
  ledger="$SHADOW_ROOT/$key/ledger.jsonl"
  pid_file="$PID_DIR/${key}.pid"
  sup_file="$PID_DIR/${key}.supervisor.pid"
  alive="no"
  if [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null; then
    alive="yes (pid $(cat "$pid_file"))"
  fi
  sup="no"
  if [[ -f "$sup_file" ]] && kill -0 "$(cat "$sup_file")" 2>/dev/null; then
    sup="yes (pid $(cat "$sup_file"))"
  fi
  lines=0
  headers=0
  observations=0
  candidates=0
  if [[ -f "$ledger" ]]; then
    lines=$(wc -l <"$ledger" | tr -d ' ')
    headers=$(grep -c '"row_type":"run_header"' "$ledger" || true)
    observations=$(grep -c '"row_type":"observation"' "$ledger" || true)
    candidates=$(grep -c '"row_type":"candidate"' "$ledger" || true)
  fi
  echo "$key: supervisor=$sup worker=$alive ledger_lines=$lines headers=$headers observations=$observations candidates=$candidates"
done

if [[ -f "$SHADOW_ROOT/run/STOP" ]]; then
  echo "STOP file present — supervisors should wind down."
fi
