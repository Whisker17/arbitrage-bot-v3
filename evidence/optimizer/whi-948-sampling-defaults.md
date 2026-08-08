# WHI-948 optimal-input sampling defaults

Declared sampling / refinement budget for multi-peak net-PnL search
(`OptimizationConfig::default` in `src/arbitrage/optimizer.rs`).

| Knob | Default | Role |
| --- | ---: | --- |
| `coarse_samples` | 24 | Log-scale coarse samples on `[1, max_input]` (endpoints always included) |
| `max_iterations` | 12 | Local ternary-refine steps per candidate interval |
| `tolerance_bps` | 5 | Stop local refine when `(high−low)·10000 ≤ mid·tolerance_bps` |
| `max_quotes` | 96 | Hard per-path quote cap (coarse + refine + endpoints) |
| `max_input` | `10^24` | Feasible-domain upper bound default (production uses discovery cap / G-3) |

Objective: `score(input) = gross_quote(input) − fee_cost(input)` with checked
subtraction (`net_score`). Fee cost is injected via `FeeCostModel`; production
uses `ZeroFeeCost` until G-2 (WHI-949) wires `fee_plan_cost(route_key(input),
fee_context)` at every sample.

`min_profit` is **not** consumed by the optimizer. Admission floor
(`DiscoveryConfig::min_profit` = bot `min_net_profit`) applies at candidate
materialization only.

## Synthetic corpus regret (unit test)

From `replay_regret_report_on_synthetic_corpus` (domain `[1,100]`, zero fee):

| shape | oracle_net | found_net | regret | quotes |
| --- | ---: | ---: | ---: | ---: |
| multi_peak | 100 | 100 | 0 | ~70 |
| right_edge | 100 | 100 | 0 | ~58 |
| large_input | 50 | 50 | 0 | ~56 |

Not a claim of global optimality on live Mantle state — only regret vs
brute-force oracle on the enumerable synthetic fixtures.
