# WHI-1423: the post-WHI-1409 binary on `arb-bot-jp` (signerless shadow) and live discovery evidence

**Status: PARTIAL.** The deploy is complete and running. Evidence was collected over ≈2 h 48 min: **4956 observed blocks, 161 of them with evaluated work**. The AC requires **≥200** blocks with meaningful evaluated work, so that AC is only partially met (see the AC table). The shadow keeps running, so this window can be extended from the same process without a redeploy (see "Refresh").

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
- Ledger block range: **101173341 → 101178398**, with **4956** distinct observed blocks (run started 02:43:20Z; collection frozen at 05:31:52Z).
  - The `block_summary` log covers 101173342 → 101178398 in 5046 lines. These include skipped and gap blocks.
  - Skip reasons: `-` 4955, `pinned_logs_unavailable` 54, `processing_failed` 23 (Moe "Arithmetic overflow while updating Moe reserves", which rc1 also produced), `pinned_header_unavailable` 13, `duplicate` 1.

| pass scope (ledger `discovery.scope`) | observed blocks | blocks with `cycles_evaluated > 0` | cycles_evaluated (Σ cycles_optimized) | cycles_total | paths_quoted |
|---|---|---|---|---|---|
| **Full** | 14 | 14 | 77,868 | 77,868 | 1,092 |
| **Touched** | 4,941 | 147 | 77,686 | 27,481,842 | 235 |
| no `discovery` object (startup baseline row, block 101173341) | 1 | – | – | – | – |
| **total** | **4,956** | **161** | **155,554** | 27,559,710 | **1,327** |

The evaluated-work rate was **≈57 blocks/h** overall. It was uneven: 37 by 03:41Z, a burst to 101 by 04:35Z, 160 by 05:31Z. It was a Sunday, with low DEX activity. The WHI-980 empty-`eth_getLogs` canary fired 35 times, which reflects the same quiet market.

## 3. Counters and the full reject breakdown (Σ of `block_summary` over the window)
| counter | value |
|---|---|
| `cycles_evaluated` | 155,554 |
| `paths_quoted` | 1,327 |
| `amm_quotes` | 52,547 |
| `affected` (Σ) | 356 |
| `candidates` / `eligible` | 2 / 0. The candidate is the same route at blocks 101176594 and 101176595, with modeled `best_net` = 22,236,164,945,446,222 wei (≈0.0222 WMNT). `attempt_outcome="production_gate_blocked"`; ledger candidate `outcome.kind = env_unsupported`. The send gate is closed, as intended. |
| `gas_rescores` / `mixed_skipped_count` | 0 / 2 |
| `fee_resolution_failures` | 58 (samples, not paths) |

| reject reason | count | share of evaluated |
|---|---|---|
| `unknown_route` | **0** | 0 % |
| `unapproved_route` | 153,534 | 98.70 % |
| `pool_lookup` | 0 | 0 % |
| `no_optimum` | 1,326 | 0.85 % |
| `zero_profit` | 0 | 0 % |
| `other` | 693 | 0.45 % |

- Consistency check: 153,534 + 1,326 + 693 = 155,553 = cycles_evaluated − 1. The one path not rejected is the path that became the candidate.
- In the ledger, `other` covers the `optimize_error` bucket. Prometheus `arbbot_discovery_rejected_total` at 05:31Z read `unapproved_route` 159,018, `no_optimum` 1,347, `optimize_error` 750. These are a few seconds later than the log sums, and all series are cumulative.
- Compare the old rc1 process: 100 % `gas_profile` rejects (8.27 M) and zero optimizer entries over 269,622 passes. After WHI-1409, `unknown_route = 0`: the optimize and materialize route keys now agree. Paths now reach the optimizer, and one reached a successful optimum and a candidate.

**WHI-1424 coverage fields**, computed from the ledger over evaluated rows. The startup row, which has no discovery object, is excluded here; the digest counts it as a coverage gap (see §5).
- paths_evaluated = 155,554; paths_quoted = 1,327 (0.853 %).
- **Full-pass** paths_evaluated = 77,868; full_pass_paths_quoted = 1,092, which is **1.402 %** and at or above `LIMITED_EVALUATION_COVERAGE_PERCENT = 1`. So `limited_evaluation_coverage` would be **false** on Full passes. The margin over the threshold is thin.
- rejects are as in the table above; fee_resolution_failures = 58.

## 4. Latency (labelled)
**What each timer measures** (`src/service/path_index.rs` at fe4a574):
- `stage::DISCOVERY` (`arbbot_pipeline_stage_duration_seconds{stage="discovery"}`) is recorded once per discovery pass. It spans the optimize loop over the selected subset: pool lookup, fee/route checks, optimizer search, and post-optimum simulation that caches `CachedGross`.
  - It is emitted **before** cached materialization and gas re-scoring of the found set.
  - It **excludes** path-index/topology build, which is paid at build time. For example, the first Full pass after startup took ≈33 s wall-clock between `block_summary` lines, but only milliseconds on this timer.
  - It is recorded for idle passes too.
- `stage::OPTIMIZE` (`…{stage="optimize"}`) is recorded **only after a successful optimum**, per path. NoOptimum, Rejected and Error outcomes are not recorded.

**After (rc2, this run).** Per-pass, **evaluated blocks only**:
- Method: a read-only sampler scraped `127.0.0.1:9464/metrics` every ~0.27 s from 03:45Z to 05:30Z. The histogram `_sum` delta was attributed to single-pass intervals (3116 single-pass intervals, 24 multi-pass intervals discarded), and each was joined to the next `block_summary`.
- **n = 122 evaluated passes.** This covers the evaluated blocks inside the sampler window, not the ~38 evaluated blocks before 03:45Z.

| timer | n | p50 | p90 | p95 | p99 | max |
|---|---|---|---|---|---|---|
| `stage::DISCOVERY`, evaluated passes | 122 | 6.849 ms | 50.898 ms | 98.070 ms | 102.143 ms | 110.214 ms |
| `stage::DISCOVERY`, idle passes (cycles_evaluated = 0), for reference | 2994 | 0.000 ms | 0.000 ms | 0.000 ms | 0.001 ms | 0.001 ms |
| `stage::OPTIMIZE` (successful optima only) | **1** | 3.755 ms (the single sample, block 101176594) | – | – | – | – |

- Cumulative Prometheus histogram over all 4956 passes (idle included, bucket-interpolated): DISCOVERY p50 0.052 ms, p90 0.093 ms, p95 0.098 ms, p99 25.37 ms, mean 0.664 ms.
- OPTIMIZE has 1 sample, in the 2.5–5 ms bucket.

**Before (pre-WHI-1409), method (b). This baseline does NOT match the after run.**
- (a) pinned replay was **not available**:
  - neither binary has a block-pinned replay mode; the CLI has only live `--once`/`--watch` at the tip;
  - rc1 (6dad49e) predates WHI-1413. It would load the 15 Moe-V1-classic `agni-v2` rows only as generic V2 pools with the hard-coded FusionX fee;
  - rc1 would run with its own rc1 gas-profile artifacts (a different profile).

  So a replay "on a matching universe" cannot be built from these binaries, and running a second RPC-syncing bot beside the live shadow was not in scope.
- (b) The rollback binary's own metrics are used instead. They come from pid 1548362 (sha256 `2abe8919…ebee5`, v0.2.2-rc1 source), scraped at 02:40Z just before it stopped.
  - That process ran on a **109-pool universe** (snapshot 100871945, fingerprint `0xee1d40b8…`), with the rc1 profile, the same default throttle (8 rps) and the same capital cap. The run was 2026-09-20T18:06Z → 2026-09-27T02:40Z.
  - `stage::DISCOVERY`, all passes (every path rejected `gas_profile` before the optimizer, so these are effectively no-optimizer passes): n = 269,622, p50 0.054 ms, p90 0.097 ms, p95 0.617 ms, p99 10.675 ms, mean 0.451 ms.
  - `stage::OPTIMIZE`: **no samples**. The series was absent because no optimum was ever reached (0 of 269,622 passes).
- Interpretation limit: before and after are **not comparable** as a speed measurement. Different universe, different profile, and the before side did no optimizer work. The honest reading is qualitative: before, the optimizer was never entered; after, 0.85 % of evaluated paths enter it and one reached a successful optimum.

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
  - `arbbot_discovery_liveness_alarm` read 0 at 05:31Z.
  - Cause, per the fields: 10 consecutive evaluated Touched passes whose paths were all `unapproved_route`. That is the approved-class policy, not a route-key contract mismatch (`unknown_route=0`). This is an observation; no fix is claimed or attempted here.
- **Digest rendering** was checked with `lark_daily_digest --dry-run` only. The rc2 binary ran under `env -i` + `unshare -n` (no webhook, keyword or state env; no network namespace), on the new ledger, `--date 2026-09-27`, at 05:32Z.
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
The rc2 shadow keeps running. To extend the ≥200-evaluated-block evidence:
- Host paths:
  - ledger `/opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/ledger.jsonl` (rotates at 64 MiB, 512 MiB total);
  - log `/opt/arbitrage-bot-v3-rc2/evidence/shadow/whi-1423/logs/signerless.log`;
  - launcher stdout `…/whi-1423/launcher.out`.
- Re-run `python3 /opt/arbitrage-bot-v3-rc2-build-logs/aggregate.py`, which reads the ledger and log segments.
- Per-pass latency needs the sampler to run again. It stopped at 05:30Z by `timeout`: restart `/opt/arbitrage-bot-v3-rc2-build-logs/sampler.sh`, then run `join.py`.
- `summary.json` in this directory is the frozen 05:31:52Z aggregate.

## 9. Explicit non-claims
- **No sends.** No transaction was signed or submitted; the send gate stayed closed throughout. The 2 candidates are `production_gate_blocked` / `env_unsupported`, and their modeled profit (≈0.0222 WMNT) is a model, **not** realized or realizable PnL.
- **No** pool registration, executor funding, `setVenue`, hot-executor change, `main` promotion or production release.
- **No** Lark or webhook message was sent or triggered by this work, and there was no test-send. The digest was only rendered via `--dry-run` with no network.
- The latency numbers are **not** a like-for-like before/after speed comparison (§4), and no latency improvement is claimed.
- A 1-sample OPTIMIZE figure is not a distribution.
- The WHI-1408 gate pass is "necessary, not sufficient".
- The high `unapproved_route` share (98.7 %) and the 04:12–04:21Z liveness alarm are reported as observed. No policy or code change is proposed or made here.
- ≈2.8 h on a Sunday is not representative of weekday market activity, and 161 evaluated blocks do **not** satisfy the ≥200 requirement.
- Recurring skip causes (Moe reserve overflow `processing_failed`, pinned-logs unavailability) predate this release and are not addressed.

## 10. Acceptance criteria
| AC | status | evidence |
|---|---|---|
| Post-1409 binary running on `arb-bot-jp` with the WHI-1410 universe; identity recorded | **met** | §1 (pid 1275022, sha f55b341b…, fingerprint 0x4c2456dd… @101165208, profile 0x3d3244e3…) |
| ≥200 observed blocks with Full/Touched counts, counters and reject breakdown in a STATUS file | **partial**: 4956 observed blocks, but only **161** with evaluated work (< 200) at ≈57/h | §2, §3, `summary.json`. Extendable per §8 |
| Before/after latency percentiles, each labelled with what the timer measures | **partial**: after is labelled and restricted to evaluated passes (n = 122; OPTIMIZE n = 1). Before is method (b), explicitly NOT matching, because pinned replay is unavailable. | §4 |
| Non-claims stated | **met** | §9 |
| WHI-1409 AC-4 comment links this evidence | **prepared, not posted**: the orchestrator posts it | handoff |
