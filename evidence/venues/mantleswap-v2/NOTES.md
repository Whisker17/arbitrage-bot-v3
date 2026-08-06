# MantleSwap V2

## Identity
- **Family:** Uniswap V2–style CPMM
- **Factory:** `0x5c84e5d27fc7575D002fe98c5A1791Ac3ce6fD2f`
- **Sample pool:** `0x94c400B9Eb9d371299143d7B1Af1202f0f956d73` (WMNT/WETH)
- Census `kind`: `v2`. External name: **MantleSwap**. Factory `allPairsLength()=33`.

## On-chain evidence (pinned block 98950889)
- `getReserves()`, `factory()`, `token0()`, `token1()`, `kLast()` OK
- No `slot0` / V3 surface
- No on-chain `fee()` / `swapFee()` accessor on pair or factory (probes revert)
- Fee **not measured live** in this pass — treat as **fee-mismatch risk** until a differential/getAmountsOut capture pins the fee in the UniV2 domain (parts per `100_000`)

## Verdict
**`drop_in_univ2`** (+ fee-mismatch risk until measured)

## bot_action
Optional later UniV2 source. Lower arb weight than Merchant Moe V1 / FusionX V2. Do not quote with hard-coded `V2_FEE=300` until fee is measured.
