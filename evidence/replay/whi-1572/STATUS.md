# WHI-1572 paired replay: discovery gas estimator off vs on (AC10)

**Status:** measured twice on 2026-09-30. **The latency ceiling is owner-pending.**
These results support implementation acceptance only; they do not authorize rollout.
The estimator stays off by default (`--estimate-unmeasured-gas`).

| run | commit (clean tree) | binary sha256 | power | files |
|---|---|---|---|---|
| **2 (final)** | `330dd0f4865f7ad80d7cee89bed193f006da11c4`, includes the review fixes PR-F1..F5 | `c81ac946…99be` | battery | `summary.json`, `host.json`, `order.jsonl`, `run_sha256s.txt` |
| 1 | `58d6534c3b4c7395f176211b6c35302d5c937588` | `d6d8f145…7cdf` | AC | `run1-58d6534/` |

Both runs produce **identical counters on every pass**. The on/off ratios agree to
within 1%. On battery, absolute times are about 40% higher: the host moved and the
ratios held, the same as WHI-1527 §11. Tables below are run 2; run 1 is in brackets
where it helps.

## Inputs and identity

| item | identity |
|---|---|
| Corpus (external, WHI-1527) | `whi1527-artifacts/corpus/corpus.jsonl`, sha256 `46c84201…32fbe9`. Verified by the driver before every run, together with `SHA256SUMS` (`501b15c7…a775`, every listed file OK) and the profile copy `mantle_mainnet_v1.json` (`3e76b9cd…95ee`, digest `0x3d3244e3…df412`) |
| Binary | `cargo build --locked --release --example discovery_replay` at the run's commit. The same binary runs both arms |
| Estimator artifact | `config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json`: sha256 `fc6a1ca6…64d3`, keccak `0x3cc808af…34a2` (pinned) |
| Venue labels (both arms) | `data/pool_universe.csv`, sha256 `702d7a32…cae0` (= the WHI-1527 universe) |
| Host | Apple M2 Pro (Mac14,9), 10 CPUs, macOS 27.0. No build or test job ran during the timed runs |
| Durable raw runs | `~/.pi/agent/sessions/--Users-whisker-Work-src-personal-arbitrage-bots-arbitrage-bot-v3--/subagent-artifacts/outputs/whi1572-replay-artifacts/{run1-58d6534-ac,run2-330dd0f-battery}/` (per-run sha256 in `run_sha256s.txt`) |

**Method.**
- The driver is `scripts/gas_estimate/paired_replay.sh run|analyze`.
- Each arm gets one warmup, then 5 balanced repeats: even rounds run off→on, odd rounds on→off (`order.jsonl`).
- The arms differ **only** by `--estimator`. They share the corpus, profile, config and venue map.
- Per pass the harness records:
  - the existing DISCOVERY timer;
  - total `discover` wall time and thread CPU time;
  - every structured counter, including the per-reason sample fee failures;
  - the discovery reject-reason metric increments.
- Timings are per-pass medians over the 5 repeats, followed by a nearest-rank percentile.
- There is no pruning or top-K; the replay adds no scheduler.

**Checks.**
- Every counter is identical across the repeats of each arm, and across the two runs.
- Per-pass `cycles_optimized` and scope are identical across the two arms.
- The four-state search partition equals `paths_quoted` on every pass of both arms.
- The **off arm reproduces WHI-1527's rc2 counters exactly**: 155,554 cycles, 1,327 completed searches, 153,534 `unapproved_route`, 693 `optimize_error`. Measured-only discovery is unchanged.

## Coverage and work (Σ over 4955 passes, one run)

| counter | off | on |
|---|---|---|
| cycles evaluated | 155,554 | 155,554 |
| completed searches (`paths_quoted`) | 1,327 (0.85%) | **130,742 (84.0%)** |
| `unknown_route` + `unapproved_route` | 153,534 | **0** |
| `optimize_error` (Moe snapshot state; unchanged cause) | 693 | 24,812 |
| `net_profit` (materialize) | 0 | 53 |
| partition: `no_fee_requested` | 987 | 104,952 |
| partition: `estimated_used` | 0 | 25,496 |
| partition: `measured_only` | 338 | 294 |
| partition: `unresolved` | 2 | 0 |
| candidate inputs (`amm_quotes`) | 52,547 | 4,056,244 |
| AMM simulations issued | 50,866 | 3,827,688 |
| sample fee resolutions, measured / estimated | 2,311 / 0 | 2,311 / 158,603 |
| sample fee-resolution failures, by reason | 58, all `policy` (unapproved bucket) | 0 |
| materialized candidates, measured / estimated (per-pass sums, cached re-materializations included) | 2 / 0 | 2 / 12,856 |
| … of which on the 4,794 passes that evaluated no cycle | 0 | 11,983 |

With the estimator on:
- No path is rejected because its class lacks measured evidence.
- The remaining failures are state failures (`optimize_error`: held or incomplete Moe snapshots) and economic ones (`net_profit`, `no_optimum`). They are counted separately.
- No sample failed on reserve, arithmetic, invalid-estimate or policy grounds in this window.
- Every estimated candidate is send-ineligible and would be recorded gate-blocked with its gas tier, extrapolation labels and venue labels.

## Latency, run 2, ms (run 1 on AC power in brackets)

| pass group | timer | off p50 / p90 / p99 | on p50 / p90 / p99 | Σ on / Σ off |
|---|---|---|---|---|
| evaluated (161) | DISCOVERY | 3.84 / 26.9 / 61.4 | 104 / 788 / 1774 | ×28.6 [×28.5] |
| evaluated (161) | `discover` wall | 34.9 / 41.7 / 61.6 | 135 / 801 / 1775 | ×9.4 [×9.5] |
| evaluated (161) | `discover` CPU | 34.6 / 41.5 / 61.0 | 134 / 794 / 1748 | ×9.4 [×9.5] |
| Full (14) | DISCOVERY | 60.1 / 61.4 / 61.6 [42.1] | **1738 / 1774 / 1782 [1220]** | ×28.9 [×28.9] |
| Touched evaluated (147) | DISCOVERY | 3.72 / 18.4 / 30.6 | 102 / 545 / 914 | ×28.3 [×28.2] |
| Touched evaluated (147) | `discover` wall | 34.7 / 38.4 / 42.0 | 133 / 564 / 923 | ×6.2 [×6.3] |
| no cycle evaluated (4794) | `discover` wall | 32.6 / 33.2 / 34.3 | 32.4 / 33.0 / 33.7 | ×0.99 [×0.99] |
| **every call (4955)** | `discover` wall, Σ | 162.3 s [112.0 s] | 211.9 s [147.8 s] | ×1.31 [×1.32] |

Wall time outside the DISCOVERY timer is about 32 ms per call in both arms. It includes
the pre-timer loop that clears the Moe cache on clean paths and the materialization of
every cached path. The two are not timed separately, so this report does not attribute
the time between them. On the passes that evaluated no cycle, re-materializing the
11,983 cached estimated candidates adds no measurable wall time (×0.99).

## Reading

- Estimation turns about 99% pre-simulation rejection into completed searches: 0.85% → 84.0% completed-search coverage.
- The cost is about 75× more AMM simulations.
- A Full pass now takes 1.2 s of single-thread CPU on AC power (1.7 s on battery). That is **more than half of Mantle's ~2 s block time**, before RPC, tip refresh and attempt work.
- Evaluated Touched passes grow from ~35 ms to ~130 ms (p50) and ~0.9 s (p99).
- Over every call the total rises ×1.31: most calls evaluate no cycle.
- Rollout needs the owner's latency decision. The options belong to separate issues; this one adds no scheduler, pruning or top-K:
  - accept the cost;
  - bound it;
  - estimate only on Touched passes.
- The DISCOVERY timer is not a per-search or per-simulation cost. The two arms do very different amounts of work inside it.
