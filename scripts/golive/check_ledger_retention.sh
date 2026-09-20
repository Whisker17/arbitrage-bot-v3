#!/usr/bin/env bash
#
# WHI-1407 acceptance: "Actual JP retained-ledger history is checked to cover
# the reporting window before this issue is called done — the 512MiB cap
# alone is not evidence."
#
# Reports the earliest/latest timestamp actually retained across the shadow
# ledger's active file + numeric rotated segments (`path`, `path.1`,
# `path.2`, …), and whether a given UTC day is inside that retained span.
# Pure read-only inspection — never mutates the ledger or the digest state.
#
# Usage:
#   scripts/golive/check_ledger_retention.sh <ledger-active-path> [YYYY-MM-DD]
#
# Requires: jq.

set -euo pipefail

if ! command -v jq >/dev/null 2>&1; then
  echo "ABORT: jq is required" >&2
  exit 1
fi

LEDGER="${1:-}"
TARGET_DAY="${2:-}"
if [[ -z "$LEDGER" ]]; then
  echo "usage: $0 <ledger-active-path> [YYYY-MM-DD]" >&2
  exit 1
fi

segments=()
[[ -f "$LEDGER" ]] && segments+=("$LEDGER")
n=1
while [[ -f "${LEDGER}.${n}" ]]; do
  segments+=("${LEDGER}.${n}")
  n=$((n + 1))
done

if [[ "${#segments[@]}" -eq 0 ]]; then
  echo "ABORT: no active file or rotated segment found at $LEDGER" >&2
  exit 1
fi

echo "segments found (active + rotated): ${#segments[@]}"
total_bytes=0
for seg in "${segments[@]}"; do
  size="$(wc -c <"$seg" | tr -d ' ')"
  total_bytes=$((total_bytes + size))
  echo "  $seg  (${size} bytes)"
done
echo "total bytes on disk: $total_bytes"

# Earliest/latest timestamp across every row type this digest windows by
# (recorded_at_unix for observation/candidate rows, identity.header.block_timestamp
# for context rows, started_at_unix for run headers) — the same union the
# aggregator's retention check uses.
timestamps="$(
  cat "${segments[@]}" 2>/dev/null | jq -s -r '
    [ .[] | (.recorded_at_unix // .started_at_unix // .identity.header.block_timestamp // empty) ]
    | select(length > 0)
    | (min, max)
  ' 2>/dev/null || true
)"

if [[ -z "$timestamps" ]]; then
  echo "RESULT: ledger has zero rows with a usable timestamp — nothing retained yet."
  exit 0
fi

earliest="$(echo "$timestamps" | sed -n '1p')"
latest="$(echo "$timestamps" | sed -n '2p')"
earliest_date="$(date -u -d "@${earliest}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -r "${earliest}" +%Y-%m-%dT%H:%M:%SZ)"
latest_date="$(date -u -d "@${latest}" +%Y-%m-%dT%H:%M:%SZ 2>/dev/null || date -u -r "${latest}" +%Y-%m-%dT%H:%M:%SZ)"
echo "earliest retained timestamp: $earliest ($earliest_date)"
echo "latest retained timestamp:   $latest ($latest_date)"

if [[ -n "$TARGET_DAY" ]]; then
  day_since="$(date -u -d "${TARGET_DAY}T00:00:00Z" +%s 2>/dev/null || date -u -j -f "%Y-%m-%dT%H:%M:%SZ" "${TARGET_DAY}T00:00:00Z" +%s)"
  day_until=$((day_since + 86400))
  if [[ "$earliest" -le "$day_since" && "$latest" -ge $((day_until - 1)) ]]; then
    echo "RESULT: $TARGET_DAY appears fully covered by retained history."
  elif [[ "$earliest" -lt "$day_until" && "$latest" -ge "$day_since" ]]; then
    echo "RESULT: $TARGET_DAY is only PARTIALLY covered — some of that day has rotated out."
    exit 2
  else
    echo "RESULT: $TARGET_DAY is OUTSIDE the currently retained range — not covered at all."
    exit 3
  fi
fi
