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
# WHI-952 / G-5: tracing logs and the shadow ledger each have independent
# size-based rotation + total-bytes retention (see scripts/golive/rotating_tee.sh
# and SHADOW_LEDGER_MAX_* / LOG_MAX_* env vars). Defaults: 64 MiB segment /
# 512 MiB total per stream.
#
#   scripts/golive/run_live.sh [extra bot args...]

set -uo pipefail
cd "$(dirname "$0")/../.."

# shellcheck source=scripts/lib/rotate_by_size.sh
. scripts/lib/rotate_by_size.sh

LOG_DIR=evidence/shadow/live-run/logs
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/live.log"
LEDGER="${LEDGER_PATH:-evidence/shadow/live-run/ledger.jsonl}"

# Tracing-log caps (also consumed by rotating_tee.sh).
export LOG_MAX_SEGMENT_BYTES="${LOG_MAX_SEGMENT_BYTES:-$((64 * 1024 * 1024))}"
export LOG_MAX_TOTAL_BYTES="${LOG_MAX_TOTAL_BYTES:-$((512 * 1024 * 1024))}"
# Shadow-ledger caps (consumed by ShadowLedgerWriter at open / append).
export SHADOW_LEDGER_MAX_SEGMENT_BYTES="${SHADOW_LEDGER_MAX_SEGMENT_BYTES:-$((64 * 1024 * 1024))}"
export SHADOW_LEDGER_MAX_TOTAL_BYTES="${SHADOW_LEDGER_MAX_TOTAL_BYTES:-$((512 * 1024 * 1024))}"

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
  # Reclaim between restarts so a crashed tee cannot leave an unbounded active file.
  ensure_under_cap "$LOG" "$LOG_MAX_SEGMENT_BYTES" "$LOG_MAX_TOTAL_BYTES"
  ensure_under_cap "$LEDGER" "$SHADOW_LEDGER_MAX_SEGMENT_BYTES" "$SHADOW_LEDGER_MAX_TOTAL_BYTES"

  echo "--- start $(date -u +%FT%TZ) (restart $n) ---" | tee -a "$LOG" >/dev/null

  set +e
  RPC_HTTP_URL="$MANTLE_RPC_URL" \
  RPC_WS_URL="$MANTLE_RPC_WS_URL" \
  MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH=evidence/shadow/candidate-window/thresholds.json \
  ./target/release/bot \
    --protocols agni-v2,agni-v3,moe \
    --watch --enable-sends \
    --ledger "$LEDGER" \
    "$@" 2>&1 | scripts/golive/rotating_tee.sh "$LOG"
  # Prefer bot's exit status over the tee's (tee exits 0 on EOF).
  rc=${PIPESTATUS[0]:-$?}
  set -e

  [[ $rc -eq 0 ]] && { echo "clean exit" | tee -a "$LOG" >/dev/null; break; }

  if is_transient_startup; then
    n=$((n+1))
    if (( n > MAX_RESTARTS )); then
      echo "ABORT: transient startup failures exceeded $MAX_RESTARTS restarts — no longer transient" | tee -a "$LOG" >/dev/null
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
    echo "transient ($reason); restart $n/$MAX_RESTARTS in ${cool}s" | tee -a "$LOG" >/dev/null
    sleep "$cool"
    continue
  fi

  echo "ABORT: non-transient exit rc=$rc — not restarting" | tee -a "$LOG" >/dev/null
  exit "$rc"
done
