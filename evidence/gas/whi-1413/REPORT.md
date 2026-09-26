# WHI-1413: gas qualification for Merchant Moe V1 classic (Mantle mainnet fork)

**Status: complete for every approved route class that Moe V1 pools reach.**

- The profile now approves **8** classes. Five were approved on the base profile and
  stay approved. Three V3-free classes are new: `h3:v2+v2+v2`, `h3:v2+moe+v2:bins=0`
  and `h3:moe+v2+v2:bins=0`.
- Nine V3-hop classes also qualified numerically. They are **withheld** under the
  WHI-1422 PR108-F2 rule (DI-50).
- Nothing was extrapolated. The mainnet gas profile is on the funds path, so a human
  merge gate applies.

| | base (dev de735db) | after WHI-1413 |
|---|---:|---:|
| `content_digest` | `0xa6bc9dac…3d8b` | **`0x6d96ee51aa573d8571b102366b8691d2f6f965f25f1ba24bcef8fbfecbbfc673`** |
| Approved | 5 | 8 |
| qualification samples | 533 | 1022 (+489 Moe V1 campaign successes) |
| research_revert samples | 18 | 44 (+26) |

`MANTLE_MAINNET_PROFILE_DIGEST` in `src/execution/gas_runtime.rs` is updated.
Regeneration is byte-identical: running
`cargo run --locked --example generate_gas_profile -- --print-digest` on the committed
samples and generator config reproduces `mantle_mainnet_v1.json`, digest `0x6d96ee51…c673`.

## Why a campaign was needed (RouteKey has no factory axis)

Moe V1 rows are `ProtocolKind::V2`, so every Approved class with a `v2` hop prices
them. They also make new classes reachable, `h3:v2+v2+v2` among them (the prompting
arb's class).

`evidence/venues/whi-1413/moe_v1_reach.txt` lists every topology that has a cycle
with a Moe V1 pool in the regenerated 151-pool universe: 1564 of 8526 WMNT cycles.
Each of those topologies has Agni-only cycles, so each is measurable.

A Moe V1 pair is a delegatecall clone. Per hop it is heavier than a FusionX V2 pair,
so the existing bounds had to be shown to hold on Moe V1 pools themselves.

## Provenance

**Harness.** `examples/remeasure_mainnet_gas_profile.rs --campaign`, the WHI-1422
harness. Same synthetic executor (executor_code_hash `0x50f51b77…26ef`), same
`eth_estimateGas` on a local anvil fork, same levers. WHI-1413 adds:

- `--campaign-require-factory 0x5bEf…EdEc`: only cycles with at least one Moe V1 pool.
- `--campaign-topologies`: restrict the run to given topologies.
- `--campaign-tag`: finalize replaces only its own samples, so the WHI-1422 samples are kept.
- Per-venue V2 fee: the harness quotes V2 hops with `service::v2_venues`, exactly as
  `AgniV2Protocol::build_amm` does.

**Fork.** Mantle mainnet (chain 5000) at block **98969898**, hash `0x77e98028…6374`.
This is the universe snapshot block and the WHI-1422 fork block. The hash was checked
on the anvil fork (`cast block 98969898 --field hash`).

```
anvil --fork-url https://rpc.mantle.xyz --fork-block-number 98969898 --port 8547 \
  --chain-id 5000 --no-mining --no-rate-limit --silent        # anvil 1.7.1
```

Pool state is read from `https://rpc.mantle.xyz` (public, read-only) at the pinned hash.
No transaction was sent, no key was used, and nothing was broadcast. Anvil was stopped
after run 2.

**Universe.** The candidate `evidence/venues/whi-1413/regen-98969898/with-moe-v1/pool_universe.csv`
(151 pools, fingerprint `0x2adb7cd6…`). It is identical to the `data/pool_universe.csv`
committed afterwards.

**Run 1.** `evidence/gas/whi-1413/`, 2026-09-26T18:54:08Z → 19:02:13Z. It ran at clean HEAD
`3a7031c`, binary sha256 `227b7cc7…b44f`, built with the WHI-1422 dev-profile flags.

```
remeasure --rpc-url https://rpc.mantle.xyz --campaign --block 98969898 \
  --measure-rpc-url http://127.0.0.1:8547 \
  --universe evidence/venues/whi-1413/regen-98969898/with-moe-v1/pool_universe.csv \
  --evidence-out evidence/gas/whi-1413 --campaign-tag "[whi-1413]" \
  --campaign-require-factory 0x5bEf015CA9424A7C07B68490616a4C1F094BEdEc \
  --cycles-per-topology 4 --rpc-timeout-secs 120
```

- 438 of 1338 executable cycles kept, over 24 topologies.
- **1080 attempts measured, 0 unfinished, 0 RPC failures.**
- Log: `logs/1-campaign-run.log`.

**Run 2.** `run2/`, 19:04:55Z → 19:05:31Z. It ran at clean HEAD `b6890cd`, binary sha256
`c6503d0a…96e6`, with the same flags plus `--campaign-topologies moe+moe+v2
--cycles-per-topology 25 --campaign-tag "[whi-1413-b]"`.

- Why it was needed: in run 1, all 4 chosen `moe+moe+v2` cycles ended "Moe state is
  incomplete for an exact quote" (the DI-51 LB bin-range limit). That left the Approved
  `h3:moe+moe+v2:bins=0` with **0** Moe V1 samples.
- Run 2 took all 25 Moe V1 cycles of that topology: 300 attempts, 43 successes.

**Finalize and fences.** Each run's finalize merged its tagged samples. Then:

1. `python3 evidence/gas/whi-1413/withhold.py` added the PR108 forced-Unsupported entries.
2. `generate_gas_profile` regenerated the profile.

Attempt outcomes over both runs (1380 records, one per attempt):

| Outcome | Attempts |
| --- | ---: |
| success | 489 |
| lever cannot settle | 282 |
| local simulation error "Moe state is incomplete for an exact quote" (DI-51) | 559 |
| open-ended bucket not measured | 24 |
| research_revert | 26 |

The research_revert samples break down as follows. All of them used the v2_boost lever
at 1000–3000 WMNT with a 5×–10× boost that exceeded the pair's reserves:

- `Moe: INSUFFICIENT_LIQUIDITY`, from Moe V1 pairs;
- `FusionX: INSUFFICIENT_LIQUIDITY`.

They never qualify and never set a limit. The first one also shows that Moe V1 pairs
revert with the standard UniV2 message.

## Per-class result for every approved class that Moe V1 reaches

Sources:

- `moe_v1_gate.txt`: `python3 evidence/gas/whi-1413/moe_v1_gate.py <both attempts files> data/pool_universe.csv <base profile> config/gas_profiles/mantle_mainnet_v1.json`.
- Moe V1 n: campaign successes whose cycle has a Moe V1 pool. All used the `v2_boost`
  lever, which overrides only a V2 hop's token balance, so V3 and Moe LB hops run on
  canonical state.
- Holdout: the generator's split, per the WHI-1422 policy (min_samples 10, holdout 20%
  heaviest, tail_max_p99 + 20% + 50k).

| Route class | Moe V1 cycles in universe | Moe V1 n | Moe V1 max gas | Base limit (base status) | New total n | New holdout n / max | New limit | Status |
|---|---:|---:|---:|---|---:|---|---:|---|
| `h2:v2+v2` | 2 | 20 | 214293 | 264886 (Approved) | 32 | 6 / 214293 | 307109 | **Approved** (was already) |
| `h2:v2+v3:ticks=0` | 10 (3 Agni-only) | 12 | 236676 | 335569 (Approved) | 36 | 7 / 238014 | 334364 | **Approved** (was already; DI-50 exposure grows, see below) |
| `h2:v2+moe:bins=0` | 3 | **2** | 313068 | 431208 (Approved) | 16 | 3 / 314406 | 431208 | **Approved** (was already; thin, DI-54) |
| `h3:v2+moe+moe:bins=0` | 25 | 10 | 485484 | 583585 (Approved) | 21 | 4 / 485484 | 583585 | **Approved** (was already) |
| `h3:moe+moe+v2:bins=0` | 25 | 35 (run 2) | 567920 | 632658 (Approved) | 49 | 9 / 567920 | 697804 | **Approved** (was already) |
| `h3:v2+v2+v2` | 18 | 41 | 341953 | — (Unsupported, 0 samples) | 41 | 8 / 341953 | 460243 | **Approved (new)** |
| `h3:v2+moe+v2:bins=0` | 12 | 12 | 426123 | — (Unsupported, 0 samples) | 12 | 2 / 426123 | 561348 | **Approved (new)** |
| `h3:moe+v2+v2:bins=0` | 19 | 7 | 433969 | — (Unsupported, 7 samples) | 14 | 2 / 435307 | 572369 | **Approved (new)**: 7 Moe V1 + 7 WHI-1422 FusionX v2_boost samples |

**Every Moe V1 sample in every one of these classes is at or below the base profile's
limit**, so the old bound was already conservative for Moe V1. The regenerated limits
include the samples:

- `h2:v2+v2` rises from 264886 to 307109, because the Moe V1 clone hop is heavier than
  WHI-557's 179071.
- `h2:v2+v3:ticks=0` falls by 1205 gas, from 335569 to 334364, because the added
  samples moved its p99. Every sample in that class, holdout included, is still below
  the new limit (holdout_max 238014).

**Enforced in CI** by `tests/gas_profile_fork_provenance.rs`
`committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools`:

- Rule: every Approved class with a `v2` hop needs ≥ 2 Moe V1 fork samples, all ≤ its
  limit, whenever the committed universe holds Moe V1 rows.
- Check that it is not vacuous: raising the threshold to 3 fails on `h2:v2+moe:bins=0`.
- `committed_profile_approvals_respect_pr108_factory_and_lever_fences` now also treats
  `[whi-1413]` / `[whi-1413-b]` samples as campaign samples that must be `v2_boost`.

## Withheld: V3-hop classes qualified numerically (PR108-F2 rule, DI-50)

These classes are forced Unsupported with reason "withheld (WHI-1413 under PR108-F2,
DI-50)", and their stats stay visible:

- `h2:v3+v2:ticks=1-5`
- `h3:v2+v2+v3:ticks=0`
- `h3:v2+v2+v3:ticks=1-5`
- `h3:v2+v3+v2:ticks=0`
- `h3:v3+v2+v2:ticks=1-5`
- `h3:v3+v2+v3:ticks=1-5`
- `h3:v3+v2+moe:ticks=0:bins=0`
- `h3:v3+v3+v2:ticks=1-5`
- `h3:moe+v2+v3:ticks=0:bins=0`

The reason is the one WHI-1422 gave: approving any of them would also price non-Agni V3
pools that were never measured. The WHI-1422 withholdings are kept.

## Explicitly not qualified

- Every other bucket variant of the 24 Moe V1 topologies stays Unsupported with its
  generator reason: insufficient samples, open-ended bucket, or withheld.
- `h3:v2+v2+moe:bins=0` has 1 Moe V1 sample, and 6 WHI-1422 samples: 7 < 10.
- `h3:moe+v2+moe:*` has no Moe V1 success. All attempts hit incomplete LB state or
  the lever could not settle.
- `h2:moe+v2:*` has no `bins=0` success.

## Effect on the pre-existing DI-50 exposure

`h2:v2+v3:ticks=0` was Approved on WHI-557 samples. With Moe V1, the universe has
**16** `v2+v3` cycles, up from 6, and **11** of them go through a non-Agni V3 pool,
up from 4. The Moe V1 side of the 10 new cycles is measured. Their non-Agni V3 side
is the same unmeasured exposure DI-50 already records, and it is recorded there.
WHI-1413 does not change the V3 factory policy.

## Limits

- **Lever-assisted.** These are lever-assisted fork estimates, not mainnet receipts
  (DI-10), exactly as in WHI-1422.
- **Thin class.** `h2:v2+moe:bins=0` rests on 2 Moe V1 samples. The universe has only
  3 such cycles, and the other amounts could not settle.
- **Holdout independence.** As in WHI-1422, the holdout is the heaviest 20% of the same
  cycles, not independent cycles.
- **No venue axis.** The route key still cannot fail closed per venue (DI-54).
