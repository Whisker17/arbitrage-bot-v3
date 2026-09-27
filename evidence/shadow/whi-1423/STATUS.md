# WHI-1423: the post-WHI-1409 binary on `arb-bot-jp` (signerless shadow) and live discovery evidence

**Status: AC1, AC2, AC3 and AC4 are met. AC3 is met via the pinned replay baseline (PR #112, dev eb824bc; §4a). AC5 belongs to the orchestrator and has not been posted yet.** The deploy is complete and still running.

**Host snapshot** (read-only ssh, 2026-09-27T12:46:20Z):
- **Shadow process:** still pid **1275022**, cwd `/opt/arbitrage-bot-v3-rc2`, exe sha256 `f55b341b…621c` (unchanged), elapsed 10:05:40. `production_send_allowed="false"`; the liveness alarm gauge reads 0.
- **Freshness:** tip **101191431** (the public RPC `eth_blockNumber` and the ledger's last row agree). Age = 101191431 − 101165208 = **26,223** blocks, well under 250,000; about 124 h remain.
- **Digest timer:** `lark-daily-digest.timer` NEXT is **2026-09-28 00:10:00 UTC** and LAST is 2026-09-27 00:10:15 UTC.
  - The 2026-09-28 00:10Z fire has **not happened yet**.
  - The service journal has no entries since the reload.
  - The state marker is still `2026-09-26`, mtime 00:10:17Z.
  - Nothing was triggered.
- **Other project:** `arb-bot.service` is active and untouched.

**AC2 refresh, frozen 2026-09-27T05:57:22Z.** Evidence covers ≈3 h 14 min (02:43:20Z–05:57:22Z): **5699 observed ledger blocks, 207 of them with evaluated work** (`cycles_evaluated > 0`; 16 Full + 191 Touched). That meets the ≥200 requirement. The 200th evaluated block was 101178951, at 05:50:21.8Z. The earlier 05:31:52Z freeze (4956 blocks, 161 evaluated) is superseded; its numbers are kept in `summary.json` under `previous_freeze_…`.

Scope, per the owner authorization of 2026-09-27 ("授权 tag 和 shadow 部署"):
- tag `v0.2.2-rc2` and deploy it to the signerless shadow **only**;
- no sends, no pool registration, no funding;
- no `deploy_only.sh`, `fund_and_canary.sh`, `deploy_and_arm.sh` or `run_live.sh`;
- no `main` promotion or GitHub production release.

`arb-bot.service` on this host belongs to a different project and was not touched.

## 1. Identity
| item | value |
|---|---|
| tag | `v0.2.2-rc2`: annotated tag object `a9c22b7d784d9006cb9d361e6f2e391425f8281b` |
| commit | `fe4a574f7f4a60a27cd059f1c585435c554c2ae0` (`dev`, squash of PR #110) |
| checkout | fresh `git clone` of the public repo into `/opt/arbitrage-bot-v3-rc2`, then `git checkout --detach v0.2.2-rc2`. Checks: `git rev-parse HEAD` = fe4a574, `git cat-file -t v0.2.2-rc2` = `tag`, `git status --porcelain` empty before the build. |
| build | `cargo build --locked --release --bin bot` with rustc 1.95.0 (59807616e 2026-04-14). Took 6 min 01 s on the host (02:24:43Z–02:30:45Z). |
| bot sha256 | `f55b341b5e49452b413875f2a18d2a5ed475067b713f30e57a59648ac8de621c`. `/proc/<pid>/exe` of the running process has the same hash. |
| `lark_daily_digest` sha256 | `3ad466b207f09e38ab45a412c2fc967050b662dce3b7bdfef0550ba1e2062b0c`, built with `--locked --release --bin lark_daily_digest` and copied to `/opt/arbitrage-bot-v3-rc2/bin/lark_daily_digest-v0.2.2-rc2` |
| universe | `data/pool_universe.csv`, 124 pools, snapshot **101165208**, hash `0xa915e1b8…a6e5c1` (matches the on-chain block hash) |
| universe fingerprint | `0x4c2456ddbe945beb0ceabdd825b8329cd5f16216ab3c7d727a1e35631c1a6188`. csv sha256 `702d7a32…1ccae0`. Both equal `config/pool_universe.pin.json`. |
| gas profile digest | `0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412`. This is the artifact's `content_digest`, equal to `MANTLE_MAINNET_PROFILE_DIGEST`, and logged as the WHI-1408 `profile_identity`. The ledger run_header's `profile_digest` field (`0xf6f1a96c…`) is a different hash domain: the shadow manifest's `digest_of(serialized artifact)`. It is not a mismatch. |
| host / cwd | `arb-bot-jp` (hostname `server`), cwd `/opt/arbitrage-bot-v3-rc2` |
| process | pid **1275022**, exe `/opt/arbitrage-bot-v3-rc2/target/release/bot`. Args: `--protocols agni-v2,agni-v3,moe --watch --ledger /opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/ledger.jsonl` |
| launch method | Same shape as the old process: `tmux new-session -d -s shadow -- /bin/bash -lc ". /opt/arbitrage-bot-v3-rc2-build-logs/launch_env.sh; exec ./scripts/golive/run_signerless_shadow.sh >>…/whi-1423/launcher.out 2>&1"`. `launch_env.sh` sources the host's existing `/opt/arbitrage-bot-v3/.env` without copying it. It pins `MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH` to the host's existing `/opt/arbitrage-bot-v3/config/gas_profiles/shadow_thresholds.v2.json` and sets `BOT_BIN`, `SHADOW_LOG_DIR` and `SHADOW_LEDGER_PATH` to the paths above. No extra bot args are passed; the launcher already passes `--watch`. |
| limits | Identical to the old process, and all are repo defaults: throttle_rps=8 (recommended 4; the same WARN as before), max_retries=5, max_hops default 3, shadow assumed capital 1e19 wei (10 WMNT), skip-ratio window 32 / 0.5. There is no `BOT_UNIVERSE_MAX_AGE_BLOCKS` or other override in the environment. The code diff rc1→rc2 changes none of these defaults. |
| send gate | `SHADOW_MODE=1`. `BOT_ENABLE_SENDS` is unset and `--enable-sends` is never passed. `production_send_allowed: false` appears in the startup log and in `arbbot_build_info`. Ledger `send_capability: "no_send"`. |
| freshness | Pre-launch at 02:24:59Z: tip 101172790, **age 7582** < 250000 (~134 h remaining). The bot enforces freshness at startup and passes silently; it would fail closed if stale. The first watched block was 101173341, age 8133. The window closes ≈ 2026-10-02 17:05Z. |

Startup log excerpts (endpoints scrubbed):
```
02:40:41.828894Z INFO bot.live: loaded unified pool universe path=data/pool_universe.csv pool_count=124 snapshot_block=Some(101165208) fingerprint=0x4c2456ddbe945beb0ceabdd825b8329cd5f16216ab3c7d727a1e35631c1a6188
02:40:41.833772Z INFO bot.live: universe gas profile compatibility validated (WHI-1408; necessary, not sufficient: …) profile_identity=0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412
02:43:17.052109Z INFO state_space::sync: cold-start pool sync complete pools=124 wall_ms=155218 batch_create_calls=554
production_send_allowed: false
02:43:20.919611Z INFO bot.live: entering multi-protocol --watch loop (single shared head source) head_source="ws" …
```

Preflight, run in the new checkout under the same launch environment:
- `CHILD_ENV_SNAPSHOT=1` gave `child_env_snapshot_ok`. All 8 forbidden or signer names were `absent` in the child, `SHADOW_MODE=1` was set and `BOT_ENABLE_SENDS=<unset>`.
- `PREFLIGHT_ONLY=1` exited 0 and printed `production_send_allowed: false`. Two preflight-only adjustments were needed:
  - `SHADOW_LEDGER_PATH` was unset, because bot's `--ledger` reads that env and is refused with `--offline`;
  - `BOT_METRICS_ADDR=127.0.0.1:9465`, because the old process still held 9464.

  Neither applies to the live launch.

## 2. Window, and the Full / Touched split
**Freeze method.**
- Once the threshold was reached, the host ledger, `signerless.log` and `stage_samples.tsv` were copied read-only (scp) to a local frozen directory, starting at 05:56:46Z.
- Both files were cut at the last block they both contain: **101179163**, whose `block_summary` was logged at 05:57:22.307Z.
- The same `aggregate.py` and `join.py` scripts as the first freeze were run on the copy. The only change was the input directory path.

**Threshold sentinel.** A single bounded read-only check ran on the host (`timeout 5900`, one ledger count every 300 s):
- 05:45:23Z: 187 evaluated blocks;
- 05:50:23Z: **200**, at which point the sentinel exited.

**Window.**
- Ledger block range: **101173341 → 101179163**, with **5699** distinct observed blocks. The run started 02:43:20Z.
- The `block_summary` log covers 101173342 → 101179163 in 5811 lines. These include skipped and gap blocks.
- Skip reasons:
  - `-` 5698;
  - `pinned_logs_unavailable` 58;
  - `processing_failed` 41 (Moe "Arithmetic overflow while updating Moe reserves", which rc1 also produced);
  - `pinned_header_unavailable` 13;
  - `duplicate` 1.

| pass scope (ledger `discovery.scope`) | observed blocks | blocks with `cycles_evaluated > 0` | cycles_evaluated (Σ cycles_optimized) | cycles_total | paths_quoted |
|---|---|---|---|---|---|
| **Full** | 16 | 16 | 88,992 | 88,992 | 1,248 |
| **Touched** | 5,682 | 191 | 89,708 | 31,603,284 | 271 |
| no `discovery` object (startup baseline row, block 101173341) | 1 | – | – | – | – |
| **total** | **5,699** | **207** | **178,700** | 31,692,276 | **1,519** |

The ledger and log agree: `lines_cycles_evaluated_gt0` = 207, and the cycles_evaluated, paths_quoted and reject sums are identical between the two.

**What the evaluated work looks like**, so that idle blocks are not counted as meeting the AC:
- 207 blocks had `cycles_evaluated > 0`. Of those, 16 are Full re-baselines and 191 are Touched passes on dirty pools.
- 115 had `paths_quoted > 0`.
- Median `cycles_evaluated` per evaluated block was 386. 173 blocks evaluated ≥100 cycles, and 14 evaluated ≤8.
- The rate was ≈64 evaluated blocks/h overall and uneven:
  - 37 by 03:42Z;
  - 161 at 05:31:52Z;
  - 187 at 05:45Z;
  - 207 at 05:57Z.
- It was a Sunday, with low DEX activity.

## 3. Counters and the full reject breakdown (Σ of `block_summary` over the window)
| counter | value |
|---|---|
| `cycles_evaluated` | 178,700 |
| `paths_quoted` | 1,519 |
| `amm_quotes` | 62,427 |
| `affected` (Σ) | 403 |
| `candidates` / `eligible` | 2 / 0. There were no new candidates since the first freeze. Both are the same route, at blocks 101176594 and 101176595, with modeled `best_net` = 22,236,164,945,446,222 wei (≈0.0222 WMNT). `attempt_outcome="production_gate_blocked"`; ledger candidate `outcome.kind = env_unsupported`. The send gate is closed, as intended. |
| `gas_rescores` / `mixed_skipped_count` | 0 / 2 |
| `fee_resolution_failures` | 70 (samples, not paths) |

| reject reason | count | share of evaluated |
|---|---|---|
| `unknown_route` | **0** | 0 % |
| `unapproved_route` | 176,300 | 98.66 % |
| `pool_lookup` | 0 | 0 % |
| `no_optimum` | 1,518 | 0.85 % |
| `zero_profit` | 0 | 0 % |
| `other` | 881 | 0.49 % |

Split by scope (ledger):
- **Full:** unapproved_route 87,744, no_optimum 1,248, other 0.
- **Touched:** unapproved_route 88,556, no_optimum 270, other 881.
- unknown_route, pool_lookup and zero_profit are 0 in both.

Notes:
- **Consistency check:** 176,300 + 1,518 + 881 = 178,699 = cycles_evaluated − 1. The one path not rejected is the path that became the candidate.
- **The `other` bucket:** in the ledger it covers `optimize_error`.
- **Prometheus cross-check:** the Prometheus `arbbot_discovery_rejected_total` counters were scraped read-only at 05:58:45Z, a little after the cut, and all series are cumulative. They read `unapproved_route` 181,984, `no_optimum` 1,539 and `optimize_error` 944.
- **Comparison with rc1:** the old rc1 process had 100 % `gas_profile` rejects (8.27 M) and zero optimizer entries over 269,622 passes. After WHI-1409:
  - `unknown_route = 0`, so the optimize and materialize route keys agree;
  - 1,519 optimizer searches completed as Ok/NoOptimum (`paths_quoted`);
  - searches that ended in an Error are counted separately, in `other`. The `other` total is 881. In the first-freeze window, all 693 were Moe `SnapshotTimestampMismatch` optimize errors, according to the replay diagnostics in §4a;
  - one path reached a successful optimum and a candidate.

**WHI-1424 coverage fields**, computed from the ledger over evaluated rows. The startup row, which has no discovery object, is excluded here; the digest counts it as a coverage gap (see §5).
- paths_evaluated = 178,700; paths_quoted = 1,519 (0.850 %).
- **Full-pass** paths_evaluated = 88,992; full_pass_paths_quoted = 1,248, which is **1.402 %** and at or above `LIMITED_EVALUATION_COVERAGE_PERCENT = 1`.
  - So `limited_evaluation_coverage` would be **false** on Full passes.
  - The margin over the threshold is thin: every Full pass quoted 78 of 5,562 paths.
- rejects are as in the table above; fee_resolution_failures = 70.

## 4. Latency (labelled). AC3 is met via the pinned replay baseline.
**What each timer measures** (`src/service/path_index.rs` at fe4a574; the replay runs each arm's own `discover` with its own recorders):
- `stage::DISCOVERY` (`arbbot_pipeline_stage_duration_seconds{stage="discovery"}`) is recorded once per discovery pass. It is **the optimize loop** over the selected subset: pool lookup, fee/route checks, optimizer search, and post-optimum simulation that caches `CachedGross`.
  - It is emitted **before** cached materialization and gas re-scoring of the found set.
  - It **excludes** path-index/topology build, which is paid at build time. For example, the first Full pass after startup took ≈33 s wall-clock between `block_summary` lines, but only milliseconds on this timer.
  - It is recorded for idle passes too.
- `stage::OPTIMIZE` (`…{stage="optimize"}`) is recorded **only on a successful optimum**, per path. NoOptimum, Rejected and Error outcomes are not recorded.

### 4a. Matching before/after: the pinned replay baseline (PR #112, dev eb824bc)
**Source.** At dev `eb824bc`, which is merged into this branch:
- `evidence/replay/whi-1527/STATUS.md`
- `evidence/replay/whi-1527/results.json` (the numbers below are copied from it)
- the companions `manifest.json`, `per_pass_evaluated.json` and `schedule.json.gz`

The baseline passed an independent review, which accepted it as this issue's AC3 evidence within the limits listed below.

**Scope of the comparison.** Every arm replays one frozen corpus of this rc2 run's window. The corpus is hash-pinned (canonical content sha256 `3a17406e…1a677c`; schedule sha256 `bc78f4a9…ca635`).
- **Window:** blocks 101173341..101178398. That is the first-freeze window of this STATUS: 4955 discovery passes, of which 161 were evaluated (14 Full + 147 Touched).
- **Same inputs on every arm:**
  - the same 124-pool universe (`0x4c2456dd…` @101165208);
  - the same gas profile (`0x3d3244e3…`);
  - 10 WMNT capital;
  - the recorded config.
- **Schedule:** the **actual live observation schedule** (5048 head events, including skips, gaps and header timeouts). It preserves the indirect effects of the live 8 RPS throttle on which blocks were seen and how dirty sets were batched.
- **No RPC inside the timers:** the replay makes no RPC calls on any arm. The capture ran at 6 RPS, outside the timers.
- **Fidelity:** the deployed arm matches this run's ledger and `block_summary` counters on 4955/4955 passes.

**Arms.** All three run the same harness bytes, built `--locked --release` with rustc 1.95.0:
- **`1bdac47`**: before the route-key fix;
- **`3cc962f`**: after the fix. The config/data diff against `1bdac47` is empty, so this pair isolates the fix;
- **`fe4a574`**: the deployed rc2.

**`stage::DISCOVERY` over the evaluated passes** (n = 161 unique passes, 5 timed repeats per arm). Each value is the per-pass median of the repeats, then a nearest-rank percentile (`results.json` → `discovery.<arm>["all/evaluated"].unique_passes_median_of_repeats`):

| arm | n | p50 | p90 | p95 | p99 |
|---|---|---|---|---|---|
| `1bdac47` (before) | 161 | 3.1626 ms | 22.5498 ms | 48.0178 ms | 48.6763 ms |
| `3cc962f` (after the fix) | 161 | 3.5764 ms | 24.9029 ms | 57.2208 ms | 58.5791 ms |
| `fe4a574` (deployed) | 161 | 3.5158 ms | 26.1734 ms | 57.6205 ms | 58.7299 ms |

By scope (same source, `full/evaluated` and `touched/evaluated`):

| arm | Full (n = 14) p50 / p90 / p95 / p99 | Touched evaluated (n = 147) p50 / p90 / p95 / p99 |
|---|---|---|
| `1bdac47` | 48.0615 / 48.6763 / 48.8595 / 48.8595 ms | 3.0190 / 15.5144 / 19.8151 / 26.1498 ms |
| `3cc962f` | 57.4797 / 58.5791 / 58.7172 / 58.7172 ms | 3.3730 / 17.2585 / 22.6978 / 29.9544 ms |
| `fe4a574` | 58.0229 / 58.7299 / 58.8556 / 58.8556 ms | 3.3966 / 17.4942 / 22.0234 / 29.7162 ms |

**Paired ratio of sums** (`results.json` → `paired_deltas`, same passes on both sides):
- **`1bdac47 → 3cc962f`:**
  - overall **×1.133**: Σ 1528.58 → 1732.46 ms over the 161 evaluated passes, 147 of which are slower after the fix. The median per-pass delta is +0.368 ms;
  - Full **×1.196**, with all 14 passes slower;
  - Touched **×1.084**.
- **`3cc962f → fe4a574`** (deployed vs after-fix): **×1.006**.

**`stage::OPTIMIZE`: insufficient samples.** n = 2 / 1 / 1 (`1bdac47` / `3cc962f` / `fe4a574`), so no percentiles are given. The populations are also selection-biased and unpaired: the pre-fix arm finds one extra `Ok` optimum at block 101176596, which materialization then rejects.

**Limits** (from the baseline's STATUS and the review):
- **One quiet window.** This is a single Sunday window with 161 evaluated passes, of which **Full n = 14**. The repeats improve measurement stability, not the size of the market sample.
- **Counterfactual fee on the pre-fix arm.** The FusionX-V2 fee is **frozen at 200** in the corpus on every arm. `1bdac47`'s own loader would build 300, so the pre-fix arm is "pre-fix code on post-fix-era inputs", not rc1 latency.
- **Absolute times.** The offline absolute times, measured on one Apple M2 Pro, are **not comparable to live** host times. They also shifted ≈40 % between replay rounds while the ratios held. Only the within-session paired ratios are the comparison.
- **×1.006 is not evidence of equivalence** between the deployed and after-fix arms.
- **The work differs across arms.** After the fix, the optimizer prices a real route key per sample. 693 searches end in a Moe `SnapshotTimestampMismatch` error (`rejects.other`) instead of `no_optimum`. `amm_quotes` also differ (`fe4a574 − 3cc962f = 26 × other` on every error-bearing pass). The ratios are pass-level observations on this schedule, not a per-search or per-simulation cost.
- **No claims are made about** a live speedup or slowdown, profitability or release readiness.

### 4b. Live after-numbers from the host (context only)
These are the live rc2 figures from this deployment. They are kept for context and are **not** the before/after comparison; absolute live times are not comparable to the offline replay.
**Live rc2, per-pass DISCOVERY, evaluated blocks only.**
- **Method:**
  - A read-only sampler scraped `127.0.0.1:9464/metrics` every ~0.27 s. It ran from **03:42:04Z** until its `timeout` stopped it at **05:30:00Z**. That produced 23,825 samples.
  - The `_sum` delta of each interval whose `_count` rose by exactly 1 (3,116 such intervals) was attributed to the next `block_summary` line. Attribution uses the scalar `_sum`/`_count`, not histogram buckets, so there is no bucket interpolation in these numbers.
  - 24 intervals that contained more than one pass were discarded.
- The sampler was **not** restarted for this refresh, because the refresh is read-only on the host. So the per-pass sample set is unchanged.
- **Why n (122) is smaller than the evaluated-block count.** The evaluated-block count was 161 at the first freeze and is 207 now. The n differs purely because of the sampler's start and stop times:

  | evaluated blocks (`cycles_evaluated > 0`) | count |
  |---|---|
  | before the sampler started (02:43:20Z–03:42:04Z). The sampler was only started at 03:42Z, after thin evaluated work was noticed at 03:41Z. | 37 (4 Full + 33 Touched) |
  | inside the sampler window (03:42:04Z–05:30:00Z) | **122** |
  | after the sampler stopped (05:30:00Z–05:57:22Z). Of these, 2 fell before the first freeze and 46 came after it. | 48 |
  | **total** | **207** (the first freeze had 37 + 122 + 2 = 161) |

  - Scrape granularity and bucket attribution cost **nothing**:
    - every one of the 122 in-window evaluated blocks was attributed exactly once;
    - none fell inside the 24 discarded multi-pass intervals (those contained idle passes only);
    - 0 intervals were unmatched.
  - Alignment check:
    - the idle-pass intervals peak at 0.001 ms;
    - the smallest evaluated-pass value is 0.058 ms, from an 8-cycle Touched pass;
    - so no evaluated pass's time was attributed to an idle block, or the reverse.
- **New n = 122**: 10 Full + 112 Touched. It covers 122 of the 207 evaluated blocks (59 %), and only the 03:42:04Z–05:30:00Z sub-window.

| timer | n | p50 | p90 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| `stage::DISCOVERY`, evaluated passes in the sampler window | 122 | 6.849 ms | 50.898 ms | 98.070 ms | 102.143 ms | 110.214 ms |
| `stage::DISCOVERY`, idle passes (cycles_evaluated = 0), for reference | 2994 | 0.000 ms | 0.000 ms | 0.000 ms | 0.001 ms | 0.001 ms |
| `stage::OPTIMIZE` (successful optima only) | **1** | **insufficient samples**: no percentiles are given | – | – | – | – |

- The single OPTIMIZE sample is at block 101176594 (3.755 ms). A single sample is **not** a distribution. At 05:58:45Z the cumulative Prometheus `optimize` count was still 1.
- The cumulative Prometheus histogram, scraped read-only at 05:58:45Z, covers all 5739 passes including idle ones and is bucket-interpolated: DISCOVERY p50 0.052 ms, p90 0.093 ms, p95 0.098 ms, p99 25.42 ms, mean 0.662 ms. This mixes idle and evaluated passes and is given for reference only.

### 4c. rc1 109-pool numbers (context only; superseded as a baseline by §4a)
- These come from the rc1 rollback process's own metrics: pid 1548362 (sha256 `2abe8919…ebee5`, v0.2.2-rc1 source), scraped at 02:40Z just before it stopped.
- That process ran on a **109-pool universe** (snapshot 100871945, fingerprint `0xee1d40b8…`), with the rc1 profile.
- `stage::DISCOVERY` over all passes, which were effectively no-optimizer passes (every path was rejected `gas_profile` before the optimizer): n = 269,622, p50 0.054 ms, p90 0.097 ms, p95 0.617 ms, p99 10.675 ms, mean 0.451 ms.
- `stage::OPTIMIZE`: no samples.
- These are **not a matching before/after**: the universe, profile and optimizer work all differ. They are kept only as history. The matching comparison is §4a.

## 5. WHI-1411 liveness and digest rendering (real output; no sends)
- **`block_summary` fields render on real output**: `paths_quoted`, `liveness_alarm`, and the six reject fields plus `fee_resolution_failures` appear on every line.
  ```
  04:32:13.537282Z INFO service.block_summary: block_summary block=101176594 affected=8 cycles_evaluated=1694 paths_quoted=2 amm_quotes=235 gas_rescores=0 candidates=1 eligible=0 mixed_skipped_count=1 best_mixed_net="22236164945446222" best_net="22236164945446222" attempt_outcome="production_gate_blocked" skip_reason="-" liveness_alarm=false unknown_route=0 unapproved_route=1686 pool_lookup=0 no_optimum=1 zero_profit=0 other=6 fee_resolution_failures=1
  ```
- **The liveness alarm fired on real output.** The sustained-window trigger went off at 04:12:01.985Z on block 101176002:
  ```
  ERROR bot.discovery: WHI-1411 liveness invariant violated: zero paths reached optimizer (discovery pipeline dead; all cycles rejected pre-simulation) cycles_optimized=1368 paths_quoted=0 amm_quotes=104 consecutive_dead_heads=10 unknown_route=0 unapproved_route=1364 … other=4 scope="touched"
  ```
  - It stayed latched (`liveness_alarm=true`) on 287 `block_summary` lines, through 04:21:33Z. The ERROR line repeated on each block, including idle `cycles_optimized=0` blocks, until a pass quoted ≥1 path again.
  - `arbbot_discovery_liveness_alarm` read 0 at 05:31Z and again at 05:58:45Z.
  - At the refresh freeze (05:57:22Z) the latched count was still 287 lines, so the alarm did not fire again after 04:21:33Z.
  - **What the alarm counts** (fe4a574 `src/service/path_index.rs`):
    - `paths_quoted` is incremented only for Ok and NoOptimum outcomes (lines 486 and 490). An Error outcome is recorded as `optimize_error`, i.e. the ledger/log `other` bucket (lines 510–511).
    - `amm_quotes` includes the work of Ok, NoOptimum **and** Error searches (line 480).
    - A pass with `cycles_optimized > 0` and `paths_quoted == 0` increments `consecutive_dead_heads`, and a pass with `paths_quoted > 0` resets it (lines 658–662). The alarm fires at 10 (line 664).
    - So the alarm tracks consecutive evaluated passes with **zero Ok/NoOptimum completions**, and searches that ended in an Error count toward it.
    - The ERROR text ("zero paths reached optimizer … all cycles rejected pre-simulation") is therefore not literally accurate when `other > 0`. This is an observation only; no change is proposed.
  - **The ten passes behind the trigger.** I recomputed these from the frozen ledger (observation rows with `cycles_optimized > 0` and block ≤ 101176002, last ten) and cross-checked them against `block_summary` in the frozen log. All ten are Touched passes with `paths_quoted = 0`. The evaluated pass before them, 101174655, had `paths_quoted = 1`.

    | block | cycles_evaluated | unapproved_route | other | amm_quotes |
    |---|---:|---:|---:|---:|
    | 101174665 | 48 | 44 | 4 | 104 |
    | 101174676 | 324 | 324 | 0 | 0 |
    | 101175264 | 324 | 324 | 0 | 0 |
    | 101175533 | 196 | 196 | 0 | 0 |
    | 101175567 | 164 | 164 | 0 | 0 |
    | 101175570 | 164 | 164 | 0 | 0 |
    | 101175715 | 48 | 44 | 4 | 104 |
    | 101175722 | 84 | 84 | 0 | 0 |
    | 101175737 | 248 | 242 | 6 | 156 |
    | 101176002 (trigger) | 1,368 | 1,364 | 4 | 104 |
    | **total** | **2,968** | **2,950** | **18** | **468** |

    `unknown_route`, `pool_lookup`, `no_optimum` and `zero_profit` are 0 on every one of these passes.
  - **Reading.** The dead window is a **mix**:
    - 2,950 paths were rejected `unapproved_route`;
    - 18 searches ran optimizer work (468 AMM quote evaluations, on 4 of the 10 passes) and ended in an Error.
    - The observed result is `unknown_route = 0`.
    - No single causal attribution, policy or otherwise, is claimed, and no fix is proposed or attempted here.
- **Digest rendering** was checked with `lark_daily_digest --dry-run` only. It was not re-run for the AC2 refresh. The rc2 binary ran under `env -i` + `unshare -n` (no webhook, keyword or state env; no network namespace), on the new ledger, `--date 2026-09-27`, at 05:32Z.
  - rc=0; the state marker's sha and mtime were unchanged.
  - Card excerpt:
    - `观测 4985 次，4985 个不同区块高度`, first 02:43:20Z (101173341), last 05:32:51Z (101178427), `数据新鲜`
    - `⚠ 部分留存缺失 … 最早可用数据从 2026-09-27 02:40:41 UTC 开始`
    - `脏池区块 D/K 139/4984`, `缺失 discovery 1`, `周期评估覆盖率 155940/27721008`
    - `优化器定价路径 1328（⚠ 部分未知 …）`, `优化器覆盖 N/A（部分评估记录缺少 paths_quoted …）`
    - `已记录候选 2 次（preflight 尝试次数，非成交次数） · env_unsupported: 2`
    - `最佳建模净利润: N/A — 候选存在但无可用净利润数据`
  - Observed rendering behaviour, by design and not a fix claim:
    1. The single startup observation with no `discovery` object (the pre-watch baseline row) makes the whole day's optimizer-coverage and liveness line "partially unknown / N/A" (WHI-1424 PR107-F1 fail-closed). This recurs on every day with a restart.
    2. The digest shows candidate profit as N/A for `production_gate_blocked` rows, even though `block_summary` carries `best_net`.

## 6. Digest migration (C2)
The owner authorized this as a separate decision ("切换新账本和程序"), relayed by the orchestrator. It is a config change to the already-approved scheduled delivery; no manual send or test-send was made.

- Before: `lark-daily-digest.service` ran `/opt/arbitrage-bot-v3/target/release/lark_daily_digest` (rc1, sha256 `2df03111…bae2`) with `--ledger /opt/arbitrage-bot-v3/evidence/shadow/signerless/ledger.jsonl` and cwd `/opt/arbitrage-bot-v3`.
- After: drop-in `/etc/systemd/system/lark-daily-digest.service.d/10-whi1423-rc2.conf` (sha256 `12aeb690…d426`), applied 2026-09-27T03:11:13Z with `daemon-reload` only:
  ```ini
  [Service]
  ExecStart=
  ExecStart=/opt/arbitrage-bot-v3-rc2/bin/lark_daily_digest-v0.2.2-rc2 \
      --ledger /opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/ledger.jsonl \
      --state /opt/arbitrage-bot-v3/data/lark_daily_digest/state.marker
  WorkingDirectory=/opt/arbitrage-bot-v3-rc2
  ```
  - The binary is v0.2.2-rc2 / fe4a574, sha256 `3ad466b2…2b0c`, at a versioned copy rather than target/release.
  - Unchanged: `EnvironmentFile=/opt/arbitrage-bot-v3/.env` (inherited), the `--state` marker (`2026-09-26`), and the timer (00:10 UTC, Persistent=true).
  - `src/notify/state.rs` is identical in rc1 and rc2, so the marker and flock are compatible and no day is re-sent.
- Pre-apply dry-run (versioned binary, `env -i` + `unshare -n`, 03:10:51Z): rc=0, marker unchanged.
- Post-apply checks:
  - `DropInPaths` is the drop-in, and ExecStart and WorkingDirectory are as above.
  - The timer's **NEXT is 2026-09-28 00:10:00 UTC** and LAST is 2026-09-27 00:10:15 UTC, both unchanged. The journal has no invocation since the reload, and the InvocationID is unchanged.
  - The marker is unchanged. Re-checked at 05:32Z.
- Rollback: `rm /etc/systemd/system/lark-daily-digest.service.d/10-whi1423-rc2.conf && rmdir /etc/systemd/system/lark-daily-digest.service.d && systemctl daemon-reload`.
- Known caveats:
  - The day-2026-09-27 card (fires 2026-09-28 00:10Z) covers rc2 rows from **02:40:41Z** only; the rc1 rows from 00:00–02:40Z are in the old ledger. It will show the retention-gap warning.
  - Restart days render "partially unknown" (§5).

## 7. Rollback refs
- Old process: pid 1548362, v0.2.2-rc1 source (6dad49e, confirmed by blob hashes; the tree has no `.git`), sha256 `2abe89197644928498fecccfb666a0f0d21af973dba43524b771d5165a6ebee5`. It was stopped at 02:40:25Z with SIGTERM and exited cleanly ("blocks_processed=269626 opportunities_found=0"). Its tmux wrapper and launcher exited on their own.
- Backup: `/opt/arbitrage-bot-v3-rollback/20260927T022008Z/` holds `bot`, `lark_daily_digest`, `SHA256SUMS`, the exact old cmdline and tmux command, env **names**, the old universe, and `RELAUNCH.md`.
- The old tree `/opt/arbitrage-bot-v3`, including its `.env`, host-local thresholds, data and the old ledger and logs, is untouched.
- Relaunch the old shadow: stop tmux session `shadow` (pid 1275022), then run the command in `RELAUNCH.md`:
  ```
  tmux new-session -d -s shadow -- /bin/bash -lc "export PATH=/usr/local/bin:/root/.cargo/bin:\$PATH; export BOT_BIN=/opt/arbitrage-bot-v3/target/release/bot; export SHADOW_LOG_DIR=/opt/arbitrage-bot-v3/evidence/shadow/signerless/logs; export SHADOW_LEDGER_PATH=/opt/arbitrage-bot-v3/evidence/shadow/signerless/ledger.jsonl; cd /opt/arbitrage-bot-v3; exec ./scripts/golive/run_signerless_shadow.sh >>/opt/arbitrage-bot-v3/evidence/shadow/signerless/launcher.out 2>&1"
  ```
- Digest rollback: see §6.

## 8. Refresh (extend the window without a redeploy)
The rc2 shadow keeps running. The AC2 refresh in §2–§4 was done this way:
1. A single bounded read-only sentinel counted evaluated ledger blocks every 300 s until the count reached ≥200.
2. The ledger, `signerless.log` and `stage_samples.tsv` were copied read-only to a local frozen directory, and both files were cut at the last common block.
3. The same `aggregate.py` and `join.py` were run with only the input path changed.

Nothing on the host was stopped, restarted or modified.

Host paths:
- ledger `/opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/ledger.jsonl` (rotates at 64 MiB, 512 MiB total);
- log `/opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/logs/signerless.log`;
- launcher stdout `…/whi-1423/launcher.out`.

Per-pass latency beyond the 03:42:04Z–05:30:00Z window would need the sampler (`/opt/arbitrage-bot-v3-rc2-build-logs/sampler.sh`) to run again. That is a host-side write, and it was not done in this read-only refresh.

`summary.json` in this directory is the frozen **05:57:22Z** refresh aggregate (cut block 101179163). The earlier 05:31:52Z numbers are kept under `previous_freeze_…`.

**Durable evidence inputs.** Every input behind the aggregates in this file has been copied out of the transient `/tmp/whi1423/` into the orchestrator run's artifact directory:
- **Locator:** `release-022/whi1423-artifacts/` (full path: `…/subagent-artifacts/outputs/3eed43fd-5a04-4643-b5db-cf1e6d074cf5/release-022/whi1423-artifacts/`).
- **Contents:**
  - the frozen ledger, log and sampler, with the raw uncut copies;
  - the aggregation scripts and their frozen variants;
  - the refresh outputs;
  - the liveness-trigger recompute (§5);
  - the digest dry-run JSON;
  - the metrics scrapes;
  - the preflight outputs (names only).
- **Integrity:** `SHA256SUMS` covers 39 files, and its own sha256 is `7e95196cc5995c4dab85f04b07f720c7f4fb4e86a4de57726710921c1e65ecdf`. Verify with `shasum -a 256 -c SHA256SUMS`.
- **Secrets:** none. The content scan found only env **names** (marked `absent`) and RPC source env names, and `.env` was never read.

## 9. Explicit non-claims
- **No sends.** No transaction was signed or submitted; the send gate stayed closed throughout. The 2 candidates are `production_gate_blocked` / `env_unsupported`, and their modeled profit (≈0.0222 WMNT) is a model, **not** realized or realizable PnL.
- **No** pool registration, executor funding, `setVenue`, hot-executor change, `main` promotion or production release.
- **No** Lark or webhook message was sent or triggered by this work, and there was no test-send. The digest was only rendered via `--dry-run` with no network.
- The matching before/after (§4a) is an **offline** pinned replay of one quiet window. It shows the post-fix DISCOVERY sums ×1.133 larger than pre-fix on that window.
  - It is not a claim about live latency.
  - It is not a per-search cost.
  - It is not a speedup or a regression bound.
  - ×1.006 (deployed vs after-fix) is not evidence of equivalence.
  - The live host numbers (§4b) and the rc1 109-pool numbers (§4c) are context only.
- OPTIMIZE has insufficient samples: live n = 1; replay n = 2/1/1. No percentile is claimed.
- DISCOVERY per-pass percentiles cover 122 of the 207 evaluated blocks (03:42:04Z–05:30:00Z) only.
- The WHI-1408 gate pass is "necessary, not sufficient".
- The high `unapproved_route` share (98.66 %) and the 04:12–04:21Z liveness alarm are reported as observed. The alarm's triggering window mixed `unapproved_route` rejects with 18 optimizer Error searches (§5), and no cause is attributed. No policy or code change is proposed or made here.
- ≈3.2 h on a Sunday is not representative of weekday market activity. The AC2 threshold is met on the count of blocks with `cycles_evaluated > 0` (207 = 16 Full + 191 Touched). It is not a claim about the volume or profitability of that work (median 386 cycles per evaluated block; 115 blocks quoted ≥1 path).
- Recurring skip causes (Moe reserve overflow `processing_failed`, pinned-logs unavailability) predate this release and are not addressed.

## 10. Acceptance criteria
| AC | status | evidence |
|---|---|---|
| Post-1409 binary running on `arb-bot-jp` with the WHI-1410 universe; identity recorded | **met** | §1 (pid 1275022, sha f55b341b…, fingerprint 0x4c2456dd… @101165208, profile 0x3d3244e3…). Still running at 2026-09-27T12:46:20Z, with the same pid and exe sha. |
| ≥200 observed blocks with Full/Touched counts, counters and reject breakdown in a STATUS file | **met**: 5699 observed blocks, **207** with evaluated work (Full 16 + Touched 191), ≥ 200 | §2, §3, `summary.json` (frozen 05:57:22Z, cut block 101179163) |
| Before/after latency percentiles, each labelled with what the timer measures | **met via the pinned replay baseline** (PR #112, dev eb824bc). Matching arms 1bdac47 / 3cc962f / fe4a574 on one hash-pinned corpus of this window. DISCOVERY over 161 evaluated passes: p50 3.16 / 3.58 / 3.52 ms, p99 48.68 / 58.58 / 58.73 ms. Paired ×1.133 (Full ×1.196, Touched ×1.084); deployed vs after-fix ×1.006. OPTIMIZE is insufficient (n = 2/1/1). Both timers are labelled, and the limits are stated. The live n = 122 figures are kept as context. | §4 (4a matching; 4b live context; 4c rc1 context) |
| Non-claims stated | **met** | §9 |
| WHI-1409 AC-4 comment links this evidence | **orchestrator's, not yet posted**: the text is prepared in the handoff | handoff |
