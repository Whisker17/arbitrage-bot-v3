# Butter (V3 CL)

## Identity
- **Family:** Uniswap V3–style concentrated liquidity
- **Factory:** `0xEECa0a86431A7B42ca2Ee5F479832c3D4a4c2644`
- **CREATE2 deployer:** none exposed (`poolDeployer()` reverts) — factory is the discovery address
- **Sample pools:**
  - `0x67f1e667ac60786b714b3D68A5aC35CC90441b73` (WMNT/WETH fee=500) — top greedy-rank V3 candidate in WHI-906
  - `0xf449ad0367829534f68161efef8d1c3b9e26f782` (USDT/WMNT fee=3000)
- Census `kind` tag: `v3` (correct). External `factory_names.json` labels it **Butter**.

## On-chain evidence (pinned block 98950889 /
`0x64828853d72f962ce13ad0ce6dd6a587ae219fee71cd97d847624df03db16dfd`)
- Pool surface matches UniV3/Agni: `slot0`, `liquidity`, `tickSpacing`, `fee`, `factory`, `token0`, `token1` all OK
- `globalState()` MISSING (not Algebra)
- `fee()` returns per-pool `uint24` (500 / 3000 observed) — readable, no hard-coded fee constant required
- Factory: `feeAmountTickSpacing(500)=10`, `(2500)=0` (not enabled), `(3000)=60`
- `getPool(WMNT,WETH,500)` → sample pool
- `slot0` decodes under the same layout as Agni control (`uint32 feeProtocol` field; values 170 and 0 observed — both fit the bot's Pancake-style width already used for Agni/FusionX)

## Verdict
**`drop_in_univ3_or_agni`**

## bot_action
**Enumerate as drop-in now** once a multi-factory V3 identity exists. Structural drop-in; needs factory registration + protocol tag only — no new AMM math. Distinct CREATE2 domain from Agni/FusionX; do not fold into Agni factory discovery.
