//! One greppable structured summary line per processed (or skipped) head (WHI-952 / G-5).
//!
//! Under `RUST_LOG=info` the watch loop demotes per-stage chatter to `debug` and emits
//! **one** `target = "service.block_summary"` info event per head so operators can rebuild
//! block behaviour without grepping megabytes of TRACE. Field set is the G-5 contract:
//!
//! `block / affected / cycles_evaluated / amm_quotes / gas_rescores / candidates /
//! eligible / mixed_skipped_count / best_mixed_net / best_net / attempt_outcome /
//! skip_reason`
//!
//! `eligible` / `mixed_skipped_count` / `best_mixed_net` are owned by WHI-951 (G-4)
//! via [`crate::service::eligibility::classify_opportunities`]. `gas_rescores` stays
//! zero until G-2 wires measured gas.

use crate::service::discovery::{DiscoveredOpportunity, DiscoveryPassStats};
use crate::service::eligibility::EligibilityView;
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
    pub amm_quotes: u64,
    pub gas_rescores: u64,
    pub candidates: u64,
    pub eligible: u64,
    pub mixed_skipped_count: u64,
    pub best_mixed_net: Option<U256>,
    pub best_net: Option<U256>,
    pub attempt_outcome: Option<&'static str>,
    pub skip_reason: Option<&'static str>,
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
            amm_quotes: stats.amm_quotes,
            gas_rescores: stats.gas_rescores,
            candidates: opportunities.len() as u64,
            eligible: eligibility.eligible_count,
            mixed_skipped_count: eligibility.mixed_skipped_count,
            best_mixed_net: eligibility.best_mixed_net,
            best_net: eligibility.best_net,
            attempt_outcome,
            skip_reason: None,
        }
    }

    /// Emit the single greppable info line.
    pub fn emit(&self) {
        info!(
            target: BLOCK_SUMMARY_TARGET,
            block = self.block,
            affected = self.affected,
            cycles_evaluated = self.cycles_evaluated,
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
            amm_quotes: 200,
            gas_rescores: 0,
            candidates: 2,
            eligible: 1,
            mixed_skipped_count: 1,
            best_mixed_net: Some(U256::from(10u64)),
            best_net: Some(U256::from(20u64)),
            attempt_outcome: Some("production_gate_blocked"),
            skip_reason: None,
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
            "amm_quotes=200",
            "gas_rescores=0",
            "candidates=2",
            "eligible=1",
            "mixed_skipped_count=1",
            "best_mixed_net=",
            "best_net=",
            "attempt_outcome=",
            "skip_reason=",
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
                amm_quotes: 10,
                gas_rescores: 0,
            },
            &[],
            &[],
        );
        assert_eq!(s.cycles_evaluated, 5);
        assert_eq!(s.amm_quotes, 10);
        assert_eq!(s.candidates, 0);
        assert_eq!(s.eligible, 0);
        assert_eq!(s.mixed_skipped_count, 0);
    }
}
