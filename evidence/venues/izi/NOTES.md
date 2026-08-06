# iZiSwap

## Identity
- **Family:** iZiSwap concentrated-liquidity (not Uniswap V3)
- **Factory:** `0x45e5F26451CDB01B0fA1f8582E0aAD9A6F27C218`
- **Sample pool:** `0x98d1e99d294e8603fa050ea129c78388408e0dd1` (WMNT/WETH; 1,098 swaps in 30d census)
- Census `kind` tag: `izi` (correct). External name: **iZiSwap**.

## On-chain evidence (pinned block 98950889)
### Present (iZi surface)
- `state()` OK — price/point packing (not `slot0`)
- `tokenX()` / `tokenY()` OK (not `token0`/`token1`)
- `pointDelta()` OK (=10; analogous to tick spacing)
- `fee()` OK (uint24, sample=500)
- `factory()` OK
- Factory `pool(tokenA,tokenB,fee)` OK

### Missing (broken layers vs UniV3/Agni reader)
- `slot0()` — **MISSING**
- `liquidity()` — **MISSING**
- `tickSpacing()` — **MISSING**
- `token0()` / `token1()` — **MISSING**
- `globalState()` — **MISSING** (not Algebra either)

## Verdict
**`adapter_required`**

## Broken layers
1. **State accessor** — `state()` instead of `slot0`
2. **Naming** — `tokenX`/`tokenY` instead of `token0`/`token1`
3. **Tick model** — `pointDelta` / points instead of ticks
4. **Liquidity accessor** — no UniV3 `liquidity()`

## bot_action
**Do not enumerate** into the UniV3 path. Phase-2 adapter issue after human ack of this matrix. WHI-906 estimates ~19 coverage points once an adapter exists (29 pools).
