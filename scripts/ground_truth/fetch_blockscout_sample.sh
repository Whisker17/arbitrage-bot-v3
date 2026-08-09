#!/usr/bin/env bash
# Fetch Blockscout API v2 transaction JSON for a sample hash list (WHI-956).
#
# Usage:
#   scripts/ground_truth/fetch_blockscout_sample.sh sample_hashes.txt /tmp/blockscout_txs
#
# Input file: one tx hash per line (optional trailing Blockscout URL column).
# Output:     <out_dir>/<tx_hash>.json
#
# Does not commit responses. Score offline with:
#   cargo run --release --bin ground_truth_collector -- verify-sample \
#     --report-in report.json --blockscout-dir /tmp/blockscout_txs \
#     --sample-hashes sample_hashes.txt --report-out report.json

set -euo pipefail

SAMPLE_FILE=${1:?sample hash file}
OUT_DIR=${2:?output directory}
BASE=${BLOCKSCOUT_BASE:-https://explorer.mantle.xyz}
SLEEP_S=${BLOCKSCOUT_SLEEP_S:-0.25}

mkdir -p "$OUT_DIR"
count=0
while IFS=$'\t ' read -r hash _rest; do
  [[ -z "${hash:-}" || "$hash" == \#* ]] && continue
  hash=$(echo "$hash" | tr '[:upper:]' '[:lower:]')
  out="$OUT_DIR/${hash}.json"
  if [[ -s "$out" ]]; then
    echo "skip existing $out"
    continue
  fi
  url="$BASE/api/v2/transactions/$hash"
  echo "GET $url"
  if ! curl -fsSL --max-time 30 "$url" -o "$out"; then
    echo "warn: fetch failed for $hash" >&2
    rm -f "$out"
    continue
  fi
  count=$((count + 1))
  sleep "$SLEEP_S"
done < "$SAMPLE_FILE"
echo "fetched $count new files → $OUT_DIR"
