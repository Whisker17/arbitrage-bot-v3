# WHI-1411 — Rejection-aware liveness: distinguishable pipeline deadness

**Issue:** [WHI-1411](https://linear.app/whisker-personal/issue/WHI-1411)

## Problem & Context

During a 41,642-block shadow run, 434,190 paths were rejected at the gas profile gate (contract mismatch). However, `block_summary` emitted:

```text
block=... affected=5 cycles_evaluated=782 amm_quotes=0 candidates=0 eligible=0 skip_reason="-"
```

The output was indistinguishable from a genuinely quiet market because:
1. `block_summary` had no breakdown of why paths were skipped/rejected.
2. The only Prometheus counter, `arbbot_discovery_rejected_total{reason="gas_profile"}`, collapsed `UnknownRoute` (contract mismatch) and `UnapprovedRoute` (policy decision) into one bucket.
3. The WHI-976 invariant (`paths_quoted > 0 && amm_quotes == 0`) was structurally unable to fire under 100% pre-simulation rejection because `paths_quoted` was 0.
4. The WHI-1407 daily Lark digest rendered a healthy-looking zero ("已记录候选 0；无套利候选；无成交") without signaling that zero paths were actually priced by the optimizer.

## Fix Implemented

### 1. Prometheus Metric Split
In `src/metrics/record.rs` and `src/service/fee_scoring.rs`:
- `reject_reason::UNKNOWN_ROUTE` (`"unknown_route"`): route key absent from gas profile.
- `reject_reason::UNAPPROVED_ROUTE` (`"unapproved_route"`): route key present but unapproved by policy.
- `discovery_fee_reject_reason()` now maps `RuntimeGasProfileError::UnknownRoute` to `unknown_route` and `RuntimeGasProfileError::UnapprovedRoute` to `unapproved_route`.

### 2. Structured Reject Counts in `block_summary` & Path Index
In `src/service/path_index.rs` and `src/service/block_summary.rs`:
- Added `DiscoveryRejectCounts` with fields: `unknown_route`, `unapproved_route`, `pool_lookup`, `no_optimum`, `zero_profit`, `other`.
- Added `paths_quoted: u64` and `rejects: DiscoveryRejectCounts` to `DiscoveryStats`, `DiscoveryPassStats`, and `BlockSummary`.
- `block_summary` emits structured fields:
  `unknown_route`, `unapproved_route`, `pool_lookup`, `no_optimum`, `zero_profit`, `other`, and `paths_quoted`.
- Rejections sum exactly to paths considered in discovery.

### 3. Corrected Liveness Invariant
In `src/service/path_index.rs`:
- Tracks `consecutive_unquoted_cycles`.
- If `paths_quoted > 0`, resets `consecutive_unquoted_cycles = 0`.
- If `cycles_optimized > 0 && paths_quoted == 0`, increments by `cycles_optimized`.
- Alarms (`tracing::error!`) on a full universe pass with 100% pre-simulation rejection (`force_full && cycles_optimized > 0 && paths_quoted == 0`) or when `consecutive_unquoted_cycles >= liveness_unquoted_cycles_threshold`.
- Tested in `service::path_index::tests::liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not` and `service::path_index::tests::liveness_alarm_fires_on_sustained_zero_quoted_window`.

### 4. Lark Daily Digest Visibility (WHI-1407)
In `src/execution/shadow/ledger.rs`, `src/notify/ledger_window.rs`, `src/notify/digest.rs`, and `src/notify/lark.rs`:
- Carries `paths_quoted` and `amm_quotes` on `LedgerDiscoveryView` and `DiscoveryRecord`.
- Pure aggregator checks whether cycles were evaluated in-window while `paths_quoted_sum == 0` (`is_pipeline_dead`).
- When `is_pipeline_dead`, the card surfaces an explicit warning:
  `"⚠ 发现管道异常：统计周期内到达优化器的路径为 0（全部在仿真前被拒绝，发现管道失效，非单纯市场安静）；已记录候选 0；无成交"`
  and displays `"优化器定价路径: 0 (⚠ 异常: 0 路径到达优化器)"`.
- Tested in `notify::lark::tests::render_card_distinguishes_zero_optimizer_path_from_quiet_market`.

## Acceptance Verification

| Acceptance Criterion | Verification | Status |
| --- | --- | --- |
| `block_summary` carries per-reason reject counts; a test asserts they sum to the paths considered | `service::block_summary::tests::block_summary_reject_counts_sum_to_paths_considered` | **PASSED** |
| `UnknownRoute` and `UnapprovedRoute` are separate metric series | `service::fee_scoring::tests::unapproved_route_bucket_fails_closed_with_unapproved_metric_label` and `unknown_route_bucket_fails_closed_with_unknown_metric_label` | **PASSED** |
| A test drives a 100%-rejection configuration and asserts the liveness alarm fires (the current invariant does not) | `service::path_index::tests::liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not` | **PASSED** |
| A digest rendered from a zero-optimizer-path window is visibly distinct from one rendered from a genuinely quiet market; both fixtures are tested | `notify::lark::tests::render_card_distinguishes_zero_optimizer_path_from_quiet_market` | **PASSED** |
| Full test suite passes | `cargo test --locked --all-targets` (1045 tests passed) | **PASSED** |

## Live / Operational Sample

Live `--watch` output sample from Mantle mainnet head processing:

```text
INFO service.block_summary: block_summary block=100960244 affected=0 cycles_evaluated=0 paths_quoted=0 amm_quotes=0 gas_rescores=0 candidates=0 eligible=0 mixed_skipped_count=0 best_mixed_net="-" best_net="-" attempt_outcome="-" skip_reason="-" unknown_route=0 unapproved_route=0 pool_lookup=0 no_optimum=0 zero_profit=0 other=0
INFO service.block_summary: block_summary block=100960245 affected=0 cycles_evaluated=0 paths_quoted=0 amm_quotes=0 gas_rescores=0 candidates=0 eligible=0 mixed_skipped_count=0 best_mixed_net="-" best_net="-" attempt_outcome="-" skip_reason="-" unknown_route=0 unapproved_route=0 pool_lookup=0 no_optimum=0 zero_profit=0 other=0
```
