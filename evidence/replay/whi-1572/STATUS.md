# WHI-1572 paired replay: discovery gas estimator off vs on (AC10)

**Status:** measured on 2026-09-30 (04:34–05:01Z). **The latency ceiling is
owner-pending.** These results support implementation acceptance only. They do not
authorize rollout, and the estimator stays off by default (`--estimate-unmeasured-gas`).

## Inputs and identity

| item | identity |
|---|---|
| Corpus (external, WHI-1527) | `whi1527-artifacts/corpus/corpus.jsonl`, sha256 `46c84201…32fbe9`. Verified before use, together with `SHA256SUMS` (`501b15c7…a775`, every listed file OK) and the profile copy `mantle_mainnet_v1.json` (`3e76b9cd…95ee`, digest `0x3d3244e3…df412`) |
| Commit | `58d6534c3b4c7395f176211b6c35302d5c937588`, clean tree |
| Binary | `cargo build --locked --release --example discovery_replay`, sha256 `d6d8f145…7cdf`. The same binary runs both arms |
| Estimator artifact | `config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json`: sha256 `fc6a1ca6…64d3`, keccak `0x3cc808af…34a2` (pinned) |
| Venue labels (both arms) | `data/pool_universe.csv`, sha256 `702d7a32…cae0` (= the WHI-1527 universe) |
| Host | Apple M2 Pro (Mac14,9), 10 CPUs, macOS 27.0, AC power. No build or test job ran during the timed runs |
| Durable raw runs | `~/.pi/agent/sessions/--Users-whisker-Work-src-personal-arbitrage-bots-arbitrage-bot-v3--/subagent-artifacts/outputs/whi1572-replay-artifacts/` (per-run sha256 in `run_sha256s.txt`) |

**Method.**
- The driver is `scripts/gas_estimate/paired_replay.sh run|analyze`.
- Each arm gets one warmup, then 5 balanced repeats: even rounds run off→on, odd rounds on→off (`order.jsonl`).
- The arms differ **only** by `--estimator`. They share the corpus, profile, config and venue map.
- Per pass the harness records:
  - the existing DISCOVERY timer;
  - total `discover` wall time and thread CPU time;
  - every structured counter, and the discovery reject reasons.
- Timings are per-pass medians over the 5 repeats, followed by a nearest-rank percentile, restricted to the 161 evaluated passes.
- There is no pruning or top-K; the replay adds no scheduler.

**Checks.**
- Every counter is identical across the repeats of each arm.
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
| sample fee-resolution failures | 58 | 0 |
| materialized candidates, measured / estimated (per-pass sums, cached re-materializations included) | 2 / 0 | 2 / 12,856 |

With the estimator on:
- No path is rejected because its class lacks measured evidence.
- The remaining failures are state failures (`optimize_error`: held or incomplete Moe snapshots) and economic ones. They are counted separately.
- Every estimated candidate is send-ineligible and would be recorded gate-blocked with its gas tier, extrapolation labels and venue labels.

## Latency (evaluated passes, ms)

| scope | timer | off p50 / p90 / p99 | on p50 / p90 / p99 | Σ on / Σ off |
|---|---|---|---|---|
| all (161) | DISCOVERY | 2.70 / 19.1 / 43.9 | 73.6 / 545 / 1257 | ×28.5 |
| all (161) | `discover` wall | 23.9 / 29.8 / 44.0 | 96.2 / 554 / 1258 | ×9.5 |
| all (161) | `discover` CPU | 23.9 / 29.7 / 44.0 | 95.7 / 554 / 1228 | ×9.5 |
| Full (14) | DISCOVERY | 42.1 / 43.9 / 44.0 | **1220 / 1257 / 1275** | ×28.9 |
| Touched (147) | DISCOVERY | 2.61 / 12.7 / 22.2 | 71.2 / 377 / 647 | ×28.2 |
| Touched (147) | `discover` wall | 23.8 / 26.8 / 29.9 | 93.5 / 391 / 653 | ×6.3 |

In the off arm, the gap between wall time and DISCOVERY on Touched passes (~21 ms) is
cached materialization. It runs outside the DISCOVERY timer.

## Reading

- Estimation turns about 99% pre-simulation rejection into completed searches: 0.85% → 84.0% completed-search coverage.
- The cost is about 75× more AMM simulations.
- A Full pass now takes about 1.2 s of single-thread CPU. That is **more than half of Mantle's ~2 s block time**, before RPC, tip refresh and attempt work.
- Rollout needs the owner's latency decision. The options belong to separate issues; this one adds no scheduler, pruning or top-K:
  - accept the cost;
  - bound it;
  - enable the estimator only on Touched passes.
- The DISCOVERY timer is not a per-search or per-simulation cost. The two arms do very different amounts of work inside it.
