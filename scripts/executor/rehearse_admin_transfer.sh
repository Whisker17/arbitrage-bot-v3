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
GUARDIAN="${GUARDIAN:-}"
ANVIL_PORT="${ANVIL_PORT:-8545}"
ANVIL_RPC="http://127.0.0.1:${ANVIL_PORT}"
WMNT="${WMNT:-0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8}"

if [[ -z "$COLD" ]]; then
  echo "error: COLD=<cold-admin-address> is required" >&2
  echo "  For a pure dry-run use any non-zero address, e.g. COLD=0x000000000000000000000000000000000000c01d" >&2
  exit 1
fi

if [[ "$(echo "$COLD" | tr '[:upper:]' '[:lower:]')" == "0x0000000000000000000000000000000000000000" ]]; then
  echo "error: COLD must not be the zero address" >&2
  exit 1
fi

# Resolve current admin from mainnet (source of truth).
CURRENT_ADMIN="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$RPC_URL")"
TIP_BLOCK="$(cast block-number --rpc-url "$RPC_URL")"

echo "=== WHI-861 fork rehearsal ==="
echo "fork_rpc=<redacted>"
echo "mainnet_tip_at_start=$TIP_BLOCK"
echo "executor=$EXECUTOR"
echo "current_admin=$CURRENT_ADMIN"
echo "hot=$HOT"
echo "cold=$COLD"
echo "guardian=${GUARDIAN:-(skip)}"
echo "anvil_port=$ANVIL_PORT"

# Fail closed if something already holds the anvil port.
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
# and refuse historical block fetches even for "now").
anvil --fork-url "$RPC_URL" --port "$ANVIL_PORT" --silent &
ANVIL_PID=$!

# Wait for anvil readiness (fork init can take a few seconds).
for _ in $(seq 1 120); do
  if cast chain-id --rpc-url "$ANVIL_RPC" >/dev/null 2>&1; then
    break
  fi
  sleep 0.5
done
if ! cast chain-id --rpc-url "$ANVIL_RPC" >/dev/null 2>&1; then
  echo "error: anvil did not become ready on $ANVIL_RPC" >&2
  exit 1
fi

FORK_BLOCK="$(cast block-number --rpc-url "$ANVIL_RPC")"
echo "anvil_fork_block=$FORK_BLOCK"

# Pre-state on the fork.
pre_admin="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$ANVIL_RPC")"
pre_paused="$(cast call "$EXECUTOR" 'paused()(bool)' --rpc-url "$ANVIL_RPC")"
pre_hot="$(cast call "$EXECUTOR" 'isHotExecutor(address)(bool)' "$HOT" --rpc-url "$ANVIL_RPC")"
pre_native="$(cast balance "$EXECUTOR" --rpc-url "$ANVIL_RPC")"
pre_wmnt="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$ANVIL_RPC")"

echo "--- pre-state ---"
echo "admin=$pre_admin"
echo "paused=$pre_paused"
echo "isHotExecutor(hot)=$pre_hot"
echo "native=$pre_native wmnt=$pre_wmnt"

if [[ "$pre_paused" != "true" ]]; then
  echo "error: expected paused=true on mainnet fork; aborting rehearsal" >&2
  exit 1
fi
if [[ "$pre_native" != "0" || "$pre_wmnt" != "0" ]]; then
  echo "error: expected zero balances; aborting rehearsal (WHI-861 must not move funds)" >&2
  exit 1
fi

# Impersonate current admin; fund gas.
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

if [[ -n "$GUARDIAN" && "$(echo "$GUARDIAN" | tr '[:upper:]' '[:lower:]')" != "0x0000000000000000000000000000000000000000" ]]; then
  echo "--- tx 2: setGuardian($GUARDIAN) ---"
  send_as_admin "$EXECUTOR" "setGuardian(address)" "$GUARDIAN"
else
  echo "--- tx 2: setGuardian skipped ---"
fi

echo "--- tx 3: transferAdmin($COLD) ---"
send_as_admin "$EXECUTOR" "transferAdmin(address)" "$COLD"

# Stop impersonation (hygiene).
cast rpc anvil_stopImpersonatingAccount "$CURRENT_ADMIN" --rpc-url "$ANVIL_RPC" >/dev/null || true

# Post-state.
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
admin_lc="$(echo "$post_admin" | tr '[:upper:]' '[:lower:]')"
cold_lc="$(echo "$COLD" | tr '[:upper:]' '[:lower:]')"
hot_lc="$(echo "$HOT" | tr '[:upper:]' '[:lower:]')"

if [[ "$admin_lc" != "$cold_lc" ]]; then
  echo "FAIL: admin is $post_admin, expected $COLD" >&2
  fail=1
fi
if [[ "$admin_lc" == "$hot_lc" ]]; then
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

# Former admin can no longer transferAdmin on the fork.
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
