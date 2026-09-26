# Backwards universe selection from missed arbs (WHI-999)

Dataset `arbs_44.jsonl` × census `census_44.json` vs the frozen 151-pool universe.

| Input | Value |
| --- | --- |
| Block range | 100651414 – 100902051 |
| Events classified | 44 |
| Source rows | 44 (0 dropped: no decodable path) |
| Universe fingerprint | `0x2adb7cd63e6ec10bb9974392797afa908b13e8549442e45d16f9f072d9e20e50` |
| Universe snapshot block | 98969898 |
| Settlement asset | `0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8` |
| Hop cap | 3 |
| TVL floor | 1000000000000000000000 WMNT wei |
| Candidate TVL | measured at block 100902051 |

## Verdict

Best loadable-venue universe reaches 35 of 44 in-scope arbs (79.5%), about 6 arbs/day over the observed window. Ignoring venue support entirely the ceiling is 44 (100.0%). Threshold for "non-trivial" was set at 25% of in-scope arbs: a reachable universe does exist.

> Count is not value. This report sizes pool coverage only: it does not price the arbs, net gas, or model race outcomes. A coverage result of any size can still be economically uninteresting, and that question is not settled here.

## Cause recount (reconciles with WHI-957)

| Cause | Count | Share of non-aggregator |
| --- | ---: | ---: |
| `not_in_universe` | 24 | 54.5% |
| `in_universe_in_scope` — WHI-957 `unattributable` | 20 | 45.5% |
| `out_of_scope_hop_cap` | 0 | 0.0% |
| `out_of_scope_non_wmnt_settlement` | 0 | 0.0% |
| `aggregator_misclass` (excluded from rates) | 0 | — |

## Residual bound

The 20 `unattributable` events all have every hop pool inside the frozen universe. This is a property of the cause ordering, not a measurement: WHI-957 tests `not_in_universe` at priority 6 and reaches the residual only at priority 11, so a residual event with a missing pool is impossible by construction (the zero below is a regression guard on that, not evidence for it). 54.5% is therefore a point estimate for the universe question, not a floor — the residual is unclassified only as to *which* in-universe cause applies (dirty-cycle vs unprofitable vs lost race), which needs a concurrent ledger (DI-35). Bounding it does not move the ranking. Every source row had a decodable path, so the residual has no undecoded remainder.

Residual events carrying a pool outside the universe: **0**.

## In-scope baseline

| Metric | Value |
| --- | ---: |
| In-scope arbs | 44 |
| Reachable on today's universe | 20 (45.5%) |
| Blocked by missing pools | 24 |
| Distinct pools in in-scope arbs | 38 |
| …held | 20 |
| …missing | 18 |

## Which filter actually excludes the missing pools

Evaluated in admission order — a pool blocked by its venue is never also blamed on TVL.

| Exclusion cause | Missing pools | In-scope arbs touched |
| --- | ---: | ---: |
| `admissible_but_absent` | 0 | 0 |
| `below_tvl_floor` | 10 | 21 |
| `cycle_filter_rejected` | 0 | 0 |
| `pool_tokens_unknown` | 0 | 0 |
| `tvl_not_measured` | 0 | 0 |
| `tvl_unavailable` | 0 | 0 |
| `venue_not_loadable` | 8 | 9 |

Arbs double-count across causes when one path has several kinds of gap.

| Venue status | Missing pools | Work required |
| --- | ---: | --- |
| `loadable_drop_in` | 10 | none — loads today |
| `quarantined_adapter_required` | 2 | tick-data batch adapter (WHI-938 quarantine reason) |
| `unregistered_v2_family_factory` | 3 | measured fee + v2_venues entry + differential and gas qualification |
| `unsupported_math_family` | 3 | new AMM adapter for the math family |

In-scope arbs whose **every** gap sits on a loadable venue: **15** — the ceiling reachable with no new adapter.

## Ranking — loadable venues only (actionable today)

| # | Pool | Venue | Pair | TVL (WMNT) | Marginal | Cumulative | Reachable | Selection | Venue status |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 | USDC/WMNT | 759 | +5 | 5 | 25 (56.8%) | `unlock` | `loadable_drop_in` |
| 2 | `0x3b4bcd4574ba95a474689223dfd6f5675b1438e0` | uniswap-v3 | WMNT/WBTC | 8 | +1 | 6 | 26 (59.1%) | `unlock` | `loadable_drop_in` |
| 3 | `0xbacd8c1591333ff6ec52610c72ae534a6e28c860` | fusionx-v2 | WBTC/mETH | 880 | +2 | 8 | 28 (63.6%) | `unlock` | `loadable_drop_in` |
| 4 | `0x32c1882baf179f6059d101aad3feac15b2b90da3` | moe-v1 | ENA/WMNT | 636 | +0 | 8 | 28 (63.6%) | `frequency_fallback` | `loadable_drop_in` |
| 5 | `0xff53524d0e01a00ecec997d2ebb5f06860068709` | moe-v1 | USDT/ENA | 555 | +5 | 13 | 33 (75.0%) | `unlock` | `loadable_drop_in` |
| 6 | `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | 589 | +1 | 14 | 34 (77.3%) | `unlock` | `loadable_drop_in` |
| 7 | `0x776d50283cd843f5e383ad6d5729db5ef4867848` | fusionx-v2 | WMNT/STG | 4 | +0 | 14 | 34 (77.3%) | `frequency_fallback` | `loadable_drop_in` |
| 8 | `0xbd51f73d81cff395172a40a4de4af7b6cc5da05b` | moe-v1 | WMNT/STG | 1 | +1 | 15 | 35 (79.5%) | `unlock` | `loadable_drop_in` |

`frequency_fallback` steps unlock nothing alone — they close one side of a multi-pool gap.

## Ranking — any venue (upper bound; needs adapters)

| # | Pool | Venue | Pair | TVL (WMNT) | Marginal | Cumulative | Reachable | Selection | Venue status |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 | USDC/WMNT | 759 | +5 | 5 | 25 (56.8%) | `unlock` | `loadable_drop_in` |
| 2 | `0x3b4bcd4574ba95a474689223dfd6f5675b1438e0` | uniswap-v3 | WMNT/WBTC | 8 | +1 | 6 | 26 (59.1%) | `unlock` | `loadable_drop_in` |
| 3 | `0xaede6c433bfb3fe714856d1b7b4b99690cc88d52` | unregistered 0x5c84…fd2f | WBTC/WETH | 2777 | +2 | 8 | 28 (63.6%) | `unlock` | `unregistered_v2_family_factory` |
| 4 | `0xbacd8c1591333ff6ec52610c72ae534a6e28c860` | fusionx-v2 | WBTC/mETH | 880 | +2 | 10 | 30 (68.2%) | `unlock` | `loadable_drop_in` |
| 5 | `0x94c400b9eb9d371299143d7b1af1202f0f956d73` | unregistered 0x5c84…fd2f | WMNT/WETH | 2094 | +1 | 11 | 31 (70.5%) | `unlock` | `unregistered_v2_family_factory` |
| 6 | `0xaaa87a36b92344436adcd880677e6842b227d931` | cleopatra-cl | USDC/WETH | 13656 | +1 | 12 | 32 (72.7%) | `unlock` | `quarantined_adapter_required` |
| 7 | `0xc6e63803544b96ea0470f9c55028db327e3cd9d9` | unregistered 0x5c84…fd2f | USDC/WMNT | 829 | +1 | 13 | 33 (75.0%) | `unlock` | `unregistered_v2_family_factory` |
| 8 | `0x32c1882baf179f6059d101aad3feac15b2b90da3` | moe-v1 | ENA/WMNT | 636 | +0 | 13 | 33 (75.0%) | `frequency_fallback` | `loadable_drop_in` |
| 9 | `0xff53524d0e01a00ecec997d2ebb5f06860068709` | moe-v1 | USDT/ENA | 555 | +5 | 18 | 38 (86.4%) | `unlock` | `loadable_drop_in` |
| 10 | `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | 589 | +1 | 19 | 39 (88.6%) | `unlock` | `loadable_drop_in` |
| 11 | `0xace7a42c030759ea903e9c39ad26a0f9b4a11927` | moe-v1 | PUFF/WMNT | 204 | +0 | 19 | 39 (88.6%) | `frequency_fallback` | `loadable_drop_in` |
| 12 | `0xbb2f514804ff60ee8a115993900bd369762ce548` | — | PUFF/WMNT | 11 | +2 | 21 | 41 (93.2%) | `unlock` | `unsupported_math_family` |
| 13 | `0x547ba2b1f5562fc72ca5f8c208724d751f1749ec` | cleopatra-cl | WMNT/aUSD | 93 | +0 | 21 | 41 (93.2%) | `frequency_fallback` | `quarantined_adapter_required` |
| 14 | `0xabaff1d3a706336570d4524ddff2b1e03e35542c` | — | USDC/aUSD | 1154 | +1 | 22 | 42 (95.5%) | `unlock` | `unsupported_math_family` |
| 15 | `0x776d50283cd843f5e383ad6d5729db5ef4867848` | fusionx-v2 | WMNT/STG | 4 | +0 | 22 | 42 (95.5%) | `frequency_fallback` | `loadable_drop_in` |
| 16 | `0xbd51f73d81cff395172a40a4de4af7b6cc5da05b` | moe-v1 | WMNT/STG | 1 | +1 | 23 | 43 (97.7%) | `unlock` | `loadable_drop_in` |
| 17 | `0x8fbad1a9ad1bafb6cc0bc7807ffa12115803e991` | v3fork-636ea2 | USDT/WMNT | 107 | +0 | 23 | 43 (97.7%) | `frequency_fallback` | `loadable_drop_in` |
| 18 | `0xa4657555cbddc069ed3389ac03330020692b13c4` | unregistered 0xc848…3913 | USDC/USDT | 9190 | +1 | 24 | 44 (100.0%) | `unlock` | `unsupported_math_family` |

`frequency_fallback` steps unlock nothing alone — they close one side of a multi-pool gap.

## Candidate sets and admission cost

| Restriction | Size | Added | Arbs unlocked | Reachable after | Pools | Cycles | Δcycles | Cycle × | Cold start | Adapter-blocked |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `loadable_only` | 5 | 5 | +13 | 33 (75.0%) | 156 | 9036 | +510 | 1.06× | 420 s | 0 |
| `loadable_only` | 10 | 8 | +15 | 35 (79.5%) | 159 | 9540 | +1014 | 1.12× | 428 s | 0 |
| `loadable_only` | 21 | 8 | +15 | 35 (79.5%) | 159 | 9540 | +1014 | 1.12× | 428 s | 0 |
| `any_venue` | 5 | 5 | +11 | 31 (70.5%) | 156 | 9628 | +1102 | 1.13× | 420 s | 2 |
| `any_venue` | 10 | 10 | +19 | 39 (88.6%) | 161 | 10870 | +2344 | 1.27× | 433 s | 4 |
| `any_venue` | 21 | 18 | +24 | 44 (100.0%) | 169 | 11708 | +3182 | 1.37× | 455 s | 8 |

Cold start is estimated linearly from WHI-936: 350 s at 130 pools (one pool per batch CREATE); cycle counts come from the production enumerator.

## Hop-cap pricing

| Hops | Arbs |
| ---: | ---: |
| **total above cap** | **0** |

| Metric | Value |
| --- | ---: |
| Arbs at exactly 4 hops | 0 |
| …already fully in universe | 0 |
| …with the largest loadable set added | 0 |
| Cycles at cap 3 | 8526 |
| Cycles at cap 4 | 180704 (21.2×) |
| Cold start impact | none (same pool set) |

Raising the cap from 3 to 4 admits only the 0 arbs at exactly 4 hops — the remaining 0 sit deeper still. Of those, 0 are already fully inside the universe, so that is what a cap change alone recovers. Cold start is unaffected (same pools); the cost is per-block: the cycle set grows 21.2× and every dirty-cycle pass re-optimizes it.

## Missing pools by appearance

| Pool | Venue | Pair | Kind | In-scope arbs | Sole blocker of | Hop positions | TVL (WMNT) | Exclusion |
| --- | --- | --- | --- | ---: | ---: | --- | ---: | --- |
| `0x32c1882baf179f6059d101aad3feac15b2b90da3` | moe-v1 | ENA/WMNT | v2 | 6 | 0 | 1:1 2:3 3:2 | 636 | `below_tvl_floor` |
| `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 | USDC/WMNT | v2 | 6 | 5 | 1:6 | 759 | `below_tvl_floor` |
| `0xff53524d0e01a00ecec997d2ebb5f06860068709` | moe-v1 | USDT/ENA | v2 | 6 | 0 | 1:3 2:3 | 555 | `below_tvl_floor` |
| `0x3b4bcd4574ba95a474689223dfd6f5675b1438e0` | uniswap-v3 | WMNT/WBTC | v3 | 5 | 1 | 2:2 3:3 | 8 | `below_tvl_floor` |
| `0xaede6c433bfb3fe714856d1b7b4b99690cc88d52` | unregistered 0x5c84…fd2f | WBTC/WETH | v2 | 3 | 1 | 1:2 2:1 | 2777 | `venue_not_loadable` |
| `0xaaa87a36b92344436adcd880677e6842b227d931` | cleopatra-cl | USDC/WETH | v3 | 2 | 0 | 2:2 | 13656 | `venue_not_loadable` |
| `0xace7a42c030759ea903e9c39ad26a0f9b4a11927` | moe-v1 | PUFF/WMNT | v2 | 2 | 0 | 1:1 2:1 | 204 | `below_tvl_floor` |
| `0xbacd8c1591333ff6ec52610c72ae534a6e28c860` | fusionx-v2 | WBTC/mETH | v2 | 2 | 0 | 1:2 | 880 | `below_tvl_floor` |
| `0xbb2f514804ff60ee8a115993900bd369762ce548` | — | PUFF/WMNT | solidly | 2 | 0 | 1:1 2:1 | 11 | `venue_not_loadable` |
| `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | v3 | 1 | 0 | 3:1 | 589 | `below_tvl_floor` |
| `0x547ba2b1f5562fc72ca5f8c208724d751f1749ec` | cleopatra-cl | WMNT/aUSD | v3 | 1 | 0 | 2:1 | 93 | `venue_not_loadable` |
| `0x776d50283cd843f5e383ad6d5729db5ef4867848` | fusionx-v2 | WMNT/STG | v2 | 1 | 0 | 1:1 | 4 | `below_tvl_floor` |
| `0x8fbad1a9ad1bafb6cc0bc7807ffa12115803e991` | v3fork-636ea2 | USDT/WMNT | v3 | 1 | 0 | 1:1 | 107 | `below_tvl_floor` |
| `0x94c400b9eb9d371299143d7b1af1202f0f956d73` | unregistered 0x5c84…fd2f | WMNT/WETH | v2 | 1 | 0 | 1:1 | 2094 | `venue_not_loadable` |
| `0xa4657555cbddc069ed3389ac03330020692b13c4` | unregistered 0xc848…3913 | USDC/USDT | algebra | 1 | 0 | 2:1 | 9190 | `venue_not_loadable` |
| `0xabaff1d3a706336570d4524ddff2b1e03e35542c` | — | USDC/aUSD | solidly | 1 | 0 | 1:1 | 1154 | `venue_not_loadable` |
| `0xbd51f73d81cff395172a40a4de4af7b6cc5da05b` | moe-v1 | WMNT/STG | v2 | 1 | 0 | 2:1 | 1 | `below_tvl_floor` |
| `0xc6e63803544b96ea0470f9c55028db327e3cd9d9` | unregistered 0x5c84…fd2f | USDC/WMNT | v2 | 1 | 0 | 1:1 | 829 | `venue_not_loadable` |

Hop positions read `position:count` over the ordered path — a pool that only ever appears at hop 1 is a different kind of gap from a mid-cycle one.

## Notes

* Scope rules mirror execution::peer_attribution (aggregator hop>50 → hop cap → non-WMNT settlement), so cause counts reconcile with WHI-957.
* "Arbs unlocked" counts only in-scope arbs. WHI-906's fully_executable includes arbs the strategy cannot take (hop>3, non-WMNT), so it reads higher.
* Cycle counts come from the production enumerator (arbitrage::pathfinder), not a re-implementation. Cold start is modelled linearly from WHI-936: 350 s at 130 pools (one pool per batch CREATE).
* Per-event rows are never written to the committed report; the arb dataset stays external (WHI-906 / WHI-956 contract).
