#!/usr/bin/env bash
# WHI-861 — mainnet custody handoff (BROADCASTS).
#
# THIS SCRIPT MOVES ADMIN ON THE LIVE EXECUTOR. It is intentionally gated.
#
# Preconditions:
#   1. COLD admin key was generated offline (hardware wallet / offline keystore).
#      Only the ADDRESS is passed here — never put the cold private key on the bot
#      host. Prefer --ledger / --interactive / --keystore for the cold-key proof
#      step after transfer (see runbook).
#   2. Fork rehearsal already passed:
#        COLD=<addr> ./scripts/executor/rehearse_admin_transfer.sh
#   3. Role choice is explicit:
#        HOT defaults to the current deployer (recommended: reuse as execute-only).
#   4. Contract is still paused and unfunded.
#
# Sequence (from CURRENT admin key — still the deploy key until transfer lands):
#   1. setHotExecutor(HOT, true)
#   2. setGuardian(GUARDIAN)   # optional
#   3. transferAdmin(COLD)     # LAST, one-way
#
# Usage:
#   COLD=0x... I_UNDERSTAND_MAINNET=1 ./scripts/executor/execute_admin_transfer.sh
#
# Signing the three txs from the CURRENT admin:
#   This one-shot path accepts an env key only (never argv) because the machine
#   that still holds the plaintext deploy key is performing the handoff that
#   retires that key's admin powers. After success, strip admin material from
#   .env (see runbook). Ledger/keystore for the *current* admin is out of scope
#   for this script (the key is already hot/plaintext by definition of the bug
#   WHI-861 fixes); use cast --ledger manually if you have re-imported it.

set -euo pipefail

# shellcheck source=scripts/executor/_common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/_common.sh"

REPO_ROOT="$(_executor_repo_root)"
_executor_load_env "$REPO_ROOT"
_executor_resolve_rpc
_executor_defaults

if [[ "${I_UNDERSTAND_MAINNET:-}" != "1" ]]; then
  echo "error: refuse to broadcast without I_UNDERSTAND_MAINNET=1" >&2
  echo "  This script changes live admin on Mantle mainnet." >&2
  exit 1
fi

COLD="${COLD:-}"
GUARDIAN="${GUARDIAN:-}"

_require_nonzero_addr "COLD" "$COLD"

chain_id="$(_require_mainnet_chain "$RPC_URL")"
CURRENT_ADMIN="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$RPC_URL")"
_read_paused_and_balances "$EXECUTOR" "$RPC_URL" "$WMNT"
tip="$(cast block-number --rpc-url "$RPC_URL")"

echo "=== WHI-861 MAINNET admin transfer ==="
echo "executor=$EXECUTOR"
echo "chain_id=$chain_id tip=$tip"
echo "current_admin=$CURRENT_ADMIN"
echo "hot=$HOT"
echo "cold=$COLD"
echo "guardian=${GUARDIAN:-(skip)}"
echo "paused=$PAUSED native=$NATIVE_BAL wmnt=$WMNT_BAL"

_require_paused_unfunded "mainnet preflight"

if _addrs_equal "$COLD" "$CURRENT_ADMIN"; then
  echo "error: COLD equals current admin — nothing to transfer (or wrong address)" >&2
  exit 1
fi
if _addrs_equal "$COLD" "$HOT"; then
  echo "error: COLD must differ from HOT (separation of roles)" >&2
  exit 1
fi

# Resolve signing: env key only (never argv). Prefer dedicated CURRENT_ADMIN_PRIVATE_KEY,
# then fall back to the legacy deploy-key names still on this host.
PK="${CURRENT_ADMIN_PRIVATE_KEY:-${MANTLE_PRIVATE_KEY:-${MANTLE_MAINNET_PRIVATE_KEY:-${PRIVATE_KEY:-}}}}"
if [[ -z "$PK" ]]; then
  echo "error: no current-admin key in environment." >&2
  echo "  Set CURRENT_ADMIN_PRIVATE_KEY (preferred) or the legacy deploy-key var" >&2
  echo "  only for this one-shot handoff." >&2
  exit 1
fi

derived="$(cast wallet address --private-key "$PK")"
if ! _addrs_equal "$derived" "$CURRENT_ADMIN"; then
  echo "error: env key address $derived does not match on-chain admin $CURRENT_ADMIN" >&2
  exit 1
fi

send_pk() {
  cast send "$@" \
    --private-key "$PK" \
    --rpc-url "$RPC_URL"
}

echo "--- mainnet tx 1: setHotExecutor ---"
tx1="$(send_pk "$EXECUTOR" "setHotExecutor(address,bool)" "$HOT" true)"
echo "$tx1"

if [[ -n "$GUARDIAN" ]] && ! _addrs_equal "$GUARDIAN" "0x0000000000000000000000000000000000000000"; then
  echo "--- mainnet tx 2: setGuardian ---"
  tx2="$(send_pk "$EXECUTOR" "setGuardian(address)" "$GUARDIAN")"
  echo "$tx2"
else
  echo "--- mainnet tx 2: setGuardian skipped ---"
fi

echo "--- mainnet tx 3: transferAdmin (ONE-WAY) ---"
tx3="$(send_pk "$EXECUTOR" "transferAdmin(address)" "$COLD")"
echo "$tx3"

unset PK CURRENT_ADMIN_PRIVATE_KEY

echo "--- verifying post-state ---"
COLD="$COLD" HOT="$HOT" EXECUTOR="$EXECUTOR" RPC_URL="$RPC_URL" \
  "$REPO_ROOT/scripts/executor/verify_executor_roles.sh"

echo ""
echo "MAINNET handoff complete."
echo "NEXT (human, mandatory):"
echo "  1. Prove cold key can sign (e.g. setGuardian to same value from cold)."
echo "  2. Remove admin powers from .env: delete MANTLE_PRIVATE_KEY if unused,"
echo "     or move the deploy key solely into BOT_HOT_EXECUTOR_PRIVATE_KEY and"
echo "     document it as execute-only."
echo "  3. Record tx hashes + post-state in evidence/deployments/."
echo "  4. Confirm: git check-ignore .env && git log -p -- .env is empty."
