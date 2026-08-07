#!/usr/bin/env bash
#
# Supervised live run. Restarts only on known-transient startup failures:
#
#   1. Canonical tip block not found at number N
#      Race between reading the tip number and fetching that block; on a
#      load-balanced RPC the second call can land on a node that does not have
#      it yet. Observed roughly 1 run in 4; the next attempt usually succeeds.
#
#   2. Moe CREATE-size retry exhausted / CreateContractSizeLimit (WHI-921)
#      After the binary's floor (backoff + no fan-out under 429 pressure) still
#      exhausts, a bounded cold restart is reasonable — the process reloads the
#      universe and re-enters sync with a cooler endpoint. Without the floor this
#      would be a hammer loop; do not widen further.
#
# Anything else is a real failure and stops here rather than being retried into
# a loop.
#
#   scripts/golive/run_live.sh [extra bot args...]

set -uo pipefail
cd "$(dirname "$0")/../.."

LOG_DIR=evidence/shadow/live-run/logs
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/live.log"

set -a; . ./.env; set +a
: "${MANTLE_RPC_URL:?}" "${MANTLE_RPC_WS_URL:?}" "${ARBITRAGE_EXECUTOR_ADDRESS:?}"

MAX_RESTARTS="${MAX_RESTARTS:-10}"
n=0

is_transient_startup() {
  # Last ~40 lines: enough for multi-line eyre reports without matching ancient history.
  local tail_txt
  tail_txt="$(tail -40 "$LOG" 2>/dev/null || true)"
  grep -q "Canonical tip block not found" <<<"$tail_txt" && return 0
  # WHI-921: only the *exhausted* floor (not mid-recovery warn lines that still
  # contain CreateContractSizeLimit while the binary is backing off).
  grep -q "CREATE-size retry exhausted" <<<"$tail_txt" && return 0
  return 1
}

while :; do
  echo "--- start $(date -u +%FT%TZ) (restart $n) ---" >> "$LOG"

  RPC_HTTP_URL="$MANTLE_RPC_URL" \
  RPC_WS_URL="$MANTLE_RPC_WS_URL" \
  MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH=evidence/shadow/candidate-window/thresholds.json \
  ./target/release/bot \
    --protocols agni-v2,agni-v3,moe \
    --watch --enable-sends \
    --ledger evidence/shadow/live-run/ledger.jsonl \
    "$@" >> "$LOG" 2>&1
  rc=$?

  [[ $rc -eq 0 ]] && { echo "clean exit" >> "$LOG"; break; }

  if is_transient_startup; then
    n=$((n+1))
    if (( n > MAX_RESTARTS )); then
      echo "ABORT: transient startup failures exceeded $MAX_RESTARTS restarts — no longer transient" >> "$LOG"
      exit 1
    fi
    # Longer cool-down after CREATE-size exhaustion so we do not immediately
    # re-hammer a rate-limited endpoint.
    if tail -40 "$LOG" | grep -q "CREATE-size retry exhausted"; then
      cool=15
      reason="CREATE-size retry exhausted"
    else
      cool=5
      reason="tip race"
    fi
    echo "transient ($reason); restart $n/$MAX_RESTARTS in ${cool}s" >> "$LOG"
    sleep "$cool"
    continue
  fi

  echo "ABORT: non-transient exit rc=$rc — not restarting" >> "$LOG"
  exit "$rc"
done
