# Mantle DEX venue inventory (WHI-765)

Evidence-backed classification of DEX venues on **Mantle mainnet (chain id 5000)**
for the bot’s factory / pool-universe work (input to WHI-536 / WHI-793).

Classification is from **on-chain contract evidence**, not branding.

## Tooling note (mandatory)

`.gitignore` ignores `*.csv` with only `!data/poolLists_moe.csv` excepted.
A default `rg` / `grep` **silently returns empty** for `data/poolLists.csv`.

**Seed step used ignore-safe search:**

- `rg --no-ignore …` for everything under `data/`
- `git grep` for tracked sources (`config/`, `tests/`, etc.)

See `_seed/commands.txt`.

## Pinned block

All live `cast` / `eth_call` transcripts in this tree use:

| field | value |
| --- | --- |
| chain_id | 5000 |
| block_number | `98797253` |
| block_hash | `0xf5d59d7f26843427e9cfe90118a9446e2571894187cceb0e32012c589b0e0e19` |
| rpc | `https://rpc.mantle.xyz` |
| captured_at_utc | 2026-08-03T02:41:00Z |

Source: `_seed/pinned_block.txt`.

## Verdict vocabulary

| verdict | meaning |
| --- | --- |
| `drop_in_univ2` | UniV2 pair/factory surface (reserves, Sync/Swap topics, CREATE2 where applicable) |
| `drop_in_univ3_or_agni` | UniV3/Agni pool surface (slot0, fee uint24, Swap topic0 `0xc42079f9…`) |
| `drop_in_moe_lb` | Merchant Moe Liquidity Book (canonical factory pin) |
| `adapter_required` | exists but a named layer breaks drop-in (math / events / fee model / provenance / executor) |
| `unsupported` | on-chain but not usable with current families |
| `not_found` | no factory/pool evidence in seed set |

**Fee-mismatch risk** may be flagged on a `drop_in_*` row: structure is fine but the
bot’s hard-coded fee would mis-quote.

V2 fee units are **parts per `100_000`** (not “bps”). See `src/amms/uniswap_v2/mod.rs`
and `V2_FEE_DOMAIN_END = 100_000` in `tests/differential.rs`.

## Seed sources (union)

1. `data/poolLists.csv` — Protocol tags `Agni` (21), `FusionX` (7); V3 fee-tier rows
2. `data/poolLists_moe.csv` — Moe LB factory `0xa6630671…` (192 rows)
3. `config/gas_profiles/approved_pools.mantle_mainnet.json` — three CREATE2 domains
4. `tests/differential.rs` — `FUSIONX_*`, `AGNI_*`, `MOE_V1_*`, `MOE_LB_*` fixtures
5. Operator-named target **Fluxion** (search only → `not_found`)

## Layout

```
evidence/venues/
  README.md              # this file
  MATRIX.md              # classification matrix + WHI-536 factory recommendation
  _seed/                 # pin, seed commands, event topic0 table
  agni-v3/               # NOTES + cast transcript
  agni-v2/               # alias note (→ FusionX V2)
  fusionx-v2/            # NOTES + transcript + fee_inference
  fusionx-v3/            # NOTES + transcript
  moe-lb/                # NOTES + transcript
  moe-v1/                # NOTES + transcript + fee_inference
  fluxion/               # not_found search evidence
```

## Out of scope (per issue)

- Implementing adapters / new `SelectedProtocol` / CLI changes
- V2 unfiltered-fallback loader bug + `V2_FEE` doc-comment (sibling WHI-764)
- Manifest generator / TVL / digests / promotion (WHI-536 / WHI-793)
- Filing per-venue adapter issues (only after matrix human-ack)
- Enabling FusionX or Fluxion on production paths
