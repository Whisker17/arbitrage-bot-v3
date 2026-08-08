#!/usr/bin/env bash
#
# WHI-953 — signerless shadow launcher.
#
# Contract:
#   * Parent may hold signing keys.
#   * Child is launched under a sanitized environment that strips every name in
#     FORBIDDEN_ENV_VAR_NAMES (plus hot/guardian keys and BOT_ENABLE_SENDS).
#   * SHADOW_MODE=1 is forced.
#   * --enable-sends is never passed.
#   * production_send_allowed stays false (bot asserts on the no_send path).
#
# Usage (from repo root):
#   scripts/golive/run_signerless_shadow.sh [extra bot args...]
#
# Special modes (for acceptance tests; do not use in production ops):
#   CHILD_ENV_SNAPSHOT=1  — print sanitized child env var names and exit 0/1
#   PREFLIGHT_ONLY=1      — run `bot --offline` under sanitized env, then exit
#
# Env (optional unless noted):
#   MANTLE_RPC_URL / MANTLE_RPC_WS_URL (or MANTLE_MAINNET_* / RPC_*) — live mode
#   LEDGER_PATH / SHADOW_LEDGER_PATH — ledger output
#   MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH — required for live --ledger
#   BOT_BIN — path to bot binary (default: ./target/release/bot)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# shellcheck source=scripts/lib/golive_common.sh
. "$ROOT/scripts/lib/golive_common.sh"
# shellcheck source=scripts/lib/rotate_by_size.sh
. "$ROOT/scripts/lib/rotate_by_size.sh"

golive_load_dotenv "$ROOT"

LOG_DIR="${SHADOW_LOG_DIR:-evidence/shadow/signerless/logs}"
mkdir -p "$LOG_DIR"
LOG="${SHADOW_LOG_PATH:-$LOG_DIR/signerless.log}"
LEDGER="${LEDGER_PATH:-${SHADOW_LEDGER_PATH:-evidence/shadow/signerless/ledger.jsonl}}"

export LOG_MAX_SEGMENT_BYTES="${LOG_MAX_SEGMENT_BYTES:-$((64 * 1024 * 1024))}"
export LOG_MAX_TOTAL_BYTES="${LOG_MAX_TOTAL_BYTES:-$((512 * 1024 * 1024))}"
export SHADOW_LEDGER_MAX_SEGMENT_BYTES="${SHADOW_LEDGER_MAX_SEGMENT_BYTES:-$((64 * 1024 * 1024))}"
export SHADOW_LEDGER_MAX_TOTAL_BYTES="${SHADOW_LEDGER_MAX_TOTAL_BYTES:-$((512 * 1024 * 1024))}"

BOT_BIN="${BOT_BIN:-$ROOT/target/release/bot}"

# Resolve RPC pair (live mode only).
HTTP_URL="${RPC_HTTP_URL:-${MANTLE_MAINNET_RPC_URL:-${MANTLE_RPC_URL:-${MANTLE_HTTP_URL:-}}}}"
WS_URL="${RPC_WS_URL:-${MANTLE_MAINNET_RPC_WS_URL:-${MANTLE_RPC_WS_URL:-${MANTLE_WS_URL:-}}}}"

# Universe fingerprint is always required (shared preflight).
golive_require_universe_fingerprint "${BOT_POOL_UNIVERSE:-$GOLIVE_DEFAULT_UNIVERSE}" >/dev/null
echo "universe fingerprint: $GOLIVE_UNIVERSE_FINGERPRINT  pools=$GOLIVE_UNIVERSE_POOL_COUNT"

# Build the sanitized child environment as a bash function so acceptance tests
# can snapshot it without forking the real bot.
run_sanitized_child() {
  # Strip every forbidden / extra signer name from this subshell.
  golive_unset_signer_env
  golive_assert_no_forbidden_env

  export SHADOW_MODE=1
  unset BOT_ENABLE_SENDS 2>/dev/null || true

  export BOT_CHAIN_ID="${BOT_CHAIN_ID:-5000}"
  if [[ -n "$HTTP_URL" ]]; then
    export RPC_HTTP_URL="$HTTP_URL"
    export MANTLE_MAINNET_RPC_URL="$HTTP_URL"
    export MANTLE_RPC_URL="$HTTP_URL"
    export MANTLE_HTTP_URL="$HTTP_URL"
  fi
  if [[ -n "$WS_URL" ]]; then
    export RPC_WS_URL="$WS_URL"
    export MANTLE_MAINNET_RPC_WS_URL="$WS_URL"
    export MANTLE_RPC_WS_URL="$WS_URL"
    export MANTLE_WS_URL="$WS_URL"
  fi
  export ARBITRAGE_EXECUTOR_ADDRESS="${ARBITRAGE_EXECUTOR_ADDRESS:-0x0000000000000000000000000000000000000002}"
  export MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH="${MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH:-$ROOT/config/gas_profiles/shadow_thresholds.mantle_mainnet.json}"
  export RUST_LOG="${RUST_LOG:-info,amms=info}"
  export SHADOW_LEDGER_MAX_SEGMENT_BYTES SHADOW_LEDGER_MAX_TOTAL_BYTES

  if [[ "${CHILD_ENV_SNAPSHOT:-0}" == "1" ]]; then
    # Print a machine-readable snapshot for tests: NAME=present|absent.
    local n
    for n in "${GOLIVE_FORBIDDEN_ENV_VAR_NAMES[@]}"; do
      if [[ -n "${!n+x}" ]]; then
        echo "FORBIDDEN $n=present"
      else
        echo "FORBIDDEN $n=absent"
      fi
    done
    echo "SHADOW_MODE=${SHADOW_MODE:-}"
    echo "BOT_ENABLE_SENDS=${BOT_ENABLE_SENDS-<unset>}"
    # Fail the snapshot if any forbidden var leaked in.
    golive_assert_no_forbidden_env
    echo "child_env_snapshot_ok"
    return 0
  fi

  if [[ "${PREFLIGHT_ONLY:-0}" == "1" ]]; then
    [[ -x "$BOT_BIN" ]] || golive_die "bot binary not executable: $BOT_BIN (cargo build --release --bin bot)"
    # Offline fixture proves production_send_allowed stays false under sanitized env.
    # --offline cannot take --ledger; that is intentional (bot.rs fail-closed).
    echo "preflight: $BOT_BIN --offline (SHADOW_MODE=1, no --enable-sends)"
    "$BOT_BIN" --offline --protocols agni-v2,agni-v3,moe
    return $?
  fi

  [[ -x "$BOT_BIN" ]] || golive_die "bot binary not executable: $BOT_BIN (cargo build --release --bin bot)"
  [[ -n "$HTTP_URL" && -n "$WS_URL" ]] \
    || golive_die "need mainnet RPC endpoints (MANTLE_RPC_URL + MANTLE_RPC_WS_URL or MANTLE_MAINNET_*)"
  [[ -f "$MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH" ]] \
    || golive_die "thresholds missing: $MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH"

  # Reject any accidental --enable-sends in extra args.
  local a
  for a in "$@"; do
    if [[ "$a" == "--enable-sends" || "$a" == "--enable-sends=true" ]]; then
      golive_die "signerless shadow launcher refuses --enable-sends"
    fi
  done

  ensure_under_cap "$LOG" "$LOG_MAX_SEGMENT_BYTES" "$LOG_MAX_TOTAL_BYTES"
  ensure_under_cap "$LEDGER" "$SHADOW_LEDGER_MAX_SEGMENT_BYTES" "$SHADOW_LEDGER_MAX_TOTAL_BYTES"
  mkdir -p "$(dirname "$LEDGER")"

  echo "starting signerless shadow: ledger=$LEDGER log=$LOG fingerprint=$GOLIVE_UNIVERSE_FINGERPRINT"
  # Never pass --enable-sends. --ledger is evidence output, not send capability.
  "$BOT_BIN" \
    --protocols agni-v2,agni-v3,moe \
    --watch \
    --ledger "$LEDGER" \
    "$@" 2>&1 | "$ROOT/scripts/golive/rotating_tee.sh" "$LOG"
  # Prefer bot exit status over tee.
  return "${PIPESTATUS[0]:-1}"
}

# Parent keeps its keys; only the child subshell is sanitized.
run_sanitized_child "$@"
rc=$?
exit "$rc"
