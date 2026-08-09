# Peer attribution (WHI-957)

Attribute **every** WHI-956 ground-truth arb to exactly one cause so a
`candidates = 0` shadow window is interpretable.

## Do not confuse with WHI-956

WHI-956 built the ground-truth **collector** and baseline aggregates under
`evidence/ground-truth/`. This issue **consumes** that output; it does not
modify the collector.

## Cause taxonomy (Linear comment on WHI-957)

| Cause | Meaning |
| --- | --- |
| `not_in_universe` | Some hop pool ∉ frozen `data/pool_universe.csv` |
| `dirty_cycle_filter_skipped` | Path over universe pools; dirty set missed every hop (WHI-940) |
| `evaluated_but_unprofitable` | Candidate / dirty-touch optimize path without profitable Pass |
| `profitable_but_not_attempted` | Eligibility / attempt budget blocked |
| `attempted_and_lost_race` | We had a profitable Pass; peer still landed |

**Separators (never mixed into the five):**

| Cause | Meaning |
| --- | --- |
| `block_skipped` | Head not processed (WHI-977) |
| `aggregator_misclass` | `hop_count > 50` (0.08% of 30d set) |
| `out_of_scope_*` | flash-loan / hop>3 / non-WMNT / adapter |
| `unattributable` | Residual with explicit detail (never silent drop) |

## Tool

```bash
cargo run --release --bin peer_attribution -- \
  --events <external>/ground_truth_events.jsonl \
  --universe data/pool_universe.csv \
  [--ledger evidence/shadow/.../ledger.jsonl ...] \
  [--block-views <external>/block_discovery.jsonl] \
  --summary-only \
  --json-out evidence/peer-attribution/report.json \
  --md-out evidence/peer-attribution/report.md
```

`block_discovery.jsonl` one object per processed/skipped head:

```json
{"block_number":123,"skipped":false,"dirty_pools":["0x…"],"scope":"touched","cycles_optimized":12,"cycles_total":7398}
```

Without this sidecar, **dirty-cycle cannot be measured** — count 0 means
`dirty_cycle_evidence=not_measured`, not a WHI-940 pass.

## Offline universe pass (committed)

`offline_universe.{json,md}` — WHI-956 30d arbs (10,501) vs current 130-pool
universe. **No concurrent ledger** (existing shadow ledgers sit at ~98.95M while
GT spans 96.81M–98.10M).

| Cause | Count |
| ---: | ---: |
| not_in_universe | 6139 |
| out_of_scope_hop_cap | 678 |
| out_of_scope_non_wmnt_settlement | 141 |
| aggregator_misclass | 8 |
| unattributable (needs concurrent ledger) | 3535 |
| dirty_cycle_filter_skipped | 0 (**not_measured**) |

Headline:

* **58.5%** of non-aggregator events have at least one pool outside the 130-pool
  universe (coverage, not a hot-path bug).
* **Hop ≤ 3 is not the bottleneck** for the in-universe remainder (operator brief).
* **dirty_cycle_filter_skipped:** count **0** with evidence **`not_measured`**.
  A true WHI-940 answer requires a concurrent shadow window + `block_views`.

## Concurrent window recipe (operator)

1. Start signerless `--watch` (post WHI-980 / WHI-977 binary) writing ledger + a
   `block_discovery.jsonl` (dirty pools / skipped per head).
2. Concurrently collect ground-truth for the **same** `[from,to]` via
   `ground_truth_collector` (WHI-956).
3. Run `peer_attribution` with `--ledger` + `--block-views`.
4. Replace this offline report; require
   `dirty_cycle_evidence=measured_zero|measured_nonzero`.

See **DI-35** in `docs/DEFERRED_ISSUES.md` for the open operator evidence item.

## Synthetic correctness (unit tests)

`cargo test --lib peer_attribution` proves:

* dirty-cycle **≠** evaluated_but_unprofitable when dirty touches path
* `block_skipped` is outside the five
* every event gets exactly one cause
