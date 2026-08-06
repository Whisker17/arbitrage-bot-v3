#!/usr/bin/env bash
# Shared helpers for WHI-861 executor custody scripts.
# Sourced only — do not execute directly.

_executor_repo_root() {
  # BASH_SOURCE[0] is this file (_common.sh) even when called from a function.
  local here
  here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
  cd "$here/../.." && pwd
}

_executor_load_env() {
  local root="$1"
  if [[ -f "$root/.env" ]]; then
    # shellcheck disable=SC1091
    set -a
    source "$root/.env"
    set +a
  fi
}

_executor_resolve_rpc() {
  RPC_URL="${RPC_URL:-${MANTLE_MAINNET_RPC_URL:-${MANTLE_RPC_URL:-}}}"
  if [[ -z "${RPC_URL}" ]]; then
    echo "error: set RPC_URL / MANTLE_MAINNET_RPC_URL / MANTLE_RPC_URL" >&2
    return 1
  fi
}

# Defaults for the WHI-547 mainnet deployment.
_executor_defaults() {
  EXECUTOR="${EXECUTOR:-0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF}"
  HOT="${HOT:-0x6A00754e22A4fcde9B5290da7A3367dfF96f6486}"
  WMNT="${WMNT:-0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8}"
  # Pinned codehash from deploy evidence (byte-identical to fork rehearsal).
  # Unset → pin; empty string → skip assert (operator override).
  if [[ ! -v EXPECT_CODEHASH ]]; then
    EXPECT_CODEHASH="0xe1acd0f6ce3257330a9ef37cf7ff29867f3533178c6ccee1a4c3ac167ad3e699"
  fi
}

_addr_lc() {
  echo "$1" | tr '[:upper:]' '[:lower:]'
}

_addrs_equal() {
  [[ "$(_addr_lc "$1")" == "$(_addr_lc "$2")" ]]
}

_require_mainnet_chain() {
  local rpc="$1"
  local chain_id
  chain_id="$(cast chain-id --rpc-url "$rpc")"
  if [[ "$chain_id" != "5000" ]]; then
    echo "error: refuse chain_id=$chain_id (expected 5000 Mantle mainnet)" >&2
    return 1
  fi
  echo "$chain_id"
}

_require_nonzero_addr() {
  local name="$1"
  local addr="$2"
  if [[ -z "$addr" ]]; then
    echo "error: $name is required" >&2
    return 1
  fi
  if _addrs_equal "$addr" "0x0000000000000000000000000000000000000000"; then
    echo "error: $name must not be the zero address" >&2
    return 1
  fi
}

_read_paused_and_balances() {
  local executor="$1"
  local rpc="$2"
  local wmnt="$3"
  PAUSED="$(cast call "$executor" 'paused()(bool)' --rpc-url "$rpc")"
  NATIVE_BAL="$(cast balance "$executor" --rpc-url "$rpc")"
  WMNT_BAL="$(cast call "$wmnt" 'balanceOf(address)(uint256)' "$executor" --rpc-url "$rpc")"
}

_require_paused_unfunded() {
  local ctx="$1"
  if [[ "$PAUSED" != "true" ]]; then
    echo "error: ($ctx) expected paused=true, got $PAUSED" >&2
    return 1
  fi
  if [[ "$NATIVE_BAL" != "0" || "$WMNT_BAL" != "0" ]]; then
    echo "error: ($ctx) expected zero balances (native=$NATIVE_BAL wmnt=$WMNT_BAL)" >&2
    return 1
  fi
}
