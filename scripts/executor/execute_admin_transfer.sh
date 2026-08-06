#!/usr/bin/env bash
# WHI-861 — mainnet custody handoff (BROADCASTS).
#
# THIS SCRIPT MOVES ADMIN ON THE LIVE EXECUTOR. It is intentionally gated.
#
# Preconditions:
#   1. COLD admin key was generated offline (hardware wallet / offline keystore).
#      Only the ADDRESS is passed here — never the cold private key to this host
#      if it can be avoided. Prefer --ledger / --interactive / --keystore for
#      the cold-key proof step after transfer.
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
#   Prefer interactive / keystore / ledger. Raw --private-key is accepted only
#   when CURRENT_ADMIN_PRIVATE_KEY is already in the environment (not argv) so
#   the machine that still holds the plaintext deploy key can finish the
#   handoff. After success, remove admin powers from .env (see runbook).

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

if [[ "${I_UNDERSTAND_MAINNET:-}" != "1" ]]; then
  echo "error: refuse to broadcast without I_UNDERSTAND_MAINNET=1" >&2
  echo "  This script changes live admin on Mantle mainnet." >&2
  exit 1
fi

EXECUTOR="${EXECUTOR:-0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF}"
HOT="${HOT:-0x6A00754e22A4fcde9B5290da7A3367dfF96f6486}"
COLD="${COLD:-}"
GUARDIAN="${GUARDIAN:-}"
WMNT="${WMNT:-0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8}"

if [[ -z "$COLD" ]]; then
  echo "error: COLD=<cold-admin-address> is required" >&2
  exit 1
fi

chain_id="$(cast chain-id --rpc-url "$RPC_URL")"
if [[ "$chain_id" != "5000" ]]; then
  echo "error: refuse to run on chain_id=$chain_id (expected 5000 Mantle mainnet)" >&2
  exit 1
fi

CURRENT_ADMIN="$(cast call "$EXECUTOR" 'admin()(address)' --rpc-url "$RPC_URL")"
paused="$(cast call "$EXECUTOR" 'paused()(bool)' --rpc-url "$RPC_URL")"
native="$(cast balance "$EXECUTOR" --rpc-url "$RPC_URL")"
wmnt_bal="$(cast call "$WMNT" 'balanceOf(address)(uint256)' "$EXECUTOR" --rpc-url "$RPC_URL")"
tip="$(cast block-number --rpc-url "$RPC_URL")"

echo "=== WHI-861 MAINNET admin transfer ==="
echo "executor=$EXECUTOR"
echo "chain_id=$chain_id tip=$tip"
echo "current_admin=$CURRENT_ADMIN"
echo "hot=$HOT"
echo "cold=$COLD"
echo "guardian=${GUARDIAN:-(skip)}"
echo "paused=$paused native=$native wmnt=$wmnt_bal"

if [[ "$paused" != "true" ]]; then
  echo "error: executor is not paused; abort" >&2
  exit 1
fi
if [[ "$native" != "0" || "$wmnt_bal" != "0" ]]; then
  echo "error: executor holds balance; WHI-861 must run while unfunded. Abort." >&2
  exit 1
fi

cold_lc="$(echo "$COLD" | tr '[:upper:]' '[:lower:]')"
admin_lc="$(echo "$CURRENT_ADMIN" | tr '[:upper:]' '[:lower:]')"
hot_lc="$(echo "$HOT" | tr '[:upper:]' '[:lower:]')"
if [[ "$cold_lc" == "$admin_lc" ]]; then
  echo "error: COLD equals current admin — nothing to transfer (or wrong address)" >&2
  exit 1
fi
if [[ "$cold_lc" == "$hot_lc" ]]; then
  echo "error: COLD must differ from HOT (separation of roles)" >&2
  exit 1
fi
if [[ "$cold_lc" == "0x0000000000000000000000000000000000000000" ]]; then
  echo "error: COLD must not be zero" >&2
  exit 1
fi

# Resolve signing: env key only (never argv). Prefer dedicated CURRENT_ADMIN_PRIVATE_KEY,
# then fall back to the legacy deploy-key names still on this host.
PK="${CURRENT_ADMIN_PRIVATE_KEY:-${MANTLE_PRIVATE_KEY:-${MANTLE_MAINNET_PRIVATE_KEY:-${PRIVATE_KEY:-}}}}"
if [[ -z "$PK" ]]; then
  echo "error: no current-admin key in environment." >&2
  echo "  Set CURRENT_ADMIN_PRIVATE_KEY (preferred) or the legacy deploy-key var" >&2
  echo "  only for this one-shot handoff. Prefer --keystore/--ledger if available." >&2
  exit 1
fi

# Verify the env key matches on-chain admin (fail closed before any tx).
derived="$(cast wallet address --private-key "$PK")"
derived_lc="$(echo "$derived" | tr '[:upper:]' '[:lower:]')"
if [[ "$derived_lc" != "$admin_lc" ]]; then
  echo "error: env key address $derived does not match on-chain admin $CURRENT_ADMIN" >&2
  exit 1
fi

send_pk() {
  # --private-key from env var expansion only (not shell history of raw hex if possible).
  cast send "$@" \
    --private-key "$PK" \
    --rpc-url "$RPC_URL"
}

echo "--- mainnet tx 1: setHotExecutor ---"
tx1="$(send_pk "$EXECUTOR" "setHotExecutor(address,bool)" "$HOT" true)"
echo "$tx1"

if [[ -n "$GUARDIAN" && "$(echo "$GUARDIAN" | tr '[:upper:]' '[:lower:]')" != "0x0000000000000000000000000000000000000000" ]]; then
  echo "--- mainnet tx 2: setGuardian ---"
  tx2="$(send_pk "$EXECUTOR" "setGuardian(address)" "$GUARDIAN")"
  echo "$tx2"
else
  echo "--- mainnet tx 2: setGuardian skipped ---"
fi

echo "--- mainnet tx 3: transferAdmin (ONE-WAY) ---"
tx3="$(send_pk "$EXECUTOR" "transferAdmin(address)" "$COLD")"
echo "$tx3"

# Wipe local references; do not print PK.
unset PK CURRENT_ADMIN_PRIVATE_KEY
# Note: cannot unset vars sourced into parent shell from a child; operator must
# edit .env after success.

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
