#!/usr/bin/env bash
# Bounded tee for bot stdout/stderr (WHI-952 / G-5).
#
# Reads stdin line-by-line, appends to ACTIVE, rotating when the active segment
# reaches LOG_MAX_SEGMENT_BYTES and reclaiming until total ≤ LOG_MAX_TOTAL_BYTES.
#
#   ./target/release/bot ... 2>&1 | scripts/golive/rotating_tee.sh evidence/.../live.log
#
# Defaults match src/ops/rotating_file.rs (64 MiB segment / 512 MiB total).

set -euo pipefail

ACTIVE="${1:?usage: rotating_tee.sh <active-log-path>}"
MAX_SEG="${LOG_MAX_SEGMENT_BYTES:-$((64 * 1024 * 1024))}"
MAX_TOTAL="${LOG_MAX_TOTAL_BYTES:-$((512 * 1024 * 1024))}"

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/rotate_by_size.sh
. "$ROOT/scripts/lib/rotate_by_size.sh"

mkdir -p "$(dirname -- "$ACTIVE")"
: >>"$ACTIVE"

while IFS= read -r line || [[ -n "${line:-}" ]]; do
  # Mirror to stdout so supervised runners / systemd still see the stream.
  printf '%s\n' "$line"
  printf '%s\n' "$line" >>"$ACTIVE"
  rotate_by_size "$ACTIVE" "$MAX_SEG" "$MAX_TOTAL"
done
