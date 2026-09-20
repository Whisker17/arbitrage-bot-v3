//! Pure aggregation (no I/O) of a [`LedgerWindowRead`] into a [`DigestAggregate`] for
//! one UTC calendar day (WHI-1407 item 2).
//!
//! Every field here is derived only from what the ledger actually recorded — this
//! module's entire reason to exist is the "ledger reality check" in the issue's
//! Context section: it must never fabricate a metric the ledger cannot currently
//! answer (actual skip rate, a confident restart count from raw header counting, a
//! completed-trade count). Where the ledger genuinely cannot answer a question, the
//! corresponding field is `None`/an explicit `N/A` variant, not a zero.

use std::collections::HashMap;

use crate::notify::ledger_window::{
    CandidateOutcomeKind, CandidateRecord, ContextRecord, LedgerRunIdentity, LedgerWindowRead,
    ObservationRecord,
};
use crate::notify::utc_date::UtcDay;

/// How many per-candidate detail lines the card shows before folding the rest into a
/// "+N more" remainder (issue item 3.6: "bounded per-candidate lines").
pub const MAX_CANDIDATE_DETAIL_LINES: usize = 5;

/// How stale the last observation in a *live* (today-containing) window may be before
/// the digest calls it `Stale` rather than `Fresh`. Chosen generously relative to
/// Mantle's block time (a couple of seconds) so ordinary RPC jitter never flaps this
/// flag; a genuinely stopped bot will still cross it within one 00:10 UTC run.
pub const FRESHNESS_STALE_THRESHOLD_SECS: u64 = 15 * 60;

/// `[since_unix, until_unix)` for one UTC calendar day, plus the day itself for
/// display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestWindow {
    pub day: UtcDay,
    pub since_unix: u64,
    pub until_unix: u64,
}

impl DigestWindow {
    pub fn for_day(day: UtcDay) -> Self {
        let (since_unix, until_unix) = day.bounds_unix();
        Self {
            day,
            since_unix,
            until_unix,
        }
    }

    fn contains(&self, unix_secs: u64) -> bool {
        unix_secs >= self.since_unix && unix_secs < self.until_unix
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedPoint {
    pub recorded_at_unix: u64,
    pub block_number: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// This window does not contain `generated_at_unix` — it is a historical/backfill
    /// day, so "freshness relative to now" does not apply.
    NotApplicableHistorical,
    /// Window contains `generated_at_unix` but has zero observations to assess.
    NoObservations,
    Fresh {
        gap_secs: u64,
    },
    Stale {
        gap_secs: u64,
    },
}

/// Whether the requested window's data actually survived the ledger's size-based
/// rotation/retention — the issue's explicit reminder that "the 512MiB cap does not
/// itself guarantee a full day's retention".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionStatus {
    /// The window is inside the range this ledger currently retains any row for.
    Covered,
    /// Some of the window is covered, but the earliest retained row is *after* the
    /// window's start — the early part of the day has rotated out.
    PartiallyRetained { earliest_retained_unix: u64 },
    /// The window does not intersect the ledger's retained range at all (rotated out
    /// entirely, or the window predates/postdates every retained row).
    OutsideRetention {
        earliest_retained_unix: u64,
        latest_retained_unix: u64,
    },
    /// The ledger has never recorded a single row of any kind (a fresh ledger) —
    /// distinct from "rotated away": there is nothing to have lost yet.
    EmptyLedger,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataHealth {
    pub observation_count: u64,
    pub distinct_block_heights: u64,
    pub first_observed: Option<ObservedPoint>,
    pub last_observed: Option<ObservedPoint>,
    pub freshness: Freshness,
    pub retention: RetentionStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CycleEvaluationCoverage {
    pub cycles_optimized_sum: u64,
    pub cycles_total_sum: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationalActivity {
    /// `D` — observations in-window whose discovery snapshot has ≥1 dirty pool.
    pub dirty_pool_blocks: u64,
    /// `K` — observations in-window carrying a discovery snapshot at all.
    pub discovery_present_count: u64,
    /// `observation_count - K`, tracked separately per the issue's Context note.
    pub missing_discovery_count: u64,
    /// `None` when the denominator is zero or no row carries both fields (issue:
    /// "N/A on zero denominator or all-missing").
    pub cycle_evaluation_coverage: Option<CycleEvaluationCoverage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GapProxy {
    pub unobserved_heights: u64,
    pub span: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuity {
    pub min_height: Option<u64>,
    pub max_height: Option<u64>,
    pub distinct_heights_in_window: u64,
    /// `unobserved heights within [min,max] observed / (max-min+1)` — an explicitly
    /// labeled *proxy*, never the actual skip rate (the ledger cannot record that;
    /// see the issue's Context note). `None` when fewer than 2 distinct heights are
    /// observed (no span to measure).
    pub gap_proxy: Option<GapProxy>,
    /// Distinct run identities (`run_id`) whose header's `started_at_unix` falls
    /// inside this window — i.e. how many times the process was observed to *start*
    /// today, not a raw `run_header` row count (segment rotation re-emits the same
    /// run's header on every new segment — see the issue's Context note). Always
    /// carries an explicit uncertainty caveat in the card: retained-history gaps or
    /// clock skew could still under/over-count relative to the true restart history.
    pub run_starts_in_window: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateJoinStatus {
    /// Candidate and context rows are both in-window and share a digest.
    Matched,
    /// The candidate is in-window but its matching context row's `block_timestamp`
    /// places it outside the window (or vice versa) — the issue's explicit example
    /// ("a candidate recorded just after UTC midnight for a block just before it").
    BoundaryMismatch,
    /// No context row exists anywhere in the ledger for this candidate, and this
    /// candidate's outcome is one that should have one (`Pass`/`Revert`/`RpcError`/
    /// `EnvUnsupported` — `call()` always records context before any of those).
    MissingContext,
    /// `SkippedApproved` / `SampledOut` candidates never reach `call()`, so having no
    /// context row is expected, not a gap.
    ExpectedNoContext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDetail {
    pub digest: String,
    pub outcome_label: &'static str,
    pub opportunity_id: Option<String>,
    pub route_pool_count: Option<usize>,
    /// Signed net profit in wei, as a decimal string (`-` prefix for negative).
    /// `None` when no context is available or the recorded value could not be
    /// parsed (surfaced instead in `malformed_net_profit_count`).
    pub net_profit_wei: Option<String>,
    pub join_status: CandidateJoinStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BestNetProfit {
    pub digest: String,
    pub opportunity_id: Option<String>,
    pub net_profit_wei: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArbitrageSummary {
    pub candidate_count: u64,
    /// Ordered `(outcome_label, count)` — deterministic display order
    /// (pass/revert/rpc_error/env_unsupported/skipped_approved/sampled_out).
    pub outcome_counts: Vec<(&'static str, u64)>,
    /// `None` ⇒ "N/A — 无候选" (never a fabricated `0`).
    pub best_net_profit: Option<BestNetProfit>,
    /// Bounded to [`MAX_CANDIDATE_DETAIL_LINES`]; `candidates_detail_remainder` holds
    /// the count folded into "+N more".
    pub candidates_detail: Vec<CandidateDetail>,
    pub candidates_detail_remainder: u64,
    pub boundary_mismatch_count: u64,
    pub missing_context_count: u64,
    /// Context rows in-window with no matching candidate anywhere in the ledger — a
    /// genuine ledger inconsistency (e.g. a crash between `record_context` and the
    /// candidate row's own write), surfaced rather than silently dropped.
    pub orphan_context_count: u64,
    pub malformed_net_profit_count: u64,
}

/// Footer identity, best-effort from the run header active at (or nearest) the
/// window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunIdentitySummary {
    pub service: Option<String>,
    pub chain_id: Option<u64>,
    pub git_commit: Option<String>,
    pub executor_contract: Option<String>,
    /// `true` when this identity came from a run whose activity actually falls
    /// inside the window; `false` when it is the ledger's last-known identity
    /// carried forward for display only (e.g. an empty window).
    pub from_window: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestAggregate {
    pub window: DigestWindow,
    pub generated_at_unix: u64,
    pub data_health: DataHealth,
    pub operational_activity: OperationalActivity,
    pub continuity: Continuity,
    pub arbitrage: ArbitrageSummary,
    pub run_identity: RunIdentitySummary,
    /// Soft diagnostics carried through from the reader (e.g. a deferred incomplete
    /// trailing line) — never a reason to fail the digest, always surfaced.
    pub data_quality_notes: Vec<String>,
}

fn outcome_order() -> [CandidateOutcomeKind; 6] {
    [
        CandidateOutcomeKind::Pass,
        CandidateOutcomeKind::Revert,
        CandidateOutcomeKind::RpcError,
        CandidateOutcomeKind::EnvUnsupported,
        CandidateOutcomeKind::SkippedApproved,
        CandidateOutcomeKind::SampledOut,
    ]
}

fn expects_context(outcome: CandidateOutcomeKind) -> bool {
    !matches!(
        outcome,
        CandidateOutcomeKind::SkippedApproved | CandidateOutcomeKind::SampledOut
    )
}

/// Parses a ledger `net_profit` decimal string into a signed `i128` (wei-scale
/// magnitudes fit comfortably; a genuinely unparseable value is reported via
/// `malformed_net_profit_count` rather than panicking or silently becoming zero).
fn parse_net_profit(value: &str) -> Option<i128> {
    value.parse::<i128>().ok()
}

fn format_signed_wei(value: i128) -> String {
    value.to_string()
}

/// Deduplicates consecutive same-`run_id` header rows into distinct "episodes" —
/// segment rotation re-emits the *same* run's header (same `run_id`, same
/// `started_at_unix`) into every new segment; only a change in `run_id` is a real
/// observed transition.
fn run_episodes(headers: &[LedgerRunIdentity]) -> Vec<&LedgerRunIdentity> {
    let mut out: Vec<&LedgerRunIdentity> = Vec::new();
    for header in headers {
        if out.last().is_none_or(|last| last.run_id != header.run_id) {
            out.push(header);
        }
    }
    out
}

/// Aggregates `read` into a [`DigestAggregate`] for `window`, pure function of its
/// inputs (`generated_at_unix` is an explicit parameter, not a live clock read, so
/// this stays deterministic and I/O-free).
pub fn aggregate_digest(
    read: &LedgerWindowRead,
    window: DigestWindow,
    generated_at_unix: u64,
) -> DigestAggregate {
    let observations_in_window: Vec<&ObservationRecord> = read
        .observations
        .iter()
        .filter(|o| window.contains(o.recorded_at_unix))
        .collect();

    let data_health = build_data_health(read, &window, &observations_in_window, generated_at_unix);
    let operational_activity = build_operational_activity(&observations_in_window);
    let continuity = build_continuity(read, &window, &observations_in_window);
    let arbitrage = build_arbitrage_summary(read, &window);
    let run_identity = build_run_identity(read, &window);

    let mut data_quality_notes = Vec::new();
    if let Some(tail) = &read.deferred_incomplete_tail {
        data_quality_notes.push(tail.clone());
    }

    DigestAggregate {
        window,
        generated_at_unix,
        data_health,
        operational_activity,
        continuity,
        arbitrage,
        run_identity,
        data_quality_notes,
    }
}

fn overall_timestamp_range(read: &LedgerWindowRead) -> Option<(u64, u64)> {
    let mut min = u64::MAX;
    let mut max = 0u64;
    let mut any = false;
    for o in &read.observations {
        any = true;
        min = min.min(o.recorded_at_unix);
        max = max.max(o.recorded_at_unix);
    }
    for c in &read.candidates {
        any = true;
        min = min.min(c.recorded_at_unix);
        max = max.max(c.recorded_at_unix);
    }
    for c in &read.contexts {
        any = true;
        min = min.min(c.block_timestamp);
        max = max.max(c.block_timestamp);
    }
    for h in &read.run_headers {
        any = true;
        min = min.min(h.started_at_unix);
        max = max.max(h.started_at_unix);
    }
    any.then_some((min, max))
}

fn build_data_health(
    read: &LedgerWindowRead,
    window: &DigestWindow,
    observations_in_window: &[&ObservationRecord],
    generated_at_unix: u64,
) -> DataHealth {
    let observation_count = observations_in_window.len() as u64;
    let distinct_block_heights = observations_in_window
        .iter()
        .map(|o| o.block_number)
        .collect::<std::collections::BTreeSet<_>>()
        .len() as u64;

    let first_observed = observations_in_window
        .iter()
        .min_by_key(|o| o.recorded_at_unix)
        .map(|o| ObservedPoint {
            recorded_at_unix: o.recorded_at_unix,
            block_number: o.block_number,
        });
    let last_observed = observations_in_window
        .iter()
        .max_by_key(|o| o.recorded_at_unix)
        .map(|o| ObservedPoint {
            recorded_at_unix: o.recorded_at_unix,
            block_number: o.block_number,
        });

    let freshness = if !window.contains(generated_at_unix) {
        Freshness::NotApplicableHistorical
    } else {
        match last_observed {
            None => Freshness::NoObservations,
            Some(point) => {
                let gap_secs = generated_at_unix.saturating_sub(point.recorded_at_unix);
                if gap_secs <= FRESHNESS_STALE_THRESHOLD_SECS {
                    Freshness::Fresh { gap_secs }
                } else {
                    Freshness::Stale { gap_secs }
                }
            }
        }
    };

    let retention = match overall_timestamp_range(read) {
        None => RetentionStatus::EmptyLedger,
        Some((min, max)) => {
            if window.since_unix > max || window.until_unix <= min {
                RetentionStatus::OutsideRetention {
                    earliest_retained_unix: min,
                    latest_retained_unix: max,
                }
            } else if min > window.since_unix {
                RetentionStatus::PartiallyRetained {
                    earliest_retained_unix: min,
                }
            } else {
                RetentionStatus::Covered
            }
        }
    };

    DataHealth {
        observation_count,
        distinct_block_heights,
        first_observed,
        last_observed,
        freshness,
        retention,
    }
}

fn build_operational_activity(
    observations_in_window: &[&ObservationRecord],
) -> OperationalActivity {
    let mut dirty_pool_blocks = 0u64;
    let mut discovery_present_count = 0u64;
    let mut cycles_optimized_sum = 0u64;
    let mut cycles_total_sum = 0u64;
    let mut any_cycle_pair = false;

    for observation in observations_in_window {
        if let Some(discovery) = &observation.discovery {
            discovery_present_count += 1;
            if discovery.dirty_pools_count > 0 {
                dirty_pool_blocks += 1;
            }
            if let (Some(optimized), Some(total)) =
                (discovery.cycles_optimized, discovery.cycles_total)
            {
                cycles_optimized_sum += optimized;
                cycles_total_sum += total;
                any_cycle_pair = true;
            }
        }
    }

    let missing_discovery_count = observations_in_window.len() as u64 - discovery_present_count;
    let cycle_evaluation_coverage = if any_cycle_pair && cycles_total_sum > 0 {
        Some(CycleEvaluationCoverage {
            cycles_optimized_sum,
            cycles_total_sum,
        })
    } else {
        None
    };

    OperationalActivity {
        dirty_pool_blocks,
        discovery_present_count,
        missing_discovery_count,
        cycle_evaluation_coverage,
    }
}

fn build_continuity(
    read: &LedgerWindowRead,
    window: &DigestWindow,
    observations_in_window: &[&ObservationRecord],
) -> Continuity {
    let heights: std::collections::BTreeSet<u64> = observations_in_window
        .iter()
        .map(|o| o.block_number)
        .collect();
    let min_height = heights.iter().next().copied();
    let max_height = heights.iter().next_back().copied();
    let gap_proxy = match (min_height, max_height) {
        (Some(min), Some(max)) if max > min => {
            let span = max - min + 1;
            let observed = heights.len() as u64;
            Some(GapProxy {
                unobserved_heights: span.saturating_sub(observed),
                span,
            })
        }
        _ => None,
    };

    let episodes = run_episodes(&read.run_headers);
    let run_starts_in_window = episodes
        .iter()
        .filter(|episode| window.contains(episode.started_at_unix))
        .count() as u64;

    Continuity {
        min_height,
        max_height,
        distinct_heights_in_window: heights.len() as u64,
        gap_proxy,
        run_starts_in_window,
    }
}

fn build_run_identity(read: &LedgerWindowRead, window: &DigestWindow) -> RunIdentitySummary {
    // Prefer the last header whose own start (or any attributed row) falls inside the
    // window; fall back to the ledger's last-known header for display only.
    let windowed_run_ids: std::collections::HashSet<&str> = read
        .observations
        .iter()
        .filter(|o| window.contains(o.recorded_at_unix))
        .map(|o| o.run_id.as_str())
        .chain(
            read.candidates
                .iter()
                .filter(|c| window.contains(c.recorded_at_unix))
                .map(|c| c.run_id.as_str()),
        )
        .chain(
            read.contexts
                .iter()
                .filter(|c| window.contains(c.block_timestamp))
                .map(|c| c.run_id.as_str()),
        )
        .collect();

    if let Some(header) = read
        .run_headers
        .iter()
        .rev()
        .find(|h| windowed_run_ids.contains(h.run_id.as_str()))
    {
        return RunIdentitySummary {
            service: Some(header.service.clone()),
            chain_id: Some(header.chain_id),
            git_commit: Some(header.git_commit.clone()),
            executor_contract: Some(header.executor_contract.clone()),
            from_window: true,
        };
    }

    if let Some(header) = read.run_headers.last() {
        return RunIdentitySummary {
            service: Some(header.service.clone()),
            chain_id: Some(header.chain_id),
            git_commit: Some(header.git_commit.clone()),
            executor_contract: Some(header.executor_contract.clone()),
            from_window: false,
        };
    }

    RunIdentitySummary::default()
}

fn build_arbitrage_summary(read: &LedgerWindowRead, window: &DigestWindow) -> ArbitrageSummary {
    let candidates_in_window: Vec<&CandidateRecord> = read
        .candidates
        .iter()
        .filter(|c| window.contains(c.recorded_at_unix))
        .collect();
    let contexts_in_window_by_digest: HashMap<&str, &ContextRecord> = read
        .contexts
        .iter()
        .filter(|c| window.contains(c.block_timestamp))
        .map(|c| (c.digest.as_str(), c))
        .collect();
    let any_context_by_digest: HashMap<&str, &ContextRecord> = read
        .contexts
        .iter()
        .map(|c| (c.digest.as_str(), c))
        .collect();
    let any_candidate_digests: std::collections::HashSet<&str> =
        read.candidates.iter().map(|c| c.digest.as_str()).collect();

    let mut outcome_counts: Vec<(&'static str, u64)> = outcome_order()
        .into_iter()
        .map(|kind| {
            let count = candidates_in_window
                .iter()
                .filter(|c| c.outcome == kind)
                .count() as u64;
            (kind.label(), count)
        })
        .collect();
    outcome_counts.retain(|(_, count)| *count > 0);

    let mut details = Vec::new();
    let mut boundary_mismatch_count = 0u64;
    let mut missing_context_count = 0u64;
    let mut malformed_net_profit_count = 0u64;
    let mut best: Option<BestNetProfit> = None;

    for candidate in &candidates_in_window {
        let windowed_context = contexts_in_window_by_digest.get(candidate.digest.as_str());
        let any_context = any_context_by_digest.get(candidate.digest.as_str());
        let (join_status, context) = match (windowed_context, any_context) {
            (Some(ctx), _) => (CandidateJoinStatus::Matched, Some(*ctx)),
            (None, Some(ctx)) => {
                boundary_mismatch_count += 1;
                (CandidateJoinStatus::BoundaryMismatch, Some(*ctx))
            }
            (None, None) => {
                if expects_context(candidate.outcome) {
                    missing_context_count += 1;
                    (CandidateJoinStatus::MissingContext, None)
                } else {
                    (CandidateJoinStatus::ExpectedNoContext, None)
                }
            }
        };

        let net_profit_wei = context.and_then(|ctx| {
            let parsed = parse_net_profit(&ctx.net_profit);
            if parsed.is_none() {
                malformed_net_profit_count += 1;
            }
            parsed
        });

        if let Some(value) = net_profit_wei {
            let is_better = best.as_ref().is_none_or(|current| {
                parse_net_profit(&current.net_profit_wei).is_none_or(|c| value > c)
            });
            if is_better {
                best = Some(BestNetProfit {
                    digest: candidate.digest.clone(),
                    opportunity_id: context.map(|c| c.opportunity_id.clone()),
                    net_profit_wei: format_signed_wei(value),
                });
            }
        }

        details.push(CandidateDetail {
            digest: candidate.digest.clone(),
            outcome_label: candidate.outcome.label(),
            opportunity_id: context.map(|c| c.opportunity_id.clone()),
            route_pool_count: context.map(|c| c.ordered_pools.len()),
            net_profit_wei: net_profit_wei.map(format_signed_wei),
            join_status,
        });
    }

    // Context rows in-window whose digest matches no candidate anywhere in the
    // ledger — a genuine inconsistency (e.g. a crash between `record_context` and
    // the candidate row's own write), not folded into the candidate loop above.
    let orphan_context_count = contexts_in_window_by_digest
        .keys()
        .filter(|digest| !any_candidate_digests.contains(*digest))
        .count() as u64;

    let candidates_detail_remainder =
        details.len().saturating_sub(MAX_CANDIDATE_DETAIL_LINES) as u64;
    details.truncate(MAX_CANDIDATE_DETAIL_LINES);

    ArbitrageSummary {
        candidate_count: candidates_in_window.len() as u64,
        outcome_counts,
        best_net_profit: best,
        candidates_detail: details,
        candidates_detail_remainder,
        boundary_mismatch_count,
        missing_context_count,
        orphan_context_count,
        malformed_net_profit_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::ledger_window::DiscoveryRecord;

    fn day(s: &str) -> DigestWindow {
        DigestWindow::for_day(UtcDay::parse(s).unwrap())
    }

    fn header(run_id: &str, started_at: u64) -> LedgerRunIdentity {
        LedgerRunIdentity {
            run_id: run_id.to_string(),
            started_at_unix: started_at,
            service: "test-service".to_string(),
            chain_id: 5000,
            git_commit: "deadbeef".to_string(),
            executor_contract: "0x02".to_string(),
            wmnt_address: "0x03".to_string(),
        }
    }

    fn observation(block: u64, recorded_at: u64, run_id: &str) -> ObservationRecord {
        ObservationRecord {
            block_number: block,
            block_timestamp: recorded_at,
            recorded_at_unix: recorded_at,
            discovery: None,
            run_id: run_id.to_string(),
        }
    }

    fn observation_with_discovery(
        block: u64,
        recorded_at: u64,
        run_id: &str,
        dirty_pools: usize,
        cycles: Option<(u64, u64)>,
    ) -> ObservationRecord {
        ObservationRecord {
            block_number: block,
            block_timestamp: recorded_at,
            recorded_at_unix: recorded_at,
            discovery: Some(DiscoveryRecord {
                skipped: false,
                skip_reason: None,
                dirty_pools_count: dirty_pools,
                cycles_optimized: cycles.map(|(o, _)| o),
                cycles_total: cycles.map(|(_, t)| t),
            }),
            run_id: run_id.to_string(),
        }
    }

    fn candidate(
        digest: &str,
        outcome: CandidateOutcomeKind,
        recorded_at: u64,
        run_id: &str,
    ) -> CandidateRecord {
        CandidateRecord {
            digest: digest.to_string(),
            outcome,
            recorded_at_unix: recorded_at,
            run_id: run_id.to_string(),
        }
    }

    fn context(
        digest: &str,
        block_timestamp: u64,
        net_profit: &str,
        run_id: &str,
    ) -> ContextRecord {
        ContextRecord {
            digest: digest.to_string(),
            opportunity_id: format!("opp-{digest}"),
            ordered_pools: vec!["0xpool1".to_string(), "0xpool2".to_string()],
            net_profit: net_profit.to_string(),
            block_timestamp,
            run_id: run_id.to_string(),
        }
    }

    #[test]
    fn zero_candidate_day_with_nonzero_observations_and_dirty_pool_activity() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                observation_with_discovery(1, since + 10, "run-a", 2, Some((3, 5))),
                observation_with_discovery(2, since + 20, "run-a", 0, Some((0, 4))),
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 100);

        assert_eq!(agg.data_health.observation_count, 2);
        assert_eq!(agg.operational_activity.dirty_pool_blocks, 1);
        assert_eq!(agg.operational_activity.discovery_present_count, 2);
        assert_eq!(agg.operational_activity.missing_discovery_count, 0);
        assert_eq!(
            agg.operational_activity.cycle_evaluation_coverage,
            Some(CycleEvaluationCoverage {
                cycles_optimized_sum: 3,
                cycles_total_sum: 9
            })
        );
        assert_eq!(agg.arbitrage.candidate_count, 0);
        assert!(agg.arbitrage.best_net_profit.is_none());
        assert!(agg.arbitrage.outcome_counts.is_empty());
    }

    #[test]
    fn missing_discovery_fields_are_counted_separately_from_zero_dirty_pools() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                observation(1, since + 10, "run-a"), // no discovery at all
                observation_with_discovery(2, since + 20, "run-a", 0, None),
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);

        assert_eq!(agg.operational_activity.missing_discovery_count, 1);
        assert_eq!(agg.operational_activity.discovery_present_count, 1);
        assert_eq!(agg.operational_activity.dirty_pool_blocks, 0);
    }

    #[test]
    fn zero_cycle_coverage_denominator_is_n_a_not_zero() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![observation_with_discovery(
                1,
                since + 10,
                "run-a",
                0,
                Some((0, 0)),
            )],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(agg.operational_activity.cycle_evaluation_coverage.is_none());
    }

    #[test]
    fn repeated_run_headers_across_rotation_do_not_inflate_run_starts() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        // Same run's header re-emitted 3 times (segment rotations) -> 1 start.
        let read = LedgerWindowRead {
            run_headers: vec![
                header("run-a", since + 5),
                header("run-a", since + 5),
                header("run-a", since + 5),
            ],
            observations: vec![],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 100);
        assert_eq!(agg.continuity.run_starts_in_window, 1);
    }

    #[test]
    fn a_genuine_restart_within_the_window_is_a_second_observed_start() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![
                header("run-a", since + 5),
                header("run-a", since + 5),   // rotation re-emit
                header("run-b", since + 200), // real restart
            ],
            observations: vec![],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 300);
        assert_eq!(agg.continuity.run_starts_in_window, 2);
    }

    #[test]
    fn a_run_that_started_yesterday_and_continues_today_is_not_counted_as_a_start_today() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since - 3600)], // started yesterday
            observations: vec![observation(1, since + 10, "run-a")],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 100);
        assert_eq!(agg.continuity.run_starts_in_window, 0);
        // But the run's identity is still attributed for the footer.
        assert_eq!(agg.run_identity.service, Some("test-service".to_string()));
        assert!(agg.run_identity.from_window);
    }

    #[test]
    fn observed_height_gap_proxy_is_labeled_and_computed_correctly() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                observation(10, since + 1, "run-a"),
                observation(12, since + 2, "run-a"),
                observation(15, since + 3, "run-a"),
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        // span 10..15 inclusive = 6 heights; 3 observed -> 3 unobserved.
        assert_eq!(
            agg.continuity.gap_proxy,
            Some(GapProxy {
                unobserved_heights: 3,
                span: 6
            })
        );
    }

    #[test]
    fn single_observed_height_has_no_gap_proxy() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![observation(10, since + 1, "run-a")],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(agg.continuity.gap_proxy.is_none());
    }

    #[test]
    fn unavailable_retention_is_flagged_when_the_window_predates_all_retained_data() {
        let window = day("2026-01-01");
        let (_, until) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", until + 10_000)],
            observations: vec![observation(1, until + 10_000, "run-a")],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, until + 20_000);
        assert!(matches!(
            agg.data_health.retention,
            RetentionStatus::OutsideRetention { .. }
        ));
        assert_eq!(agg.data_health.observation_count, 0);
    }

    #[test]
    fn partially_retained_when_only_the_tail_of_the_day_survived() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since + 40_000)],
            observations: vec![observation(1, since + 40_000, "run-a")],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 41_000);
        assert!(matches!(
            agg.data_health.retention,
            RetentionStatus::PartiallyRetained {
                earliest_retained_unix
            } if earliest_retained_unix == since + 40_000
        ));
    }

    #[test]
    fn empty_ledger_is_distinguished_from_rotated_out_history() {
        let window = day("2026-06-15");
        let read = LedgerWindowRead::default();
        let agg = aggregate_digest(&read, window, window.since_unix);
        assert_eq!(agg.data_health.retention, RetentionStatus::EmptyLedger);
    }

    #[test]
    fn freshness_is_not_applicable_for_a_historical_backfill_window() {
        let window = day("2020-01-01");
        let agg = aggregate_digest(&LedgerWindowRead::default(), window, 1_800_000_000);
        assert_eq!(
            agg.data_health.freshness,
            Freshness::NotApplicableHistorical
        );
    }

    #[test]
    fn freshness_is_stale_when_the_gap_exceeds_the_threshold() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![observation(1, since + 10, "run-a")],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let generated_at = since + 10 + FRESHNESS_STALE_THRESHOLD_SECS + 1;
        let agg = aggregate_digest(&read, window, generated_at);
        assert!(matches!(agg.data_health.freshness, Freshness::Stale { .. }));
    }

    #[test]
    fn matched_candidate_and_context_produce_a_clean_detail_row() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![],
            candidates: vec![candidate(
                "0xabc",
                CandidateOutcomeKind::Pass,
                since + 5,
                "run-a",
            )],
            contexts: vec![context("0xabc", since + 5, "1000", "run-a")],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.candidate_count, 1);
        assert_eq!(agg.arbitrage.candidates_detail.len(), 1);
        assert_eq!(
            agg.arbitrage.candidates_detail[0].join_status,
            CandidateJoinStatus::Matched
        );
        assert_eq!(
            agg.arbitrage
                .best_net_profit
                .as_ref()
                .unwrap()
                .net_profit_wei,
            "1000"
        );
        assert_eq!(agg.arbitrage.outcome_counts, vec![("pass", 1)]);
    }

    #[test]
    fn candidate_recorded_in_window_with_context_just_before_midnight_is_a_boundary_mismatch() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since - 100)],
            observations: vec![],
            candidates: vec![candidate(
                "0xabc",
                CandidateOutcomeKind::Pass,
                since + 1,
                "run-a",
            )],
            // Context's block_timestamp is one second *before* midnight.
            contexts: vec![context("0xabc", since - 1, "1000", "run-a")],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.boundary_mismatch_count, 1);
        assert_eq!(
            agg.arbitrage.candidates_detail[0].join_status,
            CandidateJoinStatus::BoundaryMismatch
        );
        // Value is still shown, not silently dropped.
        assert_eq!(
            agg.arbitrage.candidates_detail[0].net_profit_wei,
            Some("1000".to_string())
        );
    }

    #[test]
    fn skipped_and_sampled_out_candidates_never_flag_missing_context() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![
                candidate(
                    "0x1",
                    CandidateOutcomeKind::SkippedApproved,
                    since + 1,
                    "run-a",
                ),
                candidate("0x2", CandidateOutcomeKind::SampledOut, since + 2, "run-a"),
            ],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.missing_context_count, 0);
        assert!(agg
            .arbitrage
            .candidates_detail
            .iter()
            .all(|d| d.join_status == CandidateJoinStatus::ExpectedNoContext));
    }

    #[test]
    fn a_pass_outcome_missing_its_context_row_is_a_real_gap() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![candidate(
                "0xabc",
                CandidateOutcomeKind::Pass,
                since + 1,
                "run-a",
            )],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.missing_context_count, 1);
        assert_eq!(
            agg.arbitrage.candidates_detail[0].join_status,
            CandidateJoinStatus::MissingContext
        );
    }

    #[test]
    fn orphan_context_with_no_matching_candidate_anywhere_is_flagged() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![],
            contexts: vec![context("0xabc", since + 1, "1000", "run-a")],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.orphan_context_count, 1);
        // No candidate row means no detail line for it, and no fabricated profit.
        assert!(agg.arbitrage.candidates_detail.is_empty());
        assert!(agg.arbitrage.best_net_profit.is_none());
    }

    #[test]
    fn best_net_profit_is_none_when_there_are_zero_candidates() {
        let window = day("2026-06-15");
        let agg = aggregate_digest(&LedgerWindowRead::default(), window, window.since_unix);
        assert!(agg.arbitrage.best_net_profit.is_none());
        assert_eq!(agg.arbitrage.candidate_count, 0);
    }

    #[test]
    fn best_net_profit_picks_the_maximum_across_multiple_candidates() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![
                candidate("0x1", CandidateOutcomeKind::Pass, since + 1, "run-a"),
                candidate("0x2", CandidateOutcomeKind::Pass, since + 2, "run-a"),
                candidate("0x3", CandidateOutcomeKind::Revert, since + 3, "run-a"),
            ],
            contexts: vec![
                context("0x1", since + 1, "500", "run-a"),
                context("0x2", since + 2, "9000", "run-a"),
                context("0x3", since + 3, "-100", "run-a"),
            ],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.best_net_profit.unwrap().digest, "0x2");
    }

    #[test]
    fn candidate_detail_list_is_bounded_with_a_remainder_count() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let candidates: Vec<CandidateRecord> = (0..8)
            .map(|i| {
                candidate(
                    &format!("0x{i}"),
                    CandidateOutcomeKind::Pass,
                    since + i as u64,
                    "run-a",
                )
            })
            .collect();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates,
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 100);
        assert_eq!(agg.arbitrage.candidate_count, 8);
        assert_eq!(
            agg.arbitrage.candidates_detail.len(),
            MAX_CANDIDATE_DETAIL_LINES
        );
        assert_eq!(
            agg.arbitrage.candidates_detail_remainder,
            8 - MAX_CANDIDATE_DETAIL_LINES as u64
        );
    }

    #[test]
    fn deferred_incomplete_tail_is_surfaced_as_a_data_quality_note() {
        let window = day("2026-06-15");
        let read = LedgerWindowRead {
            deferred_incomplete_tail: Some(
                "ledger.jsonl: deferred 12 trailing byte(s)".to_string(),
            ),
            ..LedgerWindowRead::default()
        };
        let agg = aggregate_digest(&read, window, window.since_unix);
        assert_eq!(agg.data_quality_notes.len(), 1);
        assert!(agg.data_quality_notes[0].contains("deferred"));
    }
}
