# Merchant Moe Liquidity Book

## Identity
- **Family:** Trader Joe V2.1–style Liquidity Book
- **Factory (canonical pin):** `0xa6630671775c4EA2743840F9A5016dCf2A104054` = `CANONICAL_MOE_FACTORY`
- **Factory creation block:** `61_742_960`
- **Sample pairs:**
  - differential: `0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B` (bin_step=25)
  - CSV: `0x0142b6a053a1c5139fb84dd59f087b48f9cb89fb` (bin_step=100)

## On-chain evidence (pinned block 98797253)
- `getNumberOfLBPairs()` → 193 (CSV has 192 historical rows — close)
- `pair.getFactory()` → canonical factory
- `getTokenX` / `getTokenY` / `getBinStep` / `getActiveId` succeed
- Pair bytecode is **minimal proxy / clone** with immutables (tokenX, tokenY, binStep packed) — not full pair runtime in the pair address
- Swap event: `Swap(address,address,uint24,bytes32,bytes32,uint24,bytes32,bytes32)` topic0 `0xad7d6f97…`
- Fee model: bin-step / dynamic LB fees (not UniV2 parts-per-100_000)

## Verdict
**`drop_in_moe_lb`**

First-class bot protocol: `SelectedProtocol::Moe` / universe label `moe`.

## bot_action
Keep as first-pass factory. Already pinned and validated offline via pool-list schema.
