# Backwards universe selection from missed arbs (WHI-999)

Dataset `arbs_month.jsonl` × census `pool_census.json` vs the frozen 130-pool universe.

| Input | Value |
| --- | --- |
| Block range | 96806569 – 98098684 |
| Events classified | 10501 |
| Source rows | 10501 (0 dropped: no decodable path) |
| Universe fingerprint | `0x0ecceac8400b64061029a8417f14330fd232efda0aed6db088292bc97df55a46` |
| Universe snapshot block | 98969898 |
| Settlement asset | `0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8` |
| Hop cap | 3 |
| TVL floor | 1000000000000000000000 WMNT wei |
| Candidate TVL | measured at block 98098684 |

## Verdict

Best loadable-venue universe reaches 5382 of 9674 in-scope arbs (55.6%), about 180 arbs/day over the observed window. Ignoring venue support entirely the ceiling is 8106 (83.8%). Threshold for "non-trivial" was set at 25% of in-scope arbs: a reachable universe does exist.

> Count is not value. Against a chain-wide atomic-arb gross of about $15/day across all bots, reaching 55.6% of in-scope arbs implies roughly $8.35/day gross before gas, and only if every reachable arb were also won. That is the number a funding decision turns on — this report sizes coverage, not profit.

## Cause recount (reconciles with WHI-957)

| Cause | Count | Share of non-aggregator |
| --- | ---: | ---: |
| `not_in_universe` | 6139 | 58.5% |
| `in_universe_in_scope` — WHI-957 `unattributable` | 3535 | 33.7% |
| `out_of_scope_hop_cap` | 678 | 6.5% |
| `out_of_scope_non_wmnt_settlement` | 141 | 1.3% |
| `aggregator_misclass` (excluded from rates) | 8 | — |

## Residual bound

The 3535 `unattributable` events all have every hop pool inside the frozen universe. This is a property of the cause ordering, not a measurement: WHI-957 tests `not_in_universe` at priority 6 and reaches the residual only at priority 11, so a residual event with a missing pool is impossible by construction (the zero below is a regression guard on that, not evidence for it). 58.5% is therefore a point estimate for the universe question, not a floor — the residual is unclassified only as to *which* in-universe cause applies (dirty-cycle vs unprofitable vs lost race), which needs a concurrent ledger (DI-35). Bounding it does not move the ranking. Every source row had a decodable path, so the residual has no undecoded remainder.

Residual events carrying a pool outside the universe: **0**.

## In-scope baseline

| Metric | Value |
| --- | ---: |
| In-scope arbs | 9674 |
| Reachable on today's universe | 3535 (36.5%) |
| Blocked by missing pools | 6139 |
| Distinct pools in in-scope arbs | 361 |
| …held | 94 |
| …missing | 267 |

## Which filter actually excludes the missing pools

Evaluated in admission order — a pool blocked by its venue is never also blamed on TVL.

| Exclusion cause | Missing pools | In-scope arbs touched |
| --- | ---: | ---: |
| `admissible_but_absent` | 3 | 558 |
| `below_tvl_floor` | 71 | 2092 |
| `cycle_filter_rejected` | 17 | 180 |
| `venue_not_loadable` | 176 | 4260 |

Arbs double-count across causes when one path has several kinds of gap.

| Venue status | Missing pools | Work required |
| --- | ---: | --- |
| `loadable_drop_in` | 91 | none — loads today |
| `quarantined_adapter_required` | 15 | tick-data batch adapter (WHI-938 quarantine reason) |
| `unregistered_v2_family_factory` | 99 | per-venue V2 fees before enumeration (V2_FEE is hard-coded) |
| `unregistered_v3_family_factory` | 29 | v3_venues registry entry + batch validation |
| `unsupported_math_family` | 33 | new AMM adapter for the math family |

In-scope arbs whose **every** gap sits on a loadable venue: **1879** — the ceiling reachable with no new adapter.

## Ranking — loadable venues only (actionable today)

| # | Pool | Venue | Pair | TVL (WMNT) | Marginal | Cumulative | Reachable | Selection | Venue status |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `0x361052be2085dfddfc7f15d9f4c901d05d086f05` | fluxion-v3 | USD1/USDT0 | 220886 | +260 | 260 | 3795 (39.2%) | `unlock` | `loadable_drop_in` |
| 2 | `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 (interim) | USDC/WMNT | 898 | +159 | 419 | 3954 (40.9%) | `unlock` | `loadable_drop_in` |
| 3 | `0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5` | agni-v3 | USDe/WMNT | 45796 | +122 | 541 | 4076 (42.1%) | `unlock` | `loadable_drop_in` |
| 4 | `0xb2810371d52e911f522314b760b7ff91d37d7ea3` | butter | USDC/WMNT | 775 | +102 | 643 | 4178 (43.2%) | `unlock` | `loadable_drop_in` |
| 5 | `0xed3ee32bdcf51632707f130af827fd849929570e` | fusionx-v3 | USDT/WMNT | 677 | +97 | 740 | 4275 (44.2%) | `unlock` | `loadable_drop_in` |
| 6 | `0x4247f6c7409832adc101d7df714f96fecb327c92` | butter | USDT/WMNT | 912 | +89 | 829 | 4364 (45.1%) | `unlock` | `loadable_drop_in` |
| 7 | `0x47453cb250f705211e7a0de2f9c5d94cfecc8abd` | fusionx-v3 | WMNT/WETH | 443 | +92 | 921 | 4456 (46.1%) | `unlock` | `loadable_drop_in` |
| 8 | `0xf231e1ca10bae443f176f907352ac575a056c843` | fusionx-v3 | USDC/WMNT | 531 | +81 | 1002 | 4537 (46.9%) | `unlock` | `loadable_drop_in` |
| 9 | `0xaaecd138ad9cd20c13f1593a41bf3941940ec41e` | fusionx-v3 | USDC/WMNT | 295 | +67 | 1069 | 4604 (47.6%) | `unlock` | `loadable_drop_in` |
| 10 | `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | 722 | +64 | 1133 | 4668 (48.3%) | `unlock` | `loadable_drop_in` |
| 11 | `0x29234f4aa0842c4b27bf3ebfec3e7a25c85cc133` | butter | USDC/WMNT | 563 | +61 | 1194 | 4729 (48.9%) | `unlock` | `loadable_drop_in` |
| 12 | `0x585ec64f06afa80e474bb6574ef7be38a8ef94a7` | fusionx-v2 (interim) | WMNT/WETH | 322 | +52 | 1246 | 4781 (49.4%) | `unlock` | `loadable_drop_in` |
| 13 | `0x8fbad1a9ad1bafb6cc0bc7807ffa12115803e991` | v3fork-636ea2 | USDT/WMNT | 323 | +51 | 1297 | 4832 (49.9%) | `unlock` | `loadable_drop_in` |
| 14 | `0x928981fe5a4c005a126662d2bd84fbf139b51876` | agni-v3 | WMNT/WETH | 124 | +51 | 1348 | 4883 (50.5%) | `unlock` | `loadable_drop_in` |
| 15 | `0x7bd7f23b34cb2cd0c42e7ed409872547783567b3` | v3fork-636ea2 | USDC/WMNT | 175 | +43 | 1391 | 4926 (50.9%) | `unlock` | `loadable_drop_in` |
| 16 | `0xe2cb2455dd51f5edd76b8e74a758c94109dc7ea4` | v3fork-636ea2 | USDC/WMNT | 231 | +40 | 1431 | 4966 (51.3%) | `unlock` | `loadable_drop_in` |
| 17 | `0xd114e1fdf9e4129b863a6af53806ae0f8c54ce88` | butter | WMNT/mETH | 32 | +37 | 1468 | 5003 (51.7%) | `unlock` | `loadable_drop_in` |
| 18 | `0x328cfb13c66065240b58bff0ae1ce798fe314ec1` | agni-v3 | USDT/WMNT | 622 | +35 | 1503 | 5038 (52.1%) | `unlock` | `loadable_drop_in` |
| 19 | `0x306ab51fd73afbafbfc096380c36211f8a12a532` | v3fork-636ea2 | USDC/WETH | 5697 | +31 | 1534 | 5069 (52.4%) | `unlock` | `loadable_drop_in` |
| 20 | `0x9d5d4064a808ba865957b1d04b20a84175dcc16d` | agni-v3 | WMNT/WETH | 672 | +28 | 1562 | 5097 (52.7%) | `unlock` | `loadable_drop_in` |
| 21 | `0x3e7c1421430acfa556dc1df339b226d7a9149391` | v3fork-636ea2 | USDT/WMNT | 94 | +23 | 1585 | 5120 (52.9%) | `unlock` | `loadable_drop_in` |
| 22 | `0xa97d73a9e9df96492e9e64f406a82de1541649b9` | butter | USDT/WMNT | 334 | +21 | 1606 | 5141 (53.1%) | `unlock` | `loadable_drop_in` |
| 23 | `0x4b96994181cb694f506bdf24a218fe7af64147cb` | agni-v3 | WMNT/mETH | 682 | +19 | 1625 | 5160 (53.3%) | `unlock` | `loadable_drop_in` |
| 24 | `0x60a9c1bd78f408df2c1d86a6941c892b37ad67e7` | v3fork-636ea2 | WMNT/WETH | 182 | +16 | 1641 | 5176 (53.5%) | `unlock` | `loadable_drop_in` |
| 25 | `0x692903acc9f3acb4e2545a37ff620f35a63976f1` | butter | WMNT/WETH | 140 | +12 | 1653 | 5188 (53.6%) | `unlock` | `loadable_drop_in` |
| 26 | `0x6c7604c157507a0aaea90f2928ca44cc1d60cd81` | fusionx-v3 | WMNT/WETH | 75 | +12 | 1665 | 5200 (53.8%) | `unlock` | `loadable_drop_in` |
| 27 | `0xd145db1dfc3fcd2e999b47f3a02c85bd7750ed09` | agni-v3 | USDT/WMNT | 488 | +11 | 1676 | 5211 (53.9%) | `unlock` | `loadable_drop_in` |
| 28 | `0x7991be74b74ea7528bfbca292926e3b05526f042` | v3fork-636ea2 | USDT/WMNT | 35 | +10 | 1686 | 5221 (54.0%) | `unlock` | `loadable_drop_in` |
| 29 | `0xe1dc93d69439a924baaeaf9e64f4ae7be0af738a` | agni-v3 | USDC/WETH | 453 | +10 | 1696 | 5231 (54.1%) | `unlock` | `loadable_drop_in` |
| 30 | `0x43294e35dcdba29615739526424f3f89bf407c09` | butter | WMNT/WETH | 66 | +9 | 1705 | 5240 (54.2%) | `unlock` | `loadable_drop_in` |
| 31 | `0xfc60a4d05ac8c93f62276e046ad5a098f5c7820a` | uniswap-v3 | WMNT/WETH | 10 | +10 | 1715 | 5250 (54.3%) | `unlock` | `loadable_drop_in` |
| 32 | `0xc203e8eec73624976ae7d9425fa777ebfe337ccc` | v3fork-636ea2 | USDC/WMNT | 32 | +7 | 1722 | 5257 (54.3%) | `unlock` | `loadable_drop_in` |
| 33 | `0x813d7f24df644550f824141bcdd8cb1cb0642d06` | agni-v3 | WMNT/mETH | 19 | +5 | 1727 | 5262 (54.4%) | `unlock` | `loadable_drop_in` |
| 34 | `0x8b4d24365d08055fd4220ef87492020ac54d0128` | agni-v3 | WMNT/cmETH | 15 | +5 | 1732 | 5267 (54.4%) | `unlock` | `loadable_drop_in` |
| 35 | `0x064d4c6e06711eaff5a9e2a19e750ee8b94159ab` | fusionx-v3 | USDC/WMNT | 144 | +3 | 1735 | 5270 (54.5%) | `unlock` | `loadable_drop_in` |
| 36 | `0x8be9c0a3e81f63cc0592302367fc673e6840fa55` | agni-v3 | WMNT/cmETH | 8 | +3 | 1738 | 5273 (54.5%) | `unlock` | `loadable_drop_in` |
| 37 | `0xd372cd4acfcd646f9332b26c7b6bfa4777d90451` | agni-v3 | USDT/WETH | 928 | +2 | 1740 | 5275 (54.5%) | `unlock` | `loadable_drop_in` |
| 38 | `0x46e15789bd1eeb975551ea12f3eb74ae9409eb99` | agni-v3 | USDT/WETH | 260 | +1 | 1741 | 5276 (54.5%) | `unlock` | `loadable_drop_in` |
| 39 | `0xb1c1df816ced51503622ec83c4c971247048eb9f` | fluxion-v3 | USDT/WMNT | 3 | +1 | 1742 | 5277 (54.5%) | `unlock` | `loadable_drop_in` |
| 40 | `0xe0d80d6377aadcb0a648cc157f593c60390385e7` | butter | USDY/WMNT | 297 | +0 | 1742 | 5277 (54.5%) | `frequency_fallback` | `loadable_drop_in` |
| 41 | `0xe38e3a804ef845e36f277d86fb2b24b8c32b3340` | agni-v3 | USDT/USDY | 58338 | +39 | 1781 | 5316 (55.0%) | `unlock` | `loadable_drop_in` |
| 42 | `0x9cd55b03c64b65ba02a1d985caef63046b2d54eb` | agni-v3 | USDC/USDY | 676411 | +19 | 1800 | 5335 (55.1%) | `unlock` | `loadable_drop_in` |
| 43 | `0xa81ede3710ea5249fdc1a81bb5664d004300ddb7` | butter | USDC/USDY | 74537 | +12 | 1812 | 5347 (55.3%) | `unlock` | `loadable_drop_in` |
| 44 | `0xe92b806c34c8beea03d322942d9f271c91028f5f` | butter | USDY/WMNT | 31 | +10 | 1822 | 5357 (55.4%) | `unlock` | `loadable_drop_in` |
| 45 | `0x04a972c3bd540d286be48e8dd61565de4b8a274d` | butter | USDY/WMNT | 26 | +9 | 1831 | 5366 (55.5%) | `unlock` | `loadable_drop_in` |
| 46 | `0x214b8d4a67a996643cdb1bd80423a5f638cf258d` | butter | USDT/USDY | 2313 | +6 | 1837 | 5372 (55.5%) | `unlock` | `loadable_drop_in` |
| 47 | `0x263fd2e2715386e6feca3f9d6de2ad94819b501f` | agni-v3 | USDY/WETH | 12579 | +3 | 1840 | 5375 (55.6%) | `unlock` | `loadable_drop_in` |
| 48 | `0x2afae423fe3ca40e7b24b48efd02e3e26d969395` | agni-v3 | USDY/mETH | 8750 | +3 | 1843 | 5378 (55.6%) | `unlock` | `loadable_drop_in` |
| 49 | `0xcb21dd38f1e7e0b06fabb01ae9bd849cd36e8296` | agni-v3 | USDY/WMNT | 23 | +3 | 1846 | 5381 (55.6%) | `unlock` | `loadable_drop_in` |
| 50 | `0xd837008202a9715b95e629d281104354f961a3ec` | butter | USDC/USDY | 8775 | +1 | 1847 | 5382 (55.6%) | `unlock` | `loadable_drop_in` |

`frequency_fallback` steps unlock nothing alone — they close one side of a multi-pool gap.

## Ranking — any venue (upper bound; needs adapters)

| # | Pool | Venue | Pair | TVL (WMNT) | Marginal | Cumulative | Reachable | Selection | Venue status |
| ---: | --- | --- | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | `0x98d1e99d294e8603fa050ea129c78388408e0dd1` | unregistered 0x45e5…c218 | — | — | +365 | 365 | 3900 (40.3%) | `unlock` | `unsupported_math_family` |
| 2 | `0x1da0925773e15359c8b87272146e86444eb4faed` | unregistered 0x45e5…c218 | — | — | +280 | 645 | 4180 (43.2%) | `unlock` | `unsupported_math_family` |
| 3 | `0x361052be2085dfddfc7f15d9f4c901d05d086f05` | fluxion-v3 | USD1/USDT0 | 220886 | +260 | 905 | 4440 (45.9%) | `unlock` | `loadable_drop_in` |
| 4 | `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 (interim) | USDC/WMNT | 898 | +162 | 1067 | 4602 (47.6%) | `unlock` | `loadable_drop_in` |
| 5 | `0xcddf50c3dfac95939167eff4c05d38781f549ea9` | unregistered 0x45e5…c218 | — | — | +149 | 1216 | 4751 (49.1%) | `unlock` | `unsupported_math_family` |
| 6 | `0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5` | agni-v3 | USDe/WMNT | 45796 | +147 | 1363 | 4898 (50.6%) | `unlock` | `loadable_drop_in` |
| 7 | `0x6483044b559f26252a659002e548480ef5b756f0` | unregistered 0x45e5…c218 | — | — | +130 | 1493 | 5028 (52.0%) | `unlock` | `unsupported_math_family` |
| 8 | `0x94c400b9eb9d371299143d7b1af1202f0f956d73` | unregistered 0x5c84…fd2f | WMNT/WETH | 2347 | +115 | 1608 | 5143 (53.2%) | `unlock` | `unregistered_v2_family_factory` |
| 9 | `0xbd2611d494f59bbbc871dfa2c2f15a3a63178b0d` | unregistered 0x45e5…c218 | — | — | +113 | 1721 | 5256 (54.3%) | `unlock` | `unsupported_math_family` |
| 10 | `0xe4765c8071f45c558e79f3ec9d77ac9af0058569` | unregistered 0x5c84…fd2f | USDT/WMNT | 1552 | +112 | 1833 | 5368 (55.5%) | `unlock` | `unregistered_v2_family_factory` |
| 11 | `0xbe8a7d2c6c286cc4f950a45f9250a8f0481107ec` | unregistered 0x45e5…c218 | — | — | +106 | 1939 | 5474 (56.6%) | `unlock` | `unsupported_math_family` |
| 12 | `0x34c38ec6add17673d6cf918377d435917524d094` | unregistered 0x45e5…c218 | — | — | +110 | 2049 | 5584 (57.7%) | `unlock` | `unsupported_math_family` |
| 13 | `0x47453cb250f705211e7a0de2f9c5d94cfecc8abd` | fusionx-v3 | WMNT/WETH | 443 | +111 | 2160 | 5695 (58.9%) | `unlock` | `loadable_drop_in` |
| 14 | `0x1a4d4aa3bd8587f6e05cc98cf87954f7d95c11c6` | unregistered 0x5bef…edec | USDC/WMNT | 830 | +107 | 2267 | 5802 (60.0%) | `unlock` | `unregistered_v2_family_factory` |
| 15 | `0xb2810371d52e911f522314b760b7ff91d37d7ea3` | butter | USDC/WMNT | 775 | +107 | 2374 | 5909 (61.1%) | `unlock` | `loadable_drop_in` |
| 16 | `0xed3ee32bdcf51632707f130af827fd849929570e` | fusionx-v3 | USDT/WMNT | 677 | +107 | 2481 | 6016 (62.2%) | `unlock` | `loadable_drop_in` |
| 17 | `0x5c4de5fd6aa5d6802f302a9f6df275cbfc6d8220` | — | USDC/WMNT | 1646 | +104 | 2585 | 6120 (63.3%) | `unlock` | `unregistered_v2_family_factory` |
| 18 | `0x4247f6c7409832adc101d7df714f96fecb327c92` | butter | USDT/WMNT | 912 | +100 | 2685 | 6220 (64.3%) | `unlock` | `loadable_drop_in` |
| 19 | `0xa375ea3e1f92d62e3a71b668bab09f7155267fa3` | unregistered 0x5bef…edec | WMNT/mETH | 1749 | +96 | 2781 | 6316 (65.3%) | `unlock` | `unregistered_v2_family_factory` |
| 20 | `0xf231e1ca10bae443f176f907352ac575a056c843` | fusionx-v3 | USDC/WMNT | 531 | +84 | 2865 | 6400 (66.2%) | `unlock` | `loadable_drop_in` |
| 21 | `0x8605c9d608a3f773b87fe1db5582ad35fe212144` | unregistered 0x45e5…c218 | — | — | +78 | 2943 | 6478 (67.0%) | `unlock` | `unsupported_math_family` |
| 22 | `0xc6e63803544b96ea0470f9c55028db327e3cd9d9` | unregistered 0x5c84…fd2f | USDC/WMNT | 1008 | +77 | 3020 | 6555 (67.8%) | `unlock` | `unregistered_v2_family_factory` |
| 23 | `0x4e7685df06201521f35a182467feefe02c53d847` | unregistered 0x5bef…edec | USDT/WMNT | 1032 | +72 | 3092 | 6627 (68.5%) | `unlock` | `unregistered_v2_family_factory` |
| 24 | `0xaaecd138ad9cd20c13f1593a41bf3941940ec41e` | fusionx-v3 | USDC/WMNT | 295 | +71 | 3163 | 6698 (69.2%) | `unlock` | `loadable_drop_in` |
| 25 | `0x33b1d7cfff71bba9dd987f96ad57e0a5f7db9ac5` | unregistered 0x5bef…edec | USDC/WETH | 103652 | +70 | 3233 | 6768 (70.0%) | `unlock` | `unregistered_v2_family_factory` |
| 26 | `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | 722 | +69 | 3302 | 6837 (70.7%) | `unlock` | `loadable_drop_in` |
| 27 | `0x526b6aec7b922c4268d7fc14a755e233457078cc` | unregistered 0x45e5…c218 | — | — | +68 | 3370 | 6905 (71.4%) | `unlock` | `unsupported_math_family` |
| 28 | `0x928981fe5a4c005a126662d2bd84fbf139b51876` | agni-v3 | WMNT/WETH | 124 | +68 | 3438 | 6973 (72.1%) | `unlock` | `loadable_drop_in` |
| 29 | `0x29234f4aa0842c4b27bf3ebfec3e7a25c85cc133` | butter | USDC/WMNT | 563 | +68 | 3506 | 7041 (72.8%) | `unlock` | `loadable_drop_in` |
| 30 | `0xdf8254d083988d517ac6d457bdcf2af6ac50682c` | unregistered 0xc848…3913 | WMNT/WETH | 242 | +65 | 3571 | 7106 (73.5%) | `unlock` | `unregistered_v3_family_factory` |
| 31 | `0x585ec64f06afa80e474bb6574ef7be38a8ef94a7` | fusionx-v2 (interim) | WMNT/WETH | 322 | +61 | 3632 | 7167 (74.1%) | `unlock` | `loadable_drop_in` |
| 32 | `0x84ec2a3907ed9e79c7a45551fef9da29d5f2ae9b` | unregistered 0xc848…3913 | USDC/WMNT | 109 | +62 | 3694 | 7229 (74.7%) | `unlock` | `unregistered_v3_family_factory` |
| 33 | `0x8fbad1a9ad1bafb6cc0bc7807ffa12115803e991` | v3fork-636ea2 | USDT/WMNT | 323 | +59 | 3753 | 7288 (75.3%) | `unlock` | `loadable_drop_in` |
| 34 | `0x73b908af8d8c31f7af826c9f7f7b531e4add7f35` | cleopatra-cl | WMNT/WETH | 252 | +54 | 3807 | 7342 (75.9%) | `unlock` | `quarantined_adapter_required` |
| 35 | `0xa4657555cbddc069ed3389ac03330020692b13c4` | unregistered 0xc848…3913 | USDC/USDT | 348876 | +54 | 3861 | 7396 (76.5%) | `unlock` | `unregistered_v3_family_factory` |
| 36 | `0x7bd7f23b34cb2cd0c42e7ed409872547783567b3` | v3fork-636ea2 | USDC/WMNT | 175 | +53 | 3914 | 7449 (77.0%) | `unlock` | `loadable_drop_in` |
| 37 | `0xd14c2a2950ea3b7badd6bddb18f7a7744cd705be` | unregistered 0xd7d3…2dde | USDT/WMNT | 359 | +52 | 3966 | 7501 (77.5%) | `unlock` | `unregistered_v2_family_factory` |
| 38 | `0x43925fffade90c48fbd12384d5f4d4da9c359fc4` | unregistered 0xd7d3…2dde | USDC/WMNT | 358 | +51 | 4017 | 7552 (78.1%) | `unlock` | `unregistered_v2_family_factory` |
| 39 | `0x7fe1d1518729697c40f6558af8826589ae208fa9` | unregistered 0x3ace…ecc8 | USDT/WMNT | 298 | +51 | 4068 | 7603 (78.6%) | `unlock` | `unregistered_v2_family_factory` |
| 40 | `0xbe18aad013699c1cdd903cb3e6d596ef99c37650` | unregistered 0x45e5…c218 | — | — | +49 | 4117 | 7652 (79.1%) | `unlock` | `unsupported_math_family` |
| 41 | `0x58e201316aa3dc7f1227a5c5c14490836926636e` | unregistered 0x45e5…c218 | — | — | +48 | 4165 | 7700 (79.6%) | `unlock` | `unsupported_math_family` |
| 42 | `0x9c3ef5c54960fe06b04fbbab6e5ad33cee59ec54` | unregistered 0x45e5…c218 | — | — | +48 | 4213 | 7748 (80.1%) | `unlock` | `unsupported_math_family` |
| 43 | `0xd114e1fdf9e4129b863a6af53806ae0f8c54ce88` | butter | WMNT/mETH | 32 | +48 | 4261 | 7796 (80.6%) | `unlock` | `loadable_drop_in` |
| 44 | `0x306ab51fd73afbafbfc096380c36211f8a12a532` | v3fork-636ea2 | USDC/WETH | 5697 | +47 | 4308 | 7843 (81.1%) | `unlock` | `loadable_drop_in` |
| 45 | `0x4a18891de69124d2853a4e27543edb7e2e001179` | unregistered 0x5bef…edec | WMNT/WETH | 371 | +48 | 4356 | 7891 (81.6%) | `unlock` | `unregistered_v2_family_factory` |
| 46 | `0xe2cb2455dd51f5edd76b8e74a758c94109dc7ea4` | v3fork-636ea2 | USDC/WMNT | 231 | +47 | 4403 | 7938 (82.1%) | `unlock` | `loadable_drop_in` |
| 47 | `0xf99907291da3b352eb6b0930f7e2a3eaf0f41a31` | unregistered 0x5b54…f249 | USDC/WMNT | 246 | +47 | 4450 | 7985 (82.5%) | `unlock` | `unregistered_v2_family_factory` |
| 48 | `0x880c77e52ca8882cb3fc6bac78921afdb4b87b4e` | unregistered 0x3aed…fb87 | USDC/WMNT | 1196 | +44 | 4494 | 8029 (83.0%) | `unlock` | `unsupported_math_family` |
| 49 | `0x2157b9dfb318e1c5a236d52f2921e7d0aa59e1d3` | unregistered 0xe67a…65d0 | WMNT/WETH | 50 | +40 | 4534 | 8069 (83.4%) | `unlock` | `unregistered_v3_family_factory` |
| 50 | `0x328cfb13c66065240b58bff0ae1ce798fe314ec1` | agni-v3 | USDT/WMNT | 622 | +37 | 4571 | 8106 (83.8%) | `unlock` | `loadable_drop_in` |

`frequency_fallback` steps unlock nothing alone — they close one side of a multi-pool gap.

## Candidate sets and admission cost

| Restriction | Size | Added | Arbs unlocked | Reachable after | Pools | Cycles | Δcycles | Cycle × | Cold start | Adapter-blocked |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `loadable_only` | 10 | 10 | +1133 | 4668 (48.3%) | 140 | 10936 | +3974 | 1.57× | 377 s | 0 |
| `loadable_only` | 25 | 25 | +1653 | 5188 (53.6%) | 155 | 20490 | +13528 | 2.94× | 417 s | 0 |
| `loadable_only` | 50 | 50 | +1847 | 5382 (55.6%) | 180 | 31058 | +24096 | 4.46× | 485 s | 0 |
| `any_venue` | 10 | 10 | +1833 | 5368 (55.5%) | 140 | 8510 | +1548 | 1.22× | 377 s | 7 |
| `any_venue` | 25 | 25 | +3233 | 6768 (70.0%) | 155 | 14558 | +7596 | 2.09× | 417 s | 16 |
| `any_venue` | 50 | 50 | +4571 | 8106 (83.8%) | 180 | 30920 | +23958 | 4.44× | 485 s | 31 |

Cold start is estimated linearly from WHI-936: 350 s at 130 pools (one pool per batch CREATE); cycle counts come from the production enumerator.

## Hop-cap pricing

| Hops | Arbs |
| ---: | ---: |
| 4 | 155 |
| 5 | 122 |
| 6 | 121 |
| 7 | 98 |
| 8 | 47 |
| 9 | 29 |
| 10 | 23 |
| 11 | 13 |
| 12 | 21 |
| 13 | 14 |
| 14 | 10 |
| 15 | 1 |
| 16 | 2 |
| 17 | 4 |
| 20 | 2 |
| 22 | 2 |
| 23 | 1 |
| 24 | 2 |
| 26 | 2 |
| 28 | 1 |
| 29 | 1 |
| 31 | 1 |
| 32 | 1 |
| 34 | 1 |
| 40 | 1 |
| 43 | 1 |
| 44 | 2 |
| **total above cap** | **678** |

| Metric | Value |
| --- | ---: |
| Arbs at exactly 4 hops | 155 |
| …already fully in universe | 78 |
| …with the largest loadable set added | 85 |
| Cycles at cap 3 | 6962 |
| Cycles at cap 4 | 134096 (19.3×) |
| Cold start impact | none (same pool set) |

Raising the cap from 3 to 4 admits only the 155 arbs at exactly 4 hops — the remaining 523 sit deeper still. Of those, 78 are already fully inside the universe, so that is what a cap change alone recovers. Cold start is unaffected (same pools); the cost is per-block: the cycle set grows 19.3× and every dirty-cycle pass re-optimizes it.

## Missing pools by appearance

| Pool | Venue | Pair | Kind | In-scope arbs | Sole blocker of | Hop positions | TVL (WMNT) | Exclusion |
| --- | --- | --- | --- | ---: | ---: | --- | ---: | --- |
| `0x98d1e99d294e8603fa050ea129c78388408e0dd1` | unregistered 0x45e5…c218 | — | izi | 591 | 365 | 1:223 2:137 3:231 | — | `venue_not_loadable` |
| `0x1da0925773e15359c8b87272146e86444eb4faed` | unregistered 0x45e5…c218 | — | izi | 355 | 260 | 1:160 2:69 3:126 | — | `venue_not_loadable` |
| `0xf9cda48949ae1823eecdd314deecd8599ceaf7cc` | fusionx-v2 (interim) | USDC/WMNT | v2 | 263 | 159 | 1:162 2:53 3:48 | 898 | `below_tvl_floor` |
| `0x361052be2085dfddfc7f15d9f4c901d05d086f05` | fluxion-v3 | USD1/USDT0 | v3 | 260 | 260 | 1:4 2:256 | 220886 | `admissible_but_absent` |
| `0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5` | agni-v3 | USDe/WMNT | algebra | 247 | 118 | 1:25 2:8 3:214 | 45796 | `admissible_but_absent` |
| `0xa375ea3e1f92d62e3a71b668bab09f7155267fa3` | unregistered 0x5bef…edec | WMNT/mETH | v2 | 194 | 85 | 1:93 2:12 3:89 | 1749 | `venue_not_loadable` |
| `0x94c400b9eb9d371299143d7b1af1202f0f956d73` | unregistered 0x5c84…fd2f | WMNT/WETH | v2 | 187 | 98 | 1:99 2:25 3:63 | 2347 | `venue_not_loadable` |
| `0xcddf50c3dfac95939167eff4c05d38781f549ea9` | unregistered 0x45e5…c218 | — | izi | 171 | 136 | 1:92 2:38 3:41 | — | `venue_not_loadable` |
| `0x1a4d4aa3bd8587f6e05cc98cf87954f7d95c11c6` | unregistered 0x5bef…edec | USDC/WMNT | v2 | 165 | 93 | 1:93 2:42 3:30 | 830 | `venue_not_loadable` |
| `0x47453cb250f705211e7a0de2f9c5d94cfecc8abd` | fusionx-v3 | WMNT/WETH | algebra | 165 | 87 | 1:73 2:31 3:61 | 443 | `below_tvl_floor` |
| `0xb2810371d52e911f522314b760b7ff91d37d7ea3` | butter | USDC/WMNT | v3 | 153 | 91 | 1:78 2:37 3:38 | 775 | `below_tvl_floor` |
| `0x4247f6c7409832adc101d7df714f96fecb327c92` | butter | USDT/WMNT | v3 | 151 | 87 | 1:74 2:37 3:40 | 912 | `below_tvl_floor` |
| `0xbe8a7d2c6c286cc4f950a45f9250a8f0481107ec` | unregistered 0x45e5…c218 | — | izi | 145 | 67 | 1:14 2:131 | — | `venue_not_loadable` |
| `0xe4765c8071f45c558e79f3ec9d77ac9af0058569` | unregistered 0x5c84…fd2f | USDT/WMNT | v2 | 141 | 103 | 1:76 2:34 3:31 | 1552 | `venue_not_loadable` |
| `0x6483044b559f26252a659002e548480ef5b756f0` | unregistered 0x45e5…c218 | — | izi | 140 | 122 | 1:68 2:22 3:50 | — | `venue_not_loadable` |
| `0x34c38ec6add17673d6cf918377d435917524d094` | unregistered 0x45e5…c218 | — | izi | 126 | 89 | 1:52 2:30 3:44 | — | `venue_not_loadable` |
| `0xed3ee32bdcf51632707f130af827fd849929570e` | fusionx-v3 | USDT/WMNT | algebra | 122 | 93 | 1:61 2:30 3:31 | 677 | `below_tvl_floor` |
| `0xbd2611d494f59bbbc871dfa2c2f15a3a63178b0d` | unregistered 0x45e5…c218 | — | izi | 117 | 105 | 1:39 2:45 3:33 | — | `venue_not_loadable` |
| `0xe0d80d6377aadcb0a648cc157f593c60390385e7` | butter | USDY/WMNT | v3 | 117 | 0 | 1:63 2:1 3:53 | 297 | `below_tvl_floor` |
| `0xace7a42c030759ea903e9c39ad26a0f9b4a11927` | unregistered 0x5bef…edec | PUFF/WMNT | v2 | 113 | 0 | 1:45 2:36 3:32 | 524 | `venue_not_loadable` |
| `0x29234f4aa0842c4b27bf3ebfec3e7a25c85cc133` | butter | USDC/WMNT | v3 | 111 | 56 | 1:54 2:27 3:30 | 563 | `below_tvl_floor` |
| `0x5c4de5fd6aa5d6802f302a9f6df275cbfc6d8220` | — | USDC/WMNT | v2 | 107 | 85 | 1:65 2:17 3:25 | 1646 | `venue_not_loadable` |
| `0x0b15691c828ff6d499375e2ca2070b08dd62369e` | butter | USDT/WMNT | v3 | 103 | 59 | 1:55 2:23 3:25 | 722 | `below_tvl_floor` |
| `0x33b1d7cfff71bba9dd987f96ad57e0a5f7db9ac5` | unregistered 0x5bef…edec | USDC/WETH | v2 | 101 | 36 | 1:6 2:95 | 103652 | `venue_not_loadable` |
| `0x4e7685df06201521f35a182467feefe02c53d847` | unregistered 0x5bef…edec | USDT/WMNT | v2 | 98 | 62 | 1:57 2:25 3:16 | 1032 | `venue_not_loadable` |
| `0xc6e63803544b96ea0470f9c55028db327e3cd9d9` | unregistered 0x5c84…fd2f | USDC/WMNT | v2 | 96 | 67 | 1:58 2:20 3:18 | 1008 | `venue_not_loadable` |
| `0x763868612858358f62b05691db82ad35a9b3e110` | unregistered 0x5bef…edec | MOE/WMNT | v2 | 93 | 0 | 1:30 2:3 3:60 | 399563 | `venue_not_loadable` |
| `0x8605c9d608a3f773b87fe1db5582ad35fe212144` | unregistered 0x45e5…c218 | — | izi | 92 | 64 | 1:50 2:13 3:29 | — | `venue_not_loadable` |
| `0xf231e1ca10bae443f176f907352ac575a056c843` | fusionx-v3 | USDC/WMNT | algebra | 91 | 75 | 1:52 2:11 3:28 | 531 | `below_tvl_floor` |
| `0x585ec64f06afa80e474bb6574ef7be38a8ef94a7` | fusionx-v2 (interim) | WMNT/WETH | v2 | 90 | 42 | 1:58 2:9 3:23 | 322 | `below_tvl_floor` |
| `0xaaecd138ad9cd20c13f1593a41bf3941940ec41e` | fusionx-v3 | USDC/WMNT | algebra | 82 | 56 | 1:47 2:24 3:11 | 295 | `below_tvl_floor` |
| `0xdf8254d083988d517ac6d457bdcf2af6ac50682c` | unregistered 0xc848…3913 | WMNT/WETH | v3 | 79 | 40 | 1:49 2:10 3:20 | 242 | `venue_not_loadable` |
| `0xd114e1fdf9e4129b863a6af53806ae0f8c54ce88` | butter | WMNT/mETH | v3 | 77 | 34 | 1:37 2:7 3:33 | 32 | `below_tvl_floor` |
| `0x928981fe5a4c005a126662d2bd84fbf139b51876` | agni-v3 | WMNT/WETH | algebra | 76 | 45 | 1:49 2:11 3:16 | 124 | `below_tvl_floor` |
| `0x84ec2a3907ed9e79c7a45551fef9da29d5f2ae9b` | unregistered 0xc848…3913 | USDC/WMNT | v3 | 72 | 46 | 1:24 2:26 3:22 | 109 | `venue_not_loadable` |
| `0x526b6aec7b922c4268d7fc14a755e233457078cc` | unregistered 0x45e5…c218 | — | izi | 69 | 58 | 1:42 2:12 3:15 | — | `venue_not_loadable` |
| `0x8fbad1a9ad1bafb6cc0bc7807ffa12115803e991` | v3fork-636ea2 | USDT/WMNT | v3 | 69 | 36 | 1:39 2:16 3:14 | 323 | `below_tvl_floor` |
| `0xbb2f514804ff60ee8a115993900bd369762ce548` | — | PUFF/WMNT | v2 | 67 | 0 | 1:36 2:31 | 30 | `venue_not_loadable` |
| `0x4a18891de69124d2853a4e27543edb7e2e001179` | unregistered 0x5bef…edec | WMNT/WETH | v2 | 64 | 28 | 1:33 2:3 3:28 | 371 | `venue_not_loadable` |
| `0x43925fffade90c48fbd12384d5f4d4da9c359fc4` | unregistered 0xd7d3…2dde | USDC/WMNT | v2 | 63 | 36 | 1:40 2:15 3:8 | 358 | `venue_not_loadable` |
| `0x7bd7f23b34cb2cd0c42e7ed409872547783567b3` | v3fork-636ea2 | USDC/WMNT | v3 | 62 | 33 | 1:38 2:20 3:4 | 175 | `below_tvl_floor` |
| `0xd14c2a2950ea3b7badd6bddb18f7a7744cd705be` | unregistered 0xd7d3…2dde | USDT/WMNT | v2 | 60 | 42 | 1:37 2:16 3:7 | 359 | `venue_not_loadable` |
| `0x73b908af8d8c31f7af826c9f7f7b531e4add7f35` | cleopatra-cl | WMNT/WETH | v3 | 59 | 44 | 1:15 2:24 3:20 | 252 | `venue_not_loadable` |
| `0x7fe1d1518729697c40f6558af8826589ae208fa9` | unregistered 0x3ace…ecc8 | USDT/WMNT | v2 | 59 | 39 | 1:38 2:16 3:5 | 298 | `venue_not_loadable` |
| `0xe2cb2455dd51f5edd76b8e74a758c94109dc7ea4` | v3fork-636ea2 | USDC/WMNT | v3 | 57 | 34 | 1:33 2:13 3:11 | 231 | `below_tvl_floor` |
| `0x3811d436b7a7be68fb78dbecebeb93eedb4fe668` | unregistered 0x5c84…fd2f | WMNT/LUSD | v2 | 55 | 0 | 1:1 2:24 3:30 | 8 | `venue_not_loadable` |
| `0xa4657555cbddc069ed3389ac03330020692b13c4` | unregistered 0xc848…3913 | USDC/USDT | v3 | 54 | 42 | 1:4 2:45 3:5 | 348876 | `venue_not_loadable` |
| `0x58e201316aa3dc7f1227a5c5c14490836926636e` | unregistered 0x45e5…c218 | — | izi | 53 | 22 | 1:2 2:51 | — | `venue_not_loadable` |
| `0xf99907291da3b352eb6b0930f7e2a3eaf0f41a31` | unregistered 0x5b54…f249 | USDC/WMNT | v2 | 53 | 30 | 1:35 2:15 3:3 | 246 | `venue_not_loadable` |
| `0x306ab51fd73afbafbfc096380c36211f8a12a532` | v3fork-636ea2 | USDC/WETH | v3 | 51 | 28 | 2:50 3:1 | 5697 | `admissible_but_absent` |
| `0xe38e3a804ef845e36f277d86fb2b24b8c32b3340` | agni-v3 | USDT/USDY | algebra | 51 | 0 | 2:51 | 58338 | `cycle_filter_rejected` |
| `0xbe18aad013699c1cdd903cb3e6d596ef99c37650` | unregistered 0x45e5…c218 | — | izi | 49 | 19 | 1:3 2:46 | — | `venue_not_loadable` |
| `0x9c3ef5c54960fe06b04fbbab6e5ad33cee59ec54` | unregistered 0x45e5…c218 | — | izi | 48 | 25 | 1:33 2:10 3:5 | — | `venue_not_loadable` |
| `0x5c819961990c9f4f9fbfd4101f1d4e565b8aa0a6` | unregistered 0x5bef…edec | USDT/mETH | v2 | 46 | 20 | 1:8 2:38 | 314496 | `venue_not_loadable` |
| `0x880c77e52ca8882cb3fc6bac78921afdb4b87b4e` | unregistered 0x3aed…fb87 | USDC/WMNT | solidly | 45 | 42 | 1:27 2:4 3:14 | 1196 | `venue_not_loadable` |
| `0x2157b9dfb318e1c5a236d52f2921e7d0aa59e1d3` | unregistered 0xe67a…65d0 | WMNT/WETH | v3 | 42 | 29 | 1:28 2:4 3:10 | 50 | `venue_not_loadable` |
| `0x7d35ba038df5afde64a1962683ffeb3e150637ff` | unregistered 0x5bef…edec | USDT/MOE | v2 | 42 | 0 | 1:1 2:41 | 13628 | `venue_not_loadable` |
| `0xfbc9441eac215662a35abafdbb7bffff2c6ea5be` | unregistered 0x3ef9…b341 | USDT/WMNT | v2 | 42 | 27 | 1:27 2:10 3:5 | 197 | `venue_not_loadable` |
| `0x328cfb13c66065240b58bff0ae1ce798fe314ec1` | agni-v3 | USDT/WMNT | algebra | 39 | 34 | 1:23 2:10 3:6 | 622 | `below_tvl_floor` |
| `0x9d5d4064a808ba865957b1d04b20a84175dcc16d` | agni-v3 | WMNT/WETH | algebra | 39 | 26 | 1:22 2:9 3:8 | 672 | `below_tvl_floor` |
| `0x681966b68b3fbec92078ec56614f0b8b7f4fd17a` | unregistered 0xead1…5252 | USDC/WMNT | v3 | 38 | 29 | 1:20 2:11 3:7 | 680 | `venue_not_loadable` |
| `0xfb16b5ccc62dc125834c33bf6b063c87e6e6f581` | unregistered 0x5bef…edec | LEND/mETH | v2 | 38 | 0 | 2:38 | 216676 | `venue_not_loadable` |
| `0x86f4596e2851efad128170338c76b4e00ed6cfb9` | unregistered 0x62db…51ce | USDT/WMNT | v2 | 36 | 30 | 1:23 2:8 3:5 | 276 | `venue_not_loadable` |
| `0x011d57bb869f6c7a9e69323c3f277720d2919be7` | unregistered 0xc848…3913 | USDT/WETH | v3 | 35 | 10 | 2:33 3:2 | 3483 | `venue_not_loadable` |
| `0xe1dc93d69439a924baaeaf9e64f4ae7be0af738a` | agni-v3 | USDC/WETH | algebra | 34 | 4 | 2:34 | 453 | `below_tvl_floor` |
| `0x9cd55b03c64b65ba02a1d985caef63046b2d54eb` | agni-v3 | USDC/USDY | algebra | 32 | 0 | 2:32 | 676411 | `cycle_filter_rejected` |
| `0x3e7c1421430acfa556dc1df339b226d7a9149391` | v3fork-636ea2 | USDT/WMNT | v3 | 31 | 20 | 1:18 2:8 3:5 | 94 | `below_tvl_floor` |
| `0x4b96994181cb694f506bdf24a218fe7af64147cb` | agni-v3 | WMNT/mETH | algebra | 29 | 18 | 1:22 2:2 3:5 | 682 | `below_tvl_floor` |
| `0x74e0655bc7141314e5bc5b93bc9a83ca80f4fd8a` | moe | PUFF/mETH | lb | 29 | 0 | 1:2 2:27 | 148803 | `cycle_filter_rejected` |
| `0xf79c37b8344c58467ec88c01b82c2fd8fccdbbd0` | cleopatra-cl | USDT/mETH | v3 | 28 | 12 | 1:1 2:27 | 157157 | `venue_not_loadable` |
| `0x30ac02b4c99d140cde2a212ca807cbda35d4f6b5` | unregistered 0x5bef…edec | LEND/WMNT | v2 | 27 | 0 | 1:20 2:1 3:6 | 323 | `venue_not_loadable` |
| `0x795ba464c12bcc5e162858d47c5f89083aab9d2d` | unregistered 0xae41…c93b | USDC/WMNT | algebra | 27 | 18 | 1:23 3:4 | 259 | `venue_not_loadable` |
| `0x94044650972fba2866c82e82bf563ce2b4021476` | unregistered 0x45e5…c218 | — | izi | 26 | 8 | 2:26 | — | `venue_not_loadable` |
| `0x32c1882baf179f6059d101aad3feac15b2b90da3` | unregistered 0x5bef…edec | ENA/WMNT | v2 | 25 | 0 | 1:16 2:2 3:7 | 481 | `venue_not_loadable` |
| `0x57dbbf8ad81898f430cf45fead615a1e049c9f76` | unregistered 0x5bef…edec | MOE/mETH | v2 | 25 | 0 | 1:2 2:23 | 29844 | `venue_not_loadable` |
| `0x60a9c1bd78f408df2c1d86a6941c892b37ad67e7` | v3fork-636ea2 | WMNT/WETH | v3 | 25 | 14 | 1:16 2:4 3:5 | 182 | `below_tvl_floor` |
| `0x6dda28e8f351852a19657906f3e603808e9d688d` | unregistered 0xae41…c93b | USDT/WMNT | algebra | 25 | 10 | 1:13 2:2 3:10 | 3474 | `venue_not_loadable` |
| `0xa81ede3710ea5249fdc1a81bb5664d004300ddb7` | butter | USDC/USDY | v3 | 25 | 0 | 2:25 | 74537 | `cycle_filter_rejected` |
| `0xca455f94225a447c677ef0bf3a0c05626c090cd1` | — | USDC/WMNT | v2 | 25 | 16 | 1:19 2:4 3:2 | 180 | `venue_not_loadable` |
| `0xff53524d0e01a00ecec997d2ebb5f06860068709` | unregistered 0x5bef…edec | USDT/ENA | v2 | 25 | 0 | 1:2 2:23 | 2632 | `venue_not_loadable` |
| `0xc151111ab16e0df74978763dc8947b8a4214aee6` | unregistered 0xd897…a4e3 | USDT/WETH | v3 | 23 | 12 | 2:23 | 22533 | `venue_not_loadable` |
| `0xa50a9b38c10ce77ac1a2d14f915b345aa5729ea5` | unregistered 0x45e5…c218 | — | izi | 22 | 12 | 2:22 | — | `venue_not_loadable` |
| `0xa97d73a9e9df96492e9e64f406a82de1541649b9` | butter | USDT/WMNT | v3 | 21 | 18 | 1:15 2:5 3:1 | 334 | `below_tvl_floor` |
| `0xae9a0d9b1c9cd31d60fdbfe270ccb8c878bb15c8` | unregistered 0x5bef…edec | PUFF/mETH | v2 | 21 | 0 | 2:21 | 228 | `venue_not_loadable` |
| `0xb670d2b452d0ecc468cccfd532482d45dddde2a1` | unregistered 0x5bef…edec | JOE/MOE | v2 | 21 | 0 | 1:1 2:20 | 2697 | `venue_not_loadable` |
| `0xd1906f3cbb610736e3bbe2604669f19928e4da78` | unregistered 0xb20a…d794 | USDC/WMNT | v2 | 21 | 8 | 1:13 2:4 3:4 | 45 | `venue_not_loadable` |
| `0xefc38c1b0d60725b824ebee8d431abfbf12bc953` | unregistered 0x5bef…edec | JOE/WMNT | v2 | 21 | 0 | 1:16 3:5 | 11761 | `venue_not_loadable` |
| `0x78f641bd6cef5224b9b2e0bd0723eae36b8f36ae` | — | USDC/WMNT | v2 | 20 | 15 | 1:13 2:5 3:2 | 141 | `venue_not_loadable` |
| `0xe92b806c34c8beea03d322942d9f271c91028f5f` | butter | USDY/WMNT | v3 | 20 | 0 | 1:12 3:8 | 31 | `below_tvl_floor` |
| `0x99c5527d168bcd9f820652e90bff3a1f059d34ce` | cleopatra-cl | USDT/WMNT | v3 | 19 | 14 | 1:12 2:6 3:1 | 179 | `venue_not_loadable` |
| `0xfc60a4d05ac8c93f62276e046ad5a098f5c7820a` | uniswap-v3 | WMNT/WETH | v3 | 19 | 4 | 1:7 2:4 3:8 | 10 | `below_tvl_floor` |
| `0x4c57be599d0e0414785943569e9b6a66da79aa6b` | fusionx-v2 (interim) | LEND/WMNT | v2 | 18 | 0 | 1:15 2:1 3:2 | 158 | `below_tvl_floor` |
| `0x692903acc9f3acb4e2545a37ff620f35a63976f1` | butter | WMNT/WETH | v3 | 18 | 9 | 1:14 2:2 3:2 | 140 | `below_tvl_floor` |
| `0x776d50283cd843f5e383ad6d5729db5ef4867848` | fusionx-v2 (interim) | WMNT/STG | v2 | 18 | 0 | 1:12 2:6 | 5 | `below_tvl_floor` |
| `0xa79ef497def85631d999c5f84afda0f9322e3f6f` | fusionx-v2 (interim) | WMNT/LUSD | v2 | 18 | 0 | 1:14 2:2 3:2 | 5 | `below_tvl_floor` |
| `0xf08764411376cce42fd05aac101494de13f1c39d` | — | WMNT/mETH | v2 | 18 | 8 | 1:13 3:5 | 220 | `venue_not_loadable` |
| `0x23a2ba0ecf564954371803bd7c59669ed166d94e` | — | USDT/LUSD | v2 | 17 | 0 | 1:5 2:12 | 212 | `venue_not_loadable` |
| `0x8a67aaa4e63f4961f472ae1ca2377648387e3f5d` | unregistered 0xd897…a4e3 | USDT/WMNT | v3 | 17 | 11 | 1:12 2:4 3:1 | 62 | `venue_not_loadable` |
| `0x43294e35dcdba29615739526424f3f89bf407c09` | butter | WMNT/WETH | v3 | 16 | 5 | 1:12 3:4 | 66 | `below_tvl_floor` |
| `0x6c7604c157507a0aaea90f2928ca44cc1d60cd81` | fusionx-v3 | WMNT/WETH | algebra | 16 | 6 | 1:14 3:2 | 75 | `below_tvl_floor` |
| `0x995377f701d3d2c572d7552fcc7bce72668af225` | unregistered 0x45e5…c218 | — | izi | 16 | 5 | 1:14 3:2 | — | `venue_not_loadable` |
| `0xd470a7a754c284603d54ed27d8a948fa4e3fd9e9` | unregistered 0x5b54…f249 | WMNT/WETH | v2 | 15 | 7 | 1:12 2:1 3:2 | 42 | `venue_not_loadable` |
| `0xda9d18fd4bc094660d79721feb2e72c962ab54fe` | unregistered 0x62db…51ce | USDC/WMNT | v2 | 15 | 11 | 1:10 2:5 | 118 | `venue_not_loadable` |
| `0xfa0d6714eeaecccade0558286398d326a2b9dbbe` | fusionx-v3 | WMNT/WBTC | algebra | 15 | 0 | 1:11 3:4 | 68 | `below_tvl_floor` |
| `0x547ba2b1f5562fc72ca5f8c208724d751f1749ec` | cleopatra-cl | WMNT/aUSD | v3 | 14 | 0 | 1:9 3:5 | 300 | `venue_not_loadable` |
| `0xcd07bcf06f3ad0eac869bdec3e9065864a348875` | cleopatra-cl | USDC/WMNT | v3 | 14 | 9 | 1:10 2:3 3:1 | 32 | `venue_not_loadable` |
| `0xd145db1dfc3fcd2e999b47f3a02c85bd7750ed09` | agni-v3 | USDT/WMNT | algebra | 14 | 10 | 1:9 2:1 3:4 | 488 | `below_tvl_floor` |
| `0x160fd5abd0711e65120c34e435fb49bfbdcd24b4` | agni-v3 | WMNT/STG | algebra | 13 | 0 | 1:4 2:9 | 6 | `below_tvl_floor` |
| `0x2488a749dae9ce73c4e5d2cd7c98d921259a1a69` | unregistered 0x5bef…edec | YAK/mETH | v2 | 13 | 0 | 2:13 | 51372 | `venue_not_loadable` |
| `0x4bbf96c93ec63a4d021020aee024abe100597a31` | unregistered 0x5bef…edec | USDT/mShards | v2 | 13 | 0 | 2:13 | 36892 | `venue_not_loadable` |
| `0xc8eb739c47f02e078d10c422895d8ef5ec549738` | unregistered 0x5bef…edec | WMNT/mShards | v2 | 13 | 0 | 1:10 3:3 | 78 | `venue_not_loadable` |
| `0xd2dd887b3cb495146884c9450c655e501f761053` | unregistered 0x5bef…edec | WMNT/YAK | v2 | 13 | 0 | 1:12 3:1 | 149 | `venue_not_loadable` |
| `0xd764c2cacada1a090e0b852a3b24e653432713cb` | unregistered 0xead1…5252 | USDC/USDT | v3 | 13 | 4 | 2:13 | 9492 | `venue_not_loadable` |
| `0xe46cfde1afa8e87bb543af0e064af52dd6805c93` | cleopatra-cl | WMNT/GRAI | v3 | 13 | 0 | 1:7 2:1 3:5 | 3932 | `venue_not_loadable` |
| `0xf032b07539965c12b2e76b5cfbe13ccbda23e866` | unregistered 0x5bef…edec | WMNT/cmETH | v2 | 13 | 5 | 1:7 2:3 3:3 | 43 | `venue_not_loadable` |
| `0x04a972c3bd540d286be48e8dd61565de4b8a274d` | butter | USDY/WMNT | v3 | 12 | 0 | 1:8 3:4 | 26 | `below_tvl_floor` |
| `0x20a2602e3e8f2fd13c374630de0545bf0ece435b` | unregistered 0x45e5…c218 | — | izi | 11 | 0 | 2:11 | — | `venue_not_loadable` |
| `0x50c4adb79d560adee85bc960c1b3eaf0bda7a15c` | unregistered 0x45e5…c218 | — | izi | 11 | 7 | 2:11 | — | `venue_not_loadable` |
| `0x7234d21d6de00d298467ccaacce17e0d0118cb3b` | unregistered 0x3ef9…b341 | WMNT/WETH | v2 | 11 | 6 | 1:9 2:1 3:1 | 34 | `venue_not_loadable` |
| `0x7991be74b74ea7528bfbca292926e3b05526f042` | v3fork-636ea2 | USDT/WMNT | v3 | 11 | 6 | 1:8 2:2 3:1 | 35 | `below_tvl_floor` |
| `0x92ef4b21e05e06e203cb3cca1038917d7dd00974` | unregistered 0x5b54…f249 | WMNT/mETH | v2 | 11 | 1 | 1:7 2:2 3:2 | 29 | `venue_not_loadable` |
| `0xc865dd3421a6dd706688955fe727c802a98c1df9` | unregistered 0x45e5…c218 | — | izi | 11 | 6 | 1:4 2:7 | — | `venue_not_loadable` |
| `0x813d7f24df644550f824141bcdd8cb1cb0642d06` | agni-v3 | WMNT/mETH | algebra | 10 | 4 | 1:4 3:6 | 19 | `below_tvl_floor` |
| `0xb1c4ffee7c4a2bc3de2da7ff22e6dc409defda14` | unregistered 0x62db…51ce | USDT/WMNT | v2 | 9 | 4 | 1:6 2:3 | 70 | `venue_not_loadable` |
| `0xbd51f73d81cff395172a40a4de4af7b6cc5da05b` | unregistered 0x5bef…edec | WMNT/STG | v2 | 9 | 0 | 1:4 2:5 | 1 | `venue_not_loadable` |
| `0xc203e8eec73624976ae7d9425fa777ebfe337ccc` | v3fork-636ea2 | USDC/WMNT | v3 | 9 | 5 | 1:7 2:2 | 32 | `below_tvl_floor` |
| `0xfbb15848b231ffb97903decf3eef7f725d8e4d0a` | unregistered 0x45e5…c218 | — | izi | 9 | 3 | 2:9 | — | `venue_not_loadable` |
| `0x064d4c6e06711eaff5a9e2a19e750ee8b94159ab` | fusionx-v3 | USDC/WMNT | algebra | 8 | 1 | 1:4 2:2 3:2 | 144 | `below_tvl_floor` |
| `0x133aa1d21f82c88843e3941836800470c5a45c07` | unregistered 0x69c4…a7b5 | WMNT/WETH | v2 | 8 | 2 | 1:7 2:1 | 25 | `venue_not_loadable` |
| `0x5126ac4145ed84ebe28cfb34bb6300bcef492bb7` | unregistered 0x5bef…edec | MINU/WMNT | v2 | 8 | 0 | 1:8 | 14454 | `venue_not_loadable` |
| `0x9698f1ab4b391d090ea38cfdca6802a20ff19642` | fusionx-v2 (interim) | WMNT/PENDLE | v2 | 8 | 0 | 1:3 2:3 3:2 | 9 | `below_tvl_floor` |
| `0xc1f43e45f86e7bfb92c3c309b0ef366f9ba33bfa` | unregistered 0x5bef…edec | USDC/USDY | v2 | 8 | 0 | 2:8 | 7317 | `venue_not_loadable` |
| `0xea32a56c9195484bed638cddc9d0cec6c4f00de1` | unregistered 0xd264…f733 | USDT/WMNT | v3 | 8 | 4 | 1:6 3:2 | 11 | `venue_not_loadable` |
| `0xf3eb01fcd4fa6006c27c504fce8790154d055caa` | — | WMNT/WETH | v2 | 8 | 5 | 1:7 2:1 | 91 | `venue_not_loadable` |
| `0x8b4d24365d08055fd4220ef87492020ac54d0128` | agni-v3 | WMNT/cmETH | algebra | 7 | 5 | 1:7 | 15 | `below_tvl_floor` |
| `0xa0d893e002b949b9b025aae2e8bcf10776f81e6d` | unregistered 0x5b54…f249 | USDT/WETH | v2 | 7 | 3 | 2:7 | 4445 | `venue_not_loadable` |
| `0xabaff1d3a706336570d4524ddff2b1e03e35542c` | — | USDC/aUSD | v2 | 7 | 0 | 1:1 2:6 | 127514 | `venue_not_loadable` |
| `0xd1311115175a9d3f14fa27db2cd6c7a60723eaab` | unregistered 0x45e5…c218 | — | izi | 7 | 0 | 2:7 | — | `venue_not_loadable` |
| `0xd27672ad4665865a453706d110aeb1b54ecdb007` | cleopatra-cl | WMNT/mETH | v3 | 7 | 3 | 1:6 3:1 | 238 | `venue_not_loadable` |
| `0xf8090c06c9086ca9aba39a89d6792291d0a06fd2` | cleopatra-cl | GRAI/mETH | v3 | 7 | 0 | 1:1 2:5 3:1 | 306633 | `venue_not_loadable` |
| `0x214b8d4a67a996643cdb1bd80423a5f638cf258d` | butter | USDT/USDY | v3 | 6 | 0 | 2:6 | 2313 | `cycle_filter_rejected` |
| `0x263fd2e2715386e6feca3f9d6de2ad94819b501f` | agni-v3 | USDY/WETH | algebra | 6 | 0 | 2:6 | 12579 | `cycle_filter_rejected` |
| `0xa30d249bd55a4b1110b98319e23d1491a79c3447` | unregistered 0x5c84…fd2f | WBTC/LUSD | v2 | 6 | 0 | 2:6 | 6 | `venue_not_loadable` |
| `0xcb21dd38f1e7e0b06fabb01ae9bd849cd36e8296` | agni-v3 | USDY/WMNT | algebra | 6 | 0 | 1:4 3:2 | 23 | `below_tvl_floor` |
| `0x05c53a5233e7105cae6c37ee5a7bc7d43131625b` | unregistered 0x5bef…edec | MINU/mETH | v2 | 5 | 0 | 2:5 | 1298 | `venue_not_loadable` |
| `0x2afae423fe3ca40e7b24b48efd02e3e26d969395` | agni-v3 | USDY/mETH | algebra | 5 | 0 | 2:5 | 8750 | `cycle_filter_rejected` |
| `0x8be9c0a3e81f63cc0592302367fc673e6840fa55` | agni-v3 | WMNT/cmETH | algebra | 5 | 1 | 1:5 | 8 | `below_tvl_floor` |
| `0x8c252f73d16988a7d925efbce148bdc29182136f` | unregistered 0x5c84…fd2f | USDT/WETH | v2 | 5 | 1 | 2:5 | 2748 | `venue_not_loadable` |
| `0x96ad892fd4a1b05ff1158001dc743af7f472eb30` | unregistered 0x5c84…fd2f | USDC/WETH | v2 | 5 | 0 | 2:5 | 6888 | `venue_not_loadable` |
| `0xb11d56e78076df5b4fea0f3f9f1febdb043fabf3` | fusionx-v3 | USDT/WBTC | algebra | 5 | 0 | 2:5 | 17376 | `cycle_filter_rejected` |
| `0xc69a23ba0ce530de100d96ed16f3614fbf8610bf` | fusionx-v2 (interim) | WMNT/WBTC | v2 | 5 | 0 | 1:4 3:1 | 4 | `below_tvl_floor` |
| `0xd4b217cca4723aff8f9e66f47fb8fb8318881d7f` | unregistered 0xc848…3913 | mETH/WETH | v3 | 5 | 2 | 2:5 | 49028 | `venue_not_loadable` |
| `0xff1b3338ecab79fa82729c9f1eae370eedc5397d` | unregistered 0x45e5…c218 | — | izi | 5 | 0 | 1:5 | — | `venue_not_loadable` |
| `0x020fbfefa8ec960f369adac02e5857036ba04fc5` | unregistered 0x5c84…fd2f | USDT/LUSD | v2 | 4 | 0 | 2:4 | 10 | `venue_not_loadable` |
| `0x14bdf0998a2313f8e5772866fdac029f3d58eb2b` | unregistered 0x62db…51ce | USDC/WMNT | v2 | 4 | 0 | 1:3 3:1 | 31 | `venue_not_loadable` |
| `0x5865ec64e4966cedbd438affc1d60015d91a5a4d` | unregistered 0x45e5…c218 | — | izi | 4 | 0 | 2:4 | — | `venue_not_loadable` |
| `0x6a89ca7890bc91e38c4b9182cdb393fb76a327b0` | unregistered 0x5bef…edec | WMNT/Zoey | v2 | 4 | 0 | 1:4 | 57 | `venue_not_loadable` |
| `0x6be3b597c2d024adeb3b87b4c8c8b05b66b7457d` | agni-v3 | USDT/aUSD | algebra | 4 | 0 | 2:4 | 4533 | `cycle_filter_rejected` |
| `0x6ff1bb4e219b5b63fab86ccdd70ab001d2cc2e4e` | unregistered 0x5bef…edec | USDC/MOE | v2 | 4 | 0 | 2:4 | 1588 | `venue_not_loadable` |
| `0x945d9a4c5eece385760e3090e9e0e48a92baacdf` | unregistered 0x45e5…c218 | — | izi | 4 | 3 | 1:4 | — | `venue_not_loadable` |
| `0x99861df55112cacbea0bd56c225d7bd567be2d65` | unregistered 0xa9f2…82ac | WMNT/Zoey | v2 | 4 | 0 | 2:4 | 8974 | `venue_not_loadable` |
| `0xaaa87a36b92344436adcd880677e6842b227d931` | cleopatra-cl | USDC/WETH | v3 | 4 | 1 | 2:4 | 94415 | `venue_not_loadable` |
| `0xaede6c433bfb3fe714856d1b7b4b99690cc88d52` | unregistered 0x5c84…fd2f | WBTC/WETH | v2 | 4 | 0 | 2:4 | 16260 | `venue_not_loadable` |
| `0xbb27ccaa52d27ab055a7f9aaedb0f04372ea74aa` | unregistered 0x62db…51ce | USDC/USDT | v2 | 4 | 2 | 1:2 2:2 | 31699 | `venue_not_loadable` |
| `0xd372cd4acfcd646f9332b26c7b6bfa4777d90451` | agni-v3 | USDT/WETH | algebra | 4 | 1 | 2:4 | 928 | `below_tvl_floor` |
| `0x09ebe89cb1ff4bd36dab6eb7ee105518c5be2df4` | unregistered 0x5bcb…6ed0 | USDC/WMNT | v3 | 3 | 2 | 1:3 | 13 | `venue_not_loadable` |
| `0x198c826af31938736539e7025d81caa7b8952094` | unregistered 0x5bef…edec | WBTC/mETH | v2 | 3 | 0 | 2:3 | 26789 | `venue_not_loadable` |
| `0x2516bd7cdfb8ce8fc5d27055a72f8bb2d6227d13` | unregistered 0x5b54…f249 | USDY/WMNT | v2 | 3 | 0 | 1:2 2:1 | 2 | `venue_not_loadable` |
| `0x35fdc1d396fbe06a30d68bbea6a36c525600c1e9` | — | LEND/WMNT | v2 | 3 | 0 | 1:3 | 23 | `venue_not_loadable` |
| `0x43f4e7185a4141729a6f410ccf15f3fa2ab28753` | — | WMNT/LUSD | v2 | 3 | 0 | 1:3 | 0 | `venue_not_loadable` |
| `0x48b1f6683279b7e7ab4c2fa4c5c5b4a6f3f759ef` | butter | WMNT/LIZ | v3 | 3 | 0 | 1:3 | 31 | `below_tvl_floor` |
| `0x628f6a4b26bde4694ae6208e52d0aa2aff8ed6c1` | agni-v3 | USDT/axlUSDC | algebra | 3 | 0 | 2:3 | 4223692 | `cycle_filter_rejected` |
| `0x65f0371e1e67d1e2413058b67b051924de98aedf` | — | WMNT/PENDLE | v2 | 3 | 0 | 2:3 | 1127 | `venue_not_loadable` |
| `0x8a6bd8d3039fd9970386bb873c26525cd0e99980` | unregistered 0xadc1…b286 | USDT/WMNT | v3 | 3 | 1 | 1:3 | 11 | `venue_not_loadable` |
| `0x9937c0c191910ff75033a1a04d050e87d67f4a1e` | unregistered 0x3ace…ecc8 | USDT/LUSD | v2 | 3 | 0 | 2:3 | 7 | `venue_not_loadable` |
| `0xa474ee9dbdd528b9c79ea3c790dd6e5821d9307d` | — | USDC/STRAT | v2 | 3 | 0 | 2:3 | 88192 | `venue_not_loadable` |
| `0xa52de28b69755761f843293f0aa2d13fb6af6839` | butter | USDC/LIZ | v3 | 3 | 0 | 2:3 | 4063 | `cycle_filter_rejected` |
| `0xace8d484f775a78c0e65e25ecb417da077e6fdd9` | unregistered 0x211b…19cb | USDT/WMNT | v2 | 3 | 2 | 1:3 | 3 | `venue_not_loadable` |
| `0xbacd8c1591333ff6ec52610c72ae534a6e28c860` | fusionx-v2 (interim) | WBTC/mETH | v2 | 3 | 0 | 2:3 | 8508 | `cycle_filter_rejected` |
| `0xcd3848389078c1cd47038aef975f4c3ff7f8b31f` | cleopatra-cl | GRAI/CLEO | v3 | 3 | 0 | 2:3 | 781 | `venue_not_loadable` |
| `0xe0c9164358fa66092cb922b78712dbe9da459d0f` | — | USDY/GRAI | v2 | 3 | 0 | 2:3 | 243 | `venue_not_loadable` |
| `0xea21853a03e55943196e368dc84b268e330730ed` | fusionx-v3 | WMNT/PENDLE | algebra | 3 | 0 | 1:3 | 4 | `below_tvl_floor` |
| `0xec516ef93fa3a18a8520718f5327351b0bf7bf6d` | unregistered 0x3575…15a1 | USDC/WMNT | v3 | 3 | 1 | 1:3 | 7 | `venue_not_loadable` |
| `0xf9810e40f60f80efc51e7a4d62f7a591ad52d27e` | unregistered 0x5b54…f249 | USDC/USDT | v2 | 3 | 2 | 2:3 | 248254 | `venue_not_loadable` |
| `0x0521080f2aa43f6fe2186e232fc5b6f176643360` | unregistered 0x5b54…f249 | USDT/WMNT | v2 | 2 | 0 | 1:2 | 0 | `venue_not_loadable` |
| `0x086f766b336dfb0f705dc030db01993b22d81266` | uniswap-v3 | USDC/WMNT | v3 | 2 | 0 | 1:2 | 26 | `below_tvl_floor` |
| `0x0f28b5bddb45599f0b4f714fe76173bdb0046cfb` | butter | USDC/LIZ | v3 | 2 | 0 | 2:2 | 1899 | `cycle_filter_rejected` |
| `0x12a5d33c4f1416208704897c2e8b34d6e6d75528` | unregistered 0xfd6d…88ee | USDC/WMNT | v3 | 2 | 0 | 1:2 | 3 | `venue_not_loadable` |
| `0x1bae52e2b8e401de1429b7ca94bb0abbf133ae34` | unregistered 0xead1…5252 | USDT/WMNT | v3 | 2 | 2 | 1:2 | 285 | `venue_not_loadable` |
| `0x25fee1b220bbca5dbcb955cff59579e89af84c79` | unregistered 0x45e5…c218 | — | izi | 2 | 0 | 2:2 | — | `venue_not_loadable` |
| `0x2bba1c3e7aa1e44cfad7245a90e2f48009515a3c` | fusionx-v2 (interim) | USDT/PENDLE | v2 | 2 | 0 | 2:2 | 29 | `below_tvl_floor` |
| `0x3029281a795bb86e04b9fc0baf73ff73b5f58cca` | unregistered 0x76a7…75c3 | WMNT/LUSD | v3 | 2 | 0 | 1:2 | 0 | `venue_not_loadable` |
| `0x347bb5065eadd5f7cb5fd0a696137d49f38ac6cb` | unregistered 0x5bef…edec | USDT/MINU | v2 | 2 | 0 | 2:2 | 176 | `venue_not_loadable` |
| `0x39369a6f2fbab5ca439f6ab5d79fe3f6ac41bcad` | fusionx-v3 | USDT/LUSD | algebra | 2 | 0 | 2:2 | 106 | `below_tvl_floor` |
| `0x3943846ffd21be07656367ea0039666bd5d8efdb` | unregistered 0xc848…3913 | LEND/SLUSH | v3 | 2 | 0 | 2:2 | 1 | `venue_not_loadable` |
| `0x3af54cb286376a45ab14e7bb8a4d7589488cd462` | unregistered 0xe67a…65d0 | USDC/USDT | v3 | 2 | 1 | 2:2 | 504 | `venue_not_loadable` |
| `0x42b67d7e60861f8caa472a89ae567bd4f9180ac5` | unregistered 0xc848…3913 | WETH/cmETH | v3 | 2 | 0 | 2:2 | 13613 | `venue_not_loadable` |
| `0x4307275b74013469d13d04b7491e1104d5573f66` | butter | SHIB/WMNT | v3 | 2 | 0 | 1:2 | 30 | `below_tvl_floor` |
| `0x44b1a7a5882964f52487cc14f0a35709868b8fd4` | unregistered 0xc848…3913 | WMNT/SLUSH | v3 | 2 | 0 | 1:1 3:1 | 71 | `venue_not_loadable` |
| `0x5e96856da95fd745987fa7d2c31f86e2d2824e3d` | unregistered 0x69c4…a7b5 | USDT/WETH | v2 | 2 | 0 | 2:2 | 213 | `venue_not_loadable` |
| `0x60579ae29ce9ed724f1c9817e1b54a630e71ade4` | cleopatra-cl | WMNT/WETH | v3 | 2 | 1 | 1:2 | 4 | `venue_not_loadable` |
| `0x70bd9ea83b9a0fbcc8d62cba01579deda355ada0` | fusionx-v2 (interim) | WMNT/axlUSDC | v2 | 2 | 0 | 1:2 | 4 | `below_tvl_floor` |
| `0x773c76128de9dc1569de12800c6a8fafc21fa614` | unregistered 0x7928…c3e6 | USDC/WMNT | v2 | 2 | 1 | 1:2 | 5 | `venue_not_loadable` |
| `0x78b7e42b4a962ab5a08a30f8f28d1c29f986be6c` | unregistered 0x5c84…fd2f | WMNT/PENDLE | v2 | 2 | 0 | 1:2 | 1 | `venue_not_loadable` |
| `0x86e3a987187fed135d6d9c114f1857d8144f01e1` | unregistered 0x5bef…edec | mETH/WETH | v2 | 2 | 0 | 2:2 | 1382297 | `venue_not_loadable` |
| `0x8e53c676e342ffa487221c01cb1de693af8efc13` | unregistered 0x3ace…ecc8 | WMNT/WETH | v2 | 2 | 0 | 1:2 | 2 | `venue_not_loadable` |
| `0x903ccd72ece3400a838ef449cb2bc8a568888afb` | — | STRAT/WMNT | v2 | 2 | 0 | 1:2 | 63 | `venue_not_loadable` |
| `0x9a5d66503a246d127fa327a0999c7485571628ab` | — | USDC/LUSD | v2 | 2 | 0 | 2:2 | 212 | `venue_not_loadable` |
| `0xc1dd6d10532cf9388ef547b1a34c0aaf87403caf` | butter | USDC/SHIB | v3 | 2 | 0 | 2:2 | 1196 | `cycle_filter_rejected` |
| `0xd837008202a9715b95e629d281104354f961a3ec` | butter | USDC/USDY | v3 | 2 | 0 | 2:2 | 8775 | `cycle_filter_rejected` |
| `0xdf322a8958a6de31af04407b19777ad98fab2172` | unregistered 0x76a7…75c3 | USDC/WMNT | v3 | 2 | 0 | 1:2 | 1 | `venue_not_loadable` |
| `0xed3cbaa8078f7292704943a3650a85d42f241b04` | fusionx-v2 (interim) | USDC/PENDLE | v2 | 2 | 0 | 2:2 | 146 | `below_tvl_floor` |
| `0xf7b5113492b5f642075bbcaa02494df8f188cade` | cleopatra-cl | WMNT/CLEO | v3 | 2 | 0 | 1:1 3:1 | 159 | `venue_not_loadable` |
| `0xf90a2020b91c6d2e39bdfac4f42e4d3f925f4fc9` | unregistered 0xd234…0c39 | USDT/WMNT | v2 | 2 | 1 | 1:2 | 5 | `venue_not_loadable` |
| `0xf99ef8df4f8b62f6031cfc6a36388ce7da48e4b8` | — | USDT/WMNT | v2 | 2 | 1 | 1:2 | 12 | `venue_not_loadable` |
| `0x0522f049b3b1ab6d2a3b2e0f4628b109a164bf99` | unregistered 0x76a7…75c3 | USDT/WMNT | v3 | 1 | 0 | 1:1 | 36 | `venue_not_loadable` |
| `0x0b0f2b0d057382b53c2aebe29de596fc6558b9fd` | unregistered 0x69c4…a7b5 | USDT/WMNT | v2 | 1 | 0 | 1:1 | 1 | `venue_not_loadable` |
| `0x0ec6844a1f071dc7ecd54b5d28011ddd2371cda9` | unregistered 0x69c4…a7b5 | USDC/WMNT | v2 | 1 | 0 | 1:1 | 0 | `venue_not_loadable` |
| `0x10276e18a1987c604741319f64640064f18d503b` | unregistered 0xc848…3913 | USDC/mUSD | v3 | 1 | 0 | 2:1 | 1169153 | `venue_not_loadable` |
| `0x19a414a6b1743315c731492cb9b7b559d7db9ab7` | unregistered 0x5bef…edec | USDT/WETH | v2 | 1 | 0 | 2:1 | 412 | `venue_not_loadable` |
| `0x22b68c64d14bfe2e41f18bb3bfb3aa2ed123bec3` | butter | WMNT/LIZ | v3 | 1 | 0 | 1:1 | 2 | `below_tvl_floor` |
| `0x26ae1d5d4ea54a15fb7af64821dac6b0456b06ae` | agni-v3 | mETH/aUSD | algebra | 1 | 0 | 2:1 | 736 | `below_tvl_floor` |
| `0x2823f3a92dabe207ac8601f947225ae10212f39f` | unregistered 0x5bef…edec | $MDragon/WMNT | v2 | 1 | 0 | 1:1 | 12 | `venue_not_loadable` |
| `0x297badff77236228471f841f50a5d2d5ed943445` | butter | USDY/WMNT | v3 | 1 | 0 | 1:1 | 1 | `below_tvl_floor` |
| `0x2c4ad151ddaf790ec335139cdfa1c0df654edeb4` | fusionx-v3 | USDC/LUSD | algebra | 1 | 0 | 2:1 | 3 | `below_tvl_floor` |
| `0x2f1d1044c46e2cb10106758a5b45f50f90767d60` | — | USDT/LUSD | v2 | 1 | 0 | 2:1 | 3 | `venue_not_loadable` |
| `0x37a6b77f1a8ef09ac96e9cda3ed56f615802d713` | cleopatra-cl | USDC/WMNT | v3 | 1 | 1 | 1:1 | 2 | `venue_not_loadable` |
| `0x3f8f436edea013aa267230d86417fa46d218c917` | fusionx-v3 | USDT/WBTC | algebra | 1 | 0 | 2:1 | 1161 | `cycle_filter_rejected` |
| `0x42e1fe63df42771d6f113f8c355b52fc5ee3656b` | agni-v3 | WMNT/CAI | algebra | 1 | 0 | 1:1 | 124 | `below_tvl_floor` |
| `0x46e15789bd1eeb975551ea12f3eb74ae9409eb99` | agni-v3 | USDT/WETH | algebra | 1 | 0 | 2:1 | 260 | `below_tvl_floor` |
| `0x5249b13b5bb27f79b2842dc72d86df6e2215ad81` | unregistered 0x69c4…a7b5 | USDC/WETH | v2 | 1 | 0 | 1:1 | 479 | `venue_not_loadable` |
| `0x562a1a3979a4a10ac2e060cfa4b53cad8011604a` | unregistered 0x5bef…edec | USDT/LEND | v2 | 1 | 0 | 2:1 | 16 | `venue_not_loadable` |
| `0x5be13e7f8312417ed42c491233898fdd5fffcbe2` | v3fork-636ea2 | WMNT/WBTC | v3 | 1 | 0 | 1:1 | 14 | `below_tvl_floor` |
| `0x5ffa5839880d99e6239d32d03d270efbd7ab7725` | unregistered 0x5b54…f249 | WMNT/mUSD | v2 | 1 | 0 | 1:1 | 2 | `venue_not_loadable` |
| `0x651c9d1f9da787688225f49d63ad1623ba89a8d5` | agni-v3 | FBTC/mETH | algebra | 1 | 0 | 2:1 | 938668 | `cycle_filter_rejected` |
| `0x6f468fe756d4eff9ad93d9c8b3302893195870d0` | unregistered 0x106b…6568 | USDT/WMNT | v3 | 1 | 0 | 1:1 | 0 | `venue_not_loadable` |
| `0x6f5989a72638e19d5420d1b45c84e51d97e0a0e2` | cleopatra-cl | LEND/WETH | v3 | 1 | 0 | 2:1 | 1705 | `venue_not_loadable` |
| `0x70fdb2de8f94e4eb08acb8b0edc7eb042b3aeb13` | unregistered 0x66d5…b81f | USDT/WMNT | algebra | 1 | 0 | 1:1 | 7 | `venue_not_loadable` |
| `0x762b916297235dc920a8c684419e41ab0099a242` | — | WMNT/CLEO | v2 | 1 | 0 | 3:1 | 2501 | `venue_not_loadable` |
| `0x76d06bb0986333b9a178bdcfacca1acf5adeac86` | unregistered 0x45e5…c218 | — | izi | 1 | 0 | 2:1 | — | `venue_not_loadable` |
| `0x85092edf4f9e4f71b196c61b74ac58fdeb257ae9` | — | USDY/WMNT | v2 | 1 | 0 | 1:1 | 10 | `venue_not_loadable` |
| `0x90334606d1c9fee77f2f1b04f7c46048037ea893` | cleopatra-cl | USDC/USDT | v3 | 1 | 1 | 2:1 | 15914 | `venue_not_loadable` |
| `0x912bd909c7a48f0f798a2e99c91a3493e1dbdd0d` | unregistered 0x45e5…c218 | — | izi | 1 | 0 | 2:1 | — | `venue_not_loadable` |
| `0x939c570374c5b076f880765fbb1cfeaa52b464ef` | fusionx-v3 | LEND/WMNT | algebra | 1 | 0 | 2:1 | 89 | `below_tvl_floor` |
| `0x97437914330b7776b43b65edd72b400374ac6e5f` | fusionx-v2 (interim) | USDT/MINU | v2 | 1 | 0 | 2:1 | 5 | `below_tvl_floor` |
| `0xa2643252665a2024abadf9a8c51e924071323b12` | — | STRAT/WMNT | v2 | 1 | 0 | 1:1 | 6 | `venue_not_loadable` |
| `0xa823612ba61248da23d83849a6968b6322f4e1ca` | fusionx-v3 | WMNT/LUSD | algebra | 1 | 0 | 1:1 | 0 | `below_tvl_floor` |
| `0xa9827e4e80cfbd46608292f6c83c9ffdd26bf6b7` | butter | USDC/SHIB | v3 | 1 | 0 | 2:1 | 583 | `below_tvl_floor` |
| `0xaf50cd8c03096a416fc1b88e328bab3e09e0f175` | unregistered 0x5bef…edec | USDC/mETH | v2 | 1 | 0 | 2:1 | 2433 | `venue_not_loadable` |
| `0xb1c1df816ced51503622ec83c4c971247048eb9f` | fluxion-v3 | USDT/WMNT | v3 | 1 | 1 | 1:1 | 3 | `below_tvl_floor` |
| `0xb5d66e901d4967487b4cfdd7c378a9b25bed1b72` | butter | WMNT/mETH | v3 | 1 | 0 | 1:1 | 24 | `below_tvl_floor` |
| `0xb70f7b25fe962eab2dbd634c756b6f8251764609` | unregistered 0x5bef…edec | LEND/MOE | v2 | 1 | 0 | 2:1 | 43 | `venue_not_loadable` |
| `0xbb99ed86c39449be9ef2b58c1928aa2be57e29ba` | fusionx-v3 | USDC/LUSD | algebra | 1 | 0 | 2:1 | 8 | `below_tvl_floor` |
| `0xbba05bfc68f918727b07dc193db3127858e9bc77` | unregistered 0x8c7d…bb08 | — | izi | 1 | 0 | 1:1 | — | `venue_not_loadable` |
| `0xc17b69de3e210e15cbace08afa0eb1b0e3fb4b9d` | butter | WMNT/LIZ | v3 | 1 | 0 | 1:1 | 11 | `below_tvl_floor` |
| `0xc82039c14c54a6cd3c99e69685c9610c2b1c75ab` | butter | SHIB/WMNT | v3 | 1 | 0 | 1:1 | 5 | `below_tvl_floor` |
| `0xcf18b5874e62f00bdb81f80346598bd32aca4294` | unregistered 0x5bef…edec | mETH/aUSD | v2 | 1 | 0 | 2:1 | 120 | `venue_not_loadable` |
| `0xd95273e610e01a967f8c1c26ccc2ae85c395774a` | unregistered 0x76a7…75c3 | USDC/WMNT | v3 | 1 | 0 | 1:1 | 0 | `venue_not_loadable` |
| `0xd99882e3404b529d5120ef16de6d9a0a11d9c516` | fusionx-v3 | mETH/aUSD | algebra | 1 | 0 | 2:1 | 154 | `below_tvl_floor` |
| `0xd99ad5fdf58e8a61af5ebe1767f4ad1c6beef3f3` | unregistered 0x45e5…c218 | — | izi | 1 | 0 | 2:1 | — | `venue_not_loadable` |
| `0xda7f72d92f6f9e94088aa5dab31326d2f4623c70` | unregistered 0x5b54…f249 | WMNT/WBTC | v2 | 1 | 0 | 1:1 | 2 | `venue_not_loadable` |
| `0xe1b9ba1641ccde125aa1ae4f1fe7f6d1bf7dd0b0` | fusionx-v3 | WMNT/axlUSDC | algebra | 1 | 0 | 1:1 | 9 | `below_tvl_floor` |
| `0xe9bc0589974b779661a1c4ceda5b0bee8d9ceda5` | unregistered 0x5bef…edec | WMNT/FBTC | v2 | 1 | 0 | 1:1 | 3 | `venue_not_loadable` |
| `0xe9c5061bee08d3c7e3b80ca5b79142b36f8e0f9c` | unregistered 0xbaa8…554c | USDC/WMNT | v3 | 1 | 0 | 1:1 | 3 | `venue_not_loadable` |
| `0xf3523ea5202d0cf1d0168e5820af6b4964ce141c` | unregistered 0x76a7…75c3 | USDC/WMNT | v3 | 1 | 0 | 1:1 | 1 | `venue_not_loadable` |
| `0xf87178ba55e905ed705b0f4859bf5679311b5db5` | unregistered 0xae41…c93b | USDT/WMNT | algebra | 1 | 1 | 1:1 | 0 | `venue_not_loadable` |
| `0xfe9784bd857e35a209196fa44521b9146b6d749b` | unregistered 0xbbd8…c0c0 | USDC/WMNT | v3 | 1 | 0 | 1:1 | 1 | `venue_not_loadable` |

Hop positions read `position:count` over the ordered path — a pool that only ever appears at hop 1 is a different kind of gap from a mid-cycle one.

## Notes

* Scope rules mirror execution::peer_attribution (aggregator hop>50 → hop cap → non-WMNT settlement), so cause counts reconcile with WHI-957.
* "Arbs unlocked" counts only in-scope arbs. WHI-906's fully_executable includes arbs the strategy cannot take (hop>3, non-WMNT), so it reads higher.
* Cycle counts come from the production enumerator (arbitrage::pathfinder), not a re-implementation. Cold start is modelled linearly from WHI-936: 350 s at 130 pools (one pool per batch CREATE).
* Per-event rows are never written to the committed report; the arb dataset stays external (WHI-906 / WHI-956 contract).
