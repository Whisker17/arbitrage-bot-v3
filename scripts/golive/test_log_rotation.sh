#!/usr/bin/env bash
# WHI-952 acceptance: force tracing-log rotation with a small threshold and
# verify retention keeps total size under the hard cap.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=scripts/lib/rotate_by_size.sh
. "$ROOT/scripts/lib/rotate_by_size.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
ACTIVE="$TMP/live.log"
MAX_SEG=32
MAX_TOTAL=80

rotations=0
for i in $(seq 0 19); do
  # 18-byte lines (matches the Rust unit test shape)
  printf 'line-%02dxxxxxxxxxx\n' "$i" >>"$ACTIVE"
  before_rotated="$(_list_rotated_ns "$ACTIVE" | wc -l | tr -d ' ')"
  rotate_by_size "$ACTIVE" "$MAX_SEG" "$MAX_TOTAL"
  after_rotated="$(_list_rotated_ns "$ACTIVE" | wc -l | tr -d ' ')"
  if (( after_rotated > before_rotated )) || [[ ! -s "$ACTIVE" && "$after_rotated" -ge 1 ]]; then
    # Count a rotation when a new segment appears or active was emptied after rotate.
    if (( after_rotated >= 1 )); then
      rotations=$((rotations + 1))
    fi
  fi
  total="$(total_bytes_for_path "$ACTIVE")"
  if (( total > MAX_TOTAL )); then
    echo "FAIL: total $total exceeded hard cap $MAX_TOTAL after write $i" >&2
    exit 1
  fi
done

if (( rotations < 1 )); then
  # Fallback: at least one rotated segment must exist after the loop.
  segs="$(_list_rotated_ns "$ACTIVE" | wc -l | tr -d ' ')"
  if (( segs < 1 )); then
    echo "FAIL: small threshold must force ≥1 rotation; none observed" >&2
    ls -la "$TMP" >&2
    exit 1
  fi
fi

total="$(total_bytes_for_path "$ACTIVE")"
echo "ok: rotations_observed>=1 total=${total} cap=${MAX_TOTAL} segs=$(_list_rotated_ns "$ACTIVE" | tr '\n' ' ')"
