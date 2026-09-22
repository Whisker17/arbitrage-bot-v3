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
4. The WHI-1407 daily Lark digest rendered a healthy-looking zero without signaling that zero paths were actually priced by the optimizer.

## Fix Implemented

### 1. Prometheus Metric Split
In `src/metrics/record.rs` and `src/service/fee_scoring.rs`:
- `reject_reason::UNKNOWN_ROUTE` (`"unknown_route"`): route key absent from gas profile.
- `reject_reason::UNAPPROVED_ROUTE` (`"unapproved_route"`): route key present but unapproved by policy.
- `discovery_fee_reject_reason()` maps `RuntimeGasProfileError::UnknownRoute` → `unknown_route`, `RuntimeGasProfileError::UnapprovedRoute` → `unapproved_route`.
- The legacy merged `gas_profile` constant is removed (had zero remaining producers after the split).
- New series are zero-initialized alongside the pre-existing `no_optimum` in `emit_zero_init()`.

### 2. Structured Reject Counts in `block_summary` & Path Index
In `src/service/path_index.rs` and `src/service/block_summary.rs`:
- `DiscoveryRejectCounts { unknown_route, unapproved_route, pool_lookup, no_optimum, zero_profit, other }`.
- `DiscoveryStats` / `DiscoveryPassStats` / `BlockSummary` all carry `paths_quoted: u64` and `rejects: DiscoveryRejectCounts`.
- `block_summary` emits all six reject-reason fields plus `paths_quoted` and `liveness_alarm`, verified live below.
- Rejections recorded through the production `DiscoveryRejectCounts::record` method sum to paths considered (unit-tested against the real 100%-rejection code path, not a hand-constructed literal).

### 3. Corrected Liveness Invariant
In `src/service/path_index.rs`:
- Two independent triggers, deliberately distinct:
  - **Exhaustive**: a `Full`-scope pass that evaluates the entire universe and resolves zero paths is already conclusive proof (not a sample) — fires immediately.
  - **Sustained window**: `consecutive_dead_heads` counts *consecutive discovery passes* (not raw path/topology count) where `cycles_optimized > 0 && paths_quoted == 0`; resets the moment any pass quotes ≥1 path. Fires once the count reaches `DEFAULT_LIVENESS_DEAD_HEADS_THRESHOLD` (10), overridable via `DiscoveryConfig::liveness_dead_heads_threshold`.
- Counting passes rather than paths means the threshold no longer scales with (and is not trivially tripped by) universe size.
- Surfaced in three places: a `tracing::error!` line, the `arbbot_discovery_liveness_alarm` Prometheus gauge (`record_discovery_liveness_alarm`), and `BlockSummary.liveness_alarm` (in the `block_summary` log line).
- Tested in `service::path_index::tests::liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not` (exhaustive branch) and `liveness_alarm_fires_on_sustained_zero_quoted_window` (drives three consecutive `Touched` passes, asserting the alarm stays quiet on passes 1–2 and only fires on pass 3 once the threshold is reached).

### 4. Lark Daily Digest Visibility (WHI-1407)
In `src/notify/ledger_window.rs`, `src/notify/digest.rs`, and `src/notify/lark.rs`:
- `DiscoveryRecord` carries `paths_quoted: Option<u64>` end-to-end from the ledger.
- `OperationalActivity` computes three mutually exclusive states over the digest window:
  - `is_pipeline_dead`: cycles were evaluated but **zero** paths ever reached the optimizer.
  - `pipeline_liveness_unknown`: cycles were evaluated but **no** observation in-window carries `paths_quoted` at all (e.g. an older ledger schema) — liveness genuinely cannot be determined. This **fails closed**: the card never silently renders the healthy clean-zero text in this case.
  - Otherwise: normal quiet-market rendering.
- Card text: `"⚠ 发现管道异常：…"` for a dead pipeline, `"⚠ 无法确认发现管道是否存活：…"` for the unknown case, distinct from the healthy `"已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"`.
- Tested in `notify::lark::tests::render_card_distinguishes_zero_optimizer_path_from_quiet_market` and `render_card_says_unknown_not_healthy_when_paths_quoted_is_never_recorded`.

## Acceptance Verification

| Acceptance Criterion | Verification | Status |
| --- | --- | --- |
| `block_summary` carries per-reason reject counts; a test asserts they sum to the paths considered | `service::block_summary::tests::block_summary_reject_counts_sum_to_paths_considered` (drives the production `.record()` method over an independent path list, not a circular literal) | **PASSED** |
| `UnknownRoute` and `UnapprovedRoute` are separate metric series | `service::fee_scoring::tests::unapproved_route_bucket_fails_closed_with_unapproved_metric_label`, `unknown_route_bucket_fails_closed_with_unknown_metric_label` | **PASSED** |
| A test drives a 100%-rejection configuration and asserts the liveness alarm fires (the current invariant does not) | `service::path_index::tests::liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not` | **PASSED** |
| A digest rendered from a zero-optimizer-path window is visibly distinct from one rendered from a genuinely quiet market; both fixtures are tested | `notify::lark::tests::render_card_distinguishes_zero_optimizer_path_from_quiet_market` | **PASSED** |
| **Empirically verified, not deferred**: a ≥200-block live run with `cycles_evaluated`, `paths_quoted`, `amm_quotes` and reject-reason counts pasted into the PR or a STATUS file | See **Live Run** below | **PASSED** |
| `cargo test --locked --all-targets` passes | 1046 passed, 0 failed (1 known-flaky, unrelated `ops::file_lock` test passes in isolation — pre-existing hazard, not touched by this change) | **PASSED** |

## Live Run (≥200 blocks, real Mantle mainnet, full 130-pool / 3-protocol universe)

`cargo run --bin bot -- --protocols agni-v2,agni-v3,moe --watch --universe-max-age-blocks 3000000` against `https://rpc.mantle.xyz` (public RPC, `RPC_HTTP_THROTTLE_RPS=4`), default `ws` head source, no code changes to the bot's discovery/logging path beyond this PR.

```text
total block_summary lines observed: 267
head-observed block range:          100964765 .. 100965101 (span 337; some heads
                                     skipped under public-RPC pin-lag — WHI-762/977,
                                     unrelated to this issue)
heads with cycles_evaluated > 0:    3 (three independent Full-scope re-baselines)
liveness_alarm=true lines:          0 (correctly quiet — see below)

Aggregate sums across the window:
  cycles_evaluated_sum  = 20886
  paths_quoted_sum      = 24
  amm_quotes_sum        = 624
  unknown_route_sum     = 20382
  unapproved_route_sum  = 480
  pool_lookup_sum       = 0
  no_optimum_sum        = 24
  zero_profit_sum       = 0
  other_sum             = 0
  candidates_sum        = 0
```

Representative full `block_summary` line (one of the three non-zero-cycle heads — this
is the exact WHI-1411 evidence pattern reproduced live: thousands of cycles evaluated,
almost all rejected pre-simulation as `unknown_route`, only a handful of paths ever
reaching the optimizer):

```text
block_summary block=100964767 affected=0 cycles_evaluated=6962 paths_quoted=8
amm_quotes=208 gas_rescores=0 candidates=0 eligible=0 mixed_skipped_count=0
best_mixed_net="-" best_net="-" attempt_outcome="-" skip_reason="-"
liveness_alarm=false unknown_route=6794 unapproved_route=160 pool_lookup=0
no_optimum=8 zero_profit=0 other=0
```

**Why `liveness_alarm=false` here is correct, not a gap:** `paths_quoted=8 > 0` on
every one of the three non-zero-cycle heads — some paths *did* reach the optimizer
this pass, so this is "priced (nearly) everything and mostly rejected pre-simulation
or found nothing profitable", not "could not price anything". That is exactly the
distinction WHI-1411 asks the invariant to draw: the alarm is reserved for
`paths_quoted == 0`, which this window never produced (inducing a genuine 100%
zero-`paths_quoted` event on mainnet to "prove" the alarm would itself be an
operational risk). The **exhaustive** and **sustained-window** zero-`paths_quoted`
branches are proven deterministically offline instead, against the same production
`DiscoveryEngine::discover` code path this live run exercised — see
`liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not` and
`liveness_alarm_fires_on_sustained_zero_quoted_window` in
`src/service/path_index.rs`.

This run independently confirms the reject-reason breakdown is wired correctly in
production: of 20,886 evaluated cycles, 20,382 were rejected as `unknown_route`
(contract mismatch — the gas profile has no bucket at all for the topology) and 480
as `unapproved_route` (bucket present but policy-unsupported), leaving only 24 paths
that ever reached the optimizer (`paths_quoted_sum`) and 624 AMM quotes spent on them
(`amm_quotes_sum`) — visible per-block via `block_summary`, not collapsed into a
single `gas_profile` bucket as before this fix.
