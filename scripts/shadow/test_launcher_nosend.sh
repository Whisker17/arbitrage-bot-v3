#!/usr/bin/env bash
# WHI-715: prove the continuous launcher fails closed when a signing key is present.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

fail=0
for var in PRIVATE_KEY EXECUTION_PRIVATE_KEY MANTLE_PRIVATE_KEY MANTLE_MAINNET_PRIVATE_KEY MANTLE_SEPOLIA_PRIVATE_KEY; do
  set +e
  out="$(
    env -u PRIVATE_KEY -u EXECUTION_PRIVATE_KEY -u MANTLE_PRIVATE_KEY \
      -u MANTLE_MAINNET_PRIVATE_KEY -u MANTLE_SEPOLIA_PRIVATE_KEY \
      "$var=deadbeef" \
      MANTLE_RPC_URL=http://example.invalid \
      MANTLE_RPC_WS_URL=ws://example.invalid \
      ./scripts/shadow/run_continuous_mainnet.sh 2>&1
  )"
  rc=$?
  set -e
  if [[ "$rc" -eq 0 ]]; then
    echo "FAIL: launcher accepted $var" >&2
    fail=1
  elif ! grep -q "$var is set" <<<"$out"; then
    echo "FAIL: launcher exit=$rc but message missing for $var: $out" >&2
    fail=1
  else
    echo "ok: refuses $var"
  fi
done

exit "$fail"
