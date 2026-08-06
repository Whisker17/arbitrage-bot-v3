# Mantle DEX classification matrix (WHI-765)

## Pins

| wave | block | hash | notes |
| --- | --- | --- | --- |
| First-wave (Agni / FusionX / Moe / Fluxion seed) | `98797253` | `0xf5d59d7f26843427e9cfe90118a9446e2571894187cceb0e32012c589b0e0e19` | PR #63 |
| Fluxion docs follow-up | `98798309` | `0x19f38c1be558e7ee78d6876b9dfcfa5972ae32871d3a0f61a4da56a8f59c53c1` | PR #63 |
| **Census expansion (WHI-906 factories)** | **`98950889`** | **`0x64828853d72f962ce13ad0ce6dd6a587ae219fee71cd97d847624df03db16dfd`** | this pass |

Chain id **5000**. Details: `README.md`, per-venue `NOTES.md` + transcripts.

Fee units for UniV2-style venues: **parts per `100_000`** (protocol-native V2 domain only; never basis-point labels for these numbers).

## Census-tag warning (read first)

`pool_census.json` (external WHI-906 dataset) tags **Agni** and **FusionX V3** factories as `kind: "algebra"`. Live accessor probes **falsify** that:

- Agni + FusionX V3: `slot0` OK, `globalState` MISSING → **UniV3-family**, not Algebra.
- True Algebra surface found only on factory `0xC848bc59…` (`slot0` MISSING, `globalState` + `tickTable` present).

**Never add a pool to the universe from a census `kind` alone.** Classification is from contract evidence below.

## Matrix

| venue | family | verdict | factory_or_deployer | sample_pool | fee_notes | bot_action | evidence_path |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Agni V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035`; deployer `0xe9827B4E…` | `0x1858d52c…` / `0xeAfc4D6d…` | pool `fee()` uint24; Agni tiers include 2500; census wrongly tags `algebra` | First-class `agni-v3`; enumerate | `agni-v3/` |
| FusionX V2 | univ2_cpmm | `drop_in_univ2` | factory `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c` | `0x3e5922cD…` | **fee = `200 / 100_000` (0.2%)** live + fixture; bot `V2_FEE = 300` (0.3%) → **fee-mismatch risk** | Enumerate as UniV2 under FusionX identity; do not quote with fee 300; reclassify interim `agni-v2` | `fusionx-v2/` |
| FusionX V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x530d2766D1988CC1c000C8b7d00334c14B69AD71`; deployer `0x8790c2C3…` | `0x262255f4…` (USDT/WMNT fee=500; top uncovered arb pool) | pool `fee()` uint24; `feeProtocol` uint32 width (222825800) already handled by existing V3 reader; census wrongly tags `algebra` | **First-pass multi-factory V3 enumerate** (identity wiring required; do not merge CREATE2 into Agni) | `fusionx-v3/` |
| Butter | univ3_cl | `drop_in_univ3_or_agni` | factory `0xEECa0a86431A7B42ca2Ee5F479832c3D4a4c2644` (no `poolDeployer`) | `0x67f1e667…` (WMNT/WETH fee=500) | per-pool `fee()`; tiers 500/3000 (no 2500) | **First-pass multi-factory V3 enumerate** | `butter/` |
| Merchant Moe LB | moe_lb | `drop_in_moe_lb` | factory `0xa6630671775c4EA2743840F9A5016dCf2A104054` | `0x2612E328…` / `0x1606c79b…` | LB bin-step / dynamic fees | First-class `moe`; enumerate | `moe-lb/` |
| Merchant Moe V1 (classic) | univ2_cpmm | `drop_in_univ2` | factory `0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc` | `0x4E7685Df…` / `0x7c88dd67…` | fee = `300 / 100_000` (matches bot default) | Optional later UniV2; ignore first-pass if capacity limited | `moe-v1/` |
| Fluxion V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0xF883162Ed9c7E8EF604214c964c678E40c9B737C` | `0xB1C1df81…` / `0x361052be…` | UniV3 uint24 fee; tiers 500/3000/10000 | **First-pass multi-factory V3 enumerate** (high swap volume) | `fluxion/` |
| Fluxion V2 | solidly_vamm | `adapter_required` | PoolFactory `0x9336B143…` | `0xd85229cb…` | Solidly dual fees; **not** UniV2 `/100_000` domain | **Ignore** — negligible TVL; no adapter planned | `fluxion/` |
| Uniswap V3 (Mantle) | univ3_cl | `drop_in_univ3_or_agni` | factory `0x0d922Fb1Bc191F64970ac40376643808b4B74Df9` | `0x48ef5640…` (fee=100) | standard UniV3 tiers + 100 | Second-pass multi-factory V3 | `uniswap-v3/` |
| V3fork-636ea2 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x636eA278699A300d3A849aB2cE36c891C4eE3Da0` | `0x1b036308…` | standard UniV3 tiers | Second-pass multi-factory V3 | `v3fork-636ea2/` |
| Cleopatra CL | univ3_cl | `drop_in_univ3_or_agni` | factory `0xAAA32926fcE6bE95ea2c51cB4Fcb60836D320C42` | `0xf79c37b8…` | standard UniV3 tiers | Second-pass multi-factory V3 | `cleopatra-cl/` |
| MantleSwap V2 | univ2_cpmm | `drop_in_univ2` | factory `0x5c84e5d27fc7575D002fe98c5A1791Ac3ce6fD2f` | `0x94c400B9…` | fee **unmeasured** on-chain → fee-mismatch risk | Optional later UniV2; measure fee before quoting | `mantleswap-v2/` |
| iZiSwap | izi_cl | `adapter_required` | factory `0x45e5F26451CDB01B0fA1f8582E0aAD9A6F27C218` | `0x98d1e99d…` | has `fee()` uint24; state via `state()` not `slot0` | **Do not enumerate** until iZi adapter; ~19 coverage points (WHI-906) | `izi/` |
| Algebra-class `0xc848bc…` | algebra_cl | `adapter_required` | factory `0xC848bc597903B4200b9427a3d7F61e3FF0553913`; deployer `0x9dE2dEA5…` | `0xa4657555…` | `globalState` + `tickTable`; **no `slot0`** | Do not treat as UniV3; true Algebra surface | `algebra-c848/` |
| Agni V2 (label) | — | **alias of FusionX V2** | same as FusionX V2 | same | see FusionX V2 | Retire interim naming | `agni-v2/` |

### Protocol tags in tracked pool CSVs

| source | Protocol / identity | maps to matrix row |
| --- | --- | --- |
| `data/poolLists.csv` | `Agni` (fee-tier rows) | **Agni V3** |
| `data/poolLists.csv` | `FusionX` (fee-tier rows) | **FusionX V3** |
| `data/poolLists_moe.csv` | factory column only | **Merchant Moe LB** |
| (no CSV tag) | bot interim `agni-v2` | **alias of FusionX V2** |

Seed step for CSVs used `rg --no-ignore` (see `README.md` / `_seed/commands.txt`).

### Distinct factories in `approved_pools.mantle_mainnet.json`

| approved_pools `protocol` | factory / deployer field | matrix row |
| --- | --- | --- |
| `uniswap_v2` | `0xE5020961…` (FusionXFactory) | FusionX V2 |
| `uniswap_v3` | `0x8790c2C3…` (FusionXV3PoolDeployer) | FusionX V3 |
| `agni` | `0xe9827B4E…` (Agni PoolDeployer) | Agni V3 |

### Top factories by arb legs (WHI-906 census) → matrix

| arb legs (30d) | factory | matrix row | verdict |
| --- | ---: | --- | --- |
| 5,609 | `0x530d2766…` | FusionX V3 | `drop_in_univ3_or_agni` |
| 5,381 | `0x25780dc8…` | Agni V3 | `drop_in_univ3_or_agni` |
| 2,217 | `0xeeca0a86…` | Butter | `drop_in_univ3_or_agni` |
| 2,170 | `0x45e5f264…` | iZiSwap | `adapter_required` |
| 1,750 | `0xf883162e…` | Fluxion V3 | `drop_in_univ3_or_agni` |
| 1,411 | `0x5bef015c…` | Merchant Moe V1 | `drop_in_univ2` |

## Drop-in evidence summary (census expansion)

### FusionX V3 top pool `0x262255f4…` vs Agni control (slot0 field-for-field)

| Field | FusionX V3 top | Agni control |
| --- | --- | --- |
| `sqrtPriceX96` | ~1.241e35 | ~1.241e35 |
| `tick` | 285303 | 285314 |
| `observationCardinality` | 1000 | 1 |
| `feeProtocol` | **222825800** | **222825800** |
| `unlocked` | true | true |
| `globalState` | MISSING | MISSING |

Verdict: **same reader path**. Census `algebra` tag is a false positive.

### Butter — UniV3 drop-in
- Selectors: `slot0`, `liquidity`, `fee`, `tickSpacing`, `token0`/`token1`, `factory`
- Factory `getPool` + `feeAmountTickSpacing` present; no separate deployer
- Per-pool fee readable (safe vs hard-coded fee)

### iZiSwap — adapter_required
- Broken layers: **state accessor** (`state` not `slot0`), **naming** (`tokenX`/`tokenY`), **tick/point model** (`pointDelta`), **liquidity accessor**

### Algebra-class `0xc848bc…` — adapter_required
- Broken layers: **state accessor** (`globalState` not `slot0`), **factory provenance**, **fee model** (no UniV3 fee-tier table)

## Recommended factory set for WHI-536 / universe generator

### Enumerate as drop-in **now** (first generator / coverage pass)

| label (suggested) | factory (discovery address) | CREATE2 deployer if different | notes |
| --- | --- | --- | --- |
| `agni-v3` | `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035` | `0xe9827B4E…` | Already first-class |
| `moe` | `0xa6630671775c4EA2743840F9A5016dCf2A104054` | n/a | Already first-class |
| `fusionx-v2` | `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c` | factory is deployer | Fee **`200 / 100_000`** — do not use bot default 300 |
| `fusionx-v3` | `0x530d2766D1988CC1c000C8b7d00334c14B69AD71` | `0x8790c2C3…` | **Highest missing V3 coverage**; needs multi-factory V3 identity |
| `butter` | `0xEECa0a86431A7B42ca2Ee5F479832c3D4a4c2644` | n/a | Drop-in UniV3; multi-factory V3 identity |
| `fluxion-v3` | `0xF883162Ed9c7E8EF604214c964c678E40c9B737C` | n/a | Drop-in UniV3; high volume |

### Second-pass drop-in (identity already multi-factory V3)

| label | factory | reason |
| --- | --- | --- |
| `uniswap-v3` | `0x0d922Fb1…` | Lower arb weight; structural free |
| `v3fork-636ea2` | `0x636eA278…` | Appears in greedy top-12; free |
| `cleopatra-cl` | `0xAAA32926…` | Free; lower priority |

### Adapter-required (do not seed into UniV2/V3/Moe paths)

| venue | broken layers | phase |
| --- | --- | --- |
| iZiSwap | state accessor, naming, point model, liquidity | Phase 2 (~19 coverage points) |
| Algebra `0xc848bc…` | state accessor, provenance, fee model | Only if coverage justifies |
| Fluxion V2 | math, fee model, provenance | **No adapter** — negligible TVL |

### Ignore / alias

| venue | reason |
| --- | --- |
| Merchant Moe V1 | Optional UniV2 later |
| MantleSwap V2 | Optional UniV2 later; fee unmeasured |
| “Agni V2” as separate factory | Alias of FusionX V2 |

### Coverage implication (from WHI-906, not re-measured here)

Adding **all drop-in** V2/V3 factories (no new AMM math) is estimated to move fully-executable arbs from **~4.4 % → ~79 %** of the 30d market. iZi adapter is the last major slice (~98 %). **This research confirms the drop-in side of that claim for the top factories by arb legs.**

### Explicit non-goals
- Does **not** enable new venues on production send paths
- Does **not** implement multi-factory V3 identity / `SelectedProtocol` variants / loaders
- Does **not** fix WHI-764 (V2 zero-match fallback / fee doc-comment)
- Does **not** file per-venue adapter issues until human matrix ack
- Does **not** implement WHI-906 scripts (separate issue)

## Human acknowledgement

This expanded matrix needs a human ack (or reject list) before follow-up identity/adapter tickets are filed. See Linear WHI-765 for the recommendation copy.
