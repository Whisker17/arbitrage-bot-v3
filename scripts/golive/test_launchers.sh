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
#   9. universe pin (WHI-1410): the regenerated arb-bot-jp universe and an
#      edited CSV fail every launcher preflight
#  10. signerless refuses a forwarded --pool-universe (both forms) before launch
#      and hands the child exactly the pin-checked BOT_POOL_UNIVERSE
#  11. universe preflight passes paths to Python as argv (quotes/backslashes),
#      and refuses malformed/missing meta, pin and CSV
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

# --- 9. universe pin (WHI-1410) ----------------------------------------------
# The captured arb-bot-jp universe is a legitimate, self-agreeing CSV + meta
# regeneration; every launcher must still refuse it because it is not pinned.
if (golive_require_universe_fingerprint data/pool_universe.csv >/dev/null); then
  pass "committed universe matches the pin"
else
  bad "committed universe rejected by its own pin"
fi
HOST_UNIVERSE="$ROOT/evidence/universe/whi-1410/arb-bot-jp/pool_universe.csv"
for launcher in signerless deploy_only fund_and_canary; do
  set +e
  case "$launcher" in
    signerless)
      out="$(BOT_POOL_UNIVERSE="$HOST_UNIVERSE" CHILD_ENV_SNAPSHOT=1 \
        "$ROOT/scripts/golive/run_signerless_shadow.sh" 2>&1)" ;;
    deploy_only)
      out="$(BOT_POOL_UNIVERSE="$HOST_UNIVERSE" MANTLE_PRIVATE_KEY="$ADMIN_PK" \
        BOT_HOT_EXECUTOR_ADDRESS="$HOT_ADDR" BOT_GUARDIAN_ADDRESS="$G_ADDR" \
        POOLS_FILE="$TMP/pools_good.txt" \
        "$ROOT/scripts/golive/deploy_only.sh" --preflight 2>&1)" ;;
    fund_and_canary)
      out="$(BOT_POOL_UNIVERSE="$HOST_UNIVERSE" \
        APPROVAL_RECORD_ID="$TMP/approval-record.txt" NOTIONAL_CAP_WMNT_ETHER=1 \
        CANARY_POOLS_FILE="$TMP/canary_pools.txt" \
        EXECUTOR=0x0000000000000000000000000000000000000001 \
        "$ROOT/scripts/golive/fund_and_canary.sh" --preflight 2>&1)" ;;
  esac
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    bad "$launcher accepted the unpinned arb-bot-jp universe"
  elif ! grep -q 'universe fingerprint 0xee1d40b8.* != pinned 0x0ecceac8' <<<"$out"; then
    bad "$launcher refused the arb-bot-jp universe for the wrong reason: $out"
  else
    pass "$launcher refuses the unpinned arb-bot-jp universe"
  fi
done

# CSV edited behind an untouched meta: the meta fingerprint still matches.
mkdir -p "$TMP/edited"
cp data/pool_universe.meta.json "$TMP/edited/pool_universe.meta.json"
sed '$d' data/pool_universe.csv >"$TMP/edited/pool_universe.csv"
set +e
out="$(golive_require_universe_fingerprint "$TMP/edited/pool_universe.csv" 2>&1)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "edited CSV with an untouched meta was accepted"
elif ! grep -q 'csv sha256 .* != pinned' <<<"$out"; then
  bad "edited CSV refused for the wrong reason: $out"
else
  pass "edited CSV behind an untouched meta is refused"
fi

# No pin → fail closed (never fall back to the self-agreeing meta check).
set +e
out="$(GOLIVE_UNIVERSE_PIN=config/does-not-exist.pin.json \
  golive_require_universe_fingerprint data/pool_universe.csv 2>&1)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]]; then
  bad "missing universe pin was accepted"
elif ! grep -q 'universe pin missing' <<<"$out"; then
  bad "missing pin refused for the wrong reason: $out"
else
  pass "missing universe pin is refused"
fi

# --- 10. signerless --pool-universe cannot bypass the pin (WHI-1410 PR-F1) ---
# Live path with a stub bot (records argv + BOT_POOL_UNIVERSE) and a stub cast
# (chain 5000); no RPC is contacted and no real bot runs.
mkdir -p "$TMP/stub" "$TMP/shadow"
cat >"$TMP/stub/bot" <<'SH'
#!/usr/bin/env bash
printf 'ARGS %s\nBOT_POOL_UNIVERSE=%s\n' "$*" "${BOT_POOL_UNIVERSE-<unset>}" >"$STUB_BOT_RECORD"
SH
printf '#!/usr/bin/env bash\necho 5000\n' >"$TMP/stub/cast"
chmod +x "$TMP/stub/bot" "$TMP/stub/cast"
# usage: run_signerless_stub [VAR=value ...] -- [bot args ...]
run_signerless_stub() {
  local envs=()
  while [[ $# -gt 0 && "$1" != "--" ]]; do envs+=("$1"); shift; done
  [[ $# -gt 0 ]] && shift
  rm -f "$TMP/shadow/bot.record"
  env -u BOT_POOL_UNIVERSE PATH="$TMP/stub:$PATH" BOT_BIN="$TMP/stub/bot" \
    STUB_BOT_RECORD="$TMP/shadow/bot.record" \
    RPC_HTTP_URL=http://example.invalid RPC_WS_URL=ws://example.invalid \
    SHADOW_LOG_PATH="$TMP/shadow/signerless.log" LEDGER_PATH="$TMP/shadow/ledger.jsonl" \
    SHADOW_RUN_PLAN_PATH="$TMP/shadow/run_plan.json" \
    SHADOW_CAPITAL_EVIDENCE_PATH="$TMP/shadow/capital.json" \
    ${envs[@]+"${envs[@]}"} "$ROOT/scripts/golive/run_signerless_shadow.sh" "$@" 2>&1
}
for form in split joined; do
  set +e
  if [[ "$form" == split ]]; then
    out="$(run_signerless_stub -- --pool-universe "$HOST_UNIVERSE")"
  else
    out="$(run_signerless_stub -- "--pool-universe=$HOST_UNIVERSE")"
  fi
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    bad "signerless accepted a forwarded --pool-universe ($form form)"
  elif [[ -e "$TMP/shadow/bot.record" ]]; then
    bad "signerless launched the bot despite --pool-universe ($form form)"
  elif ! grep -q 'refuses --pool-universe.*set BOT_POOL_UNIVERSE' <<<"$out"; then
    bad "signerless refused --pool-universe ($form form) for the wrong reason: $out"
  else
    pass "signerless refuses forwarded --pool-universe ($form form) before launch"
  fi
done
# Valid pinned universe via BOT_POOL_UNIVERSE: the child runs with exactly it.
PINNED_UNIVERSE="$ROOT/data/pool_universe.csv"
set +e
out="$(run_signerless_stub BOT_POOL_UNIVERSE="$PINNED_UNIVERSE" -- --once)"
rc=$?
set -e
record="$(cat "$TMP/shadow/bot.record" 2>/dev/null || true)"
if [[ "$rc" -ne 0 ]]; then
  bad "signerless rejected the pinned universe: $out"
elif ! grep -qx "BOT_POOL_UNIVERSE=$PINNED_UNIVERSE" <<<"$record"; then
  bad "signerless child did not get the validated universe path: $record"
elif grep -q -- '--pool-universe' <<<"$record" || ! grep -q -- '--once' <<<"$record"; then
  bad "signerless child argv unexpected: $record"
else
  pass "signerless launches the pinned universe with BOT_POOL_UNIVERSE set to the validated path"
fi
# Default (BOT_POOL_UNIVERSE unset): the child is pinned to the default path.
set +e
out="$(run_signerless_stub --)"
rc=$?
set -e
record="$(cat "$TMP/shadow/bot.record" 2>/dev/null || true)"
if [[ "$rc" -ne 0 ]]; then
  bad "signerless rejected the default universe: $out"
elif ! grep -qx "BOT_POOL_UNIVERSE=$GOLIVE_DEFAULT_UNIVERSE" <<<"$record"; then
  bad "signerless child not pinned to the default universe: $record"
else
  pass "signerless exports the default validated universe to the child"
fi

# --- 11. universe preflight treats paths as data (WHI-1410 PR-F2) -------------
# The pinned universe and the pin, copied under paths with a space, an
# apostrophe and a backslash, must pass unchanged. Before the fix such a path
# was spliced into `python3 -c` source (SyntaxError, or code execution).
ODD_ROOT="$TMP/Alice's repo\\x"
mkdir -p "$ODD_ROOT/data" "$ODD_ROOT/config"
cp data/pool_universe.csv data/pool_universe.meta.json "$ODD_ROOT/data/"
cp config/pool_universe.pin.json "$ODD_ROOT/config/"
ODD_CSV="$ODD_ROOT/data/pool_universe.csv"
# Run the preflight with the pin resolved under ODD_ROOT instead of the repo.
odd_preflight() {
  ( golive_repo_root() { printf '%s' "$ODD_ROOT"; }
    golive_require_universe_fingerprint "$1" )
}
pinned_fp="$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["fingerprint"])' config/pool_universe.pin.json)"
set +e
out="$(odd_preflight "$ODD_CSV" 2>&1)"
rc=$?
set -e
if [[ "$rc" -ne 0 ]]; then
  bad "pinned universe + pin under a quote/backslash path were refused: $out"
elif [[ "$out" != "$pinned_fp" ]]; then
  bad "quote/backslash path returned the wrong fingerprint: $out"
else
  pass "universe preflight accepts CSV/meta/pin paths with space, apostrophe and backslash"
fi
# The host universe under the same odd path is still refused on fingerprint.
mkdir -p "$ODD_ROOT/host"
cp "$HOST_UNIVERSE" "${HOST_UNIVERSE%.csv}.meta.json" "$ODD_ROOT/host/"
set +e
out="$(odd_preflight "$ODD_ROOT/host/pool_universe.csv" 2>&1)"
rc=$?
set -e
if [[ "$rc" -eq 0 ]] || ! grep -q 'universe fingerprint 0xee1d40b8.* != pinned 0x0ecceac8' <<<"$out"; then
  bad "unpinned universe under a quote/backslash path not refused on fingerprint (rc=$rc): $out"
else
  pass "unpinned universe under a quote/backslash path is refused"
fi
# Malformed/missing inputs fail closed with a clear reason and no evaluation.
for case_ in bad-meta no-meta bad-count missing-csv malformed-pin array-pin missing-pin; do
  rm -rf "$ODD_ROOT/case"
  cp -R "$ODD_ROOT/data" "$ODD_ROOT/case"
  cp config/pool_universe.pin.json "$ODD_ROOT/config/pool_universe.pin.json"
  csv="$ODD_ROOT/case/pool_universe.csv"
  case "$case_" in
    bad-meta)      echo '{not json' >"$ODD_ROOT/case/pool_universe.meta.json"
                   want='could not read meta/pin/csv' ;;
    no-meta)       rm "$ODD_ROOT/case/pool_universe.meta.json"; want='universe meta missing' ;;
    bad-count)     python3 - "$ODD_ROOT/case/pool_universe.meta.json" "$TMP/pwned" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
d["pool_count"] = "a[$(touch '%s')]" % sys.argv[2]
json.dump(d, open(sys.argv[1], "w"))
PY
                   want='pool_count is not a positive integer' ;;
    missing-csv)   rm "$csv"; want='pool universe missing' ;;
    malformed-pin) echo '{"fingerprint": ' >"$ODD_ROOT/config/pool_universe.pin.json"
                   want='could not read meta/pin/csv' ;;
    array-pin)     echo '["0x0ecceac8"]' >"$ODD_ROOT/config/pool_universe.pin.json"
                   want='could not read meta/pin/csv' ;;
    missing-pin)   rm "$ODD_ROOT/config/pool_universe.pin.json"; want='universe pin missing' ;;
  esac
  set +e
  out="$(odd_preflight "$csv" 2>&1)"
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    bad "universe preflight accepted $case_"
  elif ! grep -q "$want" <<<"$out"; then
    bad "universe preflight refused $case_ for the wrong reason: $out"
  elif [[ -e "$TMP/pwned" ]]; then
    bad "universe preflight evaluated a meta value as code ($case_)"
  else
    pass "universe preflight refuses $case_"
  fi
done

echo
if [[ "$fail" -ne 0 ]]; then
  echo "WHI-953 launcher tests: FAILED" >&2
  exit 1
fi
echo "WHI-953 launcher tests: all passed"
