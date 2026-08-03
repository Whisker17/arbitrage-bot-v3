# Fluxion (Mantle mainnet)

## Source

Official docs (operator-supplied):  
https://fluxion-network.gitbook.io/fluxion-network/developer-resources/contracts

Docs list separate **AMM V2** and **AMM V3** deployments. Both have live bytecode on
chain id 5000 (verified at pin below). Earlier seed-only repo search found nothing;
that was an incomplete seed, not absence on chain.

## Pin

| field | value |
| --- | --- |
| block | `98798309` |
| hash | `0x19f38c1be558e7ee78d6876b9dfcfa5972ae32871d3a0f61a4da56a8f59c53c1` |
| rpc | `https://rpc.mantle.xyz` |

(First-wave venues were pinned at `98797253`; Fluxion follow-up uses this later pin.)

## Documented addresses

### AMM V2

| role | address |
| --- | --- |
| Pool Implementation | `0x8D4b46B6ce7C59Ec42eea9e67b0eeFFd29D921E6` |
| PoolFees Implementation | `0xac130562C9406b87A84883437C39190a009e27b1` |
| PoolFactory | `0x9336B143C572D75F1f2b7374532e8C96Eed41fe9` |
| FactoryRegistry | `0x47c401407F11482d562E2c00b67944c379fD8710` |
| Router | `0xd772E655af24Fe5Af92504D613D1Da0d9cFb6408` |

### AMM V3

| role | address |
| --- | --- |
| Factory | `0xF883162Ed9c7E8EF604214c964c678E40c9B737C` |
| “WETH” in docs | `0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8` (**WMNT** on Mantle) |
| SwapRouter | `0x5628a59dF0ECAC3f3171f877A94bEb26BA6DFAa0` |
| PositionManager | `0x2b70C4e7cA8E920435A5dB191e066E9E3AFd8DB3` |
| QuoterV2 | `0x3E4eE18Ac7280813236a1EB850679Da5322E14CE` |

---

## Fluxion V2 — verdict: `adapter_required`

**Family:** Solidly / Velodrome-style volatile–stable AMM (not UniV2 CPMM).

### On-chain surface
- Factory: `allPoolsLength() = 1`, `allPools(0)`, `implementation()`, `getPool(tokenA,tokenB,stable)`, `isPool`, `stableFee` / `volatileFee` / `MAX_FEE`, `getFee(pool, bool)`
- **Not** UniV2: `allPairsLength` / `getPair(address,address)` revert
- Sample pool `0xd85229cb09b3AFc0DB96180adeCC19Ae9d038ECe`:
  - `name()` = `"Volatile AMM - USDC/WMNT"`, `symbol()` = `"vAMM-USDC/WMNT"`
  - `stable() = false`, `factory()` → PoolFactory
  - `metadata()` returns dec0/dec1/r0/r1/stable/token0/token1
  - `getReserves()` works; pool-level `getAmountOut(amount, token)` present
- Router: `defaultFactory()` → PoolFactory (no `factory()`)

### Fee model
- `stableFee() = 5`, `volatileFee() = 30`, `MAX_FEE() = 300`
- `getFee(samplePool, false) = 30` (volatile path)
- Domain is **not** UniV2 parts-per-`100_000`. Solidly/Velodrome-class factories typically use **parts per `10_000`** (so volatile `30` ≈ **0.30%**). Treat as a separate fee domain until an explicit on-pool quoter differential pins the denominator.

### Broken layers (why not `drop_in_univ2`)
1. **math** — dual curve: volatile CPMM vs stable invariant; not a single UniV2 `fee/100_000` path  
2. **fee model** — stable/volatile fees + non-UniV2 domain  
3. **factory provenance** — `getPool(a,b,stable)` + clone `implementation`; no UniV2 CREATE2 salt shape  
4. **events / executor** — not exercised here; Solidly-style Swap/Sync differ from UniV2 topics (adapter must re-verify before execution)

### bot_action
Ignore for first-pass WHI-536 enumeration. Do **not** enable production. Future work needs a dedicated Solidly-style adapter issue (only after matrix ack).

---

## Fluxion V3 — verdict: `drop_in_univ3_or_agni`

**Family:** Uniswap V3–style concentrated liquidity (standard UniV3 fee tiers).

### On-chain surface
- Factory `getPool` / `feeAmountTickSpacing` / `owner` succeed  
- **No** `poolDeployer()` (unlike Agni / FusionX V3) — factory itself is the CREATE path  
- Router + NPM `factory()` → Fluxion V3 factory  
- Sample pool `0xB1C1df816ceD51503622Ec83C4c971247048EB9F` (USDT/WMNT, fee=3000):
  - `factory`, `token0`, `token1`, `fee`, `tickSpacing=60`, `liquidity`, `slot0` all succeed

### Fee tiers (enabled = non-zero tick spacing)
| fee | tickSpacing |
| --- | --- |
| 100 | 0 (disabled) |
| 500 | 10 |
| 2500 | 0 (disabled — **not** Agni-style 2500) |
| 3000 | 60 |
| 10000 | 200 |

Fee is UniV3 `uint24` millionths of notional (3000 = 0.30%).

### Caveats
- CREATE2 deployer / init_code_hash **not** pinned yet (no `poolDeployer`; needs separate CREATE2 reproduction before allowlist provenance)  
- Fee-tier set is **standard UniV3**, not Agni’s 100/500/2500/10000 set  
- Structurally same Swap/slot0 family as Agni/UniV3; still **no** `SelectedProtocol` / production path

### bot_action
Discovery-only / non-executable for first generator pass. Optional second-pass multi-factory V3 identity alongside FusionX V3 — only after human ack. Do **not** silently merge into Agni factory enumeration.

---

## Matrix rows

| venue | verdict | sample |
| --- | --- | --- |
| Fluxion V2 | `adapter_required` | `0xd85229cb09b3AFc0DB96180adeCC19Ae9d038ECe` |
| Fluxion V3 | `drop_in_univ3_or_agni` | `0xB1C1df816ceD51503622Ec83C4c971247048EB9F` |
