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
//! Counters that G-2 / G-4 will later own (`gas_rescores`, refined eligibility) are
//! emitted honestly as zero / best-effort today rather than omitted.

use crate::service::discovery::{DiscoveredOpportunity, DiscoveryPassStats};
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
    pub fn from_discovery(
        block: u64,
        affected: usize,
        stats: &DiscoveryPassStats,
        opportunities: &[DiscoveredOpportunity],
        attempts: &[(DiscoveredOpportunity, ExecutionAttempt)],
    ) -> Self {
        let mut best_net: Option<U256> = None;
        let mut best_mixed_net: Option<U256> = None;
        let mut mixed_skipped_count = 0u64;
        // G-4 will refine eligibility; until then pure (non-cross) candidates are
        // treated as eligible and cross-protocol ones are counted as mixed skips
        // (they cannot take the armed pure-route canary slot).
        let mut eligible = 0u64;
        for opp in opportunities {
            let net = opp.candidate.net_profit;
            best_net = Some(best_net.map_or(net, |b| b.max(net)));
            if opp.is_cross_protocol {
                mixed_skipped_count += 1;
                best_mixed_net = Some(best_mixed_net.map_or(net, |b| b.max(net)));
            } else {
                eligible += 1;
            }
        }

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
            eligible,
            mixed_skipped_count,
            best_mixed_net,
            best_net,
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
