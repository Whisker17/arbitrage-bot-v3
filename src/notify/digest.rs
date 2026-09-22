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
        // Delegates to `UtcDay::contains_unix` rather than re-comparing
        // `since_unix`/`until_unix` here — those two fields are cached copies of
        // exactly `self.day.bounds_unix()`, so this stays the single definition of
        // "is this instant inside the day".
        self.day.contains_unix(unix_secs)
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
    /// Total paths that reached the optimizer binary search in-window (WHI-1411).
    pub paths_quoted_sum: u64,
    /// True if at least one observation in-window carried a non-None `paths_quoted`.
    pub any_paths_quoted_recorded: bool,
    /// True when cycles were evaluated in-window but zero paths reached the optimizer (WHI-1411).
    pub is_pipeline_dead: bool,
    /// True when cycles were evaluated in-window but **no** observation carries a
    /// `paths_quoted` value at all (e.g. an older ledger schema without the field) —
    /// pipeline liveness genuinely cannot be determined from this window. Fails
    /// closed: this must never be silently treated as "healthy" (WHI-1411).
    pub pipeline_liveness_unknown: bool,
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
    /// candidate's wire row carries a `block_tag` (i.e. `has_block_tag` — see
    /// [`crate::notify::ledger_window::CandidateRecord::has_block_tag`]), meaning a
    /// real semantic call *was* attempted and should have written one. A `Pass`/
    /// `Revert`/`RpcError` row, or an `EnvUnsupported` row from a real provenance-
    /// rejected call, always has a `block_tag`; a genuinely missing context on one
    /// of those is a real gap.
    MissingContext,
    /// No context row exists, but no real call was ever attempted either
    /// (`has_block_tag == false`) — not a gap. Covers `SkippedApproved`/`SampledOut`
    /// (policy-skipped before `call()`) **and** an `EnvUnsupported` row written by
    /// `record_production_gate_blocked` (bot.rs's most common shadow-mode outcome:
    /// production send is gated off before any candidate ever gets a
    /// `FinalRequest`). The outcome kind alone cannot distinguish this case from
    /// `MissingContext`'s `EnvUnsupported` shape — only `has_block_tag` can.
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
    /// `None` ⇒ either zero candidates ("N/A — 无候选") or ≥ 1 candidate but no
    /// usable modeled-profit value (a distinct, non-misleading label — see
    /// [`crate::notify::lark::render_card`]).
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
    /// A matched context row whose `profit_basis` this reader doesn't recognize as
    /// `"simulated"` — every row this ledger writes today carries that basis, but
    /// the net-profit display is gated on it rather than assumed, so a future basis
    /// this reader doesn't model is labeled, not silently shown as a WMNT amount.
    pub unmodeled_profit_basis_count: u64,
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
    /// Number of ledger segments (active file + rotated) actually read to build
    /// this aggregate — footer diagnostic; never the raw paths themselves (those
    /// can carry a host-local directory layout, not something to leak into an
    /// external webhook body).
    pub segments_read_count: usize,
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

/// `true` when this candidate's context row (if any) is genuinely expected. The
/// signal is the wire row's `has_block_tag`, **not** the outcome kind alone: a
/// `SkippedApproved`/`SampledOut` row never reaches `call()`, but so does an
/// `EnvUnsupported` row written by `record_production_gate_blocked` (bot.rs) before
/// any candidate ever gets a `FinalRequest` — both shapes carry no `block_tag` and
/// no matching context, and neither is a real data gap.
fn expects_context(candidate: &CandidateRecord) -> bool {
    candidate.has_block_tag
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
        segments_read_count: read.segments_read.len(),
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
    let mut paths_quoted_sum = 0u64;
    let mut any_paths_quoted_recorded = false;
    // WHI-1411: true when at least one non-skipped observation did not carry a
    // `paths_quoted` value for that same row and we lack positive proof there was
    // nothing to evaluate — a per-row coverage gap. A window can mix rows with and
    // without the field (e.g. the day a fleet upgrades to a binary that started
    // recording it, or an even older schema that also predates `cycles_optimized`);
    // relying only on "not one row records it" would let a single healthy-looking
    // recorded row mask an unrecorded row that was actually dead.
    let mut has_paths_quoted_coverage_gap = false;

    for observation in observations_in_window {
        if let Some(discovery) = &observation.discovery {
            discovery_present_count += 1;
            if discovery.dirty_pools_count > 0 {
                dirty_pool_blocks += 1;
            }
            if let (Some(optimized), Some(total)) =
                (discovery.cycles_optimized, discovery.cycles_total)
            {
                cycles_optimized_sum = cycles_optimized_sum.saturating_add(optimized);
                cycles_total_sum = cycles_total_sum.saturating_add(total);
                any_cycle_pair = true;
            }
            match discovery.paths_quoted {
                Some(quoted) => {
                    paths_quoted_sum = paths_quoted_sum.saturating_add(quoted);
                    any_paths_quoted_recorded = true;
                }
                None => {
                    // A non-skipped row missing `paths_quoted` is a coverage gap unless we
                    // have positive proof there was nothing to evaluate (`Some(0)`). Note
                    // this also covers `cycles_optimized: None` (an even older schema that
                    // predates *that* field too) — not just `Some(n > 0)` — since an absent
                    // cycle count is not proof of zero cycles either; only `skipped` rows
                    // (a deliberate "discovery did not run" state, unrelated to missing
                    // telemetry) are exempted.
                    if !discovery.skipped && discovery.cycles_optimized != Some(0) {
                        has_paths_quoted_coverage_gap = true;
                    }
                }
            }
        }
    }

    let is_pipeline_dead = any_paths_quoted_recorded
        && any_cycle_pair
        && cycles_optimized_sum > 0
        && paths_quoted_sum == 0;

    // WHI-1411 fails closed: cycles were evaluated in-window (so the pipeline *was* running
    // discovery), but liveness genuinely cannot be determined for at least part of that
    // evaluated work — either no observation carries `paths_quoted` at all (e.g. an older
    // ledger schema), or some do and some don't (a coverage gap: a healthy-looking
    // recorded row must never be allowed to mask an unrecorded row that could have been
    // 100% dead). `is_pipeline_dead` takes precedence when it can be conclusively proven
    // from the rows that do carry the field — that is a stronger, more specific signal
    // than "some of our data has gaps".
    let pipeline_liveness_unknown = !is_pipeline_dead
        && any_cycle_pair
        && cycles_optimized_sum > 0
        && has_paths_quoted_coverage_gap;

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
        paths_quoted_sum,
        any_paths_quoted_recorded,
        is_pipeline_dead,
        pipeline_liveness_unknown,
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

fn run_identity_summary_from(header: &LedgerRunIdentity, from_window: bool) -> RunIdentitySummary {
    RunIdentitySummary {
        service: Some(header.service.clone()),
        chain_id: Some(header.chain_id),
        git_commit: Some(header.git_commit.clone()),
        executor_contract: Some(header.executor_contract.clone()),
        from_window,
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
        return run_identity_summary_from(header, true);
    }

    if let Some(header) = read.run_headers.last() {
        return run_identity_summary_from(header, false);
    }

    RunIdentitySummary::default()
}

fn build_arbitrage_summary(read: &LedgerWindowRead, window: &DigestWindow) -> ArbitrageSummary {
    let candidates_in_window: Vec<&CandidateRecord> = read
        .candidates
        .iter()
        .filter(|c| window.contains(c.recorded_at_unix))
        .collect();
    // Joined by `(run_id, digest)`, not `digest` alone (issue: "Define the join (by
    // run identity + digest)") — `FinalRequestDigest` is a hash of route/amount/
    // min_profit, so a fixed trial amount on the same route can legitimately repeat
    // across different runs (restarts) or different days; joining on digest alone
    // would let an unrelated run's context silently answer a different run's
    // candidate.
    let contexts_in_window_by_key: HashMap<(&str, &str), &ContextRecord> = read
        .contexts
        .iter()
        .filter(|c| window.contains(c.block_timestamp))
        .map(|c| ((c.run_id.as_str(), c.digest.as_str()), c))
        .collect();
    let any_context_by_key: HashMap<(&str, &str), &ContextRecord> = read
        .contexts
        .iter()
        .map(|c| ((c.run_id.as_str(), c.digest.as_str()), c))
        .collect();
    let any_candidate_keys: std::collections::HashSet<(&str, &str)> = read
        .candidates
        .iter()
        .map(|c| (c.run_id.as_str(), c.digest.as_str()))
        .collect();

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
    let mut unmodeled_profit_basis_count = 0u64;
    // Tracks the raw signed value alongside the rendered `BestNetProfit` so each
    // comparison is a plain `i128` compare, not a re-parse of the previous winner's
    // formatted string.
    let mut best: Option<(i128, BestNetProfit)> = None;

    for candidate in &candidates_in_window {
        let key = (candidate.run_id.as_str(), candidate.digest.as_str());
        let windowed_context = contexts_in_window_by_key.get(&key);
        let any_context = any_context_by_key.get(&key);
        let (join_status, context) = match (windowed_context, any_context) {
            (Some(ctx), _) => (CandidateJoinStatus::Matched, Some(*ctx)),
            (None, Some(ctx)) => {
                boundary_mismatch_count += 1;
                (CandidateJoinStatus::BoundaryMismatch, Some(*ctx))
            }
            (None, None) => {
                if expects_context(candidate) {
                    missing_context_count += 1;
                    (CandidateJoinStatus::MissingContext, None)
                } else {
                    (CandidateJoinStatus::ExpectedNoContext, None)
                }
            }
        };

        let net_profit_wei = context.and_then(|ctx| {
            if ctx.profit_basis != "simulated" {
                unmodeled_profit_basis_count += 1;
                return None;
            }
            let parsed = parse_net_profit(&ctx.net_profit);
            if parsed.is_none() {
                malformed_net_profit_count += 1;
            }
            parsed
        });

        if let Some(value) = net_profit_wei {
            let is_better = best.as_ref().is_none_or(|(current, _)| value > *current);
            if is_better {
                best = Some((
                    value,
                    BestNetProfit {
                        digest: candidate.digest.clone(),
                        opportunity_id: context.map(|c| c.opportunity_id.clone()),
                        net_profit_wei: format_signed_wei(value),
                    },
                ));
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

    // Context rows in-window whose `(run_id, digest)` matches no candidate anywhere
    // in the ledger — a genuine inconsistency (e.g. a crash between
    // `record_context` and the candidate row's own write), not folded into the
    // candidate loop above.
    let orphan_context_count = contexts_in_window_by_key
        .keys()
        .filter(|key| !any_candidate_keys.contains(key))
        .count() as u64;

    let candidates_detail_remainder =
        details.len().saturating_sub(MAX_CANDIDATE_DETAIL_LINES) as u64;
    details.truncate(MAX_CANDIDATE_DETAIL_LINES);

    ArbitrageSummary {
        candidate_count: candidates_in_window.len() as u64,
        outcome_counts,
        best_net_profit: best.map(|(_, best)| best),
        candidates_detail: details,
        candidates_detail_remainder,
        boundary_mismatch_count,
        missing_context_count,
        orphan_context_count,
        malformed_net_profit_count,
        unmodeled_profit_basis_count,
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
                paths_quoted: cycles.map(|(o, _)| o),
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
        candidate_with_block_tag(digest, outcome, recorded_at, run_id, true)
    }

    fn candidate_with_block_tag(
        digest: &str,
        outcome: CandidateOutcomeKind,
        recorded_at: u64,
        run_id: &str,
        has_block_tag: bool,
    ) -> CandidateRecord {
        CandidateRecord {
            digest: digest.to_string(),
            outcome,
            recorded_at_unix: recorded_at,
            has_block_tag,
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
            profit_basis: "simulated".to_string(),
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

    /// WHI-1411 acceptance: `is_pipeline_dead` is set directly by the pure aggregator
    /// (not just observable through the rendered Lark card) when cycles were evaluated
    /// in-window but every recorded `paths_quoted` was zero.
    #[test]
    fn is_pipeline_dead_flags_when_paths_quoted_is_zero_but_cycles_were_optimized() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![ObservationRecord {
                block_number: 1,
                block_timestamp: since + 10,
                recorded_at_unix: since + 10,
                discovery: Some(DiscoveryRecord {
                    skipped: false,
                    skip_reason: None,
                    dirty_pools_count: 2,
                    cycles_optimized: Some(50),
                    cycles_total: Some(100),
                    paths_quoted: Some(0),
                }),
                run_id: "run-a".to_string(),
            }],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(agg.operational_activity.is_pipeline_dead);
        assert!(!agg.operational_activity.pipeline_liveness_unknown);
    }

    /// WHI-1411 acceptance (fail-closed): when **no** observation in-window carries a
    /// `paths_quoted` value at all (e.g. an older ledger schema), liveness is genuinely
    /// undeterminable — this must never be reported as either healthy or confirmed-dead.
    #[test]
    fn pipeline_liveness_unknown_when_no_observation_ever_records_paths_quoted() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![ObservationRecord {
                block_number: 1,
                block_timestamp: since + 10,
                recorded_at_unix: since + 10,
                discovery: Some(DiscoveryRecord {
                    skipped: false,
                    skip_reason: None,
                    dirty_pools_count: 2,
                    cycles_optimized: Some(50),
                    cycles_total: Some(100),
                    paths_quoted: None,
                }),
                run_id: "run-a".to_string(),
            }],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(!agg.operational_activity.is_pipeline_dead);
        assert!(agg.operational_activity.pipeline_liveness_unknown);
    }

    /// WHI-1411 acceptance (fail-closed, partial coverage): a window can mix rows that
    /// record `paths_quoted` with rows that don't (e.g. the day a fleet upgrades to a
    /// binary that started recording the field). A single healthy-looking recorded row
    /// (`paths_quoted > 0`) must never mask an unrecorded row that could itself have been
    /// 100% dead — the window must still report "unknown", not silently "healthy".
    #[test]
    fn pipeline_liveness_unknown_when_some_but_not_all_observations_record_paths_quoted() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                // Tiny healthy-looking recorded pass: 1 of 10 cycles reached the optimizer.
                ObservationRecord {
                    block_number: 1,
                    block_timestamp: since + 10,
                    recorded_at_unix: since + 10,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10),
                        cycles_total: Some(10),
                        paths_quoted: Some(1),
                    }),
                    run_id: "run-a".to_string(),
                },
                // Much larger unrecorded pass: liveness for this pass is genuinely unknown
                // (could have been 100% dead) and must not be masked by the row above.
                ObservationRecord {
                    block_number: 2,
                    block_timestamp: since + 20,
                    recorded_at_unix: since + 20,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10_000),
                        cycles_total: Some(10_000),
                        paths_quoted: None,
                    }),
                    run_id: "run-a".to_string(),
                },
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(
            !agg.operational_activity.is_pipeline_dead,
            "a recorded paths_quoted > 0 anywhere means we cannot claim confirmed-dead"
        );
        assert!(
            agg.operational_activity.pipeline_liveness_unknown,
            "the unrecorded pass's liveness must not be silently assumed healthy"
        );
    }

    /// WHI-1411 round-3: an even-older-schema row that predates *both* `paths_quoted`
    /// and `cycles_optimized` (so neither field is recorded at all) must still count as
    /// a coverage gap — not just rows where `cycles_optimized` happens to be recorded as
    /// a positive number. An absent cycle count is not proof of zero cycles either.
    #[test]
    fn pipeline_liveness_unknown_when_a_row_is_missing_both_cycles_optimized_and_paths_quoted() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                ObservationRecord {
                    block_number: 1,
                    block_timestamp: since + 10,
                    recorded_at_unix: since + 10,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10),
                        cycles_total: Some(10),
                        paths_quoted: Some(1),
                    }),
                    run_id: "run-a".to_string(),
                },
                // Neither cycles_optimized/cycles_total nor paths_quoted recorded at all,
                // and not a skip — must still be treated as an unresolved coverage gap.
                ObservationRecord {
                    block_number: 2,
                    block_timestamp: since + 20,
                    recorded_at_unix: since + 20,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: None,
                        cycles_total: None,
                        paths_quoted: None,
                    }),
                    run_id: "run-a".to_string(),
                },
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(!agg.operational_activity.is_pipeline_dead);
        assert!(
            agg.operational_activity.pipeline_liveness_unknown,
            "a row missing every discovery field (not a skip) must still register as a gap"
        );
    }

    /// WHI-1411 round-3: a genuinely *skipped* discovery row (a deliberate "discovery did
    /// not run" state, e.g. nothing dirty this head) must never itself be treated as a
    /// coverage gap just because it also happens to have no `paths_quoted`/`cycles_optimized`.
    #[test]
    fn skipped_rows_never_spuriously_trigger_a_paths_quoted_coverage_gap() {
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![header("run-a", since)],
            observations: vec![
                ObservationRecord {
                    block_number: 1,
                    block_timestamp: since + 10,
                    recorded_at_unix: since + 10,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10),
                        cycles_total: Some(10),
                        paths_quoted: Some(1),
                    }),
                    run_id: "run-a".to_string(),
                },
                ObservationRecord {
                    block_number: 2,
                    block_timestamp: since + 20,
                    recorded_at_unix: since + 20,
                    discovery: Some(DiscoveryRecord {
                        skipped: true,
                        skip_reason: Some("nothing_dirty".to_string()),
                        dirty_pools_count: 0,
                        cycles_optimized: None,
                        cycles_total: None,
                        paths_quoted: None,
                    }),
                    run_id: "run-a".to_string(),
                },
            ],
            candidates: vec![],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since);
        assert!(!agg.operational_activity.is_pipeline_dead);
        assert!(
            !agg.operational_activity.pipeline_liveness_unknown,
            "a legitimately skipped row must not be mistaken for missing telemetry"
        );
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
            // No `block_tag` on either row — matches the real wire shape: neither
            // `SkippedApproved` nor `SampledOut` ever reaches `call()`.
            candidates: vec![
                candidate_with_block_tag(
                    "0x1",
                    CandidateOutcomeKind::SkippedApproved,
                    since + 1,
                    "run-a",
                    false,
                ),
                candidate_with_block_tag(
                    "0x2",
                    CandidateOutcomeKind::SampledOut,
                    since + 2,
                    "run-a",
                    false,
                ),
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
    fn a_production_gate_blocked_row_never_flags_a_fake_missing_context_gap() {
        // `ShadowExecutionContext::record_production_gate_blocked` writes an
        // `EnvUnsupported` candidate row with no `block_tag` — no `call()` was ever
        // attempted, so no context row can exist. This must render exactly like
        // `SkippedApproved`/`SampledOut` (`ExpectedNoContext`), never as a real
        // "missing context" data gap, even though the outcome kind is
        // `EnvUnsupported` (which a *real* provenance-rejected call also uses).
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![candidate_with_block_tag(
                "0xabc",
                CandidateOutcomeKind::EnvUnsupported,
                since + 1,
                "run-a",
                false,
            )],
            contexts: vec![],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.missing_context_count, 0);
        assert_eq!(
            agg.arbitrage.candidates_detail[0].join_status,
            CandidateJoinStatus::ExpectedNoContext
        );
    }

    #[test]
    fn an_env_unsupported_row_with_a_block_tag_still_flags_a_real_gap() {
        // The other `EnvUnsupported` shape: a real provenance-rejected call did
        // reach `call()` (has a `block_tag`) but its context row is missing —
        // this IS a genuine data gap and must still be flagged.
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![candidate_with_block_tag(
                "0xabc",
                CandidateOutcomeKind::EnvUnsupported,
                since + 1,
                "run-a",
                true,
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
    fn a_context_from_a_different_run_sharing_the_same_digest_never_cross_matches() {
        // Same digest can legitimately recur across two different runs (a fixed
        // trial amount on the same route) — the join must be scoped by run_id, not
        // digest alone (issue: "Define the join (by run identity + digest)").
        let window = day("2026-06-15");
        let (since, _) = window.day.bounds_unix();
        let read = LedgerWindowRead {
            run_headers: vec![],
            observations: vec![],
            candidates: vec![candidate(
                "0xabc",
                CandidateOutcomeKind::Pass,
                since + 1,
                "run-b",
            )],
            // Same digest, but recorded under a different run.
            contexts: vec![context("0xabc", since + 1, "999999", "run-a")],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(
            agg.arbitrage.candidates_detail[0].join_status,
            CandidateJoinStatus::MissingContext,
            "a different run's context must never silently answer this candidate"
        );
        assert_eq!(agg.arbitrage.missing_context_count, 1);
        assert!(agg.arbitrage.best_net_profit.is_none());
        // The unmatched context (different run) is its own orphan, not folded away.
        assert_eq!(agg.arbitrage.orphan_context_count, 1);
    }

    #[test]
    fn context_with_an_unrecognized_profit_basis_is_not_shown_as_a_modeled_profit() {
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
            contexts: vec![ContextRecord {
                digest: "0xabc".to_string(),
                opportunity_id: "opp-0xabc".to_string(),
                ordered_pools: vec!["0xpool1".to_string()],
                net_profit: "12345".to_string(),
                profit_basis: "realized".to_string(),
                block_timestamp: since + 1,
                run_id: "run-a".to_string(),
            }],
            deferred_incomplete_tail: None,
            segments_read: vec![],
        };
        let agg = aggregate_digest(&read, window, since + 10);
        assert_eq!(agg.arbitrage.unmodeled_profit_basis_count, 1);
        assert!(agg.arbitrage.best_net_profit.is_none());
        assert!(agg.arbitrage.candidates_detail[0].net_profit_wei.is_none());
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
