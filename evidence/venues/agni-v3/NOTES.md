# Agni V3

## Identity
- **Family:** Uniswap V3–style concentrated liquidity (Agni fork)
- **Factory:** `0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035`
- **CREATE2 deployer (poolDeployer):** `0xe9827B4EBeB9AE41FC57efDdDd79EDddC2EA4d03`
- **init_code_hash:** `0xaf9bd540c3449b723624376f906d8d3a0e6441ff18b847f05f4f85789ab64d9a`
- **Sample pool:** `0xeAfc4D6d4c3391Cd4Fc10c85D2f5f972d58C0dD5` (USDe/WMNT, fee=2500)

## On-chain evidence (pinned block 98797253)
- `factory.poolDeployer()` → deployer above
- `pool.factory()` → Agni factory
- `pool.fee()` → 2500 (protocol fee units: millionths of the notional, same as UniV3 `uint24 fee`)
- CREATE2 reproduction matches sample pool exactly (`cast create2`)
- `getPool(token0,token1,2500)` → sample pool
- `feeAmountTickSpacing(2500)` → 50
- Swap event signature is UniV3/Agni: `Swap(address,address,int256,int256,uint160,uint128,int24)` topic0 `0xc42079f9…`

## Verdict
**`drop_in_univ3_or_agni`**

First-class bot protocol: `SelectedProtocol::AgniV3` / universe label `agni-v3`.

## bot_action
Keep as first-pass factory for WHI-536 / `universe_gen` discovery. Enumerate via Agni factory.

## Census expansion (2026-08-06, block 98950889)

Census tags all Agni pools `kind: "algebra"`. Live check: `slot0` OK, `globalState` MISSING — **not Algebra**. Same uint32-style `feeProtocol` packing as FusionX V3. No change to verdict or bot_action.
