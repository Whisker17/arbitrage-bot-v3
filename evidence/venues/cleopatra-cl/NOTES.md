# Cleopatra CL

## Identity
- **Family:** Uniswap V3–style CL (Ramses/Cleopatra fork)
- **Factory:** `0xAAA32926fcE6bE95ea2c51cB4Fcb60836D320C42`
- **Sample pool:** `0xf79c37b8344c58467ec88c01b82c2fd8fccdbbd0` (USDT/mETH fee=500)
- Census `kind`: `v3`. External name: **CleopatraCL**.

## On-chain evidence (pinned block 98950889)
- Full UniV3 surface OK at the selector level (`slot0`, `fee`, `tickSpacing`, …)
- `globalState()` MISSING
- `slot0.feeProtocol = 17` (fits classic uint8; also fine under uint32 decode)
- Factory fee tiers: 500→10, 2500→0, 3000→60

## WHI-938 re-validation (ticks + bitmap, 2026-08-08)

Direct eth_call (public Mantle RPC) on sample pool
`0xf79c37b8344c58467ec88c01b82c2fd8fccdbbd0`:

| call | result |
| --- | --- |
| `slot0()` | OK |
| `ticks(0)` (UniV3 return tuple) | OK |
| `tickBitmap(0)` | OK |

Despite matching direct `ticks` / `tickBitmap` selectors, **all 7 Cleopatra pools
in the frozen universe reverted inside the Agni tick-data batch CREATE** under
the Agni batch ABI (WHI-929 live cold-start on `4d6f997`). The WHI-929
single-item skip left them in state with **empty tick data**, where they
quoted as dead weight and inflated coverage.

Root cause class: **batch-ABI / tick-struct compatibility**, not missing
`slot0`. The WHI-910 drop-in test was too shallow (slot0 only).

## Verdict
**`adapter_required`** (was `drop_in_univ3_or_agni` under WHI-910)

Quarantined from the loadable universe (WHI-938 option 2):
- Not in `DROP_IN_V3_VENUES`
- Seed rows → `pool_universe.quarantine.json` with reason
  `tick_data_batch_abi_incompatible: Agni tick-data batch CREATE reverts (WHI-938)`
- Restoring requires a venue-specific tick-data batch contract (option 1),
  not re-adding the factory under the Agni ABI.

## bot_action
**Do not seed / do not load.** Quarantined until a Cleopatra/Ramses tick batch
ABI lands. Lower priority than Butter / FusionX V3 / Fluxion V3 adapters.
