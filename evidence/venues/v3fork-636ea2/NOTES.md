# V3 fork `0x636ea2…`

## Identity
- **Family:** Uniswap V3–style CL (unnamed fork)
- **Factory:** `0x636eA278699A300d3A849aB2cE36c891C4eE3Da0`
- **Sample pool:** `0x1b03630817c64cf69fc4bcbc70bc3acba1d63ce3` (USDT/WETH fee=500; appears in WHI-906 top-12 greedy list as `kind:v3`)
- Census `kind`: `v3`. External name: **V3fork-636ea2**.

## On-chain evidence (pinned block 98950889)
- Full UniV3 surface OK (`slot0`/`liquidity`/`fee`/`token0`/`token1`/`tickSpacing`)
- `globalState()` MISSING
- Factory fee tiers: 500→10, 2500→0, 3000→60 (standard UniV3, no Agni 2500)

## Verdict
**`drop_in_univ3_or_agni`**

## bot_action
Discovery-only second-pass. Structural drop-in; needs multi-factory V3 identity wiring.
