# WHI-1520: gas re-qualification on the 124-pool universe at block 101165208

**Status.** Every Approved class is measured on its exact class, and on every admitted V2 venue it
prices in the new committed universe. Where a class could not be measured, it is withheld
fail-closed with a stated reason.

- **Guards:** rules, thresholds and venue attribution are unchanged. The only guard-file edit
  adds this issue's three campaign tags to the PR108 lever fence's tag list. They are held to
  the same `v2_boost` assertion.
- **Merge gate:** the mainnet gas profile is on the funds path, so a human merge gate applies.
- **Deviations:** three, all approved by the orchestrator (listed at the end).

| | base (dev 2b13b8f) | WHI-1520 |
|---|---:|---:|
| `content_digest` | `0x87f5d70a…109c` | **`0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412`** |
| Approved | 7 | **6** |
| profiles | 307 | 307 |
| fork samples added | — | 141 successes (65 `[whi-1520]` + 65 `[whi-1520-b]` + 11 `[whi-1520-c]`), all `v2_boost`, block 101165208; plus 2 research_revert |

- **Digest constant:** `MANTLE_MAINNET_PROFILE_DIGEST` in `src/execution/gas_runtime.rs` is updated.
- **Regeneration is byte-identical.** Running
  `cargo run --locked --example generate_gas_profile -- --print-digest` over the committed
  `samples.jsonl` + `generator_config.json` reproduces `mantle_mainnet_v1.json` with the digest above.
- **Withholds are idempotent.** `python3 evidence/gas/whi-1520/withhold.py` leaves
  `generator_config.json` byte-identical on a re-run (sha256 `c5fc71d0…1a76`).
- **Regeneration procedure used:**
  1. Start from the pre-WHI-1520 generator config (`655d943`).
  2. `generate_gas_profile`, giving pre-withhold digest `0x479604eb…c86d`.
  3. `withhold.py`.
  4. `generate_gas_profile`.

## Why re-qualification was needed

On the regenerated universe (`evidence/universe/whi-1520/README.md`), both gas guards failed:

```
committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools:
  h2:v2+v2: Approved and it prices Moe V1 pools, but has only 0 Moe V1 fork samples
committed_approved_classes_are_measured_on_every_v2_venue_they_price:
  h2:v2+v3:ticks=0: prices moe-v1 (4 universe cycles) on 0 exact-class fork samples (< 2)
  h2:v2+moe:bins=0: prices moe-v1 (1 universe cycles) on 0 exact-class fork samples (< 2)
  h3:v2+moe+moe:bins=0: prices moe-v1 (14 universe cycles) on 0 exact-class fork samples (< 2)
  h3:moe+v2+v2:bins=0: prices moe-v1 (14 universe cycles) on 0 exact-class fork samples (< 2)
```

**Cause:** Moe V1 USDT/WMNT `0x4e7685df…` fell below the TVL floor at B* (815.6 WMNT).

- 297 of the WHI-1413 Moe V1 samples touched that pool.
- The guards count only samples that touch a pool **of the committed universe**, so those samples no
  longer count.
- This is the guard working as designed, and it is unchanged. The fragility is recorded as **DI-56**
  (follow-up only).

## Provenance (all runs)

**Harness:** `examples/remeasure_mainnet_gas_profile.rs --campaign`, the WHI-1422/WHI-1413 harness.
The following are all unchanged:

- synthetic executor (executor_code_hash `0x50f51b77…26ef`);
- `eth_estimateGas` on a local anvil fork;
- levers (cycles with a V2 hop use only `v2_boost`);
- holdout policy (min_samples 10, holdout 20% heaviest, tail_max_p99 + 20% + 50k);
- per-venue V2 fee.

Build: `CARGO_PROFILE_DEV_{LTO=false,OPT_LEVEL=1,DEBUG=0,CODEGEN_UNITS=256} cargo build --locked --example remeasure_mainnet_gas_profile`.

**Fork:** Mantle mainnet (chain 5000) at **101165208**, hash `0xa915e1b8…e5c1`. This is the universe
snapshot block.

- anvil 1.7.1: `anvil --fork-url https://rpc.mantle.xyz --fork-block-number 101165208 --port 8547 --chain-id 5000 --no-mining --no-rate-limit --silent`.
- Before each run, the hash was checked on the fork with `cast block 101165208 --field hash`.
- Pool state was read from `https://rpc.mantle.xyz` (public, read-only) at the pinned hash.
- No transaction was sent, no key was used, and nothing was broadcast.
- Anvil was stopped after run 2 and again after run 3 (23:36:35Z).

**Mixed fork blocks:** the committed samples already mix WHI-557 (98478154) with WHI-1422/1413
(98969898). Adding 101165208 is the same by-design case. The generator and the provenance tests
accept it, and neither was changed.

Every run used `--campaign-require-factory 0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc` (Moe V1 cycles
only), `--cycles-per-topology 25` (all cycles), `--rpc-timeout-secs 120`, `--measure-rpc-url http://127.0.0.1:8547`.

| run | tag | clean HEAD | binary sha256 | `--universe` | topologies | window (UTC) | attempts | successes | unfinished |
|---|---|---|---|---|---|---|---:|---:|---:|
| 1 (`./`) | `[whi-1520]` | `09472b7` | `5590573e…07e9` | `data/pool_universe.csv` | v2+v3, v2+moe, v2+moe+moe, moe+v2+v2 | 22:53:29 → 22:54:20 | 360 | 65 (all moe+v2+v2) | 0 |
| 2 (`run2/`) | `[whi-1520-b]` | `bd435d2` | `395f5337…1ec8` | `data/pool_universe.csv` | v2+v3, v2+moe, v2+moe+moe | 23:01:27 → 23:02:17 | 192 | 65 | 0 |
| 3 (`run3/`) | `[whi-1520-c]` | `621f40b` | `395f5337…1ec8` (example source unchanged since bd435d2) | **`measurement-only/pool_universe.measurement-only.csv`** | v2+v2, v2+moe | 23:35:24 → 23:35:28 | 48 | 11 | 0 |

No run had an RPC failure. Logs are in `logs/`, `run2/logs/` and `run3/logs/`.

### Run 1

Every `v2+v3`, `v2+moe` and `v2+moe+moe` attempt whose boosted hop is Moe V1 WMNT/mETH `0xa375ea3e…`
ended "lever cannot settle".

- mETH keeps its ERC-20 balance mapping at **storage slot 51** (OpenZeppelin upgradeable layout).
  This was checked on the fork: `cast storage` at `keccak(pool . 51)` equals `balanceOf(pool)`.
- The harness's balance-slot probe stopped at 20.
- WHI-1413 hit the same limit on these cycles (0 of 96 attempts).

### Deviation 1: harness balance-slot probe widened (`bd435d2`)

`Ctx::balance_slot` now probes `0..=BALANCE_SLOT_PROBE_MAX` (127) instead of `0..=20`.

- A slot is still accepted only on a write-then-`balanceOf` readback of a magic value on the fork.
- Lever semantics are unchanged.
- The harness has no unit tests, since every probe is an RPC call. Run 2 is the check: it produced
  65 successes on exactly the cycles that were "cannot settle" before.

### Run 2 outcomes

| topology | success | lever cannot settle | DI-51 incomplete LB state | open-ended not measured |
|---|---:|---:|---:|---:|
| v2+v3 | 10 | 2 | 0 | 0 |
| v2+moe | 3 | 0 | 4 | 5 |
| v2+moe+moe | 52 | 0 | 111 | 5 |

The universe's only Moe V1 `v2+moe` cycle is `0xa375ea3e…` > LB `0xf59c79b9…`. It crosses **4+ bins
even at 0.01 WMNT**: the active bin holds 0.00049 WMNT. It can never give a `bins=0` sample.

### Deviation 2: run 3 on a measurement-only cycle file

**Why.** The new universe has **no `v2+v2` cycle**. The Moe V1 guard still requires ≥ 2 `h2:v2+v2`
fork samples that touch one of the universe's Moe V1 pools. Withholding `h2:v2+v2` would have broken
~37 tests that use it as the pinned V2 fixture route.

**What was approved.** The gas bound depends on the venues' pool code and on the executor path, not on
whether a pool is in the universe. So the orchestrator approved measuring the class on a cycle that closes
outside the committed universe. The file `measurement-only/pool_universe.measurement-only.csv`:

- is labelled "measurement-only, not a runtime universe" (`measurement-only/README.md`);
- is referenced by no bot, launcher, pin or runtime config;
- is the committed 124 rows (byte-identical prefix) plus exactly two rows on admitted, registered
  factories:
  - **FusionX V2 WMNT/mETH `0xe1c44356…`**: fee 200. `getPair` and CREATE2 under the approved_pools
    `init_code_hash` both give this address. Its reserves are 0.171 WMNT, below the floor. It closes
    `v2+v2` with the universe's Moe V1 WMNT/mETH `0xa375ea3e…`.
  - **Moe LB MOE/WMNT `0xd4a1f03c…`** (bin step 100, a committed Moe seed row, confirmed by
    `getLBPairInformation`). This was the best-effort attempt at `h2:v2+moe:bins=0` with the
    universe's Moe V1 MOE/WMNT `0x76386861…`.

**Outcome.**

- **`h2:v2+v2`: 8 successes**, all touching `0xa375ea3e…`, gas 249458–249616, limit **307152**.
  There were also 2 `FusionX: INSUFFICIENT_LIQUIDITY` research reverts, which never qualify, and
  14 "lever cannot settle".
- **LB cycle: 0 successes.** All 12 amounts ended "Moe state is incomplete for an exact quote" (DI-51).
  A read-only pre-screen of every LB X/WMNT pair (`getAllLBPairs` at B*) behind the four WMNT-paired
  Moe V1 universe pools (MINU, MOE, mETH, JOE) found none with ≥ 0.01 WMNT on the needed side of the
  active bin. The largest was 0.000645 WMNT. So `h2:v2+moe:bins=0` could not be measured at B*.

## Withholds (`evidence/gas/whi-1520/withhold.py`)

The script only adds forced-Unsupported entries, and it keeps every WHI-1422/WHI-1413 entry. It
applies the existing PR109-F1 and Moe V1 rules unchanged, plus one scope rule decided by the
orchestrator.

| Class | Rule | Reason (abridged) | Cycles it prices in the 124 universe |
|---|---|---|---:|
| `h2:v2+moe:bins=0` | PR109-F1 | moe-v1 is on 1 of its 3 cycles with 0 exact-class samples; that cycle never reaches `bins=0`, and no LB alternative exists at B* | 3 |
| `h3:moe+v2+v2:bins=1-3` | WHI-1520 scope | newly qualified by run 1 (31 Moe V1 + 4 FusionX samples); not Approved on the base profile; a regeneration does not widen the approved set | (15) |
| `h3:v2+moe+moe:bins=1-3` | WHI-1520 scope | newly qualified by run 2 (13 Moe V1 + 7 FusionX samples); same rule | (21) |

The two scope withholds are **follow-up candidates**. Their samples stay visible as stats.

## Per-class result

How to read the table:

- **Cycles** counts the WMNT cycles of the class's topology, 8526-cycle universe → 5562-cycle universe.
- **Venue columns** show, on the 124 universe: universe cycles / exact-class fork samples touching the venue / their max gas.

| Class | Base status / limit | WHI-1520 status / limit | Cycles | FusionX V2 | Moe V1 | New samples |
|---|---|---|---:|---|---|---:|
| `h2:v2+v2` | Approved 307109 | **Approved 307152** | 2 → 0 | — (no cycle) | Moe V1 guard: **8** samples on `0xa375ea3e…`, max 249616 | 8 |
| `h2:v2+v3:ticks=0` | Approved 334364 | **Approved 335588** | 16 → 9 | 5 / 24 / 238014 | 4 / **7** / 272029 | 7 |
| `h2:v2+moe:bins=0` | Approved 431208 | **Unsupported** (PR109-F1) | 5 → 3 | 2 / 14 / 317673 | 1 / **0** / — | 0 |
| `h3:v2+v2+v2` | Approved 460243 | Approved 460243 | 18 → 12 | 6 / 20 / 341953 | 12 / 41 / 341953 | 0 |
| `h3:v2+moe+moe:bins=0` | Approved 583585 | **Approved 698556** | 33 → 21 | 7 / 11 / 444654 | 14 / **31** / 569179 | 31 |
| `h3:moe+v2+v2:bins=0` | Approved 572369 | **Approved 583820** | 21 → 15 | 4 / 14 / 444081 | 14 / **34** / 465948 | 34 |
| `h3:moe+moe+v2:bins=0` | Approved 697804 | Approved 697804 | 33 → 21 | 7 / 14 / 485548 | 14 / 21 / 567920 | 0 |
| `h3:moe+v2+v2:bins=1-3` | Unsupported | Unsupported (scope) | 21 → 15 | 4 / 4 / 481779 | 14 / 31 / 492665 | 31 |
| `h3:v2+moe+moe:bins=1-3` | Unsupported | Unsupported (scope) | 33 → 21 | 7 / 7 / 512180 | 14 / 13 / 548979 | 13 |

**Approved-topology cycles:**

| Profile | Universe | Cycles |
|---|---|---:|
| base | 8526-cycle universe | 128 |
| base | 124-pool universe (if left untouched) | 81 |
| **WHI-1520** | 124-pool universe | **78** |

- 128 → 81 is the universe shrinking under the unchanged floor and cycle filter at B*, that is,
  market movement.
- 81 → 78 is the `h2:v2+moe:bins=0` withhold.
- The startup gate is still satisfied: 6 of its 36 count-based topologies are supported (7 on the base
  profile). One of them, `v2+v2`, counts only because the gate is count-based; `h2:v2+v2` is Approved
  but has no cycle in this universe.

**Limit changes** come from the generator's unchanged policy over the added samples:

- `h2:v2+v2` 307109 → 307152;
- `h2:v2+v3:ticks=0` 334364 → 335588;
- `h3:moe+v2+v2:bins=0` 572369 → 583820;
- `h3:v2+moe+moe:bins=0` 583585 → **698556** (+114971). Its WMNT/mETH cycles are heavier: max 569179,
  against the old class max of 485484. The new limit is 22.7% above its heaviest sample.

## Deviation 3: test expectations that follow the profile

All three changes are test code only, and no assertion was relaxed.

- **Limit pins.** The `h2:v2+v2` limit/expected pins move from 307109/214123 to **307152/214147**
  (`src/execution/gas_runtime_tests.rs`, `tests/gas_runtime_identity.rs` ×2), because of the 8 new
  Moe V1 samples.
- **Approved count.** `approved_routes_in_profile.len()` goes 7 → **6** (`src/service/fee_scoring.rs`).
- **Moved fixtures.** Three fixtures used `h2:v2+moe:bins=0`, which is now Unsupported (PR109-F1
  withhold, above). They move to the Approved **`h3:v2+moe+moe:bins=0`**:
  - `tests/pipeline_wiring.rs` `moe_route_runs_through_closed_pipeline_head` and
    `…_with_risk_tiered_preflight`. They still push a Moe LB `pool_type` (2) through the closed
    pipeline head, now twice, with the same assertions.
  - `build_scenario` now builds one hop per route-key protocol. Mid tokens are `0x55`, `0x56`; pools
    are `0x03`, `0x04`, `0x05`. For every existing 2-hop caller it produces exactly the old inputs.
  - `src/service/path_index.rs` `measured_fee_quote_soft_skips_incomplete_moe_state_instead_of_erroring`.
    It keeps the snapshot-less Moe hop right after the V2 hop and adds a second Moe hop back to WMNT.
    The topology is now Approved at some bucket, so the WHI-1409 pre-check still lets it reach the
    measured-fee quote closure, and the `NoOptimum` assertion is unchanged. Before the move, it failed
    with `Rejected { reason: "unapproved_route" }`.

## Guards (final HEAD)

- `committed_approved_classes_are_measured_on_every_v2_venue_they_price`: pass.
- `committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools`: pass.
- `committed_profile_approvals_respect_pr108_factory_and_lever_fences`: pass.
  - Its campaign-tag list now includes `[whi-1520]`, `[whi-1520-b]` and `[whi-1520-c]`, held to the same
    `lever=v2_boost` assertion.
  - Without that change, the fence rejects these samples as "non-campaign".
  - Every WHI-1520 sample is `v2_boost`.
- `venue_guard_rejects_the_45c0bd9_approval_of_h3_v2_moe_v2_bins_0`: pass and unchanged. It still fails
  on exactly the FusionX gap of 4 cycles.
- `committed_universe_topologies_have_zero_unknown_route`: pass (306 explicit keys).

## Limits

These are the same as in WHI-1422/WHI-1413:

- The samples are lever-assisted fork estimates, not mainnet receipts (DI-10).
- The holdout is the heaviest 20% of the same cycles.
- RouteKey still has no venue axis (DI-54).
- The venue guard attributes samples by universe membership (DI-56).
- `h2:v2+v3:ticks=0`'s new Moe V1 samples come from its one Agni V3 Moe V1 cycle. Its other 3 Moe V1
  cycles go through non-Agni V3 pools; this is the existing DI-50 exposure, and the V3 factory policy is
  unchanged.
