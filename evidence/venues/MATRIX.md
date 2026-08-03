# Mantle DEX classification matrix (WHI-765)

Pinned evidence block: **`98797253`** /
`0xf5d59d7f26843427e9cfe90118a9446e2571894187cceb0e32012c589b0e0e19`
(chain id 5000). Details: `README.md`, per-venue `NOTES.md` + `transcript.txt`.

Fee units for UniV2-style venues: **parts per `100_000`** (protocol-native V2 domain).

## Matrix

| venue | family | verdict | factory_or_deployer | sample_pool | fee_notes | bot_action | evidence_path |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Agni V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035`; CREATE2 deployer `0xe9827B4EBeB9AE41FC57efDdDd79EDddC2EA4d03` | `0xeAfc4D6d4c3391Cd4Fc10c85D2f5f972d58C0dD5` | pool `fee()` uint24 (e.g. 2500); tick spacing via factory | First-class `agni-v3`; enumerate | `agni-v3/` |
| FusionX V2 | univ2_cpmm | `drop_in_univ2` | factory `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c` | `0x3e5922cD0CeC71dc2d60eC8b36aa4C05B7c1672f` | **fee = `200 / 100_000` (0.2%)** live + fixture; bot `V2_FEE = 300` (0.3%) → **fee-mismatch risk** | Enumerate as UniV2 under correct FusionX identity; do not quote with fee 300; reclassify interim `agni-v2` label | `fusionx-v2/` |
| FusionX V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x530d2766D1988CC1c000C8b7d00334c14B69AD71`; CREATE2 deployer `0x8790c2C3BA67223D83C8FCF2a5E3C650059987b4` | `0xD3d3127D9654f806370da592eb292eA0a347f0e3` | pool `fee()` uint24 (e.g. 2500); CSV FusionX rows belong here | Discovery-only / non-executable until explicit multi-factory V3 identity; do not merge into Agni CREATE2 domain | `fusionx-v3/` |
| Merchant Moe LB | moe_lb | `drop_in_moe_lb` | factory `0xa6630671775c4EA2743840F9A5016dCf2A104054` (`CANONICAL_MOE_FACTORY`) | `0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B` | LB bin-step / dynamic fees (not UniV2 domain) | First-class `moe`; enumerate | `moe-lb/` |
| Merchant Moe V1 (classic) | univ2_cpmm | `drop_in_univ2` | factory `0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc` | `0x4E7685Df06201521F35A182467FeEFe02C53d847` | fee = `300 / 100_000` (0.3%) live (matches bot default) | Ignore first-pass (not in seeds/allowlist); optional later | `moe-v1/` |
| Agni V2 (label) | — | **alias of FusionX V2** | same as FusionX V2 factory | same as FusionX V2 | see FusionX V2 | Retire interim naming; not a separate factory | `agni-v2/` |
| Fluxion | — | `not_found` | — | — | — | Ignore until a factory address is supplied | `fluxion/` |

### Protocol tags in tracked pool CSVs

| source | Protocol / identity | maps to matrix row |
| --- | --- | --- |
| `data/poolLists.csv` | `Agni` (21 rows, fee tiers) | **Agni V3** |
| `data/poolLists.csv` | `FusionX` (7 rows, fee tiers) | **FusionX V3** |
| `data/poolLists_moe.csv` | factory column only | **Merchant Moe LB** |
| (no CSV tag) | bot interim `agni-v2` | **alias of FusionX V2** |

### Distinct factories in `approved_pools.mantle_mainnet.json`

| approved_pools `protocol` | factory / deployer field | matrix row |
| --- | --- | --- |
| `uniswap_v2` | `0xE5020961…` (FusionXFactory) | FusionX V2 |
| `uniswap_v3` | `0x8790c2C3…` (FusionXV3PoolDeployer) | FusionX V3 |
| `agni` | `0xe9827B4E…` (Agni PoolDeployer) | Agni V3 |

## Drop-in evidence summary

### Agni V3 — `drop_in_univ3_or_agni`
- Selectors: `poolDeployer`, `getPool`, `feeAmountTickSpacing`, pool `slot0`/`fee`/`liquidity`
- CREATE2: deployer + `init_code_hash` reproduces sample pool
- Swap topic0: UniV3/Agni `0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67`

### FusionX V2 — `drop_in_univ2` (+ fee-mismatch risk)
- Selectors: `factory`, `getPair`, `allPairsLength`, `getReserves`, `token0`/`token1`
- CREATE2: factory as deployer + packed(token0,token1) salt + init_code_hash → sample pair
- Sync topic0 `0x1c411e9a…`, Swap topic0 `0xd78ad95f…`
- Fee `200 / 100_000` confirmed at pin block and in differential fixture

### FusionX V3 — `drop_in_univ3_or_agni`
- Same ABI family as Agni; **distinct** factory, deployer, init_code_hash
- CREATE2 reproduces sample; CSV FusionX pools’ `factory()` → FusionX V3 factory

### Merchant Moe LB — `drop_in_moe_lb`
- Canonical factory pin; `getNumberOfLBPairs`, pair `getFactory`/`getTokenX`/`getTokenY`/`getBinStep`
- Pairs are minimal proxies with immutables; Swap topic0 `0xad7d6f97…`

### Merchant Moe V1 — `drop_in_univ2`
- Live factory/pair/router; fee `300 / 100_000`
- Not first-class; differential-only seed

## Recommended first-pass factory set for WHI-536 / universe generator

### Enumerate as drop-in **now** (first generator pass)

| label (suggested) | factory (discovery address) | CREATE2 deployer if different | notes |
| --- | --- | --- | --- |
| `agni-v3` | `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035` | `0xe9827B4EBeB9AE41FC57efDdDd79EDddC2EA4d03` | Already first-class |
| `moe` | `0xa6630671775c4EA2743840F9A5016dCf2A104054` | n/a (LB factory) | Already first-class |
| `fusionx-v2` (or keep interim `agni-v2` **only** with explicit doc) | `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c` | factory is deployer | **Drop-in UniV2 structure**; quotes require fee **`200 / 100_000`**, not bot default 300. Prefer renaming off “Agni V2” after human ack. |

### Discovery-only / non-executable pending identity wiring

| label | factory | reason |
| --- | --- | --- |
| `fusionx-v3` | `0x530d2766D1988CC1c000C8b7d00334c14B69AD71` (deployer `0x8790c2C3…`) | ABI drop-in, but no `SelectedProtocol` / currently filtered out of V3 CSV seed; do not silently fold into Agni factory discovery |

### Ignore for first pass

| venue | reason |
| --- | --- |
| Merchant Moe V1 classic | Not in operator CSVs or approved_pools; optional later UniV2 source |
| Fluxion | `not_found` |
| “Agni V2” as separate factory | Alias of FusionX V2 — no independent factory |

### Explicit non-goals of this recommendation
- Does **not** enable FusionX on production execution paths
- Does **not** change loaders, CLI, or `SelectedProtocol`
- Does **not** fix WHI-764 (V2 zero-match fallback / fee doc-comment)
- Adapter issues for fee wiring or multi-factory V3 are **not filed here** (per issue: file only after human matrix ack)

## Human acknowledgement

This matrix needs a human ack (or reject list) before follow-up adapter/identity issues are filed. See Linear WHI-765 closing comment for the recommendation copy.
