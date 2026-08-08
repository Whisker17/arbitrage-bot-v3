# WHI-938: ticks() + tickBitmap re-validation (all seven V3 factories)

Captured: 2026-08-08 against public Mantle RPC (`https://rpc.mantle.xyz`).

Selectors: `ticks(int24)` = `0xf30dba93`, `tickBitmap(int16)` = `0x5339c296`.
`ticks` decoded with the classic UniV3 return tuple
`(uint128,int128,uint256,uint256,int56,uint160,uint32,bool)`.

| venue | sample pool | slot0 | ticks(0) | tickBitmap(0) | loadable? |
| --- | --- | --- | --- | --- | --- |
| agni-v3 | `0x0d290c8e7f3ffa5267de4b1f9f6f6d8d578624ac` | OK | OK | OK | yes |
| fusionx-v3 | `0x262255f4770aebe2d0c8b97a46287dcecc2a0aff` | OK | OK | OK | yes |
| butter | `0x67f1e667ac60786b714b3d68a5ac35cc90441b73` | OK | OK | OK | yes |
| fluxion-v3 | `0xb1c1df816ced51503622ec83c4c971247048eb9f` | OK | OK | OK | yes |
| cleopatra-cl | `0xf79c37b8344c58467ec88c01b82c2fd8fccdbbd0` | OK | OK | OK | **no** — Agni **batch CREATE** tick-data path reverts (WHI-929 live, all 7 pools) |
| v3fork-636ea2 | `0x1b03630817c64cf69fc4bcbc70bc3acba1d63ce3` | OK | OK | OK | yes |
| uniswap-v3 | `0x48ef5640e71001cac842f5627a0bfec1ef09deb7` | OK | OK | OK | yes |

## Interpretation

Direct `ticks` / `tickBitmap` eth_calls succeed on Cleopatra. The failure mode
that forced quarantine is the **Agni tick-data batch contract CREATE** used at
cold start — observed on every Cleopatra pool in the 137-pool universe
(`tick-data CREATE reverted; skipping item`). That skip left empty tick maps
in the graph (WHI-938).

Other six factories: direct probes OK and no whole-venue batch-revert class
observed in the WHI-929 / full-universe cold start (only Cleopatra).

## Decision

Quarantine Cleopatra CL (option 2). Restoring requires a venue-specific batch
ABI (option 1), not re-enabling under Agni.
