# Algebra-class V3 fork `0xc848bc…` (true Algebra surface)

## Identity
- **Family:** Algebra-style CL (not UniV3 slot0)
- **Factory:** `0xC848bc597903B4200b9427a3d7F61e3FF0553913`
- **CREATE2 deployer (`poolDeployer`):** `0x9dE2dEA5c68898eb4cb2DeaFf357DFB26255a4aa`
- **Sample pool:** `0xa4657555cbddc069ed3389ac03330020692b13c4` (USDC/USDT)
- Census `kind`: `v3` (misleading — see evidence). External name: **V3fork-c848**.

## On-chain evidence (pinned block 98950889)
### Present (Algebra-ish)
- `globalState()` **selector exists**: raw eth_call returns 6×32-byte words (see `globalstate_raw.txt`). A UniV3-shaped typed decode fails — needs Algebra ABI, not proof of absence.
- `tickTable(int16)` OK
- `liquidity()`, `tickSpacing()`, `fee()`, `factory()`, `token0()`, `token1()` OK
- Factory exposes `poolDeployer()`

### Missing vs UniV3/Agni drop-in
- **`slot0()` reverts** — this is the drop-in test failure (raw call in `globalstate_raw.txt`)
- Factory has no `feeAmountTickSpacing` (Algebra uses a different fee/spacing model)

## Verdict
**`adapter_required`**

## Broken layers
1. **State accessor** — `globalState` instead of `slot0` (true Algebra)
2. **Factory provenance** — separate poolDeployer + no UniV3 fee-tier table
3. **Possibly fee model** — dynamic / non-tier fees typical of Algebra

## bot_action
Do **not** treat as UniV3 drop-in. Do not confuse with census `algebra` tags on Agni/FusionX (those are false positives — see MATRIX census-tag note). File Algebra adapter only after human ack if coverage justifies it (9 pools, modest swap weight).
