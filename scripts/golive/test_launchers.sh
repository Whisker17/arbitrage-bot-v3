#!/usr/bin/env bash
# WHI-953 acceptance tests (offline — no anvil, no mainnet).
#
# Covers:
#   1. Parent holds forbidden keys → signerless child env snapshot is clean
#   2. PREFLIGHT_ONLY offline bot under sanitized env → no_send (exit 0)
#   3. Direct SHADOW_MODE=1 + forbidden var on continuous launcher → non-zero
#      (reuses scripts/shadow/test_launcher_nosend.sh)
#   4. fund-and-canary missing each approval param → non-zero
#   5. registry ⟷ universe consistency (match + deliberate mismatch)
#   6. deploy_only --preflight requires addresses and drops hot/guardian keys
#   7. retired deploy_and_arm.sh exits non-zero
#   8. run_live.sh refuses SHADOW_MODE=1
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

# shellcheck source=scripts/lib/golive_common.sh
. "$ROOT/scripts/lib/golive_common.sh"

fail=0
pass() { echo "ok: $*"; }
bad()  { echo "FAIL: $*" >&2; fail=1; }

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# --- 1. child env snapshot with parent keys present --------------------------
# Parent shell carries every forbidden name; child must report absent.
out="$(
  MANTLE_PRIVATE_KEY=deadbeef \
  MANTLE_MAINNET_PRIVATE_KEY=deadbeef \
  MANTLE_SEPOLIA_PRIVATE_KEY=deadbeef \
  PRIVATE_KEY=deadbeef \
  EXECUTION_PRIVATE_KEY=deadbeef \
  BOT_HOT_EXECUTOR_PRIVATE_KEY=deadbeef \
  BOT_GUARDIAN_PRIVATE_KEY=deadbeef \
  CHILD_ENV_SNAPSHOT=1 \
  "$ROOT/scripts/golive/run_signerless_shadow.sh" 2>&1
)" || { bad "signerless CHILD_ENV_SNAPSHOT exited non-zero with parent keys"; echo "$out" >&2; }
if grep -q 'STRIPPED.*=present' <<<"$out"; then
  bad "child env still contains a stripped var: $out"
elif ! grep -q 'child_env_snapshot_ok' <<<"$out"; then
  bad "child env snapshot missing ok marker: $out"
elif ! grep -q 'SHADOW_MODE=1' <<<"$out"; then
  bad "child env snapshot missing SHADOW_MODE=1: $out"
elif ! grep -q 'STRIPPED BOT_HOT_EXECUTOR_PRIVATE_KEY=absent' <<<"$out"; then
  bad "child env snapshot missing hot-key absent: $out"
else
  pass "signerless child env clean while parent holds keys"
fi

# --- 2. PREFLIGHT_ONLY offline bot (no_send) ---------------------------------
if [[ -x "$ROOT/target/release/bot" ]]; then
  BOT="$ROOT/target/release/bot"
elif [[ -x "$ROOT/target/debug/bot" ]]; then
  BOT="$ROOT/target/debug/bot"
else
  echo "building bot for preflight (debug)…"
  cargo build --locked --bin bot >/dev/null
  BOT="$ROOT/target/debug/bot"
fi

out="$(
  MANTLE_PRIVATE_KEY=deadbeef \
  PRIVATE_KEY=deadbeef \
  PREFLIGHT_ONLY=1 \
  BOT_BIN="$BOT" \
  "$ROOT/scripts/golive/run_signerless_shadow.sh" 2>&1
)" || { bad "PREFLIGHT_ONLY bot failed under sanitized env"; echo "$out" >&2; }
if grep -qi 'ForbiddenEnvVarPresent\|forbidden signer env var' <<<"$out"; then
  bad "preflight saw forbidden env (sanitization failed): $out"
elif ! grep -q 'production_send_allowed: false' <<<"$out"; then
  bad "preflight must report production_send_allowed: false: $out"
elif grep -qi 'production_send_allowed: true' <<<"$out"; then
  bad "preflight reported production_send_allowed true"
else
  pass "PREFLIGHT_ONLY offline bot runs no_send under sanitized env"
fi

# Parent still holds keys after child subshell returns (key boundary).
parent_key_probe="$(
  export MANTLE_PRIVATE_KEY=parent_still_holds_this
  CHILD_ENV_SNAPSHOT=1 "$ROOT/scripts/golive/run_signerless_shadow.sh" >/dev/null
  if [[ "${MANTLE_PRIVATE_KEY:-}" == "parent_still_holds_this" ]]; then
    echo parent_keys_retained
  else
    echo parent_keys_lost
  fi
)"
if [[ "$parent_key_probe" == "parent_keys_retained" ]]; then
  pass "parent retains keys after signerless child exits"
else
  bad "parent keys were stripped by signerless launcher (must use subshell)"
fi

# --- 3. direct SHADOW_MODE + forbidden key refuses (existing continuous) -----
# Continuous launcher sources $ROOT/.env and fail-closes on any forbidden name.
# If a developer .env is present with keys, move it aside for the duration so
# test_launcher_nosend.sh can isolate one var at a time (always run, never skip).
ENV_BACKUP=""
if [[ -f "$ROOT/.env" ]]; then
  ENV_BACKUP="$TMP/dotenv.backup"
  mv "$ROOT/.env" "$ENV_BACKUP"
fi
set +e
nosend_out="$("$ROOT/scripts/shadow/test_launcher_nosend.sh" 2>&1)"
nosend_rc=$?
set -e
if [[ -n "$ENV_BACKUP" ]]; then
  mv "$ENV_BACKUP" "$ROOT/.env"
fi
if [[ "$nosend_rc" -eq 0 ]]; then
  pass "continuous launcher refuses SHADOW_MODE + forbidden keys"
else
  bad "scripts/shadow/test_launcher_nosend.sh failed: $nosend_out"
fi

# Also: running bot directly with SHADOW_MODE=1 + PRIVATE_KEY must fail.
set +e
direct_out="$(
  env -u MANTLE_PRIVATE_KEY -u MANTLE_MAINNET_PRIVATE_KEY \
    -u MANTLE_SEPOLIA_PRIVATE_KEY -u EXECUTION_PRIVATE_KEY \
    SHADOW_MODE=1 PRIVATE_KEY=deadbeef \
    "$BOT" --offline 2>&1
)"
direct_rc=$?
set -e
if [[ "$direct_rc" -eq 0 ]]; then
  bad "bot --offline accepted SHADOW_MODE=1 + PRIVATE_KEY"
elif ! grep -qi 'forbidden\|PRIVATE_KEY' <<<"$direct_out"; then
  bad "bot refused SHADOW_MODE+key but message unexpected: $direct_out"
else
  pass "bot directly refuses SHADOW_MODE=1 + PRIVATE_KEY (rc=$direct_rc)"
fi

# --- 4. fund-and-canary missing approval params ------------------------------
for missing in APPROVAL_RECORD_ID NOTIONAL_CAP_WMNT_ETHER CANARY_POOLS_FILE EXECUTOR; do
  set +e
  # shellcheck disable=SC2086
  case "$missing" in
    APPROVAL_RECORD_ID)
      out="$(env -u APPROVAL_RECORD_ID \
        NOTIONAL_CAP_WMNT_ETHER=1 \
        CANARY_POOLS_FILE="$ROOT/scripts/golive/pools.txt" \
        EXECUTOR=0x0000000000000000000000000000000000000001 \
        "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1)"
      ;;
    NOTIONAL_CAP_WMNT_ETHER)
      out="$(env -u NOTIONAL_CAP_WMNT_ETHER \
        APPROVAL_RECORD_ID=test-approval \
        CANARY_POOLS_FILE="$ROOT/scripts/golive/pools.txt" \
        EXECUTOR=0x0000000000000000000000000000000000000001 \
        "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1)"
      ;;
    CANARY_POOLS_FILE)
      out="$(env -u CANARY_POOLS_FILE \
        APPROVAL_RECORD_ID=test-approval \
        NOTIONAL_CAP_WMNT_ETHER=1 \
        EXECUTOR=0x0000000000000000000000000000000000000001 \
        "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1)"
      ;;
    EXECUTOR)
      out="$(env -u EXECUTOR \
        APPROVAL_RECORD_ID=test-approval \
        NOTIONAL_CAP_WMNT_ETHER=1 \
        CANARY_POOLS_FILE="$ROOT/scripts/golive/pools.txt" \
        "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1)"
      ;;
  esac
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    bad "fund-and-canary accepted missing $missing"
  elif ! grep -q "$missing" <<<"$out"; then
    bad "fund-and-canary missing $missing but message lacks name: $out"
  else
    pass "fund-and-canary refuses missing $missing"
  fi
done

# All params present → preflight ok (no chain).
# Need a real pools file: generate if missing.
if [[ ! -f "$ROOT/scripts/golive/pools.txt" ]]; then
  "$ROOT/scripts/golive/gen_pools_from_universe.sh" >/dev/null
fi
# Use a 1-line subset for canary.
head -1 "$ROOT/scripts/golive/pools.txt" >"$TMP/canary_pools.txt"
# Approval id as a path: file must exist.
echo "test approval" >"$TMP/approval-record.txt"
set +e
out="$(
  APPROVAL_RECORD_ID="$TMP/approval-record.txt" \
  NOTIONAL_CAP_WMNT_ETHER=1 \
  CANARY_POOLS_FILE="$TMP/canary_pools.txt" \
  EXECUTOR=0x0000000000000000000000000000000000000001 \
  "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1
)"
rc=$?
set -e
if [[ "$rc" -ne 0 ]]; then
  bad "fund-and-canary --preflight with all params failed: $out"
else
  pass "fund-and-canary --preflight accepts complete approval params"
fi

# Path-shaped approval id that does not exist → refuse.
set +e
out="$(
  APPROVAL_RECORD_ID="$TMP/does-not-exist.md" \
  NOTIONAL_CAP_WMNT_ETHER=1 \
  CANARY_POOLS_FILE="$TMP/canary_pools.txt" \
  EXECUTOR=0x0000000000000000000000000000000000000001 \
  "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1
)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "fund-and-canary accepted missing approval record path"
else
  pass "fund-and-canary refuses missing approval record path"
fi

# --- 5. registry ⟷ universe --------------------------------------------------
"$ROOT/scripts/golive/gen_pools_from_universe.sh" "$TMP/pools_good.txt" >/dev/null
if golive_assert_pools_match_universe "$TMP/pools_good.txt" data/pool_universe.csv >/dev/null; then
  pass "generated pools match universe"
else
  bad "generated pools do not match universe"
fi

# deliberate mismatch
cp "$TMP/pools_good.txt" "$TMP/pools_bad.txt"
# flip first line's type if possible
python3 - "$TMP/pools_bad.txt" <<'PY'
import sys
path = sys.argv[1]
lines = open(path).read().splitlines()
if not lines:
    raise SystemExit("empty")
pool, t = lines[0].split()
lines[0] = f"{pool} {0 if t != '0' else 1}"
open(path, "w").write("\n".join(lines) + "\n")
PY
set +e
out="$(golive_assert_pools_match_universe "$TMP/pools_bad.txt" data/pool_universe.csv 2>&1)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "mismatch pools file was accepted"
else
  pass "registry ⟷ universe rejects deliberate mismatch"
fi

# subset check: good subset
if golive_assert_pools_subset_of_universe "$TMP/canary_pools.txt" data/pool_universe.csv >/dev/null; then
  pass "canary subset ⊆ universe"
else
  bad "valid canary subset rejected"
fi

# --- 6. deploy_only --preflight (addresses, no hot keys) ---------------------
# Need a throwaway admin key for address derivation in preflight.
ADMIN_PK="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
HOT_ADDR="0x70997970C51812dc3A010C7d01b50e0d17dc79C8"
G_ADDR="0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"
set +e
out="$(
  MANTLE_PRIVATE_KEY="$ADMIN_PK" \
  BOT_HOT_EXECUTOR_ADDRESS="$HOT_ADDR" \
  BOT_GUARDIAN_ADDRESS="$G_ADDR" \
  BOT_HOT_EXECUTOR_PRIVATE_KEY=should_be_unset \
  BOT_GUARDIAN_PRIVATE_KEY=should_be_unset \
  POOLS_FILE="$TMP/pools_good.txt" \
  "$ROOT/scripts/golive/deploy_only.sh" --preflight 2>&1
)"
rc=$?
set -e
if [[ "$rc" -ne 0 ]]; then
  bad "deploy_only --preflight failed: $out"
elif ! grep -q 'hot/guardian private keys absent: yes' <<<"$out"; then
  bad "deploy_only preflight missing key-absent marker: $out"
else
  pass "deploy_only --preflight works with addresses; drops hot/guardian keys"
fi

# Missing address → fail
set +e
out="$(
  MANTLE_PRIVATE_KEY="$ADMIN_PK" \
  BOT_HOT_EXECUTOR_ADDRESS="$HOT_ADDR" \
  env -u BOT_GUARDIAN_ADDRESS -u GUARDIAN_ADDRESS \
  "$ROOT/scripts/golive/deploy_only.sh" --preflight 2>&1
)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "deploy_only accepted missing guardian address"
else
  pass "deploy_only refuses missing guardian address"
fi

# --- 7. retired deploy_and_arm -----------------------------------------------
set +e
out="$("$ROOT/scripts/golive/deploy_and_arm.sh" 2>&1)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "retired deploy_and_arm.sh exited 0"
elif ! grep -qi 'retired\|WHI-953' <<<"$out"; then
  bad "deploy_and_arm message unexpected: $out"
else
  pass "deploy_and_arm.sh retired (non-zero)"
fi

# --- 8. run_live refuses SHADOW_MODE -----------------------------------------
set +e
out="$(
  SHADOW_MODE=1 \
  MANTLE_RPC_URL=http://example.invalid \
  MANTLE_RPC_WS_URL=ws://example.invalid \
  ARBITRAGE_EXECUTOR_ADDRESS=0x0000000000000000000000000000000000000001 \
  "$ROOT/scripts/golive/run_live.sh" 2>&1
)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "run_live accepted SHADOW_MODE=1"
elif ! grep -qi 'SHADOW_MODE\|signerless' <<<"$out"; then
  bad "run_live SHADOW_MODE refusal message unexpected: $out"
else
  pass "run_live.sh refuses SHADOW_MODE=1"
fi

echo
if [[ "$fail" -ne 0 ]]; then
  echo "WHI-953 launcher tests: FAILED" >&2
  exit 1
fi
echo "WHI-953 launcher tests: all passed"
