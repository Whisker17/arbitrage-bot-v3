# Mantle arb-bot ground-truth collector (WHI-956)

Supplies the **re-runnable collector** and **aggregate reports** that the
WHI-715 known-bot comparator never received. Real event datasets stay
**external** (same contract as WHI-906 / `arb_coverage`).

## What shipped

| Path | Role |
| --- | --- |
| `src/service/ground_truth.rs` | Offline collect / exclude / fingerprint / verification |
| `src/bin/ground_truth_collector.rs` | CLI: `collect`, `sample`, `verify-sample` |
| `scripts/ground_truth/dune_atomic_arbs.sql` | Dune discovery query (parameterised block range) |
| `scripts/ground_truth/fetch_blockscout_sample.sh` | Optional Blockscout API v2 fetch |
| `tests/fixtures/ground_truth/` | Synthetic candidates + labels |
| `evidence/ground-truth/baseline_report.{json,md}` | 30-day historical baseline (aggregates only) |
| `evidence/ground-truth/fixture_exclusion_report.{json,md}` | Exclusion-counter demo on fixtures |

## Explicit heuristic

```
single_tx AND swap_events>=2 AND pos_nonempty
AND entity_net_negative_eq0 AND msg_value_wei<=1e18
AND NOT (gross_out=false) AND NOT liquidation AND NOT jit_lp AND NOT sandwich
```

Embedded in every report as `heuristic` (`ACCEPTANCE_HEURISTIC`).

The collector **requires** a non-empty `pos` (net-positive entity leg) — Dune
exports that only list multi-swap txs without transfer nets are excluded as
`not_closed_cycle` until settlement/pos is filled. `gross_out` fails closed
only when explicitly `false`.

## Misclassification exclusions

Counted per category (never silent): `cex_dex`, `liquidation`, `jit_lp`,
`sandwich`, `insufficient_swaps`, `not_closed_cycle`, `no_gross_out`,
`out_of_range`, `missing_fields`.

See `fixture_exclusion_report.json` for a synthetic pass that exercises
cex-dex / liquidation / JIT / sandwich counters.

## Reproduce the baseline (external inputs)

```bash
# 30-day WHI-906 arbs + census (paths stay outside this repo)
cargo run --release --bin ground_truth_collector -- collect \
  --input <external>/arbs_month.jsonl \
  --from-block 96806569 --to-block 98098684 \
  --census <external>/pool_census.json \
  --known-bots-out <external>/known_bots.json \
  --events-out <external>/ground_truth_events.jsonl \
  --report-out evidence/ground-truth/baseline_report.json \
  --md-out evidence/ground-truth/baseline_report.md
```

Identical inputs + block range → identical `events_fingerprint`.

### Concurrent shadow window (WHI-957)

Re-run with the **same** `[from_block, to_block]` as the signerless shadow
ledger. Historical dumps size the market; only concurrent collection feeds
the comparator buckets.

```bash
# Dune export path
# 1) Run scripts/ground_truth/dune_atomic_arbs.sql with the shadow window
# 2) Export CSV → collect
cargo run --release --bin ground_truth_collector -- collect \
  --input /path/to/dune_export.csv \
  --from-block <shadow_from> --to-block <shadow_to> \
  --census <external>/pool_census.json \
  --known-bots-out <external>/known_bots_shadow.json \
  --events-out <external>/events_shadow.jsonl \
  --report-out evidence/ground-truth/shadow_window_report.json
```

## Verification sample

```bash
cargo run --release --bin ground_truth_collector -- sample \
  --events <external>/ground_truth_events.jsonl \
  --sample-size 40 \
  --out /tmp/sample_hashes.txt

# Prefer Blockscout when healthy:
scripts/ground_truth/fetch_blockscout_sample.sh /tmp/sample_hashes.txt /tmp/blockscout

# During the baseline run, explorer.mantle.xyz returned HTTP 502. Verification
# used eth_getTransactionReceipt on Mantle public RPC with the same structural
# heuristic (status success, ≥2 swap-family topics, msg.value ≤ 1 MNT, no
# liquidation). Closed-cycle was confirmed by joining the sample back to the
# source arbs_month `pos` legs (non-empty net-positive entity deltas).
# Labels → verify-sample:
cargo run --release --bin ground_truth_collector -- verify-sample \
  --report-in evidence/ground-truth/baseline_report.json \
  --labels /tmp/verification_labels.json \
  --report-out evidence/ground-truth/baseline_report.json \
  --md-out evidence/ground-truth/baseline_report.md
```

**Precision caveats.** Structural RPC checks do not re-simulate profit or re-derive
token nets. The baseline sample cross-checks (1) RPC structure and (2) source
`pos` non-empty. Operator review of Blockscout token-transfer pages remains the
preferred human path when the explorer is healthy.

## Baseline headline (committed report)

| Metric | Value |
| --- | ---: |
| Block range | 96,806,569 – 98,098,684 |
| Accepted arbs | **10,501** |
| Distinct bot addresses | **68** |
| Events fingerprint | `0x19f9b9df…30bcaa` |
| Hop mode | 3-hop (6,768), 2-hop (3,047) |
| Funding | 100% `self_funded` (no flash-loan markers on this extract) |
| Sample precision | **40 / 40 = 100%** (RPC receipt check) |

Full hop / venue / settlement distributions: `baseline_report.json`.

## Feed WHI-715 comparator

```bash
cargo run --locked --example shadow_bot_benchmark -- compare \
  --ledger path/to/shadow_ledger.jsonl \
  --known-bots <external>/known_bots.json \
  --json-out evidence/shadow/.../benchmark_report.json \
  --md-out evidence/shadow/.../benchmark_report.md
```

`KnownBotEvent` now optionally carries `hop_count`, `funding`, `venues`,
`settlement_asset` for WHI-957 out-of-scope attribution; the comparator
matcher ignores them.
