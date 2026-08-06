#!/usr/bin/env bash
# WHI-861 — public post-state reads for ArbitrageExecutor roles.
# No private keys. Safe to run anytime.
#
# Usage:
#   ./scripts/executor/verify_executor_roles.sh
#   EXECUTOR=0x... HOT=0x... COLD=0x... ./scripts/executor/verify_executor_roles.sh
#
# Env (optional overrides):
#   RPC_URL / MANTLE_MAINNET_RPC_URL / MANTLE_RPC_URL
#   EXECUTOR  (default: mainnet WHI-547 deployment)
#   HOT       (address expected isHotExecutor=true; default: former deployer)
#   COLD      (if set, require admin() == COLD)
#   EXPECT_PAUSED (default: true)

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ -f "$REPO_ROOT/.env" ]]; then
  # shellcheck disable=SC1091
  set -a
  source "$REPO_ROOT/.env"
  set +a
fi

RPC_URL="${RPC_URL:-${MANTLE_MAINNET_RPC_URL:-${MANTLE_RPC_URL:-}}}"
if [[ -z "${RPC_URL}" ]]; then
  echo "error: set RPC_URL / MANTLE_MAINNET_RPC_URL / MANTLE_RPC_URL" >&2
  exit 1
fi

EXECUTOR="${EXECUTOR:-0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF}"
HOT="${HOT:-0x6A00754e22A4fcde9B5290da7A3367dfF96f6486}"
COLD="${COLD:-}"
WMNT="${WMNT:-0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8}"
EXPECT_PAUSED="${EXPECT_PAUSED:-true}"

chain_id="$(cast chain-id --rpc-url "$RPC_URL")"
block="$(cast block-number --rpc-url "$RPC_URL")"
admin="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$RPC_URL")"
guardian="$(cast call "$EXECUTOR" 'guardian()(address)' --rpc-url "$RPC_URL")"
paused="$(cast call "$EXECUTOR" 'paused()(bool)' --rpc-url "$RPC_URL")"
hot_ok="$(cast call "$EXECUTOR" 'isHotExecutor(address)(bool)' "$HOT" --rpc-url "$RPC_URL")"
native="$(cast balance "$EXECUTOR" --rpc-url "$RPC_URL")"
wmnt_bal="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$RPC_URL")"
codehash="$(cast keccak "$(cast code "$EXECUTOR" --rpc-url "$RPC_URL")")"

echo "executor=$EXECUTOR"
echo "chain_id=$chain_id"
echo "block=$block"
echo "admin=$admin"
echo "guardian=$guardian"
echo "paused=$paused"
echo "isHotExecutor($HOT)=$hot_ok"
echo "native_balance=$native"
echo "wmnt_balance=$wmnt_bal"
echo "codehash=$codehash"

fail=0

if [[ "$chain_id" != "5000" ]]; then
  echo "FAIL: expected chain id 5000, got $chain_id" >&2
  fail=1
fi

if [[ -n "$COLD" ]]; then
  # cast may return checksummed or lower-case — compare case-insensitively
  admin_lc="$(echo "$admin" | tr '[:upper:]' '[:lower:]')"
  cold_lc="$(echo "$COLD" | tr '[:upper:]' '[:lower:]')"
  if [[ "$admin_lc" != "$cold_lc" ]]; then
    echo "FAIL: admin()=$admin expected COLD=$COLD" >&2
    fail=1
  fi
  hot_lc="$(echo "$HOT" | tr '[:upper:]' '[:lower:]')"
  if [[ "$admin_lc" == "$hot_lc" ]]; then
    echo "FAIL: admin and hot executor must differ" >&2
    fail=1
  fi
fi

if [[ "$hot_ok" != "true" ]]; then
  echo "FAIL: isHotExecutor($HOT) is not true" >&2
  fail=1
fi

if [[ "$EXPECT_PAUSED" == "true" && "$paused" != "true" ]]; then
  echo "FAIL: expected paused=true, got $paused" >&2
  fail=1
fi

if [[ "$native" != "0" ]]; then
  echo "FAIL: native balance must be 0 (got $native)" >&2
  fail=1
fi

if [[ "$wmnt_bal" != "0" ]]; then
  echo "FAIL: WMNT balance must be 0 (got $wmnt_bal)" >&2
  fail=1
fi

if [[ "$fail" -ne 0 ]]; then
  echo "verify_executor_roles: FAILED" >&2
  exit 1
fi

echo "verify_executor_roles: OK"
