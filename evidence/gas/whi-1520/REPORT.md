# WHI-1520: gas re-qualification on the 124-pool universe at block 101165208

**Status: every Approved class is measured, on the exact class, on every admitted V2 venue it
prices in the new committed universe. Any class that could not be measured is withheld fail-closed,
with a reason.**

- Rules and thresholds are unchanged, and so is venue attribution.
- The lever fence's campaign-tag list now also includes this issue's two tags; they are held to the same `v2_boost` rule.
- The mainnet gas profile is on the funds path, so a human merge gate applies.

| | base (dev 2b13b8f) | WHI-1520 |
|---|---:|---:|
| `content_digest` | `0x87f5d70a…109c` | **`0xde16710cfc562222d68689347da6d8a00a961aa7e9c9807ac1507c2c5b4134c0`** |
| Approved | 7 | 5 |
| profiles | 307 | 307 |
| fork samples added | — | 130 (65 `[whi-1520]` + 65 `[whi-1520-b]`), all `v2_boost`, block 101165208 |

- `MANTLE_MAINNET_PROFILE_DIGEST` in `src/execution/gas_runtime.rs` is updated.
- Regeneration is byte-identical. `cargo run --locked --example generate_gas_profile -- --print-digest`
  over the committed `samples.jsonl` + `generator_config.json` reproduces `mantle_mainnet_v1.json` with
  that digest. The campaign's own finalize printed the same pre-withhold digest, `0xefb157b9…7520`.
- `python3 evidence/gas/whi-1520/withhold.py` is idempotent: a re-run leaves `generator_config.json`
  byte-identical (sha256 `33316bbf…dbda`).

## Why re-qualification was needed

On the regenerated universe (`evidence/universe/whi-1520/README.md`) both gas guards failed:

```
committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools:
  h2:v2+v2: Approved and it prices Moe V1 pools, but has only 0 Moe V1 fork samples
committed_approved_classes_are_measured_on_every_v2_venue_they_price:
  h2:v2+v3:ticks=0: prices moe-v1 (4 universe cycles) on 0 exact-class fork samples (< 2)
  h2:v2+moe:bins=0: prices moe-v1 (1 universe cycles) on 0 exact-class fork samples (< 2)
  h3:v2+moe+moe:bins=0: prices moe-v1 (14 universe cycles) on 0 exact-class fork samples (< 2)
  h3:moe+v2+v2:bins=0: prices moe-v1 (14 universe cycles) on 0 exact-class fork samples (< 2)
```

**Cause.** Moe V1 USDT/WMNT `0x4e7685df…` fell below the TVL floor at B* (815.6 WMNT).

- 297 of the WHI-1413 Moe V1 samples touched it.
- The guards count only samples that touch a pool **of the committed universe**, so none of those
  samples counts any more.
- This is the guard working as designed. It was not changed.

## Provenance

**Harness.** `examples/remeasure_mainnet_gas_profile.rs --campaign`, the WHI-1422/WHI-1413 harness.
Everything is the same as before:

- synthetic executor (executor_code_hash `0x50f51b77…26ef`);
- `eth_estimateGas` on a local anvil fork;
- levers;
- holdout policy (min_samples 10, holdout 20% heaviest, tail_max_p99 + 20% + 50k);
- per-venue V2 fee.

Build flags: `CARGO_PROFILE_DEV_{LTO=false,OPT_LEVEL=1,DEBUG=0,CODEGEN_UNITS=256} cargo build --locked --example remeasure_mainnet_gas_profile`.

**Fork.** Mantle mainnet (chain 5000) at **101165208**, hash `0xa915e1b8…e5c1`. This is the universe snapshot block.

- The hash was checked on the fork before each run.
- anvil 1.7.1 command: `anvil --fork-url https://rpc.mantle.xyz --fork-block-number 101165208 --port 8547 --chain-id 5000 --no-mining --no-rate-limit --silent`.
- Pool state was read from `https://rpc.mantle.xyz` (public, read-only) at the pinned hash.
- No transaction was sent, no key was used and nothing was broadcast. Anvil was stopped after run 2.

**Mixed fork blocks.** The committed samples already mix blocks: WHI-557 at 98478154 and
WHI-1422/1413 at 98969898. Adding 101165208 is the same by-design case. The generator and the
provenance tests accept it, and neither was changed.

### Run 1: `[whi-1520]` (this directory)

- Run: 2026-09-26T22:53:29Z → 22:54:20Z, at clean HEAD `09472b7` (the new universe committed). Binary sha256 `5590573e…07e9`.
- Flags: `--campaign --block 101165208 --measure-rpc-url http://127.0.0.1:8547 --universe data/pool_universe.csv --evidence-out evidence/gas/whi-1520 --campaign-tag "[whi-1520]" --campaign-require-factory 0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc --campaign-topologies v2+v3,v2+moe,v2+moe+moe,moe+v2+v2 --cycles-per-topology 25 --rpc-timeout-secs 120`. Topologies were scoped to the failing classes; `v2+v2` has no cycle to sample.
- Coverage: 30 executable Moe V1 cycles, which is all of them for those topologies. The 46 non-Agni V3 pools are excluded because they have no `agniSwapCallback`.
- Outcome: **360 attempts, 65 successes (all `moe+v2+v2`), 0 unfinished, 0 RPC failures.** Log: `logs/1-campaign-run.log`.
- **Every `v2+v3`, `v2+moe` and `v2+moe+moe` attempt whose boosted hop is Moe V1 WMNT/mETH `0xa375ea3e…` ended "lever cannot settle".**
  - mETH keeps its ERC-20 balance mapping at **storage slot 51** (OpenZeppelin upgradeable layout). This was verified on the fork: `cast storage` at `keccak(pool . 51)` equals `balanceOf(pool)`.
  - The harness's balance-slot probe stopped at slot 20.
  - WHI-1413's run hit the same limit on these cycles (0 of 96 attempts).

### Harness fix, approved by the orchestrator (`bd435d2`)

`Ctx::balance_slot` now probes `0..=BALANCE_SLOT_PROBE_MAX` (127) instead of `0..=20`.

- A slot is still accepted only on a write-then-`balanceOf` readback of a magic value on the fork.
- Lever semantics are unchanged: v2_boost still overrides only the boosted V2 hop's output-token
  balance, and cycles with a V2 hop still use only v2_boost (no inflate/displace).
- The harness has no unit tests (every probe is an RPC call). Run 2 is the check: 65 successes on
  exactly the cycles that were "cannot settle" before.

### Run 2: `[whi-1520-b]` (`run2/`)

- Run: 2026-09-26T23:01:27Z → 23:02:17Z, at clean HEAD `bd435d2`. Binary sha256 `395f5337…1ec8`.
- Flags: same as run 1, plus `--evidence-out evidence/gas/whi-1520/run2 --campaign-tag "[whi-1520-b]" --campaign-topologies v2+v3,v2+moe,v2+moe+moe`.
- Coverage: 16 executable Moe V1 cycles.
- Outcome: **192 attempts, 65 successes, 0 unfinished, 0 RPC failures.** Log: `run2/logs/1-campaign-run.log`.

| topology | success | lever cannot settle | DI-51 incomplete LB state | open-ended not measured |
|---|---:|---:|---:|---:|
| v2+v3 | 10 | 2 | 0 | 0 |
| v2+moe | 3 | 0 | 4 | 5 |
| v2+moe+moe | 52 | 0 | 111 | 5 |

The single Moe V1 `v2+moe` cycle is `0xa375ea3e…` > LB `0xf59c79b9…`. It crosses **4+ bins even at
0.01 WMNT**, so it can never give a `bins=0` sample.

## Withholds (`evidence/gas/whi-1520/withhold.py`)

The script only adds forced-Unsupported entries, and it keeps every WHI-1422/WHI-1413 entry. It applies the
existing rules unchanged, plus one scope rule:

| Class | Rule | Reason (abridged) | Cycles it prices in the 124 universe |
|---|---|---|---:|
| `h2:v2+v2` | Moe V1 guard | 0 samples on the universe's Moe V1 pools (its Moe V1 samples used `0x4e7685df…`); `v2+v2` has **0 cycles**, so no campaign can measure it | 0 |
| `h2:v2+moe:bins=0` | PR109-F1 | moe-v1 is on 1 of its 3 cycles with 0 exact-class samples; that cycle never reaches `bins=0` | 3 |
| `h3:moe+v2+v2:bins=1-3` | WHI-1520 scope | newly qualified by run 1 (31 Moe V1 + 4 FusionX samples), not Approved on the base profile; a regeneration does not widen the approved set | (15) |
| `h3:v2+moe+moe:bins=1-3` | WHI-1520 scope | newly qualified by run 2 (13 Moe V1 + 7 FusionX samples); same rule | (21) |

- The two scope withholds were decided by the orchestrator. They are **follow-up candidates**: their
  samples stay visible as stats.
- The withholds were measured before they were committed, and they remove 3 approved-topology cycles.
  That is well above the orchestrator's floor of 48. The startup gate stays satisfiable: 5 of its 36 count-based topologies are supported (7 on the base profile).

## Per-class result

Columns:

- **Cycles** are the WMNT cycles of the class's topology: 8526-cycle universe → 5562-cycle universe.
- **Venue** is universe cycles / exact-class fork samples touching the venue / their max gas, on the 124 universe.

| Class | Base status / limit | WHI-1520 status / limit | Cycles | FusionX V2 | Moe V1 | New samples |
|---|---|---|---:|---|---|---:|
| `h2:v2+v2` | Approved 307109 | **Unsupported** (Moe V1 guard) | 2 → 0 | — | — | 0 |
| `h2:v2+v3:ticks=0` | Approved 334364 | **Approved 335588** | 16 → 9 | 5 / 24 / 238014 | 4 / **7** / 272029 | 7 |
| `h2:v2+moe:bins=0` | Approved 431208 | **Unsupported** (PR109-F1) | 5 → 3 | 2 / 14 / 317673 | 1 / **0** / — | 0 |
| `h3:v2+v2+v2` | Approved 460243 | Approved 460243 | 18 → 12 | 6 / 20 / 341953 | 12 / 41 / 341953 | 0 |
| `h3:v2+moe+moe:bins=0` | Approved 583585 | **Approved 698556** | 33 → 21 | 7 / 11 / 444654 | 14 / **31** / 569179 | 31 |
| `h3:moe+v2+v2:bins=0` | Approved 572369 | **Approved 583820** | 21 → 15 | 4 / 14 / 444081 | 14 / **34** / 465948 | 34 |
| `h3:moe+moe+v2:bins=0` | Approved 697804 | Approved 697804 | 33 → 21 | 7 / 14 / 485548 | 14 / 21 / 567920 | 0 |
| `h3:moe+v2+v2:bins=1-3` | Unsupported | Unsupported (scope) | 21 → 15 | 4 / 4 / 481779 | 14 / 31 / 492665 | 31 |
| `h3:v2+moe+moe:bins=1-3` | Unsupported | Unsupported (scope) | 33 → 21 | 7 / 7 / 512180 | 14 / 13 / 548979 | 13 |

**Approved-topology cycles**

| profile | universe | cycles |
|---|---|---:|
| base | 8526-cycle universe | 128 |
| base | 124-pool universe (had it been left untouched) | 81 |
| **WHI-1520** | 124-pool universe | **78** |

- The 128 → 81 drop is the universe shrinking (market movement at B*).
- The 81 → 78 drop is the `h2:v2+moe:bins=0` withhold.
- Every class that stays Approved has, on every V2 venue it prices, ≥ 2 exact-class samples, all ≤ its limit.

**Limit changes come from the generator's unchanged policy over the added samples:**

- `h2:v2+v3:ticks=0`: 334364 → 335588, +1224.
- `h3:moe+v2+v2:bins=0`: 572369 → 583820, +11451.
- `h3:v2+moe+moe:bins=0`: 583585 → **698556**, +114971. The WMNT/mETH cycles are heavier (max 569179 vs the
  old class max 485484); the new limit is still 22.7% above its heaviest sample.

## Guards (final HEAD)

- `committed_approved_classes_are_measured_on_every_v2_venue_they_price`: pass.
- `committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools`: pass.
- `committed_profile_approvals_respect_pr108_factory_and_lever_fences`: pass.
  - Its campaign-tag list now includes `[whi-1520]` and `[whi-1520-b]`, held to the same `lever=v2_boost` assertion.
  - Without this, the fence rejects these samples as "non-campaign". All 130 new samples are `v2_boost`.
  - The rule, the PR108-F2 V3 check and the threshold are unchanged.
- `venue_guard_rejects_the_45c0bd9_approval_of_h3_v2_moe_v2_bins_0`: pass, unchanged. The injected
  approval still fails on exactly `h3:v2+moe+v2:bins=0: prices fusionx-v2 (4 universe cycles) on 0 exact-class fork samples (< 2)`.
- `committed_universe_topologies_have_zero_unknown_route`: pass. Every one of the 306 keys has an explicit entry.

## Limits

These are the same caveats as WHI-1422/WHI-1413:

- The samples are lever-assisted fork estimates, not mainnet receipts (DI-10).
- The holdout is the heaviest 20% of the same cycles.
- RouteKey still has no venue axis (DI-54).
- `h2:v2+v3:ticks=0`'s new Moe V1 samples come from its one Agni V3 Moe V1 cycle. Its 3 other Moe V1
  cycles go through non-Agni V3 pools, which is the existing DI-50 exposure. The V3 factory policy is unchanged.
