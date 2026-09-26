//! One greppable structured summary line per processed (or skipped) head (WHI-952 / G-5).
//!
//! Under `RUST_LOG=info` the watch loop demotes per-stage chatter to `debug` and emits
//! **one** `target = "service.block_summary"` info event per head so operators can rebuild
//! block behaviour without grepping megabytes of TRACE. Field set is the G-5 contract
//! (extended by WHI-1411 with `paths_quoted`, `liveness_alarm`, and a per-reason reject
//! breakdown, and by WHI-1424 with `fee_resolution_failures`):
//!
//! `block / affected / cycles_evaluated / paths_quoted / amm_quotes / gas_rescores /
//! candidates / eligible / mixed_skipped_count / best_mixed_net / best_net /
//! attempt_outcome / skip_reason / liveness_alarm / unknown_route / unapproved_route /
//! pool_lookup / no_optimum / zero_profit / other / fee_resolution_failures`
//!
//! `eligible` / `mixed_skipped_count` / `best_mixed_net` are owned by WHI-951 (G-4)
//! via [`crate::service::eligibility::classify_opportunities`]. `gas_rescores`
//! counts cached gross quotes re-screened when fee factors change (WHI-949).
//! `paths_quoted` distinguishes paths that reached the optimizer from paths rejected
//! before simulation; `liveness_alarm` and the six reject-reason fields make a dead
//! discovery pipeline distinguishable from a genuinely quiet market (WHI-1411).
//!
//! Counter semantics (WHI-1424; authoritative docs on
//! [`crate::service::path_index::DiscoveryStats`]): `paths_quoted` is optimizer-entry
//! coverage (search completed, `Ok` / `NoOptimum`), **not** fee-pricing coverage;
//! `amm_quotes` counts candidate inputs evaluated on every outcome that ran a search,
//! including `optimize_error`; `fee_resolution_failures` counts profitable **samples**
//! whose real route could not be fee-priced — it is not a path count and is never part
//! of the six reject fields, whose sum is the paths evaluated.

use crate::service::discovery::{DiscoveredOpportunity, DiscoveryPassStats};
use crate::service::eligibility::EligibilityView;
use crate::service::path_index::DiscoveryRejectCounts;
use crate::service::protocol::ExecutionAttempt;
use alloy::primitives::U256;
use tracing::info;

/// Greppable log target for the per-block summary line.
pub const BLOCK_SUMMARY_TARGET: &str = "service.block_summary";

/// Message body — stable string so `rg block_summary` hits every head.
pub const BLOCK_SUMMARY_MESSAGE: &str = "block_summary";

/// Soft upper bound on **info**-level lines the watch path may emit per head under
/// `RUST_LOG=info` once stage chatter is demoted (WHI-952 acceptance).
///
/// Counts: the block_summary itself (1) plus at most one skip/warn companion on the
/// failure path. Successful heads should be exactly 1 info line from this module +
/// demoted stages.
pub const MAX_INFO_LINES_PER_BLOCK: usize = 3;

/// Structured counters for one head.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockSummary {
    pub block: u64,
    pub affected: u64,
    pub cycles_evaluated: u64,
    pub paths_quoted: u64,
    pub amm_quotes: u64,
    pub gas_rescores: u64,
    pub candidates: u64,
    pub eligible: u64,
    pub mixed_skipped_count: u64,
    pub best_mixed_net: Option<U256>,
    pub best_net: Option<U256>,
    pub attempt_outcome: Option<&'static str>,
    pub skip_reason: Option<&'static str>,
    pub rejects: DiscoveryRejectCounts,
    /// Sample-level fee-resolution failures (WHI-1424) — samples, not paths;
    /// never part of `rejects`.
    pub fee_resolution_failures: u64,
    /// True when the WHI-1411 liveness invariant detected a dead discovery pipeline this pass.
    pub liveness_alarm: bool,
}

impl BlockSummary {
    /// Skipped head: no discovery ran.
    ///
    /// `reason` is the stable metric label (e.g. [`crate::service::BlockSkipReason::as_metric_label`]).
    pub fn skipped(block: u64, reason: &'static str) -> Self {
        Self {
            block,
            skip_reason: Some(reason),
            ..Self::default()
        }
    }

    /// Successful (or rebaselined) head after discovery + optional attempt.
    ///
    /// Prefer [`Self::from_eligibility`] when a WHI-951 classification is available;
    /// this fallback applies pure-vs-mixed only (no cap / profile filters).
    pub fn from_discovery(
        block: u64,
        affected: usize,
        stats: &DiscoveryPassStats,
        opportunities: &[DiscoveredOpportunity],
        attempts: &[(DiscoveredOpportunity, ExecutionAttempt)],
    ) -> Self {
        let view = crate::service::eligibility::classify_opportunities(
            opportunities,
            &crate::service::eligibility::EligibilityBounds::unrestricted(),
            |_| true,
        );
        Self::from_eligibility(block, affected, stats, opportunities, &view, attempts)
    }

    /// Summary from a precomputed WHI-951 eligibility view (caps + profile + mix).
    pub fn from_eligibility(
        block: u64,
        affected: usize,
        stats: &DiscoveryPassStats,
        opportunities: &[DiscoveredOpportunity],
        eligibility: &EligibilityView,
        attempts: &[(DiscoveredOpportunity, ExecutionAttempt)],
    ) -> Self {
        let attempt_outcome = attempts.first().map(|(_, a)| match a {
            ExecutionAttempt::Submitted(_) => "submitted",
            ExecutionAttempt::ProductionGateBlocked { .. } => "production_gate_blocked",
        });

        Self {
            block,
            affected: affected as u64,
            cycles_evaluated: stats.cycles_evaluated,
            paths_quoted: stats.paths_quoted,
            amm_quotes: stats.amm_quotes,
            gas_rescores: stats.gas_rescores,
            candidates: opportunities.len() as u64,
            eligible: eligibility.eligible_count,
            mixed_skipped_count: eligibility.mixed_skipped_count,
            best_mixed_net: eligibility.best_mixed_net,
            best_net: eligibility.best_net,
            attempt_outcome,
            skip_reason: None,
            rejects: stats.rejects,
            fee_resolution_failures: stats.fee_resolution_failures,
            liveness_alarm: stats.liveness_alarm,
        }
    }

    /// Emit the single greppable info line.
    pub fn emit(&self) {
        info!(
            target: BLOCK_SUMMARY_TARGET,
            block = self.block,
            affected = self.affected,
            cycles_evaluated = self.cycles_evaluated,
            paths_quoted = self.paths_quoted,
            amm_quotes = self.amm_quotes,
            gas_rescores = self.gas_rescores,
            candidates = self.candidates,
            eligible = self.eligible,
            mixed_skipped_count = self.mixed_skipped_count,
            best_mixed_net = self
                .best_mixed_net
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            best_net = self
                .best_net
                .map(|v| v.to_string())
                .unwrap_or_else(|| "-".into()),
            attempt_outcome = self.attempt_outcome.unwrap_or("-"),
            skip_reason = self.skip_reason.unwrap_or("-"),
            liveness_alarm = self.liveness_alarm,
            unknown_route = self.rejects.unknown_route,
            unapproved_route = self.rejects.unapproved_route,
            pool_lookup = self.rejects.pool_lookup,
            no_optimum = self.rejects.no_optimum,
            zero_profit = self.rejects.zero_profit,
            other = self.rejects.other,
            fee_resolution_failures = self.fee_resolution_failures,
            "{BLOCK_SUMMARY_MESSAGE}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::discovery::DiscoveryPassStats;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn summary_line_is_greppable_and_carries_required_fields() {
        let buf = BufferWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let summary = BlockSummary {
            block: 42,
            affected: 3,
            cycles_evaluated: 100,
            paths_quoted: 50,
            amm_quotes: 200,
            gas_rescores: 0,
            candidates: 2,
            eligible: 1,
            mixed_skipped_count: 1,
            best_mixed_net: Some(U256::from(10u64)),
            best_net: Some(U256::from(20u64)),
            attempt_outcome: Some("production_gate_blocked"),
            skip_reason: None,
            rejects: DiscoveryRejectCounts {
                unknown_route: 20,
                unapproved_route: 10,
                pool_lookup: 5,
                no_optimum: 40,
                zero_profit: 20,
                other: 3,
            },
            fee_resolution_failures: 7,
            liveness_alarm: false,
        };
        summary.emit();

        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(
            text.contains(BLOCK_SUMMARY_MESSAGE),
            "summary must be greppable by message; got: {text}"
        );
        // tracing-subscriber quotes &str fields; match on key presence + value tokens.
        for key in [
            "block=42",
            "affected=3",
            "cycles_evaluated=100",
            "paths_quoted=50",
            "amm_quotes=200",
            "gas_rescores=0",
            "candidates=2",
            "eligible=1",
            "mixed_skipped_count=1",
            "best_mixed_net=",
            "best_net=",
            "attempt_outcome=",
            "skip_reason=",
            "liveness_alarm=false",
            "unknown_route=20",
            "unapproved_route=10",
            "pool_lookup=5",
            "no_optimum=40",
            "zero_profit=20",
            "other=3",
            "fee_resolution_failures=7",
        ] {
            assert!(
                text.contains(key),
                "missing field fragment {key:?} in: {text}"
            );
        }
        assert!(
            text.contains("production_gate_blocked"),
            "attempt_outcome value missing in: {text}"
        );
    }

    /// WHI-952 acceptance: `process_observed_head` must not emit `info!` —
    /// stage chatter is `debug`, and the only per-head info line is
    /// `BlockSummary::emit` (plus at most warn companions on the skip path).
    #[test]
    fn process_observed_head_source_has_no_info_macro() {
        let src = include_str!("block_loop.rs");
        let start = src
            .find("pub async fn process_observed_head")
            .expect("process_observed_head present");
        // Next top-level async fn after process_observed_head.
        let rest = &src[start..];
        let end = rest
            .find("\nasync fn fetch_logs_for_head")
            .or_else(|| rest.find("\npub async fn fetch_logs_for_head"))
            .unwrap_or(rest.len());
        let body = &rest[..end];
        assert!(
            !body.contains("info!("),
            "process_observed_head must not emit info! under RUST_LOG=info \
             (WHI-952 per-block bound); stage logs are debug, summary is separate. \
             Found info! in body snippet."
        );
        assert!(
            body.contains("BlockSummary::"),
            "process_observed_head must emit BlockSummary for greppable contract"
        );
        assert!(
            MAX_INFO_LINES_PER_BLOCK >= 1 && MAX_INFO_LINES_PER_BLOCK <= 5,
            "MAX_INFO_LINES_PER_BLOCK={MAX_INFO_LINES_PER_BLOCK} out of sane range"
        );
    }

    /// WHI-952 acceptance: under `RUST_LOG=info`, a successful head's summary
    /// emission is a single greppable info line (stage chatter is demoted
    /// elsewhere; this asserts the summary itself does not fan out).
    #[test]
    fn info_level_summary_emit_is_one_line_under_upper_bound() {
        let buf = BufferWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        BlockSummary {
            block: 99,
            affected: 1,
            cycles_evaluated: 10,
            amm_quotes: 16,
            ..BlockSummary::default()
        }
        .emit();
        // A skip path may also emit a warn companion elsewhere; the summary
        // contract alone must stay within the declared bound.
        BlockSummary::skipped(100, "duplicate").emit();

        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let info_lines = text
            .lines()
            .filter(|l| l.contains("INFO") && l.contains(BLOCK_SUMMARY_MESSAGE))
            .count();
        assert_eq!(info_lines, 2, "two heads → two summary lines; got:\n{text}");
        assert!(
            info_lines <= MAX_INFO_LINES_PER_BLOCK * 2,
            "summary info volume {info_lines} exceeds bound {} per head × 2",
            MAX_INFO_LINES_PER_BLOCK
        );
        // Per-head: exactly one summary info line.
        assert_eq!(
            text.lines()
                .filter(|l| l.contains("block=99") && l.contains(BLOCK_SUMMARY_MESSAGE))
                .count(),
            1
        );
    }

    #[test]
    fn skipped_summary_records_reason_and_zero_work() {
        let s = BlockSummary::skipped(7, "pinned_logs_unavailable");
        assert_eq!(s.block, 7);
        assert_eq!(s.skip_reason, Some("pinned_logs_unavailable"));
        assert_eq!(s.cycles_evaluated, 0);
        assert_eq!(s.candidates, 0);
    }

    #[test]
    fn from_discovery_counts_mixed_and_eligible() {
        // Empty opportunities → zeros.
        let s = BlockSummary::from_discovery(
            1,
            0,
            &DiscoveryPassStats {
                cycles_evaluated: 5,
                paths_quoted: 5,
                amm_quotes: 10,
                gas_rescores: 0,
                rejects: DiscoveryRejectCounts::default(),
                fee_resolution_failures: 0,
                liveness_alarm: false,
            },
            &[],
            &[],
        );
        assert_eq!(s.cycles_evaluated, 5);
        assert_eq!(s.paths_quoted, 5);
        assert_eq!(s.amm_quotes, 10);
        assert_eq!(s.candidates, 0);
        assert_eq!(s.eligible, 0);
        assert_eq!(s.mixed_skipped_count, 0);
    }

    /// WHI-1411 acceptance: block_summary carries per-reason reject counts;
    /// a test asserts they sum to the paths considered.
    ///
    /// This exercises the production `DiscoveryRejectCounts::record` bucketing logic
    /// directly at the `block_summary` field-wiring layer. The complementary proof that
    /// the invariant holds end-to-end through the real `DiscoveryEngine::discover` code
    /// path (not just this struct's plumbing) lives in
    /// `service::path_index::tests::liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not`,
    /// which asserts `stats.rejects.total() == stats.cycles_optimized` from a real
    /// 100%-pre-simulation-rejection discovery pass.
    #[test]
    fn block_summary_reject_counts_sum_to_paths_considered() {
        // Build `rejects` by driving the production `DiscoveryRejectCounts::record`
        // method over a list of reasons (as `discover()` does per-path), rather than
        // writing the field literals directly — the paths-considered count below
        // (100) is an independent constant, not derived from the struct under test,
        // so this actually exercises the recording logic instead of restating it.
        let reasons: Vec<&str> = [
            (crate::metrics::reject_reason::UNKNOWN_ROUTE, 12),
            (crate::metrics::reject_reason::UNAPPROVED_ROUTE, 8),
            (crate::metrics::reject_reason::POOL_LOOKUP, 4),
            (crate::metrics::reject_reason::NO_OPTIMUM, 50),
            (crate::metrics::reject_reason::ZERO_PROFIT, 20),
            ("some_other_reason_not_individually_bucketed", 6),
        ]
        .iter()
        .flat_map(|(reason, count)| std::iter::repeat(*reason).take(*count))
        .collect();

        const PATHS_CONSIDERED: u64 = 100;
        assert_eq!(reasons.len() as u64, PATHS_CONSIDERED, "fixture reason list must match the paths-considered constant");

        let mut rejects = DiscoveryRejectCounts::default();
        for reason in &reasons {
            rejects.record(reason);
        }
        assert_eq!(rejects.unknown_route, 12);
        assert_eq!(rejects.unapproved_route, 8);
        assert_eq!(rejects.pool_lookup, 4);
        assert_eq!(rejects.no_optimum, 50);
        assert_eq!(rejects.zero_profit, 20);
        assert_eq!(rejects.other, 6);

        let summary = BlockSummary {
            block: 500,
            affected: 5,
            cycles_evaluated: PATHS_CONSIDERED,
            paths_quoted: 50,
            amm_quotes: 150,
            gas_rescores: 0,
            candidates: 0,
            eligible: 0,
            mixed_skipped_count: 0,
            best_mixed_net: None,
            best_net: None,
            attempt_outcome: None,
            skip_reason: None,
            rejects,
            // WHI-1424: sample-level, deliberately outside the conservation sum.
            fee_resolution_failures: 999,
            liveness_alarm: false,
        };

        let sum = summary.rejects.unknown_route
            + summary.rejects.unapproved_route
            + summary.rejects.pool_lookup
            + summary.rejects.no_optimum
            + summary.rejects.zero_profit
            + summary.rejects.other;
        assert_eq!(sum, summary.cycles_evaluated);
        assert_eq!(summary.rejects.total(), summary.cycles_evaluated);
    }
}
