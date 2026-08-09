# Peer attribution report (WHI-957)

- Schema: `whisker-arb/peer-attribution/v1`
- Block range: Some(96806569)–Some(98098684)
- Events: 10501
- Attributed: 10501 (must equal events)
- Analysis denominator (excl. aggregator_misclass): 10493
- Reachable-miss rate (five-core / denom): **58.51%**
- Out-of-scope rate: **7.81%**
- **dirty_cycle_filter_skipped == 0?** **true** (count=0)
- **dirty_cycle_evidence:** `not_measured` (only `measured_zero` answers WHI-940)

## Five core causes

| Cause | Count |
| --- | ---: |
| `not_in_universe` | 6139 |
| `dirty_cycle_filter_skipped` | 0 |
| `evaluated_but_unprofitable` | 0 |
| `profitable_but_not_attempted` | 0 |
| `attempted_and_lost_race` | 0 |

## Separators (not mixed into the five)

| Cause | Count |
| --- | ---: |
| `block_skipped` | 0 |
| `aggregator_misclass` | 8 |
| `out_of_scope_flash_loan` | 0 |
| `out_of_scope_hop_cap` | 678 |
| `out_of_scope_non_wmnt_settlement` | 141 |
| `out_of_scope_adapter_required` | 0 |
| `unattributable` | 3535 |

## Classification rules

- aggregator_misclass: hop_count > 50
- out_of_scope_flash_loan: funding=flash_loan
- out_of_scope_hop_cap: hop_count > 3
- out_of_scope_non_wmnt_settlement: settlement_asset set and ≠ WMNT
- out_of_scope_adapter_required: pool factory in adapter set
- not_in_universe: any ordered_pool ∉ universe CSV
- block_skipped: block_views.skipped or absent observation (policy)
- attempted_and_lost_race: ledger profitable Pass on matching route
- profitable_but_not_attempted: ledger outcome eligibility/budget
- evaluated_but_unprofitable: ledger non-Pass candidate, or dirty touched path with no candidate
- dirty_cycle_filter_skipped: all pools in universe, hop≤max, not skipped, dirty∩path=∅, no candidate
- unattributable: residual with detail (e.g. no concurrent dirty view)

## Notes

- Cause priority: aggregator_misclass → oos(flash/hop/wmnt/adapter) → not_in_universe → block_skipped → ledger outcomes → dirty_cycle_filter_skipped → evaluated_but_unprofitable → unattributable.
- dirty_cycle_filter_skipped requires concurrent block_views with dirty_pools; never inferred from 'no candidate' alone.
- block_skipped is counted separately from the five core causes (WHI-977).
- aggregator_misclass: hop_count > 50 excluded from rate denominators.
- unattributable=3535 — residual explicit; not silently dropped
- dirty_cycle_evidence=not_measured: no --block-views sidecar; count 0 is not a WHI-940 pass
- per-event rows stripped from committed report (re-run with external events JSONL to regenerate)
