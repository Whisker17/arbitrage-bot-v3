# WHI-1422: route-class qualification campaign (Mantle mainnet fork)

**Status:** partial qualification. The campaign ran to completion. After review
fix round 1, **5** route classes are Approved: the 3 carried over from WHI-557 and 2
new V2+Moe 3-hop classes. **16** classes that qualified numerically are **withheld**
(forced Unsupported with a reason) because of review findings PR108-F2 (V3 factory
exposure) and PR108-F3 (lever-state conservatism); see
[Fix round 1](#fix-round-1-pr108-f2f3-disposition). Every other bucket variant of
every 2- and 3-hop topology over {v2, v3, moe} is **explicitly Unsupported with a
reason**. Over the committed universe the profile gives zero `UnknownRoute`. No
class was approved by extrapolation, and no class was approved to make a test
pass. A human merge gate applies because this is the mainnet gas profile.

## What changed in the profile

| | before (WHI-557, `0xa18811da…`) | after (this campaign + fix round 1) |
|---|---:|---:|
| profile entries | 24 | 307 (every 2..3-hop bucket variant plus the old h4 entry) |
| Approved | 3 | 5 (the 3 WHI-557 classes stay Approved, and `h2:v2+v3:ticks=0` gas_limit goes from 328088 to 335569 because campaign samples were added; new: `h3:v2+moe+moe:bins=0`, `h3:moe+moe+v2:bins=0`). The pre-fix draft (`0x6d3daa2f…7799`) had 21. |
| `content_digest` | `0xa18811daf4a27e071575db61de1cb57751fb619378f754188c569c4144c1ccba` | `0xa6bc9dacb5509d6ac480ad5ba1eea52b5d260605b09000198cc80fada5dd3d8b` |
| qualification samples | 48 | 533 (485 of them from this campaign) |
| research_revert samples | 1 | 18 |

Runtime identity: `MANTLE_MAINNET_PROFILE_DIGEST` in `src/execution/gas_runtime.rs`
is now `0xa6bc9dac…3d8b`. Executor identity is unchanged (`executor_code_hash`
`0x50f51b77…26ef`, ABI digest `0x9f2f241b…a0d6`). Running
`cargo run --example generate_gas_profile` on the committed samples and
generator config gives back a byte-identical `mantle_mainnet_v1.json`.

The profile now covers **every 2..3-hop topology** that the committed universe's
count-based census generates. That is 36 topologies and 306 bucket keys, each with
an explicit entry. `service::fee_scoring::tests::committed_universe_topologies_have_zero_unknown_route`
asserts this: it calls the WHI-1421 `topology_profile_support` for each topology
and `inspect_route` for each key.

## Provenance

- **Harness:** `examples/remeasure_mainnet_gas_profile.rs` with `--campaign`
  (`examples/remeasure_mainnet_gas_profile/campaign.rs`). This extends the WHI-557
  harness: same synthetic executor, same override helpers
  (`src/execution/mainnet_fork_harness.rs`), same `eth_estimateGas` measurement.
- **Commit:** every record in `attempts.jsonl` and `runs.jsonl` carries
  `run_git_head = 2cb5ecf4f6ae0fc871ba24172d0580d658b7b094`. The tree was clean at
  each start (`git_dirty=false`, checked by the harness; the campaign's own output
  directory is excluded from the check).
- **Binary:** built once from clean `2cb5ecf` and copied to a stamped path.
  sha256 `7e55b36c9806abfd3455a48158b6cd10d87d7ee276772964a0d48c41d98474b3`.
  Build flags: `CARGO_PROFILE_DEV_{LTO=false,OPT_LEVEL=1,DEBUG=0,CODEGEN_UNITS=256} cargo build --locked --example remeasure_mainnet_gas_profile`.
- **Fork:** Mantle mainnet (chain 5000) at block **98969898**, block hash
  **`0x77e9802850e09580e8527b3ea75ed93510331e91339b8272af4ed5660ef76374`**.
  I checked this hash on both the anvil fork and the upstream endpoint.
- **RPC class:** pool state is read from the public endpoint `https://rpc.mantle.xyz`,
  read-only, at the pinned block hash. `eth_estimateGas` runs on a local anvil fork,
  `http://127.0.0.1:8547`:
  `anvil --fork-url https://rpc.mantle.xyz --fork-block-number 98969898 --port 8547 --chain-id 5000 --no-mining --no-rate-limit --silent`
  (anvil 1.7.1). No transaction was ever sent, no private key was used, and nothing
  was broadcast.
- **Commands and times (UTC, 2026-09-26).** All runs used the same arguments:
  `--campaign --block 98969898 --measure-rpc-url http://127.0.0.1:8547 --cycles-per-topology 3 --rpc-timeout-secs 120`.
  1. Main run, 09:23:43 → 10:08:05. Measured 1502 attempts; 15 were left
     unfinished, all RPC timeouts in `v2+v3+v3` (1) and `v3+v3+v3` (14). The
     harness exited non-zero by design and kept the evidence.
     Log: `logs/1-campaign-run.log`.
  2. One bounded `--resume`, 10:08:35 → 10:25:43, after restarting anvil with the
     same flags. It measured the 15 and reused 1497. Two attempts still timed out
     (listed below). Log: `logs/2-campaign-resume.log`.
  3. `--campaign-finalize`, 10:25:43 → 10:29:38. This measured nothing. It rebuilt
     the samples from the attempts recorded at the same HEAD and wrote
     `config/gas_profiles/pinned/samples.jsonl`, `generator_config.json` and
     `mantle_mainnet_v1.json`. Log: `logs/3-campaign-finalize.log`.
  4. Review fix round 1 measured nothing. `fix1_withhold.py` edited
     `generator_config.json`, adding the forced PR108-F2/F3 entries and the
     corrected `sampling_policy.description`. `generate_gas_profile` then
     regenerated `mantle_mainnet_v1.json`. `samples.jsonl` is unchanged.
- **Raw outputs:** `attempts.jsonl` has one record per attempt, including the
  `GasSample` for each success or revert. The file is append-only; the last record
  per attempt id wins. `runs.jsonl` has one provenance record per invocation.
  `topology_rank.txt` holds the cycle counts per topology.
- **Tables** below: `python3 evidence/gas/whi-1422/make_tables.py`. The fix-round
  status-change table: `python3 evidence/gas/whi-1422/make_tables.py --before <382d840 profile>`
  (command in the script's docstring).
  **Ranking:** `python3 evidence/gas/whi-1422/rank_topologies.py data/pool_universe.csv`.

### Pilot runs (not used as evidence)

- **Pilot pid 2488.** Its binary predates commit `de32280`, so it is not from a
  clean committed SHA (docs/TRAPS.md #7). It recorded 704 attempts and then hung
  for 18 minutes in one socket read on the fork.
- **First clean run at `53ffcbb`.** It hung at **the same attempt**:
  `v2+moe+v3`, 300 WMNT, `v2_boost`, right after a 311-tick, 8.1M-gas sample at
  100 WMNT. The new request timeout surfaced it as an `RPC-FAILURE` line.

This was a deterministic deep V3 walk, not a network stall. Anvil lazily fetches
every tick and bitmap word from upstream, so the walk has no bound in fork time
either. `2cb5ecf` fixes this. Open-ended buckets are forced Unsupported whatever
is measured, so the harness now takes one informational sample per cycle and
lever in them, and never sends a hop with more than 100 crossings. Both partial
outputs are kept outside the repo in the orchestrator's artifact directory.

## Methodology, and how it differs from WHI-557 / DI-10

What is canonical:

- Real venue contracts: Agni V3, Merchant Moe **Liquidity Book**, and the
  universe's V2 pools. All are real universe pools at the pinned block.
- Production calldata: `executeArbitrage` from the WHI-501 patched runtime.
- The production route key: every sample's key, crossing bucket included, comes
  from `simulate_mixed_path_with_route_key`. That is the function the live
  optimizer and materialize use.
- Gas: `eth_estimateGas` on the fork.

What is **not** canonical: the harness picks cycles by topology, not by
profitability. At the pinned block the sampled cycles do not clear the
executor's on-chain profit check, so an honest sample would revert. A cycle across
pools with a real price discrepancy can be profitable, but the campaign cannot
count on finding one per class at one block. Each sample therefore uses one
**settlement lever**, a state override on the fork:

- **`v2_boost`**: one V2 hop pays out more than its reserves imply, backed by a
  balance override. V2 has no crossings, so every V3 and Moe hop in the route is
  real.
- **`displace`**: the last V3 or Moe pool is moved to the state that a real WMNT-in
  swap of size D would leave behind. That state is computed by the pool's own
  simulation. The last hop then really crosses those ticks or bins back.
- **`inflate`**, the WHI-557 lever: the last V3 or Moe hop gets a favourable price
  and inflated liquidity, so it crosses nothing. It is used only when every other
  hop also crosses nothing, which makes it an honest bucket-0 sample.

WHI-557 and DI-10 ask for canonical-state measurement. These samples are
lever-induced state and are not replays of real profitable transactions. Treat the
numbers as fork-measured and lever-assisted, not as mainnet receipts.

`v2_boost` leaves every V3 and Moe hop on unmodified pool state. `inflate` and
`displace` rewrite V3/Moe state (slot0 price and tick, liquidity, bin words). Review
PR108-F3 pointed out that this can make a same-class sample *cheaper* than
canonical. The route bucket counts only **initialized** ticks, so inflated liquidity
can remove raw-tick and bitmap-word traversal and tick-dependent oracle work while
keeping a zero-crossing key. Storage-transition costs and arithmetic branches also
depend on the starting state. A holdout drawn from the same lever cannot detect
that bias. Fix round 1 therefore approves no class whose campaign samples use
`inflate` or `displace` (see below).

**Lever artifacts I observed:**

- On one `v3+v3+v3` cycle (`…>0x8e2c009e…`), `displace` samples keyed `ticks=1-5`
  cost about **20.8M gas**, against a simulated 0 crossings on the displaced last
  hop. The simulation and the execution diverged there. The holdout gate rejected
  `h3:v3+v3+v3:ticks=1-5` (holdout_max 20,862,522 > limit 817,988), which is the
  fail-closed direction.
- Four `v3+v3+v3` "reverted" records are anvil upstream-fetch errors (`-32603 failed
  to get storage`), not EVM reverts.
- Thirteen `FusionX` reverts come from the V2 hop's own check under `v2_boost`.

All of these are `research_revert` samples: they never qualify and never set a
limit.

**Scope limits:**

- Only Agni-factory V3 pools are measured. That is 33 of the 87 `agni-v3` rows. The
  WHI-501 executor implements only `agniSwapCallback`, and the other UniV3-family
  factories in the universe were excluded without being tested. The profile key
  does not include the factory (see DI-50).
- Deep Moe coverage is limited by the default `MoeSnapshotSyncConfig` bin range:
  227 attempts ended "Moe state is incomplete for an exact quote".
- Only **zero-bucket** classes qualified. Two nonzero-bucket classes reached
  `min_samples` = 10 (`h2:v3+v3:ticks=1-5` and `h3:v3+v3+v3:ticks=1-5`) and both
  failed holdout. Every other nonzero bucket is below `min_samples`. No zero-bucket
  measurement prices a crossing route.

## Ranking (which classes unlock cycles)

Evidence from the live WHI-1411 run
(`evidence/shadow/whi-1411-rejection-liveness/STATUS.md`): `cycles_evaluated_sum=20886`,
`unknown_route_sum=20382`, `unapproved_route_sum=480`, `paths_quoted_sum=24`.

**After fix round 1:** only `v2+v3` (6), `v2+moe` (2), `v2+moe+moe` (8) and
`moe+moe+v2` (8) have an Approved variant. That is **24 of 6962 cycles (0.34%)**,
at zero crossing only. The rest of this section describes the pre-fix draft
(`0x6d3daa2f…7799`), whose 21 approvals covered 35.8%. Every universe topology
still has an explicit entry, so the remaining cycles are rejected as Unapproved,
never Unknown.

Cycles per topology over the committed universe are in `topology_rank.txt`
(WMNT-settled, 2..3 hops, all 130 rows). With the pre-fix draft profile, the topologies
that had an Approved variant covered **2490 of 6962 cycles (35.8%)**. The biggest
remaining block is `v3+v3+v3`, with 3436 cycles (49%). It stays Unsupported: none
of its `ticks=0` attempts settled (inflate needs every hop at 0 crossings), and
`ticks=1-5` failed holdout.

```
total_cycles 6962
3436 3 v3+v3+v3
679 3 v3+v3+moe
679 3 moe+v3+v3
440 3 v3+moe+v3
336 3 v3+v2+v3
217 3 v3+moe+moe
217 3 moe+moe+v3
146 2 v3+v3
140 3 v2+v3+v3
140 3 v3+v3+v2
110 3 moe+v3+moe
90 3 moe+moe+moe
59 3 v3+v2+moe
59 3 moe+v2+v3
26 2 v3+moe
26 2 moe+v3
21 3 v2+moe+v3
21 3 v3+moe+v2
19 3 v2+v2+v3
19 3 v3+v2+v2
16 3 v2+v3+moe
16 3 moe+v3+v2
8 3 v2+moe+moe
8 3 moe+moe+v2
8 3 moe+v2+moe
6 2 v2+v3
6 2 v3+v2
6 2 moe+moe
2 3 v2+v2+moe
2 2 v2+moe
2 3 moe+v2+v2
2 2 moe+v2
```

## Every class, with sample count, holdout result and final status

"campaign n" counts this campaign's successful samples. "total n in profile" counts
qualification samples, WHI-557 samples included. Every bucket variant that is not
listed below had **no** samples, and is Unsupported with "insufficient
qualification samples: 0 < min_samples 10" or with the open-ended reason. A
withheld class publishes no limit or holdout; its pre-fix numbers are in
[Fix round 1](#fix-round-1-pr108-f2f3-disposition).

### Every measured class (campaign successes or reverts > 0)

| route key | campaign n | reverts | total n in profile | holdout n / max | gas_limit | expected | status / reason |
|---|---:|---:|---:|---|---:|---:|---|
| `h2:moe+moe:bins=0` | 7 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 7 < min_samples 10 |
| `h2:moe+v2:bins=1-3` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h2:moe+v2:bins=11+` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h2:moe+v2:bins=4-10` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h2:moe+v3:ticks=0:bins=0` | 11 | 0 | 11 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h2:v2+moe:bins=0` | 2 | 0 | 14 | 2 / 314406 (pass) | 431208 | 317673 | **Approved** |
| `h2:v2+moe:bins=1-3` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h2:v2+moe:bins=11+` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h2:v2+moe:bins=4-10` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h2:v2+v3:ticks=0` | 12 | 0 | 24 | 4 / 238014 (pass) | 335569 | 231740 | **Approved** |
| `h2:v2+v3:ticks=1-5` | 3 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 3 < min_samples 10 |
| `h2:v2+v3:ticks=21+` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h2:v2+v3:ticks=6-20` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h2:v3+moe:ticks=0:bins=0` | 18 | 0 | 18 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h2:v3+v2:ticks=0` | 9 | 0 | 15 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h2:v3+v2:ticks=1-5` | 6 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 6 < min_samples 10 |
| `h2:v3+v2:ticks=21+` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h2:v3+v2:ticks=6-20` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h2:v3+v3:ticks=0` | 23 | 0 | 29 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h2:v3+v3:ticks=1-5` | 10 | 0 | 10 | 2 / 770919 (FAIL) | 509836 | 310658 | Unsupported: holdout/train failed limit gate: holdout_max=770919 gas_limit=509836 min_block=60000000 |
| `h2:v3+v3:ticks=21+` | 3 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h2:v3+v3:ticks=6-20` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:moe+moe+v2:bins=0` | 14 | 0 | 14 | 2 / 485548 (pass) | 632658 | 409266 | **Approved** |
| `h3:moe+moe+v3:ticks=0:bins=0` | 14 | 0 | 14 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:moe+v2+moe:bins=0` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:moe+v2+moe:bins=1-3` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:moe+v2+moe:bins=11+` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:moe+v2+moe:bins=4-10` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:moe+v2+v2:bins=0` | 7 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 7 < min_samples 10 |
| `h3:moe+v2+v3:ticks=0:bins=0` | 6 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 6 < min_samples 10 |
| `h3:moe+v2+v3:ticks=1-5:bins=0` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:moe+v3+moe:ticks=0:bins=0` | 7 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 7 < min_samples 10 |
| `h3:moe+v3+v2:ticks=0:bins=0` | 12 | 0 | 12 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:moe+v3+v2:ticks=1-5:bins=0` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:moe+v3+v3:ticks=0:bins=0` | 12 | 0 | 12 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:moe+v3+v3:ticks=1-5:bins=0` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:v2+moe+moe:bins=0` | 11 | 0 | 11 | 2 / 444654 (pass) | 583585 | 438490 | **Approved** |
| `h3:v2+moe+moe:bins=1-3` | 7 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 7 < min_samples 10 |
| `h3:v2+moe+moe:bins=4-10` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v2+moe+v3:ticks=0:bins=0` | 10 | 0 | 10 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v2+moe+v3:ticks=0:bins=1-3` | 4 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 4 < min_samples 10 |
| `h3:v2+moe+v3:ticks=1-5:bins=0` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:v2+moe+v3:ticks=1-5:bins=1-3` | 4 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 4 < min_samples 10 |
| `h3:v2+moe+v3:ticks=1-5:bins=11+` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v2+moe+v3:ticks=21+:bins=0` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v2+moe+v3:ticks=6-20:bins=0` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v2+v2+moe:bins=0` | 6 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 6 < min_samples 10 |
| `h3:v2+v2+v3:ticks=0` | 8 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 8 < min_samples 10 |
| `h3:v2+v2+v3:ticks=1-5` | 5 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 5 < min_samples 10 |
| `h3:v2+v2+v3:ticks=21+` | 0 | 2 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v2+v2+v3:ticks=6-20` | 1 | 1 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v2+v3+moe:ticks=0:bins=0` | 15 | 0 | 15 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v2+v3+moe:ticks=0:bins=1-3` | 3 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 3 < min_samples 10 |
| `h3:v2+v3+moe:ticks=0:bins=4-10` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v2+v3+v3:ticks=0` | 19 | 2 | 19 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v2+v3+v3:ticks=1-5` | 5 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 5 < min_samples 10 |
| `h3:v2+v3+v3:ticks=21+` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v2+v3+v3:ticks=6-20` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:v3+moe+moe:ticks=0:bins=0` | 12 | 0 | 12 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+v2:ticks=0:bins=0` | 17 | 2 | 17 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+v2:ticks=1-5:bins=0` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:v3+moe+v2:ticks=21+:bins=1-3` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+moe+v3:ticks=0:bins=0` | 16 | 0 | 16 | - | - | - | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+v3:ticks=1-5:bins=0` | 6 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 6 < min_samples 10 |
| `h3:v3+moe+v3:ticks=21+:bins=0` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v2+moe:ticks=0:bins=0` | 9 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 9 < min_samples 10 |
| `h3:v3+v2+moe:ticks=0:bins=1-3` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v3+v2+moe:ticks=0:bins=11+` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v2+moe:ticks=0:bins=4-10` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v3+v2+moe:ticks=1-5:bins=0` | 4 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 4 < min_samples 10 |
| `h3:v3+v2+v2:ticks=0` | 18 | 0 | 18 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+v2+v2:ticks=1-5` | 6 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 6 < min_samples 10 |
| `h3:v3+v2+v2:ticks=21+` | 1 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v2+v2:ticks=6-20` | 2 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |
| `h3:v3+v2+v3:ticks=0` | 12 | 0 | 12 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+v2+v3:ticks=1-5` | 8 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 8 < min_samples 10 |
| `h3:v3+v2+v3:ticks=21+` | 1 | 2 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v2+v3:ticks=6-20` | 0 | 2 | 0 | - | - | - | Unsupported: insufficient qualification samples: 0 < min_samples 10 |
| `h3:v3+v3+moe:ticks=0:bins=0` | 5 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 5 < min_samples 10 |
| `h3:v3+v3+v2:ticks=0` | 21 | 2 | 21 | - | - | - | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+v3+v2:ticks=1-5` | 4 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 4 < min_samples 10 |
| `h3:v3+v3+v2:ticks=21+` | 2 | 0 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v3+v2:ticks=6-20` | 1 | 0 | 0 | - | - | - | Unsupported: insufficient qualification samples: 1 < min_samples 10 |
| `h3:v3+v3+v3:ticks=1-5` | 16 | 2 | 16 | 3 / 20862522 (FAIL) | 817988 | 578729 | Unsupported: holdout/train failed limit gate: holdout_max=20862522 gas_limit=817988 min_block=60000000 |
| `h3:v3+v3+v3:ticks=21+` | 2 | 1 | 0 | - | - | - | Unsupported: open-ended crossing bucket (ticks=21+ and/or bins=11+): gas grows with every extra crossing and a finite sampl |
| `h3:v3+v3+v3:ticks=6-20` | 2 | 1 | 0 | - | - | - | Unsupported: insufficient qualification samples: 2 < min_samples 10 |

### Profile status summary per topology (all bucket variants)

| topology | variants | approved | withheld (PR108-F2/F3) | unsupported (open-ended) | unsupported (other) |
|---|---:|---:|---:|---:|---:|
| `h2:v2+v2` | 1 | 1 | 0 | 0 | 0 |
| `h2:v2+v3` | 4 | 1 | 0 | 1 | 2 |
| `h2:v3+v2` | 4 | 0 | 1 | 1 | 2 |
| `h2:v3+v3` | 4 | 0 | 1 | 1 | 2 |
| `h2:moe+v2` | 4 | 0 | 0 | 1 | 3 |
| `h2:moe+v3` | 16 | 0 | 1 | 7 | 8 |
| `h2:v2+moe` | 4 | 1 | 0 | 1 | 2 |
| `h2:v3+moe` | 16 | 0 | 1 | 7 | 8 |
| `h2:moe+moe` | 4 | 0 | 0 | 1 | 3 |
| `h3:v2+v2+v2` | 1 | 0 | 0 | 0 | 1 |
| `h3:v2+v2+v3` | 4 | 0 | 0 | 1 | 3 |
| `h3:v2+v3+v2` | 4 | 0 | 0 | 1 | 3 |
| `h3:v2+v3+v3` | 4 | 0 | 1 | 1 | 2 |
| `h3:v3+v2+v2` | 4 | 0 | 1 | 1 | 2 |
| `h3:v3+v2+v3` | 4 | 0 | 1 | 1 | 2 |
| `h3:v3+v3+v2` | 4 | 0 | 1 | 1 | 2 |
| `h3:v3+v3+v3` | 4 | 0 | 0 | 1 | 3 |
| `h3:moe+v2+v2` | 4 | 0 | 0 | 1 | 3 |
| `h3:moe+v2+v3` | 16 | 0 | 0 | 7 | 9 |
| `h3:moe+v3+v2` | 16 | 0 | 1 | 7 | 8 |
| `h3:moe+v3+v3` | 16 | 0 | 1 | 7 | 8 |
| `h3:v2+moe+v2` | 4 | 0 | 0 | 1 | 3 |
| `h3:v2+moe+v3` | 16 | 0 | 1 | 7 | 8 |
| `h3:v2+v2+moe` | 4 | 0 | 0 | 1 | 3 |
| `h3:v2+v3+moe` | 16 | 0 | 1 | 7 | 8 |
| `h3:v3+moe+v2` | 16 | 0 | 1 | 7 | 8 |
| `h3:v3+moe+v3` | 16 | 0 | 1 | 7 | 8 |
| `h3:v3+v2+moe` | 16 | 0 | 0 | 7 | 9 |
| `h3:v3+v3+moe` | 16 | 0 | 0 | 7 | 9 |
| `h3:moe+moe+v2` | 4 | 1 | 0 | 1 | 2 |
| `h3:moe+moe+v3` | 16 | 0 | 1 | 7 | 8 |
| `h3:moe+v2+moe` | 4 | 0 | 0 | 1 | 3 |
| `h3:moe+v3+moe` | 16 | 0 | 0 | 7 | 9 |
| `h3:v2+moe+moe` | 4 | 1 | 0 | 1 | 2 |
| `h3:v3+moe+moe` | 16 | 0 | 1 | 7 | 8 |
| `h3:moe+moe+moe` | 4 | 0 | 0 | 1 | 3 |
| `h4:v2+v2+v2+v2` | 1 | 0 | 0 | 0 | 1 |

Attempt outcomes (last record per attempt, 1512 unique of 1527 records):
- skipped: lever cannot settle: 725
- success: 485
- skipped: local simulation error: 227
- skipped: open-ended bucket not measured: 56
- reverted (research_revert sample): 17
- rpc_error (unmeasured): 2

Unmeasured attempts:
- v3+v3+v3 0x1858d52cf57c07a018171d7a1e68dc081f17144f>0x2bd0f40c241eabd326545a6467bb2da88bb46181>0x54169896d28dec0ffabe3b16f90f71323774949f mWMNT=30000 lever=displace key=h3:v3+v3+v3:ticks=6-20: rpc_error: rpc failure: estimateGas: TIMEOUT RPC request timed out after 120000ms
- v3+v3+v3 0x9ec313ff05946b6f3860a99b470625abba7eb0a2>0x2bd0f40c241eabd326545a6467bb2da88bb46181>0x8e2c009e45420d2b36bc15315f9de8ceca2cc724 mWMNT=1000 lever=displace key=h3:v3+v3+v3:ticks=1-5: rpc_error: rpc failure: estimateGas: TIMEOUT RPC request timed out after 120000ms
Approved keys: 5 of 307


The two unmeasured attempts above timed out in both the main run and the resume,
with 2 tries each. Their classes fall to Unsupported for lack of samples. I did not
loop the retries further.

## Fix round 1: PR108-F2/F3 disposition

The independent review of draft PR #108 (`review-pr108.md`, REVIEWER
mantle/gpt-6-astra, effort high) returned **BLOCKING** at `382d840`. This section
records what changed in response. The generator input is edited by
`python3 evidence/gas/whi-1422/fix1_withhold.py`, which is deterministic and
idempotent. It adds forced-Unsupported entries (the existing
`unsupported_route_classes` mechanism) to `config/gas_profiles/pinned/generator_config.json`,
and `cargo run --example generate_gas_profile` then regenerates the profile.
No sample was added, removed or changed. The test
`committed_profile_approvals_respect_pr108_factory_and_lever_fences`
(`tests/gas_profile_fork_provenance.rs`) enforces both rules on the committed
profile. It fails on the pre-fix profile (`h3:v2+v3+v3:ticks=0`).

### PR108-F2: V3 factory exposure (agreed, fixed within the fences)

`RouteKey` and runtime pricing have no factory axis. The campaign measured only
Agni-factory V3 pools, because the executor implements only `agniSwapCallback`.
54 of the committed universe's 87 `agni-v3` rows come from five other factories.
Of the universe's 6834 WMNT cycles that have a V3 hop, **6062 use at least one
non-Agni V3 pool**. Any V3 approval therefore also prices pools whose gas and
executability were never measured.

**Rule:** a class that this PR would newly approve and that has a V3 hop is
forced Unsupported with the reason "withheld (PR108-F2, DI-50): …". Its stats
stay visible. **16 classes** are withheld. The universe, pin, `path_index` and
execution code are not changed; factory-aware venue eligibility is a separate
decision (DI-50).

**Pre-existing exposure (not changed by this PR):** `h2:v2+v3:ticks=0` was Approved
on the base profile (`0xa18811da…`) and stays Approved. 4 of the universe's 6
`v2+v3` cycles use a non-Agni V3 pool. This is recorded in DI-50. `h2:v2+v2` and
`h2:v2+moe:bins=0` have no V3 hop, and the universe's V2 and Moe rows each come
from a single factory.

### PR108-F3: lever conservatism (agreed, fixed)

**Rule:** a class may stay Approved only if every campaign sample in its train
**and** holdout sets used `v2_boost`. That lever overrides only a V2 hop's token
balance, so every V3 and Moe hop runs on unmodified pool state. A class whose
samples include `inflate` or `displace` is withheld. None of these classes has a
measured comparison showing its lever samples cost at least as much gas as
canonical state.

Lever audit of the 21 pre-fix approvals, train / holdout split as in the generator:

| class | train | holdout | disposition |
|---|---|---|---|
| `h2:v2+v2` | WHI-557 10 | WHI-557 2 | Approved (base; no V3/Moe hop) |
| `h2:v2+v3:ticks=0` | WHI-557 12, v2_boost 8 | v2_boost 4 | Approved (base) |
| `h2:v2+moe:bins=0` | WHI-557 12 | v2_boost 2 | Approved (base) |
| `h3:v2+moe+moe:bins=0` | v2_boost 9 | v2_boost 2 | **Approved** (new) |
| `h3:moe+moe+v2:bins=0` | v2_boost 12 | v2_boost 2 | **Approved** (new) |
| `h2:v3+v2:ticks=0` | WHI-557 6, v2_boost 6 | v2_boost 3 | withheld F2 |
| `h3:v2+v3+v3:ticks=0` | v2_boost 16 | v2_boost 3 | withheld F2 |
| `h3:v2+v3+moe:ticks=0:bins=0` | v2_boost 12 | v2_boost 3 | withheld F2 |
| `h3:v2+moe+v3:ticks=0:bins=0` | v2_boost 8 | v2_boost 2 | withheld F2 |
| `h3:v3+v2+v2:ticks=0` | v2_boost 15 | v2_boost 3 | withheld F2 |
| `h3:v3+v2+v3:ticks=0` | v2_boost 10 | v2_boost 2 | withheld F2 |
| `h3:v3+v3+v2:ticks=0` | v2_boost 17 | v2_boost 4 | withheld F2 |
| `h3:v3+moe+v2:ticks=0:bins=0` | v2_boost 14 | v2_boost 3 | withheld F2 |
| `h3:moe+v3+v2:ticks=0:bins=0` | v2_boost 10 | v2_boost 2 | withheld F2 |
| `h2:v3+v3:ticks=0` | WHI-557 6, inflate 11, displace 7 | inflate 3, displace 2 | withheld F2+F3 |
| `h2:v3+moe:ticks=0:bins=0` | inflate 14, displace 1 | inflate 3 | withheld F2+F3 |
| `h2:moe+v3:ticks=0:bins=0` | displace 3, inflate 6 | inflate 1, displace 1 | withheld F2+F3 |
| `h3:v3+moe+v3:ticks=0:bins=0` | inflate 8, displace 5 | inflate 1, displace 2 | withheld F2+F3 |
| `h3:v3+moe+moe:ticks=0:bins=0` | inflate 10 | inflate 2 | withheld F2+F3 |
| `h3:moe+v3+v3:ticks=0:bins=0` | inflate 7, displace 3 | displace 2 | withheld F2+F3 |
| `h3:moe+moe+v3:ticks=0:bins=0` | inflate 7, displace 5 | displace 2 | withheld F2+F3 |

**The base approvals.** The WHI-557 samples behind `h2:v2+v3:ticks=0` and
`h2:v2+moe:bins=0` nudged the V3 or Moe hop's state themselves (see their
`notes`). That predates this PR, so their status stays as on base. The campaign
added canonical-state (`v2_boost`) samples to both classes, which gives a measured
comparison:

- **`h2:v2+v3:ticks=0`:** 12 `v2_boost` samples, 231260–238014 gas, on two Agni
  pools. The 12 WHI-557 samples are all 231740. The canonical-state maximum is
  2.7% above the nudged value. The 4 `v2_boost` holdout samples are below the
  limit (335569). A limit derived from the canonical samples alone would be
  335617.
- **`h2:v2+moe:bins=0`:** 2 `v2_boost` samples, 226535 and 314406, are both below
  the WHI-557 value (317673) and the limit (431208).

**The ~20.8M-gas divergence.** This was `displace` on one `v3+v3+v3` cycle: the
simulation said the displaced last hop crossed 0 ticks, but execution walked
many. It comes from spliced slot0/liquidity words that no longer match the pool's
tick data. None of the 5 remaining approvals has an `inflate` or `displace`
campaign sample. Their V3/Moe hops ran on unmodified state, where the quote is the
production simulator on real state. Gas per class shows no outlier:

- `h2:v2+v3:ticks=0`: 231260–238014
- `h2:v2+moe:bins=0`: 226535–317673
- `h3:v2+moe+moe:bins=0`: 327942–444654
- `h3:moe+moe+v2:bins=0`: 406172–485548

Every value is under its limit.

**Limits of this evidence:**

- The two new classes rest on 2 cycles each, out of 8 universe cycles per
  topology. All of their V2 hops go through one V2 pool (`0x3e5922…`).
- The generator's split puts the heaviest samples in the holdout. The holdout
  therefore comes from the same cycles, not from independent ones.

**Additional canonical samples: not taken.** After F2, extra `v2_boost` samples
could only change V2+Moe classes. The largest of those still unapproved,
`moe+moe+moe`, covers 90 cycles; the others cover at most 8. Earlier fork runs
stalled on anvil's lazy tick fetching, so a new campaign carries real risk. It
would also need a new harness SHA, and the harness refuses to mix evidence
across SHAs. That is not worth doing for this coverage.

### Status changes against the pre-fix profile (0x6d3daa2f)

| route key | before | holdout n / max (before) | gas_limit (before) | campaign levers (successes) | after |
|---|---|---|---:|---|---|
| `h2:moe+v3:ticks=0:bins=0` | approved | 2 / 397483 | 526918 | displace 4, inflate 7 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h2:v3+moe:ticks=0:bins=0` | approved | 3 / 361828 | 484088 | displace 1, inflate 17 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h2:v3+v2:ticks=0` | approved | 3 / 237574 | 335272 | v2_boost 9 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h2:v3+v3:ticks=0` | approved | 5 / 289524 | 378496 | displace 9, inflate 14 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:moe+moe+v3:ticks=0:bins=0` | approved | 2 / 506405 | 657658 | displace 7, inflate 7 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:moe+v3+v2:ticks=0:bins=0` | approved | 2 / 492586 | 641104 | v2_boost 12 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:moe+v3+v3:ticks=0:bins=0` | approved | 2 / 515025 | 666781 | displace 5, inflate 7 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:v2+moe+v3:ticks=0:bins=0` | approved | 2 / 453877 | 593465 | v2_boost 10 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v2+v3+moe:ticks=0:bins=0` | approved | 3 / 494166 | 636859 | v2_boost 15 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v2+v3+v3:ticks=0` | approved | 3 / 369488 | 490703 | v2_boost 19 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+moe:ticks=0:bins=0` | approved | 2 / 594067 | 733774 | inflate 12 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+v2:ticks=0:bins=0` | approved | 3 / 451540 | 583770 | v2_boost 17 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+moe+v3:ticks=0:bins=0` | approved | 3 / 458941 | 600662 | displace 7, inflate 9 | Unsupported: withheld by review PR108-F2+F3 (qualified numerically; see Fix round 1) |
| `h3:v3+v2+v2:ticks=0` | approved | 3 / 353128 | 473675 | v2_boost 18 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+v2+v3:ticks=0` | approved | 2 / 377920 | 492707 | v2_boost 12 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |
| `h3:v3+v3+v2:ticks=0` | approved | 4 / 369071 | 483574 | v2_boost 21 | Unsupported: withheld by review PR108-F2 (qualified numerically; see Fix round 1) |

### Consequences

- **Coverage:** the Approved topologies cover **24 of 6962** universe cycles
  (0.34%), against 35.8% before the fix. Every topology still has an explicit
  entry, and `committed_universe_topologies_have_zero_unknown_route` still passes.
- **Production configuration:** the agni-v3 + moe only universe fails closed at
  the startup gas gate again, as it did before WHI-1422. The difference is that
  its 12 topologies are now all Unapproved with a reason; before, 6 were Unknown.
- **Unblocking the withheld classes:**
  - For F2: factory-aware V3 venue eligibility. Either restrict pricing and the
    universe to executable factories, or qualify the other factories. Then
    re-qualify the 9 F2-only classes.
  - For F2+F3: the same eligibility work, plus canonical-state samples or a
    measured conservative comparison for the 7 F2+F3 classes (DI-10).
- **Other review findings:**
  - PR108-F5 is fixed: `sampling_policy.description` and the profit-check wording
    above.
  - PR108-F4 is recorded as DI-53.
  - PR108-F1 (the `path_index` expectations) waits for WHI-1424 to merge.

## Remaining gaps (explicitly Unsupported, not approved)

- **Open-ended buckets** (`ticks=21+`, `bins=11+`): forced Unsupported with a
  reason, whatever was measured.
- **Nonzero bounded buckets** (`ticks=1-5`, `6-20`, `bins=1-3`, `4-10`): below
  `min_samples`, or failed holdout (`h2:v3+v3:ticks=1-5`, `h3:v3+v3+v3:ticks=1-5`).
  Closing this needs more cycles per topology, and canonical-state replays for the
  deep buckets (DI-10 still applies to them).
- **Every class with a V3 hop except `h2:v2+v3:ticks=0`**: withheld or not
  qualified. See PR108-F2/F3 above.
- **`moe+v2`, `moe+moe`, `moe+moe+moe`, `moe+v2+moe`, `v2+v2+moe`,
  `moe+v2+v2`, `v2+moe+v2`, `v2+v2+v2`**: no Approved variant. See the tables for the reason
  per bucket.
- **Non-Agni UniV3-family pools**: executability and gas are unverified (DI-50).
