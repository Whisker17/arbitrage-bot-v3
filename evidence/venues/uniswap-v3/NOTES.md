# Uniswap V3 (Mantle deployment)

## Identity
- **Family:** Uniswap V3 CL
- **Factory:** `0x0d922Fb1Bc191F64970ac40376643808b4B74Df9`
- **Sample pool:** `0x48ef5640e71001cac842f5627a0bfec1ef09deb7` (mETH/WETH fee=100, tickSpacing=1)
- Census `kind`: `v3`. External name: **UniswapV3**.

## On-chain evidence (pinned block 98950889)
- Full UniV3 pool surface: `slot0`, `liquidity`, `tickSpacing`, `fee`, `factory`, `token0`, `token1` OK
- `globalState()` MISSING
- `feeProtocol` field in slot0 = 0 (classic UniV3 uint8-compatible)
- Factory `feeAmountTickSpacing(500)=10`, `(2500)=0`, `(3000)=60` — standard UniV3 tiers (no Agni 2500)
- Sample uses fee tier **100** (0.01 %) with tickSpacing **1**

## Verdict
**`drop_in_univ3_or_agni`**

## bot_action
Discovery-only / second-pass enumerate under multi-factory V3 identity. Lower arb-leg weight than FusionX V3 / Butter / Fluxion; still structurally free.
