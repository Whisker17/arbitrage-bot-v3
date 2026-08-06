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
#   EXPECT_CODEHASH (default: pinned mainnet codehash; set empty to skip)

set -euo pipefail

# shellcheck source=scripts/executor/_common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

REPO_ROOT="$(_executor_repo_root)"
_executor_load_env "$REPO_ROOT"
_executor_resolve_rpc
_executor_defaults

COLD="${COLD:-}"
EXPECT_PAUSED="${EXPECT_PAUSED:-true}"

chain_id="$(_require_mainnet_chain "$RPC_URL")"
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

if [[ -n "$COLD" ]]; then
  if ! _addrs_equal "$admin" "$COLD"; then
    echo "FAIL: admin()=$admin expected COLD=$COLD" >&2
    fail=1
  fi
  if _addrs_equal "$admin" "$HOT"; then
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

if [[ -n "${EXPECT_CODEHASH}" ]]; then
  # codehash is 0x-hex; compare case-insensitively
  if [[ "$(_addr_lc "$codehash")" != "$(_addr_lc "$EXPECT_CODEHASH")" ]]; then
    echo "FAIL: codehash=$codehash expected $EXPECT_CODEHASH" >&2
    fail=1
  fi
fi

if [[ "$fail" -ne 0 ]]; then
  echo "verify_executor_roles: FAILED" >&2
  exit 1
fi

echo "verify_executor_roles: OK"
