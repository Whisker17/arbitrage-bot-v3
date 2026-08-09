# Ground-truth collector report (WHI-956)

- Schema: `whisker-arb/ground-truth-collector/v1`
- Block range: `100`–`200`
- Input: `candidates.jsonl`
- Candidates seen: 6
- Accepted: 2
- Distinct bots: 1
- Events fingerprint: `0x26f45428f59f14347ae07fc9475b94a8e8bb32fdf99f6a902fcfe7f7564d4ebd`
- Heuristic: `single_tx AND swap_events>=2 AND entity_net_positive_ge1 AND entity_net_negative_eq0 AND entity_gross_out AND msg_value_wei<=1e18 AND NOT liquidation AND NOT jit_lp AND NOT sandwich`

## Exclusion counts

| Category | Count |
| --- | ---: |
| cex_dex | 1 |
| liquidation | 1 |
| jit_lp | 1 |
| sandwich | 1 |
| insufficient_swaps | 0 |
| not_closed_cycle | 0 |
| no_gross_out | 0 |
| out_of_range | 0 |
| missing_fields | 0 |
| **total excluded** | 4 |

## Hop-count distribution

- `2`: 1
- `3`: 1

## Funding distribution

- `flash_loan`: 1
- `self_funded`: 1

## Settlement-asset distribution (top)

- `0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8`: 2

## Venue (factory) distribution (top)

- `unknown`: 2

## Notes

- Fixture-only run demonstrating exclusion category counters (cex_dex, liquidation, jit_lp, sandwich).
