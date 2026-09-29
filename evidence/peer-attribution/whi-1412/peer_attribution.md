# Peer attribution report (WHI-957)

- Schema: `whisker-arb/peer-attribution/v1`
- Block range: Some(101211744)–Some(101245872)
- Events: 252
- Attributed: 252 (must equal events)
- Analysis denominator (excl. aggregator_misclass): 252
- Reachable-miss rate (five-core / denom): **68.65%**
- Out-of-scope rate: **28.57%**
- **dirty_cycle_filter_skipped == 0?** **false** (count=123)
- **dirty_cycle_evidence:** `measured_nonzero` (only `measured_zero` answers WHI-940)

## Five core causes

| Cause | Count |
| --- | ---: |
| `not_in_universe` | 39 |
| `dirty_cycle_filter_skipped` | 123 |
| `evaluated_but_unprofitable` | 5 |
| `profitable_but_not_attempted` | 6 |
| `attempted_and_lost_race` | 0 |

## Separators (not mixed into the five)

| Cause | Count |
| --- | ---: |
| `block_skipped` | 0 |
| `aggregator_misclass` | 0 |
| `out_of_scope_flash_loan` | 0 |
| `out_of_scope_hop_cap` | 46 |
| `out_of_scope_non_wmnt_settlement` | 26 |
| `out_of_scope_adapter_required` | 0 |
| `unattributable` | 7 |

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
- unattributable=7 — residual explicit; not silently dropped
- dirty_cycle_evidence=measured_nonzero: dirty_cycle_filter_skipped=123
- pre_state: ledger evidence keyed by observed block + 1 (event at N judged against post-(N-1) state); WHI-715 bucket cross-check skipped
- per-event rows stripped from committed report (re-run with external events JSONL to regenerate)

## Six-way breakdown (WHI-1412)

- Gas profile: `0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412`; pre_state: true
- Events: 252; out of strategy scope: 72; in-scope denominator: **180**

| Cause | Count | Share of in-scope |
| --- | ---: | ---: |
| `absent_pool` | 39 | 21.67% |
| `route_class_unknown` | 0 | 0.00% |
| `route_class_unapproved` | 135 | 75.00% |
| `evaluated_and_unprofitable` | 0 | 0.00% |
| `profitable_but_not_attempted` | 6 | 3.33% |
| `attempted_and_lost_race` | 0 | 0.00% |
| residual `block_skipped` | 0 | |
| residual `dirty_cycle_filter_skipped` | 0 | |
| residual `unattributable` | 0 | |
| out of scope `out_of_scope_hop_cap` | 46 | |
| out of scope `out_of_scope_non_wmnt_settlement` | 26 | |
