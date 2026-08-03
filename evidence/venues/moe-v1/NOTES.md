# Merchant Moe V1 (classic AMM)

## Identity
- **Family:** Uniswap V2–style constant product (Merchant Moe “V1” classic pairs)
- **Factory:** `0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc` (`MOE_V1_FACTORY` in `tests/differential.rs`)
- **Router:** `0xeaEE7EE68874218c3558b40063c42B82D3E7232a`
- **Sample pair:** `0x4e7685df06201521f35a182467feefe02c53d847` (USDT/WMNT)

## On-chain evidence (pinned block 98797253)
- Live code on factory, pair, router
- `pair.factory()` → Moe V1 factory; `router.factory()` same
- `allPairsLength()` → 230
- Standard V2 surface (`getReserves`, token0/1)
- **Fee:** multi-sample router inference → unique match **`300 / 100_000` (0.3%)** — same as bot `V2_FEE = 300`

## Bot status
- Present only as differential capture candidate; **not** in `approved_pools`, pool CSVs, or `SelectedProtocol`
- Distinct factory from FusionX V2 and from Moe LB

## Verdict
**`drop_in_univ2`** (structurally; fee matches bot default)

## bot_action
Ignore for first-pass WHI-536 factory set (not in operator seeds / gas allowlist). Optional later discovery if product wants classic Moe V1 depth. Do not confuse with Moe LB.
