# Deploy-only anvil fork rehearsal (WHI-953 / WHI-547)

**Date (UTC):** 2026-08-08  
**Mode:** `scripts/golive/deploy_only.sh --dry-run`  
**Fork:** `anvil --fork-url $MANTLE_RPC_URL --port 8545`  
**Chain id:** 5000  

## Inputs

| Field | Value |
| --- | --- |
| Admin | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` (from `MANTLE_PRIVATE_KEY` only) |
| Hot executor | `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` (**address**, not private key) |
| Guardian | `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` (**address**, not private key) |
| Pools | `scripts/golive/pools.txt` (137) — asserted equal to `data/pool_universe.csv` on `(pool, poolType)` |
| Universe fingerprint | `0x3a7ba09e6463a302cb37bb2b38a1d4a0508a8e1bf2137bf5145a6af7a5a77f2e` |

## Result

| Field | Value |
| --- | --- |
| Executor | `0xBaDEA1Fc93a3a4c9d8680CF4FfaA065B0eb3A0d1` |
| Codehash | `0xe2f8a1e096446aadf231ca7ea5d4771008d1c40fa4dded8b2d0681ad9afaeafd` (matches `config/executor_identity.json`) |
| `paused()` | **true** |
| Executor WMNT balance | **0** |
| Pools registered | 137 / 0 failed |
| Gas (approx) | 14_576_725 (~0.0146 MNT at observed gas price) |
| Hot/guardian private keys in process | **absent** (unset after `.env` load; assert) |

## Steps executed

1. Deploy from committed canonical artifact (`ArbitrageExecutor.full.json`) — not `forge create`
2. Immediate `pause()` + assert `paused() == true`
3. Codehash vs identity
4. `setHotExecutor` / `setGuardian` by address
5. `registerPool` × 137

**Not executed:** fund, unpause (WHI-547 boundary).

## Command

```bash
BOT_HOT_EXECUTOR_ADDRESS=0x70997970C51812dc3A010C7d01b50e0d17dc79C8 \
BOT_GUARDIAN_ADDRESS=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC \
scripts/golive/deploy_only.sh --dry-run
```
