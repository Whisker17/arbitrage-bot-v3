# FusionX V2

## Identity
- **Family:** Uniswap V2–style constant product
- **Factory:** `0xE5020961fA51ffd3662CDf307dEf18F9a87Cce7c`
- **init_code_hash:** `0x58c684aeb03fe49c8a3080db88e425fae262c5ef5bf0e8acffc0526c6e3c03a0`
- **Router:** `0xDd0840118bF9CCCc6d67b2944ddDfbdb995955FD`
- **Sample pair:** `0x3e5922cD0CeC71dc2d60eC8b36aa4C05B7c1672f` (USDT/WMNT)

## On-chain evidence (pinned block 98797253)
- `pair.factory()` → FusionX V2 factory
- `factory.getPair(USDT,WMNT)` → sample pair
- `factory.allPairsLength()` → 355
- CREATE2 with packed(token0,token1) salt + init_code_hash reproduces sample pair
- Standard V2 surface: `getReserves`, `token0`/`token1`, Sync/Swap topics
- **Fee:** live multi-sample router `getAmountsOut` search over domain `[0, 100_000]` yields unique match **`200 / 100_000` (0.2%)**. Confirms differential fixture `tests/fixtures/differential/uniswap_v2_fusionx_usdt_wmnt.json` (`state.fee = 200`, provenance `inferred_verified`).
- Bot hard-codes `V2_FEE = 300` (`src/service/protocol.rs`) → **fee-mismatch risk** under drop-in UniV2 math.

## Bot identity gap
- `universe_gen` currently labels this factory as interim **`agni-v2`** (not FusionX).
- No `SelectedProtocol::FusionX` exists.
- Gas/provenance already knows FusionX V2 (`approved_pools.mantle_mainnet.json`).

## Verdict
**`drop_in_univ2`** with **fee-mismatch risk** (true fee `200 / 100_000` vs bot `300 / 100_000`).

## bot_action
- Bucket: **`seed_no_amm_code`** — structure is UniV2 drop-in under a **correct venue identity** (not “Agni V2”).
- **Gate seed behind fee wiring:** do not put this factory on an executable path while the bot hard-codes `V2_FEE = 300`. Wire fee **`200 / 100_000`** (or per-pool fee) first. Sibling WHI-764 owns doc-comment / unfiltered-load; fee wiring is a separate follow-up.
- Reclassify the `agni-v2` interim label after human ack of this matrix.
