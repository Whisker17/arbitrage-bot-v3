# Mantle DEX classification matrix (WHI-765)

Primary pin (first-wave venues): **`98797253`** /
`0xf5d59d7f26843427e9cfe90118a9446e2571894187cceb0e32012c589b0e0e19`.

Fluxion follow-up pin: **`98798309`** /
`0x19f38c1be558e7ee78d6876b9dfcfa5972ae32871d3a0f61a4da56a8f59c53c1`.

(chain id 5000). Details: `README.md`, per-venue `NOTES.md` + `transcript.txt`.

Fee units for UniV2-style venues: **parts per `100_000`** (protocol-native V2 domain).
Fluxion V2 uses a **different** fee domain (Solidly-class; see row).

## Matrix

| venue | family | verdict | factory_or_deployer | sample_pool | fee_notes | bot_action | evidence_path |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Agni V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035`; CREATE2 deployer `0xe9827B4EBeB9AE41FC57efDdDd79EDddC2EA4d03` | `0xeAfc4D6d4c3391Cd4Fc10c85D2f5f972d58C0dD5` | pool `fee()` uint24 (e.g. 2500); tick spacing via factory | First-class `agni-v3`; enumerate | `agni-v3/` |
| FusionX V2 | univ2_cpmm | `drop_in_univ2` | factory `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c` | `0x3e5922cD0CeC71dc2d60eC8b36aa4C05B7c1672f` | **fee = `200 / 100_000` (0.2%)** live + fixture; bot `V2_FEE = 300` (0.3%) → **fee-mismatch risk** | Enumerate as UniV2 under correct FusionX identity; do not quote with fee 300; reclassify interim `agni-v2` label | `fusionx-v2/` |
| FusionX V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0x530d2766D1988CC1c000C8b7d00334c14B69AD71`; CREATE2 deployer `0x8790c2C3BA67223D83C8FCF2a5E3C650059987b4` | `0xD3d3127D9654f806370da592eb292eA0a347f0e3` | pool `fee()` uint24 (e.g. 2500); CSV FusionX rows belong here | Discovery-only / non-executable until explicit multi-factory V3 identity; do not merge into Agni CREATE2 domain | `fusionx-v3/` |
| Merchant Moe LB | moe_lb | `drop_in_moe_lb` | factory `0xa6630671775c4EA2743840F9A5016dCf2A104054` (`CANONICAL_MOE_FACTORY`) | `0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B` | LB bin-step / dynamic fees (not UniV2 domain) | First-class `moe`; enumerate | `moe-lb/` |
| Merchant Moe V1 (classic) | univ2_cpmm | `drop_in_univ2` | factory `0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc` | `0x4E7685Df06201521F35A182467FeEFe02C53d847` | fee = `300 / 100_000` (0.3%) live (matches bot default) | Ignore first-pass (not in seeds/allowlist); optional later | `moe-v1/` |
| Agni V2 (label) | — | **alias of FusionX V2** | same as FusionX V2 factory | same as FusionX V2 | see FusionX V2 | Retire interim naming; not a separate factory | `agni-v2/` |
| Fluxion V2 | solidly_vamm | `adapter_required` | PoolFactory `0x9336B143C572D75F1f2b7374532e8C96Eed41fe9` (impl `0x8D4b46B6…`) | `0xd85229cb09b3AFc0DB96180adeCC19Ae9d038ECe` (vAMM-USDC/WMNT) | `stableFee=5` / `volatileFee=30` / `MAX_FEE=300`; sample `getFee(pool,false)=30`; **not** UniV2 `/100_000` domain (Solidly-class, typically `/10_000`) | **Ignore permanently for now** — structurally needs Solidly adapter, but TVL is negligible (`allPoolsLength=1`; sample ~0.20 USDC + ~0.46 WMNT ≪ 1000 WMNT floor). **No adapter issue planned.** | `fluxion/` |
| Fluxion V3 | univ3_cl | `drop_in_univ3_or_agni` | factory `0xF883162Ed9c7E8EF604214c964c678E40c9B737C` (no separate `poolDeployer`) | `0xB1C1df816ceD51503622Ec83C4c971247048EB9F` (USDT/WMNT fee=3000) | UniV3 `uint24` fee; tiers 500/3000/10000 enabled (no Agni 2500) | Discovery-only; do not merge into Agni CREATE2 domain; pin CREATE2 later | `fluxion/` |

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

### Fluxion V2 — `adapter_required` (ignore — low TVL)
- Docs: https://fluxion-network.gitbook.io/fluxion-network/developer-resources/contracts
- Solidly/Velodrome surface: `getPool(a,b,stable)`, `allPools`, `stable()` / `vAMM-*` naming
- **Broken layers:** math (stable vs volatile curves), fee model (dual fees, non-UniV2 domain), factory provenance (clone impl + stable flag), events/executor unproven for UniV2 path
- **Product decision:** do **not** file a Solidly adapter. Factory has a single pool; sample reserves ≈ **0.20 USDC + 0.46 WMNT** (orders of magnitude under the universe 1000 WMNT TVL floor). Revisit only if liquidity becomes material.

### Fluxion V3 — `drop_in_univ3_or_agni`
- Docs as above; live factory + USDT/WMNT fee=3000 pool
- Selectors: `getPool`, `feeAmountTickSpacing`, pool `slot0`/`fee`/`liquidity`/`tickSpacing`
- No `poolDeployer()`; CREATE2 hash not yet reproduced
- Fee tiers match standard UniV3 (500/3000/10000), not Agni’s 2500 set

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
| `fluxion-v3` | `0xF883162Ed9c7E8EF604214c964c678E40c9B737C` | UniV3 ABI drop-in; no `SelectedProtocol`; CREATE2 not pinned; standard UniV3 fee tiers (not Agni 2500) |

### Ignore for first pass

| venue | reason |
| --- | --- |
| Merchant Moe V1 classic | Not in operator CSVs or approved_pools; optional later UniV2 source |
| Fluxion V2 | `adapter_required` structurally, but **no adapter planned** — single pool, sub-1 WMNT-class TVL |
| “Agni V2” as separate factory | Alias of FusionX V2 — no independent factory |

### Explicit non-goals of this recommendation
- Does **not** enable FusionX or Fluxion on production execution paths
- Does **not** change loaders, CLI, or `SelectedProtocol`
- Does **not** fix WHI-764 (V2 zero-match fallback / fee doc-comment)
- Adapter issues for fee wiring or multi-factory V3 are **not filed here** (per issue: file only after human matrix ack)
- **Fluxion V2 Solidly adapter is explicitly out of plan** given negligible TVL (operator decision)

## Human acknowledgement

This matrix needs a human ack (or reject list) before follow-up adapter/identity issues are filed. See Linear WHI-765 closing comment for the recommendation copy.
