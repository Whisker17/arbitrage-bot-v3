# WHI-1413: gas qualification for Merchant Moe V1 classic (Mantle mainnet fork)

**Status: every Approved route class is measured, on the exact class, on every admitted
V2 venue it prices in the committed universe.** Fix round 1 (PR109-F1) tightened this
from "on Moe V1".

- The profile now approves **7** classes. Five were approved on the base profile and
  stay approved. Two V3-free classes are new: `h3:v2+v2+v2` and `h3:moe+v2+v2:bins=0`.
- `h3:v2+moe+v2:bins=0` qualified numerically on 12 Moe V1/Moe V1 samples. It is
  **withheld** under PR109-F1 (fix round 1, below): 4 of its 12 universe cycles contain
  FusionX V2, and there are 0 FusionX samples of that exact class.
- Nine V3-hop classes also qualified numerically. They are **withheld** under the
  WHI-1422 PR108-F2 rule (DI-50).
- No approval rests on samples from another bucket or another venue. At 45c0bd9 this
  report said "nothing was extrapolated" while `h3:v2+moe+v2:bins=0` was approved
  for FusionX V2 on Moe V1 samples only; that approval is withdrawn. The mainnet gas
  profile is on the funds path, so a human merge gate applies.

| | base (dev de735db) | 45c0bd9 (before fix round 1) | after WHI-1413 fix round 1 |
|---|---:|---:|---:|
| `content_digest` | `0xa6bc9dac…3d8b` | `0x6d96ee51…c673` | **`0x87f5d70ab8a7361498eda2f0a425cdb7d3a5fc30f70650e537c805aa32a4109c`** |
| Approved | 5 | 8 | 7 |
| qualification samples | 533 | 1022 (+489 Moe V1 campaign successes) |
| research_revert samples | 18 | 44 (+26) |

`MANTLE_MAINNET_PROFILE_DIGEST` in `src/execution/gas_runtime.rs` is updated.
Regeneration is byte-identical: running
`cargo run --locked --example generate_gas_profile -- --print-digest` on the committed
samples and generator config reproduces `mantle_mainnet_v1.json`, digest `0x87f5d70a…109c`.

## Why a campaign was needed (RouteKey has no factory axis)

Moe V1 rows are `ProtocolKind::V2`, so every Approved class with a `v2` hop prices
them. They also make new classes reachable, `h3:v2+v2+v2` among them (the prompting
arb's class).

`evidence/venues/whi-1413/moe_v1_reach.txt` lists every topology that has a cycle
with a Moe V1 pool in the regenerated 151-pool universe: 1564 of 8526 WMNT cycles.
Each of those topologies has Agni-only cycles, so each is measurable.

A Moe V1 pair is a delegatecall clone, so the existing bounds had to be shown to
hold on Moe V1 pools themselves. Whether a clone hop costs more than a FusionX V2 hop
is **not** a general ordering this campaign establishes. Some measured classes point
that way: in `h2:v2+v2` the Moe V1 max is 214293, against WHI-557's FusionX 179071.
Others do not: in `h3:moe+v2+v2:bins=0` the FusionX-only V2 legs peak at 435307 and
the FusionX/Moe V1 legs at 433969. Each venue therefore needs its own exact-class
samples (fix round 1, below). No bound is justified by a presumed venue ordering.

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
   In fix round 1 it also applies the PR109-F1 venue rule.
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
| `h3:v2+moe+v2:bins=0` | 12 | 12 | 426123 | — (Unsupported, 0 samples) | 12 | 2 / 426123 | (561348) | **Unsupported: withheld (PR109-F1)**. Approved at 45c0bd9; 0 FusionX samples of this exact class |
| `h3:moe+v2+v2:bins=0` | 19 | 7 | 433969 | — (Unsupported, 7 samples) | 14 | 2 / 435307 | 572369 | **Approved (new)**: 7 Moe V1 + 7 WHI-1422 FusionX v2_boost samples |

**Every Moe V1 sample in every one of these classes is at or below the base profile's
limit**, so the old bound was already conservative for Moe V1. The regenerated limits
include the samples:

- `h2:v2+v2` rises from 264886 to 307109, because its Moe V1 samples (max 214293)
  are heavier than WHI-557's 179071 in this class.
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
- This Moe V1 guard alone was **not sufficient**: it could not see a class priced for
  FusionX V2 on Moe V1 samples only. Fix round 1 adds the per-venue, exact-class guard
  below.

## Fix round 1: every admitted V2 venue, on the exact class (PR109-F1)

**Finding (review of 45c0bd9).** `h3:v2+moe+v2:bins=0` was approved (limit 561348) on
12 samples. Every one of them used Moe V1 for both V2 hops. 4 of the class's 12
universe cycles contain FusionX V2, and RouteKey has no venue axis, so the approval
also priced FusionX. The campaign did sample one of those cycles, `0x3e5922cd…`
(FusionX) > `0x3eb7e346…` (Moe LB) > `0x76386861…` (Moe V1). But its 7 successes all
crossed at least one bin, even at 0.01 WMNT, so they fall in `bins=1-3 / 4-10 / 11+`,
not in `bins=0`. The old guard only asked for "some Moe V1 pool" per class, so it
could not see this reverse exposure.

**Rule.** For every Approved class, every admitted V2 venue (factory) occurring on the
committed universe's WMNT cycles of that class's topology needs **>= 2 fork samples
of the exact class** (same topology and crossing buckets) touching one of its pools,
all <= the class's limit. Samples from another bucket or another venue never count.
`withhold.py` applies it to every Approved class, the five base classes included.

**Enforced in CI** by `tests/gas_profile_fork_provenance.rs`:

- `committed_approved_classes_are_measured_on_every_v2_venue_they_price`. Its cycle
  enumerator is asserted equal to the production `PathFinder` count
  (`service::count_settlement_cycles`, 8526 cycles).
- **Non-vacuity check.** With 45c0bd9's `mantle_mainnet_v1.json` swapped in, the guard
  fails with exactly `h3:v2+moe+v2:bins=0: prices fusionx-v2 (4 universe cycles) on
  0 exact-class fork samples (< 2)`. The test
  `venue_guard_rejects_the_45c0bd9_approval_of_h3_v2_moe_v2_bins_0` pins this in
  memory.

**Per venue, for each class Approved at 45c0bd9 (the samples are unchanged).** Columns: universe cycles of the topology
containing the venue / exact-class fork samples touching it / their max gas.

| Class | Limit | FusionX V2 | Moe V1 | Result |
|---|---:|---|---|---|
| `h2:v2+v2` | 307109 | 2 / 32 / 214293 | 2 / 20 / 214293 | Approved |
| `h2:v2+v3:ticks=0` | 334364 | 6 / 24 / 238014 | 10 / 12 / 236676 | Approved |
| `h2:v2+moe:bins=0` | 431208 | 2 / 14 / 317673 | 3 / 2 / 313068 | Approved (Moe V1 thin, DI-54) |
| `h3:v2+moe+moe:bins=0` | 583585 | 8 / 11 / 444654 | 25 / 10 / 485484 | Approved |
| `h3:moe+moe+v2:bins=0` | 697804 | 8 / 14 / 485548 | 25 / 35 / 567920 | Approved |
| `h3:v2+v2+v2` | 460243 | 8 / 20 / 341953 | 18 / 41 / 341953 | Approved |
| `h3:moe+v2+v2:bins=0` | 572369 | 8 / 14 / 435307 | 19 / 7 / 433969 | Approved |
| `h3:v2+moe+v2:bins=0` | (561348) | **4 / 0 / —** | 12 / 12 / 426123 | **withheld** |

No other Approved class, base or new, lacks a venue under the stricter rule.

**Why withhold rather than measure.** Of the 4 FusionX cycles:

- The one measured cycle never reached `bins=0` at any campaign amount.
- Its reverse, `0x76386861…` > `0x3eb7e346…` > `0x3e5922cd…`, uses the same LB pair in
  the direction where every attempt on the Moe-V1-only analogue ended "Moe state is
  incomplete for an exact quote" (DI-51).
- The remaining two cycles go through LB pair `0x3f004760…`. At best they could give
  about 6 small-amount samples each. Those would be 12 samples from 2 cycles, of
  near-identical gas, resting on an unmeasured assumption that they reach `bins=0` at
  all.

The fail-closed default applies. The class stays explicit Unsupported; its samples
remain visible as stats. It can be re-approved only by exact-class FusionX
measurements that pass this guard.

**Profile.** Digest `0x87f5d70a…109c`, 307 profiles, 7 Approved, 300 Unsupported.
Relative to 45c0bd9 only that one entry changes. `withhold.py` is idempotent (a
re-run leaves the config byte-identical), and regeneration is byte-identical.

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
- **No venue axis.** The route key still cannot fail closed per venue (DI-54). The
  per-venue exact-class guard keeps each Approved class measured on every admitted V2
  venue it prices in the committed universe. A universe regeneration that adds such
  a (class, venue) pair without samples fails that test, and the class must then be
  measured or withheld.
