# WHI-1527: pinned offline replay baseline for DISCOVERY latency, before and after the route-key fix

**Status: complete for the frozen rc2 window, revised in fix round 1.**
- Initial capture and run: 2026-09-27, 06:06–09:20Z.
- Fix round 1: re-emulation from the RPC caches, then 5 balanced repeats, 10:25–11:40Z.
- Hardware: one Apple M2 Pro, macOS 27.0.
- §11 compares the round-1 results with the first ones.

**Where things are**

| item | location | identity |
|---|---|---|
| Corpus (not in git) | `ARTIFACTS/corpus/corpus.jsonl`, 37,113,591 B, 4955 passes | bytes sha256 `46c8420141a6dcd73307bff1df8b2fe706da42c7dc272c81db5ae4531b32fbe9`; canonical content sha256 `3a17406e5f3af1612c6e712bbf880d7cce3c801537380a4c546eee3e9b1a677c` (§2) |
| Explicit pass schedule (in git) | `schedule.json.gz`: all 5048 events, with order, kind/skip reason, block ids, ledger scope and ledger dirty addresses | decompressed sha256 `bc78f4a9143b687f5de264bed235e363a42dbb4803dd304ee2713eda125ca635` |
| Durable artifacts | `ARTIFACTS` = the orchestrator run's `release-022/whi1527-artifacts/`. Full path and every file checksum are in `manifest.json` under `artifacts`, including `SHA256SUMS` (sha256 `501b15c7…`). | contents below |
| Machine-readable results | `results.json` (aggregates and checks), `per_pass_evaluated.json` (the 161 evaluated passes, per arm), `manifest.json` (every identity) | — |

The durable artifacts directory holds:
- `corpus/`: the corpus, `emulation.jsonl`, the capture report and log, and the profile copy;
- `rpc_cache/`: the hash-pinned header, log and bootstrap-state caches;
- `runs/`: the raw per-pass replay outputs of all 18 runs;
- `sources/`: the ledger window rows, a `signerless` log excerpt, `run_plan.json`, `capital_evidence.json`, the schedule and the harness diffs;
- `previous/`: the round-0 corpus and runs;
- `f4_pilots/`: the two reproducibility pilots;
- `diagnostics/`: the full-corpus debug replays.

The pre-fix arm's result is a **counterfactual**: pre-fix code running on post-fix-era inputs (see §9).

## 1. Headline

DISCOVERY over the **161 evaluated passes** (`cycles_optimized > 0`). Each value is the per-pass median of 5 timed repeats, then a nearest-rank percentile, in ms.

| arm | n | p50 | p90 | p95 | p99 |
|---|---|---|---|---|---|
| `1bdac47` (before the route-key fix) | 161 | 3.16 | 22.55 | 48.02 | 48.68 |
| `3cc962f` (after it; isolates the fix) | 161 | 3.58 | 24.90 | 57.22 | 58.58 |
| `fe4a574` (deployed rc2) | 161 | 3.52 | 26.17 | 57.62 | 58.73 |

**Paired `1bdac47 → 3cc962f`** over the same 161 passes:
- median per-pass delta **+0.37 ms**; delta p90 +2.58, p95 +8.93, p99 +10.49 ms;
- Σ DISCOVERY 1528.6 → 1732.5 ms, a **×1.133** ratio of sums;
- 147 of 161 passes are slower after the fix.

By scope:
- **Full** passes (n=14): median delta +9.09 ms, ratio ×1.196, all 14 slower.
- **Touched** evaluated passes (n=147): ×1.084.

`3cc962f → fe4a574` is ×1.006. Round 0 gave ×1.143, ×1.219, ×1.084 and ×0.996 for the same four figures (§11).

**The work inside the timer differs between the arms (§6).** These ratios are measured pass-level observations on this schedule. They are not a per-search or per-simulation cost, and no bound on such a cost is claimed.
- The candidate-evaluation count is essentially the same on both sides: `amm_quotes` for `1bdac47` equals `fe4a574` on every pass but one.
- What differs is how much each candidate costs:
  - the post-fix arm builds and prices a real route key per sample;
  - samples that hit a held Moe snapshot fail at that hop;
  - 693 searches, which run to completion, end with a stored error instead of `no_optimum`.

Absolute offline times moved about 40% between the two rounds while the ratios held (§11). The round-1 runs were on battery power, and host state is recorded in `manifest.json` under `runs.host`.

## 2. Checks

| check | result |
|---|---|
| **Same harness bytes at every arm** | `examples/discovery_replay.rs` sha256 `7d6aa04b…c81` plus one `[[example]]` stanza. The harness diff sha256 is `ae1813fb…ade5`, **identical** at 1bdac47, 3cc962f and fe4a574. Each arm builds with `cargo build --locked --release --example discovery_replay`, rustc 1.95.0 (59807616e 2026-04-14). Binaries: `d7f727af…` / `b5449465…` / `c51172e7…`. No `src/`, `Cargo.lock` or dependency change at any arm. |
| **`src/amms` compatibility across arms** | `git diff --stat 1bdac47 fe4a574 -- src/amms` shows **only** `src/amms/error.rs +16` (it adds the method `AMMError::is_incomplete_state`; no serde type changes). 3cc962f equals fe4a574 under `src/amms`. All three arms deserialize and replay the same corpus. The capture proves a canonical serde round-trip on 124/124 bootstrap pools. |
| **Pinning** | Bootstrap state is read at the pinned hash `0xcda68e4a…a093` (block 101173340). Headers 101173340..101178398 are parent-linked (5058 links). 4956/4956 ledger block hashes are canonical. Logs come from `eth_getLogs` by `blockHash` for all 5058 blocks, and each returned log's block identity is checked. |
| **Input fidelity** | The replay's head-applied dirty set equals the ledger `dirty_pools` on **4955/4955** passes. All 23 live `processing_failed` heads reproduce the same `StateSpace::sync` error (Moe reserve arithmetic overflow). 0 unexpected sync errors. |
| **Header-timeout transitions (round 1)** | All 13 live `pinned_header_unavailable` skips are the outer `CanonicalHeaderLoad::TimedOut` branch (`block_loop.rs:2162`, "HTTP has not served announced hash within deadline"). None is the inner base-fee/gas-limit branch. They are now no-ops. For each, the next event is `processed`, and the emulation's gap range is `[timeout, next]`. The live `small gap backfill … previous=` line for that next block names `timeout − 1` in all 13 cases. Pool inputs are unchanged: the timeout blocks have no cached logs, the next pass's dirty set is empty, and it refreshes 0 Moe pools. Evidence: `results.json` → `header_timeout_transitions`. |
| **Gap semantics vs the live log** | All 94 live `small gap backfill block=N previous=P` lines in the window match the emulated gap ranges (`[P+1, N]`). There are 0 emulated gaps absent from the log. |
| **Input-workload consistency** | Per-pass `cycles_optimized` is equal across all three arms on **4955/4955** passes, and so are `cycles_total`, `dirty_pools` and `scope`. This is an input check only: `cycles_optimized` is `to_optimize.len()`, taken **before** fee gating (1bdac47 `path_index.rs:434`, 3cc962f `:438`, fe4a574 `:449`). It does **not** show that the same paths reached the optimizer (§6). |
| **Determinism** | Every counter, OPTIMIZE count, opportunity count and liveness flag is identical across all 6 runs of each arm (warmup plus 5 repeats): 0/4955 passes differ on every arm. The counter totals equal round 0 exactly. |
| **Fidelity vs rc2 ledger (fe4a574 arm)** | **4955/4955 passes match** on scope, `cycles_optimized`, `cycles_total`, `paths_quoted`, all six reject buckets, `fee_resolution_failures` (from the ledger) and `amm_quotes` (from the `block_summary` log). **No mismatches to classify.** |
| **Recorder pairing** | Exactly one DISCOVERY sample per `discover` call, on every pass, arm and run. OPTIMIZE samples equal the number of `Ok` optima. |
| **Corpus reproducibility (round 1)** | The capture now writes pools in canonical form (sorted keys; the only all-numeric arrays, the `tick_bitmap_coverage` HashSets, sorted), so repeat captures give identical bytes. Two independent 30-event recaptures from the same caches, with fresh hash-pinned Moe refreshes (the Full pass at 101173343 included), give **identical bytes** `705465fa…`. The format-independent content digest (sorted keys and sets, meta `capture_commit` excluded) is `3a17406e…` for both this corpus and the round-0 corpus `3817e43f…`: the content is unchanged and only the encoding differs. |

## 3. Method

**Arms (D2).**
- `1bdac47` is before the route-key fix, and `3cc962f` is after it. `git diff --stat 1bdac47 3cc962f -- config data` is empty, so this pair isolates the fix.
- `fe4a574` is the deployed rc2.

**Corpus (D3, D1, D4).** Captured with `examples/replay_capture.rs` at fe4a574 (source sha256 `6cabb3c0…`, binary `946511c8…`).
- **Bootstrap:** all 124 pools are batch-initialised at the pinned hash of block 101173340, then Moe snapshots are taken at that hash. These are the per-variant loaders `StateSpaceBuilder::sync` uses, called directly because that builder always resolves the live tip. 101173340 is the block the live process believed its state was at (the post-sync re-baseline logged `from=101173340`).
- **Fee:** the FusionX-V2 fee of 200 is **frozen corpus state on every arm** (D1). No sensitivity corpus (D1), and no historical control corpus (D4).
- **Endpoint:** public `https://rpc.mantle.xyz`, with no API key, throttle 6 rps, read-only. There was no host process.
- **Round-1 reads:** the round-1 re-emulation reused the header, log and bootstrap caches. Its only new reads were the hash-pinned Moe tip refreshes (4081 batch `eth_call`s).

**Pass schedule: the rc2 ledger's actual observations, not "every block".**
- Window: 101173341..101178398, 5058 blocks. The live process observed 5048 head events in it, and 4955 of them ran discovery.
- `scripts/replay_baseline.sh schedule` builds the schedule from the rc2 ledger and the same run's log. Skip reasons, and the origin of each header skip, are only in the log.
- The schedule is committed as `schedule.json.gz`.
- It regenerates bit-identically from the archived `sources/ledger_window_rows.jsonl` and `sources/signerless_excerpt.log`.

The capture re-enacts, per recorded head, the live watch loop's state handling (fe4a574 `block_loop.rs`):

| recorded head outcome (count) | live state effect | replay capture | `discover` pass |
|---|---|---|---|
| `processed` (4955) | `process_observed_head` applies this head's hash-pinned logs through `StateSpace::sync`. On a small gap it widens the dirty set with the gap blocks' log addresses; those logs are **never** applied to state. Then `refresh_selected_tip_state(scope)`, write-back, publish. | same calls | yes; scope from the ledger row (14 Full, 4941 Touched) |
| `header_timeout` (13): outer `CanonicalHeaderLoad::TimedOut` (`:2162`) | `process_observed_head` is **never called**: no logs, no tip refresh, no publish. The next head sees a gap from the last published tip. | no-op; the published tip does not move | no |
| `pinned_header_unavailable`, inner (0 in this window): header loaded, but base fee missing or gas limit 0 | logs applied, tip refresh, write-back, publish; discovery skipped | same (kept for other windows) | no |
| `processing_failed` (23): Moe reserve overflow | `StateSpace::sync` errors mid-block (partial application); nothing published | same call; the failure reproduces 23/23 | no |
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
- Counters: from the structured `DiscoveryStats` fields common to all arms. `fee_resolution_failures` exists only at fe4a574, so it alone is read from `Debug`.
- **Excluded from timing:** corpus parsing, pool-vector assembly and output all happen outside `discover`. There is no provider in the replay, so no RPC or capture time can enter either timer.

**Runs.**
- One warmup per arm, then 5 repeats in a balanced Latin rotation (round r starts at arm r mod 3), sequentially on one machine.
- No build, test or analysis job ran during the timed runs. The order and timestamps are in `manifest.json` under `runs`.
- A cosmetic driver error was printed at the very end (bash re-read `scripts/replay_baseline.sh`, which had been edited mid-run). It came after all 18 runs and the host record were written.

## 4. What each timer measures

- **DISCOVERY** is one sample per `discover` call. It covers the optimize loop over the selected subset: pool lookup, fee/route checks, the optimizer search, and the post-optimum mixed simulation that fills `CachedGross`.
  - It starts before the loop (`discovery_start`: 1bdac47 `:433`, 3cc962f `:437`, fe4a574 `:448`).
  - It is emitted **before cached materialization** and gas re-scoring (emit `:532/:536/:556`; materialize `:567/:571/:591`).
  - It excludes the path-index build and includes **no RPC**.
- **OPTIMIZE** is one `optimize_path` call, recorded **only on a successful optimum** (`OptimizeOutcome::Ok`). It includes no RPC.

## 5. Per-arm tables

**DISCOVERY, ms.** "n unique" means one value per pass, the median of 5 repeats. "pooled" means all 5 repeats (n×5). **Repeats are not independent samples**, so the unique-pass percentiles are the primary figures. "bucket-interp" is Prometheus-style interpolation over `STAGE_BUCKETS`, given for comparability with live scrapes. All 14 Full passes are evaluated, so the Full rows cover every Full pass. The all-passes rows are dominated by 4794 idle Touched passes (`cycles_optimized = 0`, sub-microsecond).

| arm | scope / passes | n unique | p50 | p90 | p95 | p99 | mean | n pooled (5 repeats) | pooled p50 / p99 | bucket-interp p50 / p99 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 | all / evaluated | 161 | 3.163 | 22.550 | 48.018 | 48.676 | 9.494 | 805 | 3.241 / 49.145 | 4.157 / 47.484 |
| 3cc962f | all / evaluated | 161 | 3.576 | 24.903 | 57.221 | 58.579 | 10.761 | 805 | 3.651 / 59.908 | 4.212 / 94.250 |
| fe4a574 | all / evaluated | 161 | 3.516 | 26.173 | 57.620 | 58.730 | 10.824 | 805 | 3.653 / 59.257 | 4.250 / 94.250 |
| 1bdac47 | full / evaluated | 14 | 48.062 | 48.676 | 48.859 | 48.859 | 48.160 | 70 | 48.124 / 50.658 | 37.500 / 49.750 |
| 3cc962f | full / evaluated | 14 | 57.480 | 58.579 | 58.717 | 58.717 | 57.592 | 70 | 57.419 / 62.240 | 75.000 / 99.500 |
| fe4a574 | full / evaluated | 14 | 58.023 | 58.730 | 58.856 | 58.856 | 57.790 | 70 | 57.950 / 63.190 | 75.000 / 99.500 |
| 1bdac47 | touched / evaluated | 147 | 3.019 | 15.514 | 19.815 | 26.150 | 5.812 | 735 | 3.037 / 26.150 | 3.750 / 31.625 |
| 3cc962f | touched / evaluated | 147 | 3.373 | 17.259 | 22.698 | 29.954 | 6.301 | 735 | 3.418 / 29.954 | 3.832 / 31.625 |
| fe4a574 | touched / evaluated | 147 | 3.397 | 17.494 | 22.023 | 29.716 | 6.351 | 735 | 3.423 / 29.716 | 3.861 / 42.650 |
| 1bdac47 | all / all passes | 4955 | 0.000 | 0.000 | 0.000 | 10.302 | 0.308 | 24775 | 0.000 / 10.274 | 0.051 / 10.198 |
| 3cc962f | all / all passes | 4955 | 0.000 | 0.000 | 0.000 | 11.381 | 0.350 | 24775 | 0.000 / 11.381 | 0.051 / 10.198 |
| fe4a574 | all / all passes | 4955 | 0.000 | 0.000 | 0.000 | 11.614 | 0.352 | 24775 | 0.000 / 11.590 | 0.051 / 10.218 |
| 1bdac47 | touched / all passes | 4941 | 0.000 | 0.000 | 0.000 | 5.156 | 0.173 | 24705 | 0.000 / 5.197 | 0.051 / 5.809 |
| 3cc962f | touched / all passes | 4941 | 0.000 | 0.000 | 0.000 | 5.503 | 0.188 | 24705 | 0.000 / 5.503 | 0.051 / 5.809 |
| fe4a574 | touched / all passes | 4941 | 0.000 | 0.000 | 0.000 | 5.519 | 0.189 | 24705 | 0.000 / 5.538 | 0.051 / 6.056 |

**Paired per-pass DISCOVERY deltas, ms** (after − before, per-pass medians):

| pair | passes | n pairs | Δ p50 | Δ p90 | Δ p95 | Δ p99 | Σ before | Σ after | ratio of sums | passes slower after |
|---|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 → 3cc962f | all / evaluated | 161 | +0.368 | +2.583 | +8.933 | +10.493 | 1528.6 | 1732.5 | 1.133 | 147 |
| 1bdac47 → 3cc962f | full / evaluated | 14 | +9.091 | +10.493 | +10.656 | +10.656 | 674.2 | 806.3 | 1.196 | 14 |
| 1bdac47 → 3cc962f | touched / evaluated | 147 | +0.344 | +1.885 | +2.092 | +3.805 | 854.3 | 926.2 | 1.084 | 133 |
| 3cc962f → fe4a574 | all / evaluated | 161 | +0.018 | +0.329 | +0.606 | +1.270 | 1732.5 | 1742.6 | 1.006 | 104 |
| 3cc962f → fe4a574 | full / evaluated | 14 | +0.327 | +1.174 | +1.929 | +1.929 | 806.3 | 809.1 | 1.003 | 10 |
| 3cc962f → fe4a574 | touched / evaluated | 147 | +0.013 | +0.236 | +0.338 | +1.028 | 926.2 | 933.5 | 1.008 | 94 |
| 1bdac47 → fe4a574 | all / evaluated | 161 | +0.381 | +3.152 | +9.472 | +10.457 | 1528.6 | 1742.6 | 1.14 | 150 |
| 1bdac47 → fe4a574 | full / evaluated | 14 | +9.671 | +10.457 | +10.838 | +10.838 | 674.2 | 809.1 | 1.2 | 14 |
| 1bdac47 → fe4a574 | touched / evaluated | 147 | +0.358 | +1.898 | +2.652 | +3.566 | 854.3 | 933.5 | 1.093 | 136 |

**Repeat spread.** The relative range across the 5 repeats on evaluated passes, median / p90:
- 1bdac47: 0.056 / 0.169;
- 3cc962f: 0.076 / 0.184;
- fe4a574: 0.057 / 0.148.

Σ evaluated DISCOVERY per run (warmup, then rep0–rep4), ms:
- 1bdac47: 1532.8, 1527.2, 1525.3, 1536.5, 1541.2, 1531.4;
- 3cc962f: 1731.3, 1739.6, 1738.6, 1727.8, 1769.1, 1732.0;
- fe4a574: 1758.5, 1751.1, 1761.9, 1750.9, 1749.1, 1734.1.

**DISCOVERY ms per `amm_quote`.** The numerator is DISCOVERY summed over **quote-positive passes only**; passes with `amm_quotes = 0` are excluded. It is not all evaluated DISCOVERY time. `amm_quotes` counts **candidate evaluations** (quote-closure calls, plus one mixed simulation per non-zero optimum). It does not count equal units of completed simulation: a post-fix candidate that fails at a held Moe hop counts once, like a pre-fix full-path simulation (§6).

| arm | quote-positive passes | p50 | p90 | p99 | Σ DISCOVERY(quote-positive) / Σ amm_quotes |
|---|---|---|---|---|---|
| 1bdac47 | 117 | 0.0199 | 0.3965 | 0.4651 | 0.0281 |
| 3cc962f | 94 | 0.1292 | 0.4541 | 0.544 | 0.0466 |
| fe4a574 | 117 | 0.0225 | 0.4479 | 0.5297 | 0.032 |

3cc962f leaves out the candidates evaluated on searches that end in `Error`, so its denominator and its set of quote-positive passes are smaller. Its row is not comparable with the other two.

**OPTIMIZE.** The minimum-n rule is a predeclared reporting convention (n < 30 is written "insufficient"). It is not a statistical-sufficiency claim.

| arm | OPTIMIZE n | Ok optimum | no_optimum | unapproved_route | unknown_route | zero_profit | pool_lookup | other | latency |
|---|---|---|---|---|---|---|---|---|---|
| 1bdac47 | 2 | 2 | 2018 | 153,535 | 0 | 0 | 0 | 0 | insufficient (n=2) |
| 3cc962f | 1 | 1 | 1326 | 153,534 | 0 | 0 | 0 | 693 | insufficient (n=1) |
| fe4a574 | 1 | 1 | 1326 | 153,534 | 0 | 0 | 0 | 693 | insufficient (n=1) |

**Counter totals** (Σ over 4955 passes, one run; identical in every run and in both rounds):

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

## 6. How the work inside the timer differs across arms

These are metric-semantics and behaviour changes between the SHAs, with evidence. **None was clamped, filtered or patched.**

1. **Held (stale) Moe snapshots: 85 evaluated passes, 693 paths.**
   - **After the fix** (3cc962f and fe4a574), the optimizer's quote closure calls `simulate_mixed_path_with_route_key(…, block_timestamp)`.
     - On a path through a Moe pool not refreshed this pass, each sample that reaches that hop fails with `MoeError::SnapshotTimestampMismatch` (fe4a574 `src/amms/moe/mod.rs:449`).
     - `PathOptimizer::optimize_with_quote_and_fee` (`src/arbitrage/optimizer.rs:218-234`, same logic at 3cc962f) handles each such sample the same way. It records the error in `quote_err`, scores that sample as unquotable, and **continues** `search_optimal_input` over the full candidate schedule.
     - Only after the search completes does it return the stored error. The path's outcome becomes `OptimizeOutcome::Error`, counted as `other` (optimize_error), and whatever the search found is discarded.
   - **Before the fix,** the quote closure is `simulate_path` (1bdac47 `src/arbitrage/optimizer.rs:606`), which takes no block timestamp. The same samples simulate through the held snapshot. The search ends as `no_optimum` on 692 paths and as `Ok` on 1 path (item 2).
   - **What changes:** the per-sample work and the outcome label. The number of candidate evaluations does not change.
     - A failing post-fix sample stops at the Moe hop.
     - Every post-fix sample also builds its real route key and prices it, with memoisation.
   - **Evidence:**
     - `amm_quotes` for 1bdac47 equals fe4a574 on every pass except 101176596 (469 vs 442; OPTIMIZE 1 vs 0).
     - A full-corpus diagnostic replay (`RUST_LOG=bot.discovery=debug`, archived under `diagnostics/`) logs exactly **693** optimize errors at both 3cc962f and fe4a574, all of them the Moe timestamp mismatch, and no other optimizer error.
   - **Counter effects:** `paths_quoted` 2,020 vs 1,327, `no_optimum` 2,018 vs 1,326, `other` 0 vs 693.
     - The liveness alarm never fires on the pre-fix arm (0 vs 287 passes), because its passes keep completing searches.
2. **One extra pre-fix optimum** (block 101176596). The pre-fix constant zero-bucket fee search finds an `Ok` optimum on a path that the post-fix arm ends as `Error`. Materialization then rejects it with the real route key as `unapproved_route`, giving 153,535 vs 153,534 and OPTIMIZE n 2 vs 1. OPTIMIZE populations are therefore selection-biased and unpaired.
3. **`amm_quotes` semantics.**
   - `amm_quotes` counts candidate evaluations, not equal units of completed simulation (see item 1).
   - fe4a574 also counts the candidates evaluated on searches that end in `Error` (the later coverage-counter change); 1bdac47 and 3cc962f count `Ok` and `NoOptimum` searches only.
   - Per pass, **fe4a574 − 3cc962f = 26 × `rejects.other`** on all 85 passes with `other > 0`, and 0 elsewhere. The total is **18,018 = 693 × 26** (`results.json` → `amm_quotes_identity_after_vs_deployed`).
   - Paths, rejects and timings are otherwise the same between those two arms (ratio ×1.006).
4. **`fee_resolution_failures`** exists only at fe4a574, where it is 58 (the sample-level count).
5. **Optimize fee pricing** is the change being measured:
   - 1bdac47 prices every sample at a constant zero-bucket topology key;
   - 3cc962f and fe4a574 build the per-sample real route key (tick/bin crossings) and price it, memoised;
   - fe4a574 additionally uses the shared profile-support predicate (a later change).
   - On this universe and profile, the pre-search gate rejects the same `unapproved_route` paths on every arm, apart from item 2.

**Consequence.** The DISCOVERY ratios in §1 and §5 are descriptive measurements of pass time on this schedule. Between the arms, the per-candidate work differs: failed samples are shorter post-fix, and route-key construction, pricing and memoisation are added. So the ratios are not a per-search or per-simulation cost, and neither is ms/quote. No bound on such a cost is claimed.

## 7. Fidelity vs the rc2 ledger (fe4a574 arm)

- **Matched inputs and schedule.** The replay's head-applied dirty set equals the ledger `dirty_pools` on 4955/4955 passes. The pass scope comes from the ledger. The skips, header timeouts and gaps are the live ones, and all 94 live small-gap lines match (§2, §3).
- **Result.** All 4955 passes match on every recorded counter (§2). `mismatches` in `results.json` is empty.
- **Classification.** None needed. There are 0 mismatches, including the first Full pass after startup (5562 / 78 / `amm_quotes` 2028), even though the live bootstrap state was older than block 101173340.
- **What this covers.** Counters only. It is not proof that every live pool-state byte was identical, and absolute times are not compared with live (§9).

## 8. Pilot (before the full capture)

- **Scope:** 180 events (101173341..101173520, 177 passes, 1 Full, Touched passes with 1–11 dirty pools).
  - 124/124 serde round-trip; 178/178 ledger hashes canonical; replay dirty set equal to the ledger on 177/177;
  - fe4a574 counters equal to the ledger on 177/177; 1 DISCOVERY sample per pass on every arm.
- **Cost:**
  - bootstrap: 424 s wall (V3 tick data; per-call latency on the public endpoint swung from 0.7 s to 60 s by backend), cached;
  - 181 headers + 180 getLogs;
  - emulation: 120 s, 267 batch `eth_call` CREATEs (Moe tip refresh);
  - corpus: 5.7 MB; about 6.4 s per replay.
- **Extrapolation:** about 10k header and log calls plus 3–5k CREATEs, 40–60 min, 100–150 MB. This was judged feasible.
- **Actual full capture, round 0:**
  - 4878 + 4878 new header and log calls (1111 s + 948 s);
  - 4081 CREATEs (1975 s);
  - 37.1 MB corpus.
- **Round-1 re-emulation:** caches only, plus 4081 hash-pinned Moe-refresh CREATEs, about 25 min.
- **Replay runs:** about 115–158 s per run.

## 9. Config provenance, non-claims and limits

**Config provenance.** The values were read read-only from arb-bot-jp: `run_plan.json` sha `f0ea307a…`, `capital_evidence.json` sha `d4b2e445…`, and the ledger `run_header`. Nothing else was read on the host, and there was no host access in round 1.
- `max_input = 10 WMNT` (1e19 wei): shadow `assumed_capital_cap_wmnt_wei` = mode cap, applied through `apply_capital_domain_to_discovery`.
- `min_profit = 1e16`: the `v3.config` floor `V3_MIN_PROFIT_FLOOR_WEI`.
- `priority_fee = 100000` and `block_gas_reserve = 1`: `ExecutorConfig` defaults.
- `max_hops = 3`.

The run_header carries digests, not these values. The last three rows are **inferred**. They follow from the rc2 log (`protocol="v3.config"`) and the recorded host env-name inventory: the `.env` has no `MIN_NET_PROFIT_WEI`, `MIN_GROSS_PROFIT_WEI` or `EXECUTOR_PRIORITY_FEE_WEI`. The host `.env` itself was not read.

`min_profit` acts only at materialization, outside DISCOVERY. No secret is recorded; the endpoint is fingerprinted as host `rpc.mantle.xyz`, public, with no key.

**Non-claims.**
- **The pre-fix arm runs on counterfactual inputs.** The FusionX-V2 fee is frozen at 200 in the corpus, while 1bdac47's own loader would build 300. The universe, profile and Moe handling are fe4a574-era. It is "pre-fix discovery code on post-fix-era inputs", not "rc1 latency".
- **Offline absolute times are not comparable to live, and not even across sessions on this machine.** They shifted about 40% between the rounds with host state (the round-1 runs were on battery; host records are in the manifest). Only arm-to-arm ratios within one session are offered.
- **No per-search or per-simulation cost claims** (§6).
- **No profitability claims.** The two opportunities are the same gate-blocked candidates live saw.
- **OPTIMIZE latency is not comparable across arms.** n = 2 / 1 / 1, and the populations are selection-biased (§6.2).

**Limits.**
- One quiet Sunday window: 161 evaluated passes, 14 of them Full. The Full-pass figures rest on n=14.
- The measured ratios are descriptive for this window. They are not a confidence bound, and ×1.006 is not evidence of equivalence.

## 10. Reproduce

```bash
ART=<ORCH>/release-022/whi1527-artifacts          # manifest.json → artifacts.locator; verify: (cd $ART && shasum -a 256 -c SHA256SUMS)
# a) re-run the timed replay on the archived corpus (no RPC):
mkdir -p /tmp/whi1527/corpus && cp $ART/corpus/{corpus.jsonl,mantle_mainnet_v1.json} /tmp/whi1527/corpus/
scripts/replay_baseline.sh arms                     # detached worktrees at the 3 SHAs, identical harness, --locked release builds
REPEATS=5 SUMMARY_LOG=$ART/sources/signerless_excerpt.log scripts/replay_baseline.sh run
# b) regenerate the schedule from the archived sources (bit-identical to schedule.json.gz, sha256 bc78f4a9…):
LEDGER=$ART/sources/ledger_window_rows.jsonl SUMMARY_LOG=$ART/sources/signerless_excerpt.log scripts/replay_baseline.sh schedule
# c) re-capture: copy $ART/rpc_cache/* into /tmp/whi1527/corpus first. Only the hash-pinned Moe
#    tip refreshes are then read from archive RPC. With the same cached inputs, the output reproduces the
#    canonical content digest 3a17406e… (and, from this capture source on, the same bytes).
SUMMARY_LOG=$ART/sources/signerless_excerpt.log scripts/replay_baseline.sh capture
# d) analysis → evidence/replay/whi-1527/{results,per_pass_evaluated,manifest}.json + schedule.json.gz:
ARTIFACTS=$ART LEDGER=$ART/sources/ledger_window_rows.jsonl SUMMARY_LOG=$ART/sources/signerless_excerpt.log scripts/replay_baseline.sh analyze
```

`WORK` defaults to `/tmp/whi1527`, where the script keeps everything it owns. `scripts/replay_baseline.sh clean` removes only its worktrees and targets. Reproducing from scratch with a fresh bootstrap read (no `rpc_cache`) gives the same content only if the archive serves the same state at the pinned hashes. The original bytes are available from `ART`.

## 11. Fix round 1: round 0 vs round 1

**What changed:**
- Outer header timeouts are now modelled as no-ops (13 events). On each following block, the emulation now has a gap range and matches the live log.
- Pool inputs and the corpus content are unchanged: the content digest `3a17406e…` is the same for both corpora.
- Corpus bytes changed from `3817e43f…` to `46c84201…` because of the canonical encoding.
- The schedule sha changed from `6f2789fb…` to `bc78f4a9…`, because the 13 events are now labelled `header_timeout`.
- The replay ran again with 5 repeats instead of 3.

| figure | v1 (984d3d2: corpus 3817e43f…, 3 repeats) | v2 (this round: corpus 46c84201…, 5 repeats) |
|---|---|---|
| 1bdac47 DISCOVERY evaluated p50 / p90 / p95 / p99 (ms) | 2.46 / 16.07 / 34.25 / 35.88 | 3.16 / 22.55 / 48.02 / 48.68 |
| 3cc962f DISCOVERY evaluated p50 / p90 / p95 / p99 (ms) | 2.64 / 18.46 / 39.84 / 44.11 | 3.58 / 24.90 / 57.22 / 58.58 |
| fe4a574 DISCOVERY evaluated p50 / p90 / p95 / p99 (ms) | 2.69 / 20.53 / 40.79 / 42.04 | 3.52 / 26.17 / 57.62 / 58.73 |
| 1bdac47 → 3cc962f all/evaluated ratio of sums (Δ p50 ms) | ×1.143 (+0.229) | ×1.133 (+0.368) |
| 1bdac47 → 3cc962f full/evaluated ratio of sums (Δ p50 ms) | ×1.219 (+6.672) | ×1.196 (+9.091) |
| 1bdac47 → 3cc962f touched/evaluated ratio of sums (Δ p50 ms) | ×1.084 (+0.197) | ×1.084 (+0.344) |
| 3cc962f → fe4a574 all/evaluated ratio of sums (Δ p50 ms) | ×0.996 (+0.024) | ×1.006 (+0.018) |
| 3cc962f → fe4a574 full/evaluated ratio of sums (Δ p50 ms) | ×0.975 (+0.390) | ×1.003 (+0.327) |
| 3cc962f → fe4a574 touched/evaluated ratio of sums (Δ p50 ms) | ×1.014 (+0.021) | ×1.008 (+0.013) |
| 1bdac47 → fe4a574 all/evaluated ratio of sums (Δ p50 ms) | ×1.138 (+0.256) | ×1.14 (+0.381) |
| 1bdac47 → fe4a574 full/evaluated ratio of sums (Δ p50 ms) | ×1.189 (+6.530) | ×1.2 (+9.671) |
| 1bdac47 → fe4a574 touched/evaluated ratio of sums (Δ p50 ms) | ×1.099 (+0.213) | ×1.093 (+0.358) |
| 1bdac47 repeat spread, median / p90 relative range | 0.416 / 0.489 | 0.056 / 0.169 |
| 3cc962f repeat spread, median / p90 relative range | 0.108 / 0.426 | 0.076 / 0.184 |
| fe4a574 repeat spread, median / p90 relative range | 0.095 / 0.214 | 0.057 / 0.148 |
| counters (all arms, all passes) | — | identical to v1 on every total |
| fidelity vs rc2 ledger (fe4a574) | 4955/4955 | 4955/4955 |

The counters are identical. The ratios hold to within 0.01–0.03, and repeat noise is much lower. Absolute times are about 40% higher in round 1 because of host state (§9).
