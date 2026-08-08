#!/usr/bin/env bash
#
# WHI-953 / WHI-548 — fund-and-canary launcher.
#
# Consumes ONLY the exact parameters produced by the second human approval.
# Refuses to start if any required approval parameter is missing.
#
# Required approval parameters (env):
#   APPROVAL_RECORD_ID          id / path of the second human approval record
#   NOTIONAL_CAP_WMNT_ETHER     exact WMNT notional to fund (e.g. 1 or 5)
#   CANARY_POOLS_FILE           pool set for the canary (must ⊆ universe)
#   EXECUTOR                    deployed executor address from deploy-only
#
# Also required:
#   MANTLE_PRIVATE_KEY          admin signer (fund + unpause)
#   MANTLE_RPC_URL              mainnet mode only
#
#   scripts/golive/fund_and_canary.sh --dry-run
#   scripts/golive/fund_and_canary.sh --mainnet
#   scripts/golive/fund_and_canary.sh --preflight   # validate params only
#
# Does NOT deploy. Does NOT register the full universe (canary subset only
# is re-asserted on-chain for the listed pools). After fund+unpause the
# supervised production runner is scripts/golive/run_live.sh (no SHADOW_MODE).

set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# shellcheck source=scripts/lib/golive_common.sh
. "$ROOT/scripts/lib/golive_common.sh"

MODE="dry-run"
for arg in "$@"; do
  case "$arg" in
    --mainnet) MODE="mainnet" ;;
    --dry-run) MODE="dry-run" ;;
    --preflight) MODE="preflight" ;;
    -h|--help)
      sed -n '2,35p' "$0"
      exit 0
      ;;
    *) golive_die "unknown arg: $arg" ;;
  esac
done

WMNT="$GOLIVE_WMNT_MAINNET"
UNIVERSE="${BOT_POOL_UNIVERSE:-$GOLIVE_DEFAULT_UNIVERSE}"
IDENTITY="${IDENTITY:-$GOLIVE_DEFAULT_IDENTITY}"

# --- approval parameter gate (fail closed) -----------------------------------
missing=()
[[ -n "${APPROVAL_RECORD_ID:-}" ]] || missing+=(APPROVAL_RECORD_ID)
[[ -n "${NOTIONAL_CAP_WMNT_ETHER:-}" ]] || missing+=(NOTIONAL_CAP_WMNT_ETHER)
[[ -n "${CANARY_POOLS_FILE:-}" ]] || missing+=(CANARY_POOLS_FILE)
[[ -n "${EXECUTOR:-}" ]] || missing+=(EXECUTOR)

if (( ${#missing[@]} > 0 )); then
  echo "ABORT: fund-and-canary refuses to start; missing approval parameters:" >&2
  for m in "${missing[@]}"; do
    echo "  - $m" >&2
  done
  echo "These must come from the second human approval (WHI-548), not defaults." >&2
  exit 1
fi

# Validate shapes before loading any signing material.
[[ -f "$CANARY_POOLS_FILE" ]] || golive_die "CANARY_POOLS_FILE not found: $CANARY_POOLS_FILE"
# Reject non-numeric notional (integer or decimal).
[[ "$NOTIONAL_CAP_WMNT_ETHER" =~ ^[0-9]+([.][0-9]+)?$ ]] \
  || golive_die "NOTIONAL_CAP_WMNT_ETHER must be a positive number, got: $NOTIONAL_CAP_WMNT_ETHER"
python3 -c "import sys; v=float('$NOTIONAL_CAP_WMNT_ETHER'); sys.exit(0 if v > 0 else 1)" \
  || golive_die "NOTIONAL_CAP_WMNT_ETHER must be > 0"

golive_require_universe_fingerprint "$UNIVERSE" >/dev/null
golive_assert_pools_subset_of_universe "$CANARY_POOLS_FILE" "$UNIVERSE"

if [[ "$MODE" == "preflight" ]]; then
  cat <<EOF
preflight ok
mode                preflight
approval_record_id  $APPROVAL_RECORD_ID
notional_cap_wmnt   $NOTIONAL_CAP_WMNT_ETHER
canary_pools_file   $CANARY_POOLS_FILE
executor            $EXECUTOR
universe_fp         $GOLIVE_UNIVERSE_FINGERPRINT
EOF
  exit 0
fi

golive_load_dotenv "$ROOT"
: "${MANTLE_PRIVATE_KEY:?MANTLE_PRIVATE_KEY missing}"
ADMIN_PK="$(golive_norm_hex "$MANTLE_PRIVATE_KEY")"
ADMIN="$(cast wallet address --private-key "$ADMIN_PK")"

# Fund-and-canary must not carry hot/guardian keys either — canary send path
# is a separate supervised process (run_live.sh) under the hot key.
unset BOT_HOT_EXECUTOR_PRIVATE_KEY BOT_GUARDIAN_PRIVATE_KEY 2>/dev/null || true

if [[ "$MODE" == "mainnet" ]]; then
  RPC="${MANTLE_RPC_URL:?MANTLE_RPC_URL missing}"
  echo "!!! MAINNET fund-and-canary. Real funds. Irreversible. !!!"
else
  RPC="http://127.0.0.1:8545"
  pgrep -f "anvil --fork-url" >/dev/null 2>&1 \
    || golive_die "no anvil fork on :8545 — start: anvil --fork-url \"\$MANTLE_RPC_URL\" --port 8545"
fi

CHAIN="$(golive_require_chain_id "$RPC")"
OBSERVED="$(golive_require_codehash "$EXECUTOR" "$RPC" "$IDENTITY")"

send() { cast send --rpc-url "$RPC" --private-key "$ADMIN_PK" "$@" 2>&1; }
ok()   { grep -qE '^status +1' <<<"$1" || { echo "$1" | golive_scrub | head -5; golive_die "$2"; }; }

cat <<EOF

mode                $MODE
chain               $CHAIN
admin               $ADMIN
executor            $EXECUTOR
codehash            $OBSERVED
approval_record_id  $APPROVAL_RECORD_ID
notional_cap_wmnt   $NOTIONAL_CAP_WMNT_ETHER
canary_pools        $CANARY_POOLS_FILE
universe_fp         $GOLIVE_UNIVERSE_FINGERPRINT
EOF

if [[ "$MODE" == "mainnet" ]]; then
  read -r -p $'\nType FUND to proceed: ' c
  [[ "$c" == "FUND" ]] || golive_die "not confirmed"
fi

# Ensure canary pools are registered (idempotent).
golive_step "register canary pools (idempotent)"
n=0; failed=""
while read -r pool ptype; do
  [[ -n "$pool" ]] || continue
  st=$(cast call "$EXECUTOR" "registeredPools(address)(uint8,address,address,uint24,bool)" "$pool" --rpc-url "$RPC" 2>/dev/null | tail -1 || true)
  if [[ "$st" == "true" ]]; then
    n=$((n+1))
    continue
  fi
  OUT=$(send "$EXECUTOR" "registerPool(address,uint8)" "$pool" "$ptype")
  if grep -qE '^status +1' <<<"$OUT"; then n=$((n+1)); else failed="$failed $pool"; fi
done < "$CANARY_POOLS_FILE"
[[ -z "$failed" ]] || golive_die "canary pool registration failed:$failed"
echo "  $n canary pools registered/present"

# Fund exactly the approved notional.
golive_step "fund $NOTIONAL_CAP_WMNT_ETHER WMNT (approved notional)"
BAL_BEFORE="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$RPC" | awk '{print $1}')"
if [[ "$BAL_BEFORE" != "0" ]]; then
  golive_die "executor already funded (WMNT=$BAL_BEFORE); refuse to double-fund without new approval"
fi
OUT=$(cast send --rpc-url "$RPC" --private-key "$ADMIN_PK" "$WMNT" "deposit()" \
        --value "${NOTIONAL_CAP_WMNT_ETHER}ether" 2>&1)
ok "$OUT" "WMNT deposit failed"
AMT=$(cast to-wei "$NOTIONAL_CAP_WMNT_ETHER" ether)
OUT=$(send "$WMNT" "transfer(address,uint256)" "$EXECUTOR" "$AMT")
ok "$OUT" "WMNT transfer failed"
BAL="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$RPC" | awk '{print $1}')"
echo "  executor WMNT $BAL"

# Unpause for canary.
golive_step "unpause"
OUT=$(send "$EXECUTOR" "unpause()"); ok "$OUT" "unpause failed"
[[ "$(cast call "$EXECUTOR" 'paused()(bool)' --rpc-url "$RPC")" == "false" ]] \
  || golive_die "still paused after unpause"

EVIDENCE_DIR="evidence/golive/fund-and-canary"
mkdir -p "$EVIDENCE_DIR"
SUMMARY="$EVIDENCE_DIR/last_run.txt"
cat >"$SUMMARY" <<EOF
mode                $MODE
executor            $EXECUTOR
codehash            $OBSERVED
approval_record_id  $APPROVAL_RECORD_ID
notional_cap_wmnt   $NOTIONAL_CAP_WMNT_ETHER
wmnt_balance        $BAL
paused              false
canary_pools_file   $CANARY_POOLS_FILE
universe_fp         $GOLIVE_UNIVERSE_FINGERPRINT
EOF

cat <<EOF

=== fund-and-canary done ===
$(cat "$SUMMARY")

Next (supervised production send — no SHADOW_MODE):
  ARBITRAGE_EXECUTOR_ADDRESS=$EXECUTOR \\
  scripts/golive/run_live.sh

Emergency stop (guardian key, separate process):
  cast send --rpc-url <rpc> --private-key \$BOT_GUARDIAN_PRIVATE_KEY $EXECUTOR "pause()"
EOF
