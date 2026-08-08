#!/usr/bin/env bash
# Regenerate scripts/golive/pools.txt from data/pool_universe.csv.
# deploy_only.sh asserts equality; do not hand-edit pools.txt.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
# shellcheck source=scripts/lib/golive_common.sh
. "$ROOT/scripts/lib/golive_common.sh"
OUT="${1:-scripts/golive/pools.txt}"
CSV="${2:-${BOT_POOL_UNIVERSE:-$GOLIVE_DEFAULT_UNIVERSE}}"
golive_write_pools_from_universe "$OUT" "$CSV"
golive_assert_pools_match_universe "$OUT" "$CSV"
