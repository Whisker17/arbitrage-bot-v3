# WHI-1527: pinned offline replay baseline for DISCOVERY latency, before and after the route-key fix

**Status: complete for the frozen rc2 window.** Captured and run on 2026-09-27, 06:06–09:20Z, on one Apple M2 Pro (macOS 27.0).

- **Corpus:** `corpus.jsonl`, sha256 `3817e43f953f865601e95f1c894a067b4fec309fd58d6325f6a1f0047e4a1a11`, 37,113,591 B, 4955 passes. It is **not in git**; it is re-derivable from `manifest.json`.
- **Machine-readable results:**
  - `results.json`: aggregates;
  - `per_pass_evaluated.json`: the 161 evaluated passes, per arm;
  - `manifest.json`: every identity.

The pre-fix arm's result is a **counterfactual**: pre-fix code running on post-fix-era inputs (see §9).

## 1. Headline

DISCOVERY over the **161 evaluated passes** (`cycles_optimized > 0`). Each value is the median of 3 timed repeats per pass, then a nearest-rank percentile, in ms.

| arm | n | p50 | p90 | p95 | p99 |
|---|---|---|---|---|---|
| `1bdac47` (before the route-key fix) | 161 | 2.47 | 16.07 | 34.25 | 35.88 |
| `3cc962f` (after it; isolates the fix) | 161 | 2.64 | 18.46 | 39.84 | 44.11 |
| `fe4a574` (deployed rc2) | 161 | 2.69 | 20.53 | 40.79 | 42.04 |

**Paired `1bdac47 → 3cc962f`** over the same 161 passes:
- median per-pass delta **+0.23 ms**; delta p90 +2.46, p95 +5.96, p99 +9.11 ms;
- Σ DISCOVERY 1107.9 → 1266.3 ms, a **×1.143** ratio of sums;
- 137 of 161 passes are slower after the fix.

By scope:
- **Full** passes (n=14): median delta +6.89 ms, ratio ×1.219, all 14 slower.
- **Touched** evaluated passes (n=147): median delta +0.20 ms, ratio ×1.084.

`3cc962f → fe4a574` is flat: ratio ×0.996, median delta +0.02 ms.

**Read these ratios together with §6.** The two arms do not do the same work inside the timer. On 85 evaluated passes, 693 paths that the pre-fix arm searches to completion (`no_optimum`) are ended early by the post-fix arm as `optimize_error`. The fix's cost per unit of work is therefore larger than the pass-level ratio shows; see ms/quote in §5.

## 2. Checks

| check | result |
|---|---|
| **Same harness bytes at every arm** | `examples/discovery_replay.rs` sha256 `7d6aa04b…c81` plus one `[[example]]` stanza. The harness diff sha256 is `ae1813fb…ade5` and is **identical** at 1bdac47, 3cc962f and fe4a574. Each arm builds with `cargo build --locked --release --example discovery_replay`, rustc 1.95.0 (59807616e 2026-04-14). No `src/`, `Cargo.lock` or dependency change at any arm. |
| **`src/amms` compatibility across arms (correction 1)** | `git diff --stat 1bdac47 fe4a574 -- src/amms` shows **only** `src/amms/error.rs +16`, which adds `AMMError::is_incomplete_state`, a method with no serde type. 3cc962f and fe4a574 are identical under `src/amms`. It is proven by use: all three arms deserialize the same corpus (4955 passes, 124 pools) with no error. The capture also proves a serde round-trip on all 124 bootstrap pools (details below). |
| **Pinning** | Bootstrap state is read at the pinned hash `0xcda68e4a…a093` (block 101173340). Headers 101173340..101178398 are parent-linked (5058 links). 4956/4956 ledger block hashes are canonical. Logs are fetched with `eth_getLogs` by `blockHash` for all 5058 blocks, and each returned log's block identity is checked. |
| **Input fidelity** | The replay's head-applied dirty set equals the ledger `dirty_pools` on **4955/4955** passes. All 23 live `processing_failed` heads (Moe reserve overflow) **reproduce** the same `StateSpace::sync` error. There are 0 unexpected sync errors. |
| **Input-workload consistency (correction 2)** | Per-pass `cycles_optimized` is equal across all three arms on **4955/4955** passes, and so are `cycles_total`, `dirty_pools` and `scope`. This is an input check only: `cycles_optimized` is `to_optimize.len()`, taken **before** fee gating (1bdac47 `path_index.rs:434`, 3cc962f `:438`, fe4a574 `:449`). It does **not** show that the same paths reached the optimizer; they did not (§6). |
| **Determinism** | Every counter, including OPTIMIZE count, opportunity count and liveness flag, is identical across all 4 runs (warmup plus 3 repeats) of each arm: 0/4955 passes differ, on every arm. |
| **Fidelity vs rc2 ledger (fe4a574 arm)** | **4955/4955 passes match** on scope, `cycles_optimized`, `cycles_total`, `paths_quoted`, all six reject buckets and `fee_resolution_failures` (ledger), and on `amm_quotes` (the `block_summary` log). There are **no mismatches to classify.** Window totals equal live: 155,554 / 1,327 / 52,547 / 153,534 / 1,326 / 693, 161 evaluated, and the liveness alarm latched on 287 passes (live: 287). |
| **Recorder pairing** | Exactly one DISCOVERY sample per `discover` call, on every pass, every arm and every run. OPTIMIZE samples equal the number of `Ok` optima. |

The serde round-trip is checked on canonical JSON: object keys sorted and numbers kept verbatim. Arrays serialized from `HashSet` fields, such as the V3 `tick_bitmap_coverage`, are compared order-insensitively. `serde_json::Value` cannot hold the pools' u128/i128 fields, so the harness parses pass lines straight into `AMM`.

## 3. Method

**Arms (D2).**
- `1bdac47` is before the route-key fix, and `3cc962f` is after it. `git diff --stat 1bdac47 3cc962f -- config data` is empty, so this pair isolates the fix.
- `fe4a574` is the deployed rc2.

**Corpus (D3, D1, D4).** The capture ran once, at fe4a574 plus `examples/replay_capture.rs` (sha256 `af88e67b…`, binary `d36d3f1a…`). It was frozen before any arm ran.
- **Bootstrap:** all 124 pools are batch-initialised at the pinned hash of block 101173340, then Moe snapshots are taken at that hash. These are the per-variant loaders that `StateSpaceBuilder::sync` uses. They are called directly because that builder always resolves the live tip.
  - 101173340 is the block the live process believed its state was at: the post-sync re-baseline logged `from=101173340`.
- **Fee:** the FusionX-V2 fee of 200 is **frozen corpus state on every arm (D1).** No fee-300 sensitivity corpus (D1), and no historical control corpus (D4).
- **Endpoint:** public `https://rpc.mantle.xyz`, with no API key, throttle 6 rps, read-only. There was no host process.

**Pass schedule: the rc2 ledger's actual observations, not "every block".**
- Window: 101173341..101178398, 5058 blocks. The live process observed 5048 head events there, and 4955 of them ran discovery.
- `scripts/replay_baseline.sh schedule` builds the schedule from the rc2 ledger copy and the `block_summary` lines of the same run. Skip reasons exist only in the log.

The capture re-enacts, per recorded head, the live watch loop's state handling (fe4a574 `block_loop.rs:1158-1810`):

| recorded head outcome (count) | live state effect | replay capture | `discover` pass |
|---|---|---|---|
| `processed` (4955) | Applies this head's hash-pinned logs through `StateSpace::sync`. On a small gap, widens the dirty set with the gap blocks' log addresses; those logs are **never** applied to state. Then `refresh_selected_tip_state(scope)`, write-back, publish. | same calls | yes; scope taken from the ledger row (14 Full, 4941 Touched) |
| `pinned_header_unavailable` (13) | Logs applied, tip refresh, write-back, publish; discovery skipped | same | no |
| `processing_failed` (23) | `StateSpace::sync` errors mid-block (partial application); nothing published | same call; the failure reproduces 23/23 | no |
| `pinned_logs_unavailable` (54), `duplicate` (1) | nothing | nothing | no |
| startup one-shot row (101173341), then pre-watch re-baseline to 101173342 | continuity moves; **no logs applied** | same | no |

What this reproduces:
- The engine sees exactly the pass sequence live ran, so the **cache history** is the live one.
- **Moe snapshot freshness** matches live. A Touched pass refreshes only dirty Moe pools, and held Moe pools keep older snapshots.
- Gap-block logs are not applied to V2/V3 state, because live does not apply them either.

**Replay.**
- Each arm builds `DiscoveryEngine` once, on the first pass.
- It then calls `discover(pools, config, scope)` once per pass, with pools in address order.
- Config: `DiscoveryConfig::for_settlement(WMNT)`, then per pass `max_hops`, `min_profit`, `max_input`, `block_timestamp` and `snapshot_id`, plus `MeasuredFeeScoring::new(profile, priority, reserve, BlockFeeContext{pinned header})`.
- The profile is the corpus copy of `config/gas_profiles/mantle_mainnet_v1.json` (sha256 `3e76b9cd…`), loaded with digest `0x3d3244e3…df412`.
- Timers: a raw `metrics::Recorder` keeps every `stage::DISCOVERY` / `stage::OPTIMIZE` sample, and every other metric is a no-op.
- Counters: numeric JSON from the structured `DiscoveryStats` fields common to all arms. `fee_resolution_failures` exists only at fe4a574, so it alone is read from `Debug`.
- **Excluded from timing:** corpus parsing, pool-vector assembly and output all happen outside `discover`. There is no provider in the replay, so no RPC or capture time can enter either timer.

**Runs.**
- One warmup per arm, then 3 repeats in a balanced Latin rotation (`ABC`, `BCA`, `CAB`), sequentially on one machine.
- The build and test jobs had finished before the first timed run.
- Order and timestamps are in `manifest.json` under `runs`.

## 4. What each timer measures

- **DISCOVERY** is one sample per `discover` call. It covers the optimize loop over the selected subset: pool lookup, fee/route checks, the optimizer search, and the post-optimum mixed simulation that fills `CachedGross`.
  - It starts before the loop (`discovery_start`: 1bdac47 `:433`, 3cc962f `:437`, fe4a574 `:448`).
  - It is emitted **before cached materialization** and gas re-scoring (emit `:532/:536/:556`; materialize `:567/:571/:591`).
  - It excludes the path-index build and includes **no RPC**.
- **OPTIMIZE** is one `optimize_path` call, recorded **only on a successful optimum** (`OptimizeOutcome::Ok`). It includes no RPC.

## 5. Per-arm tables

**DISCOVERY, ms.** "n unique" means one value per pass, the median of 3 repeats. "pooled" means all 3 repeats (n×3). **Repeats are not independent samples**; percentiles over unique passes are the primary figures. "bucket-interp" is Prometheus-style interpolation over `STAGE_BUCKETS`, given for comparability with live scrapes.

| arm | scope / passes | n unique | p50 | p90 | p95 | p99 | mean | n pooled | pooled p50 / p99 | bucket-interp p50 / p99 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 | all / evaluated | 161 | 2.465 | 16.065 | 34.254 | 35.884 | 6.882 | 483 | 2.987 / 48.210 | 2.453 / 47.317 |
| 3cc962f | all / evaluated | 161 | 2.636 | 18.462 | 39.835 | 44.113 | 7.865 | 483 | 2.821 / 56.813 | 3.052 / 48.911 |
| fe4a574 | all / evaluated | 161 | 2.685 | 20.526 | 40.788 | 42.040 | 7.833 | 483 | 2.753 / 42.631 | 3.160 / 47.317 |
| 1bdac47 | full / evaluated (= all Full) | 14 | 34.388 | 35.884 | 36.433 | 36.433 | 34.502 | 42 | 34.498 / 49.119 | 37.5 / 49.75 |
| 3cc962f | full / evaluated | 14 | 40.398 | 44.113 | 56.530 | 56.530 | 42.069 | 42 | 40.539 / 64.107 | 38.46 / 93.0 |
| fe4a574 | full / evaluated | 14 | 40.805 | 42.040 | 42.299 | 42.299 | 41.008 | 42 | 40.807 / 56.363 | 37.5 / 49.75 |
| 1bdac47 | touched / evaluated | 147 | 2.313 | 11.236 | 15.094 | 18.465 | 4.251 | 441 | 2.509 / 22.907 | 2.234 / 24.648 |
| 3cc962f | touched / evaluated | 147 | 2.513 | 12.504 | 16.558 | 21.574 | 4.608 | 441 | 2.525 / 21.574 | 2.537 / 24.680 |
| fe4a574 | touched / evaluated | 147 | 2.529 | 12.807 | 16.735 | 21.619 | 4.673 | 441 | 2.555 / 22.433 | 2.674 / 24.680 |
| 1bdac47 | all / all passes | 4955 | 0.000 | 0.0001 | 0.0001 | 7.222 | 0.224 | 14865 | 0.000 / 7.260 | 0.052 / 5.958 |
| 3cc962f | all / all passes | 4955 | 0.000 | 0.000 | 0.0001 | 8.032 | 0.256 | 14865 | 0.000 / 8.032 | 0.052 / 6.309 |
| fe4a574 | all / all passes | 4955 | 0.000 | 0.000 | 0.0001 | 8.162 | 0.255 | 14865 | 0.000 / 8.162 | 0.052 / 6.309 |
| 1bdac47 | touched / all passes | 4941 | 0.000 | 0.0001 | 0.0001 | 3.774 | 0.127 | 14823 | 0.000 / 3.992 | 0.051 / 3.999 |
| 3cc962f | touched / all passes | 4941 | 0.000 | 0.000 | 0.0001 | 4.030 | 0.137 | 14823 | 0.000 / 3.954 | 0.051 / 4.308 |
| fe4a574 | touched / all passes | 4941 | 0.000 | 0.000 | 0.0001 | 4.109 | 0.139 | 14823 | 0.000 / 4.030 | 0.051 / 4.347 |

All 14 Full passes are evaluated, so "full / all passes" equals "full / evaluated". The all-passes rows are dominated by the 4794 idle Touched passes (`cycles_optimized = 0`, sub-microsecond). Live reported evaluated passes only.

**Paired per-pass DISCOVERY deltas, ms** (after − before, per-pass medians):

| pair | passes | n pairs | Δ p50 | Δ p90 | Δ p95 | Δ p99 | Σ before | Σ after | ratio of sums | passes slower after |
|---|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 → 3cc962f | all / evaluated | 161 | +0.229 | +2.456 | +5.959 | +9.108 | 1107.9 | 1266.3 | 1.143 | 137 |
| 1bdac47 → 3cc962f | full / evaluated | 14 | +6.672 | +9.108 | +22.142 | +22.142 | 483.0 | 589.0 | 1.219 | 14 |
| 1bdac47 → 3cc962f | touched / evaluated | 147 | +0.197 | +1.319 | +1.748 | +3.109 | 624.9 | 677.3 | 1.084 | 123 |
| 3cc962f → fe4a574 | all / evaluated | 161 | +0.024 | +0.556 | +1.075 | +1.839 | 1266.3 | 1261.0 | 0.996 | 105 |
| 1bdac47 → fe4a574 | all / evaluated | 161 | +0.256 | +3.213 | +5.902 | +8.346 | 1107.9 | 1261.0 | 1.138 | 146 |

**Repeat spread.** The relative range across the 3 repeats on evaluated passes has a median of 0.42 for 1bdac47, 0.11 for 3cc962f and 0.10 for fe4a574.
- Σ evaluated DISCOVERY per run shows the drift. 1bdac47: warmup 1519, rep0 1531, rep1 1086, rep2 1093 ms. 3cc962f: 1746, 1385, 1293, 1229 ms. fe4a574: 1738, 1258, 1259, 1318 ms.
- The machine ran slower for its first ≈4 runs (host load average ≈4–5 from unrelated processes). A read-only analysis script also briefly overlapped 1bdac47 rep0.
- The per-pass median of 3 absorbs one slow repeat.
- On the steady-state repeats alone (rep1 + rep2), the ratio is about the same: 1bdac47 ≈1090 vs 3cc962f ≈1261 ms, ×1.16.

**ms per `amm_quote`**, only on passes with quotes > 0:

| arm | passes | p50 | p90 | p99 | Σms / Σquotes |
|---|---|---|---|---|---|
| 1bdac47 | 117 | 0.0148 | 0.2899 | 0.3514 | 0.0204 |
| 3cc962f | 94 | 0.0932 | 0.3369 | 0.4012 | 0.0341 |
| fe4a574 | 117 | 0.0169 | 0.3219 | 0.3857 | 0.0232 |

`amm_quotes` does not mean the same thing across these arms (§6). 3cc962f does not count the quotes spent on `optimize_error` paths, so its passes-with-quotes set and its denominator are smaller. Compare ms/quote only between 1bdac47 and fe4a574, which both count every search's quotes.
- Σ quotes: 1bdac47 52,574; fe4a574 52,547.
- Σ evaluated DISCOVERY per quote: 0.0204 vs 0.0232 ms, ×1.14.

**OPTIMIZE.** The minimum-n rule is a predeclared reporting convention (n < 30 is written "insufficient"). It is not a statistical-sufficiency claim.

| arm | OPTIMIZE n | Ok optimum | no_optimum | unapproved_route | unknown_route | zero_profit | pool_lookup | other | latency |
|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 | 2 | 2 | 2018 | 153,535 | 0 | 0 | 0 | 0 | insufficient (n=2) |
| 3cc962f | 1 | 1 | 1326 | 153,534 | 0 | 0 | 0 | 693 | insufficient (n=1) |
| fe4a574 | 1 | 1 | 1326 | 153,534 | 0 | 0 | 0 | 693 | insufficient (n=1) |

**Counter totals** (Σ over 4955 passes, one run; identical in every run):

| counter | 1bdac47 | 3cc962f | fe4a574 | rc2 live |
|---|---|---|---|---|
| cycles_optimized | 155,554 | 155,554 | 155,554 | 155,554 |
| paths_quoted | 2,020 | 1,327 | 1,327 | 1,327 |
| amm_quotes | 52,574 | 34,529 | 52,547 | 52,547 |
| unapproved_route | 153,535 | 153,534 | 153,534 | 153,534 |
| unknown_route | 0 | 0 | 0 | 0 |
| no_optimum | 2,018 | 1,326 | 1,326 | 1,326 |
| other | 0 | 693 | 693 | 693 |
| fee_resolution_failures | n/a | n/a | 58 | 58 |
| OPTIMIZE samples (Ok optima) | 2 | 1 | 1 | 1 |
| opportunities (materialized) | 2 | 2 | 2 | 2 |
| passes evaluated / with paths_quoted > 0 | 161 / 117 | 161 / 94 | 161 / 94 | 161 / 94 |
| passes with the liveness alarm latched | 0 | 287 | 287 | 287 |

The rc2 live column is the ledger plus `block_summary` sums. The liveness value comes from the deployment issue's evidence, which records 287 latched blocks.

## 6. Why the work populations differ across arms

These are metric-semantics and behaviour changes between the SHAs, with evidence. **None was clamped, filtered or patched.**

1. **Held (stale) Moe snapshots on Touched passes.** This covers 85 evaluated passes and 693 paths.
   - After the fix, the optimizer quotes each sample through `simulate_mixed_path_with_route_key(…, block_timestamp)`. A Moe pool not refreshed this pass has an older snapshot timestamp, so the quote fails with `MoeError::SnapshotTimestampMismatch` (fe4a574 `src/amms/moe/mod.rs:449`). The path ends as `OptimizeOutcome::Error`, which counts in `other`.
   - Diagnostic replay (`RUST_LOG=bot.discovery=debug`, pilot window): the error text is "Moe quote timestamp … does not match snapshot timestamp …".
   - Before the fix, the search uses `simulate_path` (1bdac47 `src/arbitrage/optimizer.rs:606`), which takes no block timestamp. It completes and records `no_optimum`.
   - Result: `paths_quoted` 2,020 vs 1,327, `no_optimum` 2,018 vs 1,326, `other` 0 vs 693.
   - The pre-fix arm therefore runs complete searches on 693 paths that the post-fix arm abandons early. The pass-level DISCOVERY ratio **understates** the fix's per-search cost.
   - The same difference is why the liveness alarm never fires on the pre-fix arm (0 vs 287 passes): its passes keep quoting ≥1 path.
2. **One extra pre-fix optimum** (block 101176596). The pre-fix constant zero-bucket fee search finds an `Ok` optimum on a path that the post-fix arm quotes as `Error`. Materialization then rejects it with the real route key as `unapproved_route`, giving 153,535 vs 153,534 and OPTIMIZE n 2 vs 1. This is the selection bias expected for OPTIMIZE: the populations are unpaired.
3. **`amm_quotes` semantics.** fe4a574 also counts quotes spent on `Error` paths (the later coverage-counter change). 1bdac47 and 3cc962f count Ok and NoOptimum only. Hence 3cc962f 34,529 vs fe4a574 52,547, with **identical** paths, rejects and timings (3cc962f → fe4a574 ratio ×0.996).
4. **`fee_resolution_failures`** exists only at fe4a574, where it is 58 (the sample-level count).
5. **Optimize fee pricing** is the change being measured:
   - 1bdac47 prices every sample at a constant zero-bucket topology key;
   - 3cc962f and fe4a574 build the real per-sample route key (tick/bin crossings) and price it, memoised;
   - fe4a574 additionally uses the shared profile-support predicate (a later change).
   - On this universe and profile, the pre-search gate rejects the same `unapproved_route` paths on every arm, apart from item 2.

## 7. Fidelity vs the rc2 ledger (fe4a574 arm)

- **Matched inputs and schedule.** The replay's head-applied dirty set equals the ledger `dirty_pools` on 4955/4955 passes. The pass scope comes from the ledger. The skips and gaps are the live ones (§3).
- **Result.** All 4955 passes match on every recorded counter (§2). `mismatches` in `results.json` is empty.
- **Classification.** None needed. There are 0 mismatches, including the first Full pass after startup (5562 / 78 / `amm_quotes` 2028), even though the live bootstrap state was older than block 101173340.
- **What this covers.** Counters only. Absolute times are not compared with live (§9).

## 8. Pilot (before the full capture)

- **Scope:** 180 events (101173341..101173520, 177 passes, 1 Full, Touched passes with 1–11 dirty pools).
  - 124/124 serde round-trip; 178/178 ledger hashes canonical; replay dirty set equal to ledger on 177/177;
  - the fe4a574 counters equal the ledger on 177/177; 1 DISCOVERY sample per pass on every arm.
- **Cost:**
  - bootstrap: 424 s wall (V3 tick data; per-call latency on the public endpoint swung from 0.7 s to 60 s by backend), cached for the full run;
  - 181 headers + 180 getLogs;
  - emulation: 120 s, 267 batch `eth_call` CREATEs (Moe tip refresh);
  - corpus: 5.7 MB; about 6.4 s per replay.
- **Extrapolation:** about 10k header and log calls plus 3–5k CREATEs, 40–60 min, 100–150 MB. This was judged feasible, and the progress update was sent.
- **Actual full capture:**
  - 4878 + 4878 new header and log calls (1111 s + 948 s);
  - 4081 CREATEs in emulation (1975 s);
  - 37.1 MB corpus;
  - about 115–158 s per replay run.

## 9. Config provenance, non-claims and limits

**Config provenance.** The values were read read-only from arb-bot-jp: `run_plan.json` sha `f0ea307a…`, `capital_evidence.json` sha `d4b2e445…`, and the ledger `run_header`. Nothing else was read on the host.
- `max_input = 10 WMNT` (1e19 wei): shadow `assumed_capital_cap_wmnt_wei` = mode cap, applied through `apply_capital_domain_to_discovery`.
- `min_profit = 1e16`: the `v3.config` floor `V3_MIN_PROFIT_FLOOR_WEI`.
- `priority_fee = 100000` and `block_gas_reserve = 1`: `ExecutorConfig` defaults.
- `max_hops = 3`.

The run_header carries digests, not these values. The last three rows follow from the rc2 log (`protocol="v3.config"`) and the recorded host env-name inventory: the `.env` has no `MIN_NET_PROFIT_WEI` or `EXECUTOR_PRIORITY_FEE_WEI`. The host `.env` itself was not read. `min_profit` acts only at materialization, outside DISCOVERY. No secret is recorded; the endpoint is fingerprinted as host `rpc.mantle.xyz`, public, with no key.

**Non-claims.**
- **The pre-fix arm runs on counterfactual inputs.** The FusionX-V2 fee is frozen at 200 in the corpus, while 1bdac47's own loader would build 300. The universe, profile and Moe handling are fe4a574-era. It is "pre-fix discovery code on post-fix-era inputs", not "rc1 latency".
- **Offline absolute times are not comparable to live.** The machine is different, and there are no concurrent RPC tasks. The metrics facade is a no-op recorder rather than Prometheus. Only arm-to-arm ratios on this machine are offered.
- **No profitability claims.** The two opportunities are the same gate-blocked candidates live saw. Nothing here says anything about profit, sends or execution.
- **OPTIMIZE latency is not comparable across arms.** n = 2 / 1 / 1, and the populations are selection-biased (§6.2).

**Limits.**
- One quiet Sunday window: 161 evaluated passes, 14 of them Full. The Full-pass figures rest on n=14.
- 3 timed repeats: the time budget allowed this, not the 5 planned. The machine drifted during the first ≈4 runs (§5).

## 10. Reproduce

```bash
export SUMMARY_LOG=/path/to/frozen/rc2/signerless.log      # sha256 34a67028…
scripts/replay_baseline.sh schedule   # -> $WORK/schedule.json  (sha256 6f2789fb…)
scripts/replay_baseline.sh capture    # -> $WORK/corpus/corpus.jsonl (sha256 3817e43f…), archive RPC
scripts/replay_baseline.sh arms       # detached worktrees at the 3 SHAs, identical harness, --locked release builds
REPEATS=3 scripts/replay_baseline.sh run
scripts/replay_baseline.sh analyze    # -> evidence/replay/whi-1527/{results,per_pass_evaluated,manifest}.json
scripts/replay_baseline.sh clean      # removes only the script's worktrees and targets
```

`WORK` defaults to `/tmp/whi1527`, where the script keeps everything it owns, including the harness diff for the historical worktrees. The ledger copy is `$WORK/host/ledger.jsonl`; its window rows have sha256 `25f75132…`.
