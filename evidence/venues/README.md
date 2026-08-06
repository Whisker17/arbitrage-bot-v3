# Mantle DEX venue inventory (WHI-765)

Evidence-backed classification of DEX venues on **Mantle mainnet (chain id 5000)**
for the bot’s factory / pool-universe work (input to WHI-536 / WHI-793).

Classification is from **on-chain contract evidence**, not branding.

## Tooling note (mandatory)

`.gitignore` has `*.csv` with exceptions `!data/poolLists_moe.csv` and
`!data/pool_universe.csv`. The legacy seed `data/poolLists.csv` remains ignored,
so a default `rg` / `grep` **silently returns empty** for it.

**Seed step used ignore-safe search:**

- `rg --no-ignore …` for everything under `data/`
- `git grep` for tracked sources (`config/`, `tests/`, etc.)

See `_seed/commands.txt`.

## Pinned blocks

First-wave venues (Agni / FusionX / Moe):

| field | value |
| --- | --- |
| chain_id | 5000 |
| block_number | `98797253` |
| block_hash | `0xf5d59d7f26843427e9cfe90118a9446e2571894187cceb0e32012c589b0e0e19` |
| rpc | `https://rpc.mantle.xyz` |
| captured_at_utc | 2026-08-03T02:41:00Z |

Fluxion follow-up (docs-sourced addresses + live probes):

| field | value |
| --- | --- |
| chain_id | 5000 |
| block_number | `98798309` |
| block_hash | `0x19f38c1be558e7ee78d6876b9dfcfa5972ae32871d3a0f61a4da56a8f59c53c1` |
| rpc | `https://rpc.mantle.xyz` |

Census expansion (WHI-906 top factories + drop-in vs adapter probes):

| field | value |
| --- | --- |
| chain_id | 5000 |
| block_number | `98950889` |
| block_hash | `0x64828853d72f962ce13ad0ce6dd6a587ae219fee71cd97d847624df03db16dfd` |
| rpc | `https://rpc.mantle.xyz` |
| captured_at_utc | 2026-08-06T16:02:15Z |

Source: `_seed/pinned_block.txt`, `_seed/census_expansion_pin.txt`,
`_seed/census_probe_raw.txt`, and per-venue transcripts.

## Verdict vocabulary

| verdict | meaning |
| --- | --- |
| `drop_in_univ2` | UniV2 pair/factory surface (reserves, Sync/Swap topics, CREATE2 where applicable) |
| `drop_in_univ3_or_agni` | UniV3/Agni pool surface (slot0, fee uint24, Swap topic0 `0xc42079f9…`) |
| `drop_in_moe_lb` | Merchant Moe Liquidity Book (canonical factory pin) |
| `adapter_required` | exists but a named layer breaks drop-in (math / events / fee model / provenance / executor) |
| `unsupported` | on-chain but not usable with current families — unused in this matrix |
| `not_found` | no factory/pool evidence in seed set (superseded for Fluxion once docs + live code were supplied) |

**Topic0 comparisons** use keccak of canonical event signatures
(`_seed/event_topics.txt`). Live `cast logs` windows around the pin were quiet
for some sample pools; empty log ranges are noted in those transcripts rather
than treated as a failed topic match.

**Fee-mismatch risk** may be flagged on a `drop_in_*` row: structure is fine but the
bot’s hard-coded fee would mis-quote.

V2 fee units are **parts per `100_000`** (not “bps”). See `src/amms/uniswap_v2/mod.rs`
and `V2_FEE_DOMAIN_END = 100_000` in `tests/differential.rs`.

## Seed sources (union)

1. `data/poolLists.csv` — Protocol tags `Agni` (21), `FusionX` (7); V3 fee-tier rows
2. `data/poolLists_moe.csv` — Moe LB factory `0xa6630671…` (192 rows)
3. `config/gas_profiles/approved_pools.mantle_mainnet.json` — three CREATE2 domains
4. `tests/differential.rs` — `FUSIONX_*`, `AGNI_*`, `MOE_V1_*`, `MOE_LB_*` fixtures
5. Operator-named target **Fluxion** — official contracts doc
   https://fluxion-network.gitbook.io/fluxion-network/developer-resources/contracts
   (repo seed had no addresses; live code + factory probes under `fluxion/`)
6. **WHI-906 census expansion** — factories weighted by 30d arb legs from
   external `pool_census.json` / `factory_names.json` (dataset stays outside
   this repo; probe transcripts are committed). Top targets: FusionX V3,
   Agni, Butter, iZi, Fluxion V3, Moe V1, plus secondary V3 forks and
   MantleSwap V2.

## Layout

```
evidence/venues/
  README.md              # this file
  MATRIX.md              # classification matrix + WHI-536 factory recommendation
  _seed/                 # pins, seed commands, event topic0, census probe raw log
  agni-v3/               # NOTES + cast transcript (+ census re-probe)
  agni-v2/               # alias note (→ FusionX V2)
  fusionx-v2/            # NOTES + transcript + fee_inference
  fusionx-v3/            # NOTES + transcript (+ census: algebra tag falsified)
  moe-lb/                # NOTES + transcript
  moe-v1/                # NOTES + transcript + fee_inference
  fluxion/               # V2 adapter_required + V3 drop_in
  butter/                # drop_in UniV3 (census expansion)
  izi/                   # adapter_required (state/tokenX/pointDelta)
  uniswap-v3/            # drop_in UniV3 (Mantle deployment)
  v3fork-636ea2/         # drop_in UniV3 fork
  cleopatra-cl/          # drop_in UniV3
  algebra-c848/          # true Algebra surface (adapter_required)
  mantleswap-v2/         # drop_in UniV2 (+ fee unmeasured)
```

## Out of scope (per issue)

- Implementing adapters / new `SelectedProtocol` / CLI changes
- Multi-factory V3 identity wiring (follow-up after human ack)
- V2 unfiltered-fallback loader bug + `V2_FEE` doc-comment (sibling WHI-764)
- Manifest generator / TVL / digests / promotion (WHI-536 / WHI-793)
- WHI-906 coverage scripts (separate issue; this issue only classifies venues)
- Filing per-venue adapter issues (only after matrix human-ack)
- Enabling new venues on production send paths
