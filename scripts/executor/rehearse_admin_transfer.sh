#!/usr/bin/env bash
# WHI-861 — fork-rehearse the custody handoff against Mantle mainnet state.
#
# Sequence (must match mainnet order; transferAdmin is last and one-way):
#   1. setHotExecutor(HOT, true)   — from current admin
#   2. setGuardian(GUARDIAN)       — optional (skipped if GUARDIAN unset/zero)
#   3. transferAdmin(COLD)         — last
#
# Uses anvil --fork-url + account impersonation. Never broadcasts to mainnet.
# Does not read or print private keys.
#
# Usage:
#   COLD=0x... ./scripts/executor/rehearse_admin_transfer.sh
#   COLD=0x... HOT=0x... GUARDIAN=0x... ./scripts/executor/rehearse_admin_transfer.sh
#
# Defaults:
#   EXECUTOR = mainnet WHI-547 address
#   HOT      = current deployer (0x6A00…); recommended reuse as execute-only hot
#   COLD     = required (use a throwaway address for pure rehearsal)

set -euo pipefail

# shellcheck source=scripts/executor/_common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

REPO_ROOT="$(_executor_repo_root)"
_executor_load_env "$REPO_ROOT"
_executor_resolve_rpc
_executor_defaults

COLD="${COLD:-}"
GUARDIAN="${GUARDIAN:-}"
ANVIL_PORT="${ANVIL_PORT:-8545}"
ANVIL_RPC="http://127.0.0.1:${ANVIL_PORT}"

_require_nonzero_addr "COLD" "$COLD"

if _addrs_equal "$COLD" "$HOT"; then
  echo "error: COLD must differ from HOT (separation of roles)" >&2
  exit 1
fi

# Fail closed on wrong network before spinning anvil.
chain_id="$(_require_mainnet_chain "$RPC_URL")"
CURRENT_ADMIN="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$RPC_URL")"
TIP_BLOCK="$(cast block-number --rpc-url "$RPC_URL")"

if _addrs_equal "$COLD" "$CURRENT_ADMIN"; then
  echo "error: COLD equals current admin — nothing to transfer (or wrong address)" >&2
  exit 1
fi

echo "=== WHI-861 fork rehearsal ==="
echo "fork_rpc=<redacted>"
echo "chain_id=$chain_id"
echo "mainnet_tip_at_start=$TIP_BLOCK"
echo "executor=$EXECUTOR"
echo "current_admin=$CURRENT_ADMIN"
echo "hot=$HOT"
echo "cold=$COLD"
echo "guardian=${GUARDIAN:-(skip)}"
echo "anvil_port=$ANVIL_PORT"

if lsof -iTCP:"$ANVIL_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "error: port $ANVIL_PORT already in use; set ANVIL_PORT or free it" >&2
  exit 1
fi

ANVIL_PID=""
cleanup() {
  if [[ -n "${ANVIL_PID}" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
    kill "$ANVIL_PID" 2>/dev/null || true
    wait "$ANVIL_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Fork latest tip (do not pin --fork-block-number: many Mantle RPCs are not archive
# and refuse historical block fetches even for "now"). Retry: non-archive nodes
# sometimes race the tip and return "Failed to get block for block number".
anvil_ready=0
for attempt in 1 2 3 4 5; do
  anvil --fork-url "$RPC_URL" --port "$ANVIL_PORT" --silent &
  ANVIL_PID=$!
  for _ in $(seq 1 40); do
    if cast chain-id --rpc-url "$ANVIL_RPC" >/dev/null 2>&1; then
      anvil_ready=1
      break
    fi
    # If anvil already died, break early and retry.
    if ! kill -0 "$ANVIL_PID" 2>/dev/null; then
      break
    fi
    sleep 0.5
  done
  if [[ "$anvil_ready" -eq 1 ]]; then
    break
  fi
  echo "anvil attempt $attempt failed; retrying..." >&2
  kill "$ANVIL_PID" 2>/dev/null || true
  wait "$ANVIL_PID" 2>/dev/null || true
  ANVIL_PID=""
  sleep 1
done
if [[ "$anvil_ready" -ne 1 ]]; then
  echo "error: anvil did not become ready on $ANVIL_RPC after retries" >&2
  exit 1
fi

FORK_BLOCK="$(cast block-number --rpc-url "$ANVIL_RPC")"
echo "anvil_fork_block=$FORK_BLOCK"

_read_paused_and_balances "$EXECUTOR" "$ANVIL_RPC" "$WMNT"
pre_admin="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$ANVIL_RPC")"
pre_hot="$(cast call "$EXECUTOR" 'isHotExecutor(address)(bool)' "$HOT" --rpc-url "$ANVIL_RPC")"

echo "--- pre-state ---"
echo "admin=$pre_admin"
echo "paused=$PAUSED"
echo "isHotExecutor(hot)=$pre_hot"
echo "native=$NATIVE_BAL wmnt=$WMNT_BAL"

_require_paused_unfunded "fork pre-state"

cast rpc anvil_impersonateAccount "$CURRENT_ADMIN" --rpc-url "$ANVIL_RPC" >/dev/null
cast rpc anvil_setBalance "$CURRENT_ADMIN" 0x56BC75E2D63100000 --rpc-url "$ANVIL_RPC" >/dev/null

send_as_admin() {
  cast send "$@" \
    --from "$CURRENT_ADMIN" \
    --unlocked \
    --rpc-url "$ANVIL_RPC" \
    --gas-limit 300000
}

echo "--- tx 1: setHotExecutor($HOT, true) ---"
send_as_admin "$EXECUTOR" "setHotExecutor(address,bool)" "$HOT" true

if [[ -n "$GUARDIAN" ]] && ! _addrs_equal "$GUARDIAN" "0x0000000000000000000000000000000000000000"; then
  echo "--- tx 2: setGuardian($GUARDIAN) ---"
  send_as_admin "$EXECUTOR" "setGuardian(address)" "$GUARDIAN"
else
  echo "--- tx 2: setGuardian skipped ---"
fi

echo "--- tx 3: transferAdmin($COLD) ---"
send_as_admin "$EXECUTOR" "transferAdmin(address)" "$COLD"

cast rpc anvil_stopImpersonatingAccount "$CURRENT_ADMIN" --rpc-url "$ANVIL_RPC" >/dev/null || true

post_admin="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$ANVIL_RPC")"
post_guardian="$(cast call "$EXECUTOR" 'guardian()(address)' --rpc-url "$ANVIL_RPC")"
post_paused="$(cast call "$EXECUTOR" 'paused()(bool)' --rpc-url "$ANVIL_RPC")"
post_hot="$(cast call "$EXECUTOR" 'isHotExecutor(address)(bool)' "$HOT" --rpc-url "$ANVIL_RPC")"
post_former_hot="$(cast call "$EXECUTOR" 'isHotExecutor(address)(bool)' "$CURRENT_ADMIN" --rpc-url "$ANVIL_RPC")"
post_native="$(cast balance "$EXECUTOR" --rpc-url "$ANVIL_RPC")"
post_wmnt="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$ANVIL_RPC")"

echo "--- post-state ---"
echo "admin=$post_admin"
echo "guardian=$post_guardian"
echo "paused=$post_paused"
echo "isHotExecutor(hot=$HOT)=$post_hot"
echo "isHotExecutor(former_admin=$CURRENT_ADMIN)=$post_former_hot"
echo "native=$post_native wmnt=$post_wmnt"

fail=0

if ! _addrs_equal "$post_admin" "$COLD"; then
  echo "FAIL: admin is $post_admin, expected $COLD" >&2
  fail=1
fi
if _addrs_equal "$post_admin" "$HOT"; then
  echo "FAIL: admin must differ from hot" >&2
  fail=1
fi
if [[ "$post_hot" != "true" ]]; then
  echo "FAIL: hot executor not registered" >&2
  fail=1
fi
if [[ "$post_paused" != "true" ]]; then
  echo "FAIL: must remain paused" >&2
  fail=1
fi
if [[ "$post_native" != "0" || "$post_wmnt" != "0" ]]; then
  echo "FAIL: balances must stay zero" >&2
  fail=1
fi

if cast send "$EXECUTOR" "transferAdmin(address)" "$CURRENT_ADMIN" \
    --from "$CURRENT_ADMIN" --unlocked --rpc-url "$ANVIL_RPC" --gas-limit 200000 \
    >/dev/null 2>&1; then
  echo "FAIL: former admin was still able to transferAdmin" >&2
  fail=1
else
  echo "former_admin_transferAdmin_rejected=true"
fi

if [[ "$fail" -ne 0 ]]; then
  echo "rehearse_admin_transfer: FAILED" >&2
  exit 1
fi

echo "rehearse_admin_transfer: OK"
echo "fork_block=$FORK_BLOCK"
echo "Note: this did not broadcast to mainnet. Record the fork block in evidence."
