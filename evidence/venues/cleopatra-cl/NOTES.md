# Cleopatra CL

## Identity
- **Family:** Uniswap V3–style CL
- **Factory:** `0xAAA32926fcE6bE95ea2c51cB4Fcb60836D320C42`
- **Sample pool:** `0xf79c37b8344c58467ec88c01b82c2fd8fccdbbd0` (USDT/mETH fee=500)
- Census `kind`: `v3`. External name: **CleopatraCL**.

## On-chain evidence (pinned block 98950889)
- Full UniV3 surface OK
- `globalState()` MISSING
- `slot0.feeProtocol = 17` (fits classic uint8; also fine under uint32 decode)
- Factory fee tiers: 500→10, 2500→0, 3000→60

## Verdict
**`drop_in_univ3_or_agni`**

## bot_action
Discovery-only second-pass under multi-factory V3 identity. Lower priority than Butter / FusionX V3 / Fluxion V3.
