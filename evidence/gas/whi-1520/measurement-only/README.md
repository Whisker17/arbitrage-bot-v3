# MEASUREMENT-ONLY cycle file. It is NOT a runtime universe.

`pool_universe.measurement-only.csv` is used for exactly one purpose: it is the `--universe` of
campaign run 3 (`[whi-1520-c]`, `evidence/gas/whi-1520/run3/`). No bot, launcher, pin or runtime
config references it. The runtime universe is `data/pool_universe.csv` at snapshot 101165208,
pinned in `config/pool_universe.pin.json`, and it is unchanged.

**Contents.** The committed `data/pool_universe.csv` (124 rows, byte-identical prefix) plus two
extra rows. Both extra rows are on admitted, registered factories. They are real pools that the
generator already enumerates, and it drops them at block 101165208.

| extra row | venue / fee | checks at block 101165208 | why it is outside the runtime universe |
|---|---|---|---|
| `0xe1c443568b556343bd0e61ebae25eb5442b6260d` WMNT/mETH | FusionX V2 `0xE502…Cce7c`, registry fee 200 / 100000 | `factory()` = FusionX V2; `getPair(WMNT, mETH)` = this pool; CREATE2(factory, keccak(token0 ++ token1), approved_pools init_code_hash `0x58c684ae…03a0`) = this pool | below the 1000 WMNT floor (reserves 0.171 WMNT / 0.0000372 mETH) |
| `0xd4a1f03c2c4981f7b84b9de1af936bf34a20fa3a` MOE/WMNT, bin step 100 | Merchant Moe LB `0xa663…4054` (the committed Moe seed row) | `getFactory()` = LB factory; `getLBPairInformation(MOE, WMNT, 100)` = this pool | not in the runtime universe at B* (below floor / no cycle) |

**Why it exists.** The committed universe has no `v2+v2` cycle, so `h2:v2+v2` cannot be measured on
it. The Moe V1 guard still needs exact-class samples on the universe's Moe V1 pools. The FusionX V2
WMNT/mETH row closes a `v2+v2` cycle with the universe's Moe V1 WMNT/mETH pool `0xa375ea3e…`.

The LB row is the best-effort attempt at `h2:v2+moe:bins=0` with the universe's Moe V1 MOE/WMNT pool
`0x76386861…`. Its active bin holds only 0.000645 WMNT, which is below the 0.01 WMNT minimum
campaign amount.

**Outcome.**
- `h2:v2+v2`: 8 successes, all touching `0xa375ea3e…`.
- The LB cycle: all 12 attempts ended "Moe state is incomplete for an exact quote" (DI-51), so
  `h2:v2+moe:bins=0` stays withheld.

The gas bound depends on the venues' pool code and the executor path, not on universe membership.
The orchestrator approved this deviation (evidence/gas/whi-1520/REPORT.md).
