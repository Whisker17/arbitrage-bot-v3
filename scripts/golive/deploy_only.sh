#!/usr/bin/env bash
#
# WHI-953 / WHI-547 — deploy-only launcher.
#
# Reuses the former deploy_and_arm.sh steps 1–5:
#   1. deploy from the canonical artifact (NOT forge create)
#   2. pause immediately + assert paused() == true
#   3. verify runtime codehash against config/executor_identity.json
#   4. register hot executor + guardian by ADDRESS (no private keys)
#   5. register pools (set asserted equal to data/pool_universe.csv)
#
# Ends paused + unfunded. Does NOT fund. Does NOT unpause.
#
#   scripts/golive/deploy_only.sh --dry-run     # anvil fork on :8545 (default)
#   scripts/golive/deploy_only.sh --mainnet     # real, irreversible
#   scripts/golive/deploy_only.sh --preflight   # offline checks only
#
# Required env:
#   MANTLE_PRIVATE_KEY              admin signer (only private key used)
#   BOT_HOT_EXECUTOR_ADDRESS        hot role address (or HOT_ADDRESS)
#   BOT_GUARDIAN_ADDRESS            guardian address (or GUARDIAN_ADDRESS)
#   MANTLE_RPC_URL                  mainnet mode only
#
# Forbidden in process env after load (fail closed):
#   BOT_HOT_EXECUTOR_PRIVATE_KEY, BOT_GUARDIAN_PRIVATE_KEY
#
# Optional:
#   POOLS_FILE   default scripts/golive/pools.txt — must match universe or be
#                regenerated via scripts/golive/gen_pools_from_universe.sh
#   BOT_POOL_UNIVERSE  default data/pool_universe.csv

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

ARTIFACT="${ARTIFACT:-$GOLIVE_DEFAULT_ARTIFACT}"
IDENTITY="${IDENTITY:-$GOLIVE_DEFAULT_IDENTITY}"
POOLS="${POOLS_FILE:-scripts/golive/pools.txt}"
UNIVERSE="${BOT_POOL_UNIVERSE:-$GOLIVE_DEFAULT_UNIVERSE}"
WMNT="$GOLIVE_WMNT_MAINNET"

[[ -f "$ARTIFACT" ]] || golive_die "artifact missing: $ARTIFACT"
[[ -f "$IDENTITY" ]] || golive_die "identity missing: $IDENTITY"

golive_load_dotenv "$ROOT"

# Capture admin key, then drop hot/guardian private keys from the process.
: "${MANTLE_PRIVATE_KEY:?MANTLE_PRIVATE_KEY missing}"
ADMIN_PK="$(golive_norm_hex "$MANTLE_PRIVATE_KEY")"

# Addresses only — never read hot/guardian private keys (WHI-953 key boundary).
HOT="${BOT_HOT_EXECUTOR_ADDRESS:-${HOT_ADDRESS:-}}"
GUARDIAN="${BOT_GUARDIAN_ADDRESS:-${GUARDIAN_ADDRESS:-}}"
[[ -n "$HOT" ]] || golive_die "BOT_HOT_EXECUTOR_ADDRESS (or HOT_ADDRESS) required — addresses only, not private keys"
[[ -n "$GUARDIAN" ]] || golive_die "BOT_GUARDIAN_ADDRESS (or GUARDIAN_ADDRESS) required — addresses only, not private keys"

# Drop hot/guardian private keys if .env loaded them; process must not hold them.
unset BOT_HOT_EXECUTOR_PRIVATE_KEY BOT_GUARDIAN_PRIVATE_KEY 2>/dev/null || true
golive_assert_no_hot_guardian_keys

ADMIN="$(cast wallet address --private-key "$ADMIN_PK")"
[[ "$ADMIN" != "$HOT" ]] || golive_die "admin and hot executor must differ"
[[ "$ADMIN" != "$GUARDIAN" ]] || golive_die "admin and guardian must differ"
[[ "$HOT" != "$GUARDIAN" ]] || golive_die "hot executor and guardian must differ"

EXPECTED_HASH="$(golive_expected_codehash "$IDENTITY")"
CREATION="$(python3 -c "import json;print(json.load(open('$ARTIFACT'))['bytecode']['object'])")"

# Shared preflight: universe fingerprint + registry ⟷ universe.
golive_require_universe_fingerprint "$UNIVERSE" >/dev/null
if [[ ! -f "$POOLS" ]]; then
  echo "pools file missing; generating from universe → $POOLS"
  golive_write_pools_from_universe "$POOLS" "$UNIVERSE"
fi
golive_assert_pools_match_universe "$POOLS" "$UNIVERSE"
POOL_COUNT="$(wc -l <"$POOLS" | tr -d ' ')"

if [[ "$MODE" == "preflight" ]]; then
  cat <<EOF
preflight ok
mode          preflight
admin         $ADMIN
hot executor  $HOT
guardian      $GUARDIAN
pools         $POOL_COUNT
universe_fp   $GOLIVE_UNIVERSE_FINGERPRINT
expected hash $EXPECTED_HASH
hot/guardian private keys absent: yes
EOF
  exit 0
fi

if [[ "$MODE" == "mainnet" ]]; then
  RPC="${MANTLE_RPC_URL:?MANTLE_RPC_URL missing}"
  echo "!!! MAINNET deploy-only. Real txs. Ends paused + unfunded. !!!"
else
  RPC="http://127.0.0.1:8545"
  pgrep -f "anvil --fork-url" >/dev/null 2>&1 \
    || golive_die "no anvil fork on :8545 — start: anvil --fork-url \"\$MANTLE_RPC_URL\" --port 8545"
fi

CHAIN="$(golive_require_chain_id "$RPC")"

send() { cast send --rpc-url "$RPC" --private-key "$ADMIN_PK" "$@" 2>&1; }
ok()   { grep -qE '^status +1' <<<"$1" || { echo "$1" | golive_scrub | head -5; golive_die "$2"; }; }
gas()  { grep -E '^gasUsed' <<<"$1" | awk '{print $2}'; }

cat <<EOF

mode          $MODE
chain         $CHAIN
admin         $ADMIN
hot executor  $HOT
guardian      $GUARDIAN
pools         $POOL_COUNT
universe_fp   $GOLIVE_UNIVERSE_FINGERPRINT
expected hash $EXPECTED_HASH
fund/unpause  NO (deploy-only; WHI-547 boundary)
EOF

if [[ "$MODE" == "mainnet" ]]; then
  read -r -p $'\nType EXECUTE to proceed: ' c
  [[ "$c" == "EXECUTE" ]] || golive_die "not confirmed"
fi

TOTAL_GAS=0
acc() { TOTAL_GAS=$(( TOTAL_GAS + ${1:-0} )); }

# --- 1. deploy ---------------------------------------------------------------
golive_step "1/5 deploy (canonical artifact, NOT forge create)"
OUT=$(cast send --rpc-url "$RPC" --private-key "$ADMIN_PK" --create "$CREATION" \
        "constructor(address,address)" "$WMNT" "$ADMIN" 2>&1)
ok "$OUT" "deployment failed"
EXEC=$(grep -E '^contractAddress' <<<"$OUT" | awk '{print $2}')
[[ -n "$EXEC" ]] || golive_die "no contract address in receipt"
acc "$(gas "$OUT")"
echo "  executor $EXEC"

# --- 2. pause immediately ----------------------------------------------------
# Constructor leaves paused = false. Deploy+pause is one containment unit.
golive_step "2/5 pause (containment)"
OUT=$(send "$EXEC" "pause()"); ok "$OUT" "pause failed"; acc "$(gas "$OUT")"
[[ "$(cast call "$EXEC" 'paused()(bool)' --rpc-url "$RPC")" == "true" ]] \
  || golive_die "paused() is not true — status 1 does NOT prove pause"

# --- 3. codehash -------------------------------------------------------------
golive_step "3/5 verify codehash"
OBSERVED="$(golive_require_codehash "$EXEC" "$RPC" "$IDENTITY")"
echo "  observed $OBSERVED"
echo "  expected $EXPECTED_HASH"
echo "  match"

# --- 4. roles (addresses only) -----------------------------------------------
golive_step "4/5 roles (addresses only — no hot/guardian keys)"
golive_assert_no_hot_guardian_keys
OUT=$(send "$EXEC" "setHotExecutor(address,bool)" "$HOT" true); ok "$OUT" "setHotExecutor failed"; acc "$(gas "$OUT")"
OUT=$(send "$EXEC" "setGuardian(address)" "$GUARDIAN");         ok "$OUT" "setGuardian failed";   acc "$(gas "$OUT")"
[[ "$(cast call "$EXEC" 'isHotExecutor(address)(bool)' "$HOT" --rpc-url "$RPC")" == "true" ]] \
  || golive_die "hot executor not registered"
[[ "$(cast call "$EXEC" 'guardian()(address)' --rpc-url "$RPC")" == "$GUARDIAN" ]] \
  || golive_die "guardian not set"

# --- 5. register pools -------------------------------------------------------
golive_step "5/5 registerPool x$POOL_COUNT"
n=0; failed=""
while read -r pool ptype; do
  [[ -n "$pool" ]] || continue
  OUT=$(send "$EXEC" "registerPool(address,uint8)" "$pool" "$ptype")
  if grep -qE '^status +1' <<<"$OUT"; then acc "$(gas "$OUT")"; else failed="$failed $pool"; fi
  n=$((n+1)); (( n % 20 == 0 )) && echo "  $n/$POOL_COUNT"
done < "$POOLS"
[[ -z "$failed" ]] || { echo "  failed:$failed"; golive_die "pool registration incomplete"; }
echo "  $n registered, 0 failed"

# --- terminal state: paused + unfunded ---------------------------------------
golive_step "assert terminal state (paused + unfunded)"
PAUSED="$(cast call "$EXEC" 'paused()(bool)' --rpc-url "$RPC")"
BAL="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXEC" --rpc-url "$RPC" | awk '{print $1}')"
[[ "$PAUSED" == "true" ]] || golive_die "terminal paused()=$PAUSED, expected true"
[[ "$BAL" == "0" ]] || golive_die "terminal WMNT balance=$BAL, expected 0"
golive_assert_no_hot_guardian_keys

GP=$(cast gas-price --rpc-url "$RPC" 2>/dev/null || echo 50000000000)
EVIDENCE_DIR="evidence/golive/deploy-only"
mkdir -p "$EVIDENCE_DIR"
SUMMARY="$EVIDENCE_DIR/last_run.txt"
cat >"$SUMMARY" <<EOF
mode          $MODE
executor      $EXEC
codehash      $OBSERVED
admin         $ADMIN
hot           $HOT
guardian      $GUARDIAN
pools         $n
universe_fp   $GOLIVE_UNIVERSE_FINGERPRINT
paused        $PAUSED
wmnt_balance  $BAL
gas           $TOTAL_GAS  ~$(python3 -c "print(f'{$TOTAL_GAS*$GP/1e18:.4f}')") MNT
EOF

cat <<EOF

=== deploy-only done (WHI-547 boundary held) ===
$(cat "$SUMMARY")

Next (requires second human approval → fund-and-canary):
  APPROVAL_RECORD_ID=... \\
  NOTIONAL_CAP_WMNT_ETHER=... \\
  CANARY_POOLS_FILE=... \\
  EXECUTOR=$EXEC \\
  scripts/golive/fund_and_canary.sh --dry-run
EOF
