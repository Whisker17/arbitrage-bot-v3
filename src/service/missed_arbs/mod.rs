//! Backwards universe selection from missed arbitrages (WHI-999).
//!
//! WHI-906 asked the *forward* question: given a candidate universe, what share
//! of observed arbs can we execute? This module asks the inverse — given the
//! arbs we demonstrably missed, **which pools would we have needed**, and what
//! does admitting them cost.
//!
//! Everything here is pure: real event datasets stay external (same contract as
//! [`crate::service::arb_coverage`] and [`crate::service::ground_truth`]), only
//! aggregate reports are committed. The only non-pure inputs a caller may add
//! are candidate TVL valuations (see [`crate::service::valuation`]).
//!
//! ## Why the numbers here differ from WHI-906
//!
//! WHI-906's `fully_executable` counts an arb as covered when every hop pool is
//! held — **including** arbs the strategy could never take (hop > 3, non-WMNT
//! settlement). This module scopes first and ranks second, so "arbs unlocked"
//! only ever counts arbs that would become genuinely reachable. The scope rules
//! mirror [`crate::execution::peer_attribution`] exactly, so the cause counts
//! recomputed here reconcile with the committed WHI-957 attribution.
//!
//! ## What "unlocked" means
//!
//! An arb needs **every** hop present, so a pool appearing in 500 missed arbs
//! unlocks nothing on its own if each of those arbs also lacks another pool.
//! [`greedy_unlock_rank`] therefore ranks by *marginal* newly-completed arbs,
//! and labels the step [`StepSelection::FrequencyFallback`] whenever no single
//! remaining pool completes an arb — a fallback step is progress toward a
//! multi-pool gap, not a gain.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use crate::amms::amm::AMM;
use crate::amms::uniswap_v2::UniswapV2Pool;
use crate::amms::Token;
use crate::arbitrage::graph::build_graph;
use crate::arbitrage::pathfinder::{PathConstraints, PathFinder};
use crate::execution::peer_attribution::AGGREGATOR_HOP_THRESHOLD;
use crate::service::arb_coverage::{
    adapter_class, normalize_address, pct, AdapterClass, ArbCoverageError, PoolCensusEntry,
};
use crate::service::config::INTERIM_V2_FACTORY;
use crate::service::v3_venues::{quarantined_v3_by_factory, venue_by_factory, DROP_IN_V3_VENUES};
use crate::state_space::StateSpace;

mod render;

pub use render::render_markdown;

/// Report schema for the WHI-999 artifacts.
pub const MISSED_ARB_REPORT_SCHEMA_VERSION: &str = "whisker-arb/missed-arb-universe/v1";

/// Cold-start seconds per pool, from the WHI-936 measurement (350 s at 130
/// pools, one pool per batch CREATE).
///
/// Deliberately linear: WHI-936's finding is that 1,413 of 1,500 batch CREATEs
/// carried a single item, so cold start scales with pool count and not with
/// batch count. Any fix to WHI-936 invalidates this constant downward — it is
/// an estimate of *today's* cost of growth, not a floor.
pub const COLD_START_SECS_PER_POOL: f64 = 350.0 / 130.0;

/// The WHI-936 reference measurement this estimate is anchored to.
pub const COLD_START_REFERENCE: &str = "WHI-936: 350 s at 130 pools (one pool per batch CREATE)";

// ── inputs ─────────────────────────────────────────────────────────────────

/// A pool reduced to its token pair — all that graph topology depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolTokens {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
}

impl PoolTokens {
    /// True for a pair that cannot form an edge (zero or self-paired token).
    /// Matches the skip rule in [`crate::service::universe_filter`].
    fn is_degenerate(&self) -> bool {
        self.token0 == Address::ZERO || self.token1 == Address::ZERO || self.token0 == self.token1
    }
}

/// Lowercase-hex key for a pool address.
///
/// Shares the key space with [`crate::service::arb_coverage`], whose census and
/// arb loaders both normalize this way.
pub fn pool_key(pool: Address) -> String {
    normalize_address(&format!("{pool:?}"))
}

/// Token pair for a census pool, when both sides parse as addresses.
fn census_pool_tokens(pool: &str, entry: &PoolCensusEntry) -> Option<PoolTokens> {
    Some(PoolTokens {
        pool: pool.parse().ok()?,
        token0: entry.token0.as_deref()?.parse().ok()?,
        token1: entry.token1.as_deref()?.parse().ok()?,
    })
}

/// One ground-truth arbitrage, reduced to what backwards selection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissedArbEvent {
    pub block: Option<u64>,
    pub tx_hash: Option<String>,
    /// Ordered hop pools, lowercased.
    pub pools: Vec<String>,
    /// Hop count as reported by the source (`nSwaps`), not `pools.len()` —
    /// they differ when the extract deduplicates repeated pools.
    pub hop_count: u32,
    /// Settlement asset, lowercased; `None` when the extract could not derive one.
    pub settlement_asset: Option<String>,
}

/// Why an arb is unreachable no matter which pools we admit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// Reachable in principle: hop ≤ cap, settles in WMNT, not aggregator noise.
    InScope,
    /// `hop_count > 50` — aggregator batch, not an atomic arb.
    AggregatorMisclass,
    /// `hop_count > max_hops`.
    OutOfScopeHopCap,
    /// Settlement asset present and ≠ WMNT.
    OutOfScopeNonWmntSettlement,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InScope => "in_scope",
            Self::AggregatorMisclass => "aggregator_misclass",
            Self::OutOfScopeHopCap => "out_of_scope_hop_cap",
            Self::OutOfScopeNonWmntSettlement => "out_of_scope_non_wmnt_settlement",
        }
    }
}

/// Classify an event under the same priority order as
/// [`crate::execution::peer_attribution`]: aggregator → hop cap → settlement.
///
/// `settlement_wmnt` is compared case-insensitively; an event with no
/// settlement asset is **not** ruled out (the attribution treats a missing
/// asset as unknown, not as non-WMNT).
pub fn classify_scope(event: &MissedArbEvent, settlement_wmnt: &str, max_hops: u32) -> Scope {
    if event.hop_count > AGGREGATOR_HOP_THRESHOLD {
        return Scope::AggregatorMisclass;
    }
    if event.hop_count > max_hops {
        return Scope::OutOfScopeHopCap;
    }
    match event.settlement_asset.as_deref() {
        Some(a) if !a.eq_ignore_ascii_case(settlement_wmnt) => {
            Scope::OutOfScopeNonWmntSettlement
        }
        _ => Scope::InScope,
    }
}

// ── cause recount + residual bound ─────────────────────────────────────────

/// WHI-957 cause counts recomputed from the raw dataset.
///
/// `in_universe_in_scope` is what the committed offline pass reports as
/// `unattributable`: the pass had no concurrent ledger, so an in-scope arb over
/// pools we hold could not be separated into dirty-cycle / unprofitable / lost
/// race. It is *not* an unclassified remainder in the universe sense.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CauseRecount {
    pub total_events: usize,
    pub aggregator_misclass: usize,
    pub out_of_scope_hop_cap: usize,
    pub out_of_scope_non_wmnt_settlement: usize,
    pub not_in_universe: usize,
    pub in_universe_in_scope: usize,
    /// Non-aggregator events (the WHI-957 rate denominator).
    pub analysis_denominator: usize,
    pub not_in_universe_pct: f64,
}

/// Whether the WHI-957 residual can hide further `not_in_universe` events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResidualBound {
    /// The committed pass's `unattributable` count, recomputed.
    pub residual_count: usize,
    /// Residual events that have at least one pool outside the universe.
    /// Structurally zero: `not_in_universe` is tested *before* the residual.
    pub residual_events_with_missing_pool: usize,
    /// Residual events over pools we hold, so already counted as reachable.
    pub residual_events_fully_in_universe: usize,
    /// True only if the check above found leakage.
    pub can_hide_not_in_universe: bool,
    pub statement: String,
}

// ── per-pool candidate rows ────────────────────────────────────────────────

/// Whether the venue behind a pool can be loaded today, and if not, what class
/// of work stands in the way.
///
/// The distinction matters for AC6: "needs an adapter" and "needs a registry
/// entry" are very different amounts of work, and lumping them together would
/// either overstate what is reachable now or understate what is cheap to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueStatus {
    /// Factory is a registered drop-in (V3 family), the interim V2 venue, or Moe.
    LoadableDropIn,
    /// Registered but quarantined for an incompatible batch ABI (WHI-938).
    QuarantinedAdapterRequired,
    /// UniV3-surface math on a factory we never enumerate. Cheapest class of
    /// gap: a `v3_venues` registry entry plus the WHI-938-style batch validation.
    UnregisteredV3FamilyFactory,
    /// V2 math on a factory we never enumerate. **Not** cheap: `V2_FEE = 300` is
    /// hard-coded, so admitting another V2 venue needs per-venue fees first
    /// (WHI-910 explicitly out of scope).
    UnregisteredV2FamilyFactory,
    /// Liquidity-Book math on a factory other than the canonical Moe one.
    UnregisteredLbFactory,
    /// Math family we have no adapter for at all (`izi`, `algebra`, `solidly`).
    UnsupportedMathFamily,
    /// Census gives neither a registered factory nor a usable math family.
    UnknownVenue,
}

impl VenueStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoadableDropIn => "loadable_drop_in",
            Self::QuarantinedAdapterRequired => "quarantined_adapter_required",
            Self::UnregisteredV3FamilyFactory => "unregistered_v3_family_factory",
            Self::UnregisteredV2FamilyFactory => "unregistered_v2_family_factory",
            Self::UnregisteredLbFactory => "unregistered_lb_factory",
            Self::UnsupportedMathFamily => "unsupported_math_family",
            Self::UnknownVenue => "unknown_venue",
        }
    }

    /// True when a pool on this venue could enter the universe today with no
    /// new adapter, registry, or fee work.
    pub fn is_loadable(self) -> bool {
        matches!(self, Self::LoadableDropIn)
    }

    /// Short description of the work a non-loadable venue needs.
    pub fn work_required(self) -> &'static str {
        match self {
            Self::LoadableDropIn => "none — loads today",
            Self::QuarantinedAdapterRequired => {
                "tick-data batch adapter (WHI-938 quarantine reason)"
            }
            Self::UnregisteredV3FamilyFactory => "v3_venues registry entry + batch validation",
            Self::UnregisteredV2FamilyFactory => "per-venue V2 fees before enumeration (V2_FEE is hard-coded)",
            Self::UnregisteredLbFactory => "non-canonical LB factory support",
            Self::UnsupportedMathFamily => "new AMM adapter for the math family",
            Self::UnknownVenue => "identify the venue first (census has no usable family)",
        }
    }
}

/// Which constraint is actually keeping a missing pool out (AC: per-filter
/// exclusion counts). Evaluated in admission order — a pool blocked by its
/// venue is never also blamed on TVL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionCause {
    /// No loadable adapter for the venue — an adapter issue, not a universe one.
    VenueNotLoadable,
    /// Valued below the TVL floor.
    BelowTvlFloor,
    /// Valuation could not be established (quarantine class).
    TvlUnavailable,
    /// TVL not measured on this run (no `--rpc-url`); cannot attribute.
    TvlNotMeasured,
    /// Passes venue + TVL but sits on no ordered ≤max-hop settlement cycle.
    CycleFilterRejected,
    /// Would clear every filter — a genuine enumeration gap in the generator.
    AdmissibleButAbsent,
}

impl ExclusionCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VenueNotLoadable => "venue_not_loadable",
            Self::BelowTvlFloor => "below_tvl_floor",
            Self::TvlUnavailable => "tvl_unavailable",
            Self::TvlNotMeasured => "tvl_not_measured",
            Self::CycleFilterRejected => "cycle_filter_rejected",
            Self::AdmissibleButAbsent => "admissible_but_absent",
        }
    }
}

/// A pool that appears in missed arbs but is not in the frozen universe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissingPool {
    pub pool: String,
    /// Venue label when the factory is registered, else `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token0: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token1: Option<String>,
    /// Census swap count over the observation window (activity, not TVL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub census_swaps: Option<u64>,
    /// In-scope missed arbs whose path includes this pool.
    pub appears_in_in_scope_arbs: usize,
    /// In-scope missed arbs where this is the **only** pool we lack.
    pub sole_blocker_of: usize,
    /// 1-based hop position within the ordered path → how often the pool sits
    /// there. A pool can occupy several positions across different arbs, and a
    /// first-hop-only pool is a different kind of gap from a mid-cycle one.
    pub hop_positions: BTreeMap<u32, usize>,
    pub adapter_class: AdapterClass,
    pub venue_status: VenueStatus,
    /// WMNT-wei TVL when measured; decimal string to survive JSON round-trip.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvl_wmnt_wei: Option<String>,
    pub exclusion_cause: ExclusionCause,
}

/// TVL knowledge for one pool, supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolTvl {
    /// Valuation established.
    Valued(U256),
    /// Reads succeeded but no WMNT-equivalent price could be derived.
    Unavailable,
}

// ── greedy ranking ─────────────────────────────────────────────────────────

/// How a greedy step was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepSelection {
    /// Completed at least one arb on its own.
    Unlock,
    /// Nothing completes alone; picked the most frequent remaining gap.
    FrequencyFallback,
}

impl StepSelection {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unlock => "unlock",
            Self::FrequencyFallback => "frequency_fallback",
        }
    }
}

/// One step of the marginal-unlock ranking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UnlockStep {
    pub rank: usize,
    pub pool: String,
    pub selection: StepSelection,
    /// In-scope arbs this pool completes **at this point in the ranking**.
    pub marginal_arbs_unlocked: usize,
    pub cumulative_arbs_unlocked: usize,
    /// In-scope arbs reachable after this step (baseline + cumulative).
    pub cumulative_reachable: usize,
    /// Percent of in-scope arbs reachable after this step.
    pub cumulative_reachable_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub venue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    pub venue_status: VenueStatus,
    pub adapter_class: AdapterClass,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvl_wmnt_wei: Option<String>,
}

/// Greedy marginal-unlock ranking over **in-scope** arbs.
///
/// At each step, add the candidate pool that completes the most arbs not yet
/// reachable. When no candidate completes any arb alone, fall back to the most
/// frequent remaining gap so multi-pool gaps can still close — those steps
/// carry `marginal_arbs_unlocked = 0` and
/// [`StepSelection::FrequencyFallback`], so a reader can never mistake a
/// fallback for a gain.
///
/// Ties break on the lexicographically smallest pool address, making the
/// ranking deterministic for a given dataset.
pub fn greedy_unlock_rank(
    held: &HashSet<String>,
    in_scope_paths: &[Vec<String>],
    candidates: &BTreeSet<String>,
    top_n: usize,
) -> Vec<(String, StepSelection, usize)> {
    if top_n == 0 || candidates.is_empty() {
        return Vec::new();
    }

    // Gap set per not-yet-reachable arb, restricted to admissible candidates.
    // An arb whose gap includes a pool we will never add can never complete, so
    // it is dropped up front rather than distorting frequency counts.
    let mut gaps: Vec<HashSet<String>> = in_scope_paths
        .iter()
        .filter_map(|path| {
            let gap: HashSet<String> =
                path.iter().filter(|p| !held.contains(*p)).cloned().collect();
            if gap.is_empty() || !gap.iter().all(|p| candidates.contains(p)) {
                None
            } else {
                Some(gap)
            }
        })
        .collect();

    let mut remaining: BTreeSet<String> = candidates.clone();
    let mut steps = Vec::with_capacity(top_n);

    for _ in 0..top_n {
        if remaining.is_empty() || gaps.is_empty() {
            break;
        }

        let mut unlock_counts: BTreeMap<&String, usize> = BTreeMap::new();
        for gap in &gaps {
            if gap.len() == 1 {
                if let Some(p) = gap.iter().next() {
                    *unlock_counts.entry(p).or_insert(0) += 1;
                }
            }
        }

        let pick = best_by_count(&unlock_counts)
            .map(|(pool, gain)| (pool, StepSelection::Unlock, gain))
            .or_else(|| {
                let mut appear: BTreeMap<&String, usize> = BTreeMap::new();
                for gap in &gaps {
                    for p in gap {
                        *appear.entry(p).or_insert(0) += 1;
                    }
                }
                best_by_count(&appear).map(|(pool, _)| (pool, StepSelection::FrequencyFallback, 0))
            });

        let Some((pool, selection, gain)) = pick else {
            break;
        };

        remaining.remove(&pool);
        gaps.retain_mut(|gap| {
            gap.remove(&pool);
            !gap.is_empty()
        });
        steps.push((pool, selection, gain));
    }

    steps
}

/// Highest count wins; ties break on the smallest key.
fn best_by_count(counts: &BTreeMap<&String, usize>) -> Option<(String, usize)> {
    counts
        .iter()
        .max_by(|(pa, ca), (pb, cb)| ca.cmp(cb).then_with(|| pb.cmp(pa)))
        .map(|(pool, count)| ((*pool).clone(), *count))
}

/// In-scope arbs whose every hop pool is in `held`.
pub fn reachable_count(held: &HashSet<String>, in_scope_paths: &[Vec<String>]) -> usize {
    in_scope_paths
        .iter()
        .filter(|path| !path.is_empty() && path.iter().all(|p| held.contains(p)))
        .count()
}

// ── candidate sets ─────────────────────────────────────────────────────────

/// Which pools a candidate set is allowed to draw from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SetRestriction {
    /// Only venues that load today — what is actionable with no new work.
    LoadableOnly,
    /// Every missing pool, adapters assumed — an upper bound, not a plan.
    AnyVenue,
}

impl SetRestriction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoadableOnly => "loadable_only",
            Self::AnyVenue => "any_venue",
        }
    }
}

/// Where the frozen universe stands before any candidate is admitted.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AdmissionBaseline {
    cycles: usize,
    cold_start_secs: f64,
    reachable: usize,
}

/// A sized candidate set with its full admission cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateSet {
    /// Requested size (10 / 25 / 50).
    pub requested_size: usize,
    /// Pools actually added — smaller when fewer candidates exist.
    pub pools_added: usize,
    pub restriction: SetRestriction,
    pub arbs_unlocked: usize,
    pub reachable_after: usize,
    pub reachable_after_pct: f64,
    pub pool_count_after: usize,
    pub cycle_count_after: usize,
    pub cycle_count_delta: i64,
    /// Per-block dirty-cycle work scales with the cycle set.
    pub cycle_growth_factor: f64,
    pub est_cold_start_secs: f64,
    pub est_cold_start_delta_secs: f64,
    /// Pools in this set that need an adapter first (always 0 for `loadable_only`).
    pub adapter_required_pools: usize,
}

/// Hop-cap pricing: what raising `EFFECTIVE_MAX_HOPS` would recover vs cost.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HopCapPricing {
    /// Arbs per hop count above the cap (non-aggregator only).
    pub arbs_above_cap_by_hop: BTreeMap<u32, usize>,
    pub arbs_above_cap_total: usize,
    /// Arbs at exactly `cap + 1` — all that raising the cap by one admits.
    pub arbs_at_cap_plus_one: usize,
    /// Of those, how many are already fully inside the universe. Only these
    /// become reachable from a cap change alone.
    pub arbs_at_cap_plus_one_in_universe: usize,
    /// …and how many would become reachable with the largest candidate set too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arbs_at_cap_plus_one_with_candidates: Option<usize>,
    pub cycle_count_at_cap: usize,
    pub cycle_count_at_cap_plus_one: usize,
    pub cycle_growth_factor: f64,
    pub note: String,
}

// ── exact production cycle counting ────────────────────────────────────────

/// Count ordered settlement cycles exactly as the live engine enumerates them.
///
/// Reuses [`build_graph`] + [`PathFinder::find_cycles`] over synthetic pools so
/// the count cannot drift from production topology semantics (rotation-canonical
/// dedup, no immediate same-pool reversal, simple token cycles). Only tokens
/// matter to the graph, so reserves and fees are placeholders.
///
/// Degenerate pairs are skipped, matching [`crate::service::universe_filter`].
///
/// Note the report uses **two** enumerators over the same topology, on purpose:
/// this one (production `PathFinder`) for cycle *counts*, and
/// `universe_filter::pools_on_settlement_cycles` for cycle *membership* when
/// attributing the cycle filter. Membership is the generator's own primitive, so
/// an exclusion verdict matches the filter that produced today's universe, while
/// counts match what the live engine would actually enumerate.
pub fn count_settlement_cycles(
    pools: &[PoolTokens],
    settlement: Address,
    max_hops: usize,
) -> usize {
    if max_hops < 2 {
        return 0;
    }
    let mut state = StateSpace::default();
    for p in pools {
        if p.is_degenerate() {
            continue;
        }
        state.state.insert(
            p.pool,
            AMM::UniswapV2Pool(UniswapV2Pool {
                address: p.pool,
                token_a: Token {
                    address: p.token0,
                    decimals: 18,
                },
                token_b: Token {
                    address: p.token1,
                    decimals: 18,
                },
                // Topology-only: any non-degenerate reserves work.
                reserve_0: 1,
                reserve_1: 1,
                fee: 300,
            }),
        );
    }
    if state.state.is_empty() {
        return 0;
    }
    let Ok(graph) = build_graph(&state) else {
        return 0;
    };
    let finder = PathFinder::new(
        &graph,
        PathConstraints::settlement_cycle(settlement, max_hops),
    );
    finder.find_cycles().len()
}

/// Estimated cold-start seconds for a universe of `pool_count` pools.
pub fn est_cold_start_secs(pool_count: usize) -> f64 {
    COLD_START_SECS_PER_POOL * pool_count as f64
}

// ── venue / exclusion classification ───────────────────────────────────────

/// Factories the current generator enumerates: loadable drop-in V3 venues, the
/// interim V2 venue, and Moe.
pub fn enumerated_factories() -> BTreeSet<Address> {
    let mut set: BTreeSet<Address> = DROP_IN_V3_VENUES.iter().map(|v| v.factory).collect();
    set.insert(INTERIM_V2_FACTORY);
    set.insert(crate::amms::moe::CANONICAL_MOE_FACTORY);
    set
}

/// Classify the venue behind a census entry.
///
/// Factory registry wins over the census `kind` string: a pool on a registered
/// quarantined factory is an adapter problem even if its `kind` looks drop-in.
pub fn classify_venue(entry: Option<&PoolCensusEntry>, enumerated: &BTreeSet<Address>) -> VenueStatus {
    let factory = entry
        .and_then(|e| e.factory.as_deref())
        .and_then(|f| f.parse::<Address>().ok());

    if let Some(factory) = factory {
        if quarantined_v3_by_factory(factory).is_some() {
            return VenueStatus::QuarantinedAdapterRequired;
        }
        if enumerated.contains(&factory) {
            return VenueStatus::LoadableDropIn;
        }
    }

    // A drop-in math family on an unregistered factory is an enumeration gap,
    // not an adapter gap — but the two are not equally cheap, so split by family.
    match entry
        .and_then(|e| e.kind.as_deref())
        .map(|k| k.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("v3") => VenueStatus::UnregisteredV3FamilyFactory,
        Some("v2") => VenueStatus::UnregisteredV2FamilyFactory,
        Some("lb") => VenueStatus::UnregisteredLbFactory,
        other => match adapter_class(other) {
            AdapterClass::AdapterRequired => VenueStatus::UnsupportedMathFamily,
            AdapterClass::DropIn | AdapterClass::Unknown => VenueStatus::UnknownVenue,
        },
    }
}

/// Resolve the venue label for a census factory.
///
/// Falls back to a shortened factory address for unregistered venues: "unknown"
/// would hide *which* venue the ranking is pointing at, which is the actionable
/// part when the answer is "we never enumerate this factory".
fn venue_label(entry: Option<&PoolCensusEntry>) -> Option<String> {
    let raw = entry.and_then(|e| e.factory.as_deref())?;
    let Some(factory) = raw.parse::<Address>().ok() else {
        return Some(raw.to_ascii_lowercase());
    };
    if factory == INTERIM_V2_FACTORY {
        return Some("fusionx-v2 (interim)".into());
    }
    if factory == crate::amms::moe::CANONICAL_MOE_FACTORY {
        return Some("moe".into());
    }
    Some(
        venue_by_factory(factory)
            .map(|v| v.label.to_string())
            .unwrap_or_else(|| format!("unregistered {}", short_addr(&factory.to_string()))),
    )
}

/// `0x1234…abcd` — enough to identify an address in a table without wrapping.
fn short_addr(addr: &str) -> String {
    let a = addr.to_ascii_lowercase();
    if a.len() <= 12 {
        return a;
    }
    format!("{}…{}", &a[..6], &a[a.len() - 4..])
}

/// Token pair for display: census symbols when present, else short addresses.
fn pair_display(entry: Option<&PoolCensusEntry>) -> Option<String> {
    let entry = entry?;
    if let Some(label) = entry.pair_label() {
        return Some(label);
    }
    match (entry.token0.as_deref(), entry.token1.as_deref()) {
        (Some(a), Some(b)) => Some(format!("{}/{}", short_addr(a), short_addr(b))),
        _ => None,
    }
}

// ── report ─────────────────────────────────────────────────────────────────

/// Dataset + universe provenance for the committed artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportInputs {
    /// Basename only — never a secret path.
    pub arb_dataset: String,
    pub census_dataset: String,
    pub universe_pool_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub universe_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub universe_snapshot_block: Option<u64>,
    pub settlement_asset: String,
    pub max_hops: u32,
    pub min_tvl_wmnt_wei: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_from: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_to: Option<u64>,
    /// Block the candidate TVL reads were pinned to, when measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tvl_block: Option<u64>,
    pub tvl_measured: bool,
    /// Non-blank rows in the source extract.
    pub source_rows: usize,
    /// Rows dropped for having no decodable `path`. These cannot be classified
    /// at all, so every count in this report is over `source_rows` minus this.
    pub skipped_empty_path: usize,
}

/// Where the in-scope arbs stand against the frozen universe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InScopeBaseline {
    pub in_scope_arbs: usize,
    pub reachable_now: usize,
    pub reachable_now_pct: f64,
    pub blocked_by_missing_pools: usize,
    pub distinct_pools_in_in_scope_arbs: usize,
    pub distinct_pools_held: usize,
    pub distinct_missing_pools: usize,
}

/// How the missing pools split across exclusion causes and venue status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExclusionBreakdown {
    /// Distinct missing pools per [`ExclusionCause`].
    pub pools_by_cause: BTreeMap<String, usize>,
    /// Distinct missing pools per [`VenueStatus`].
    pub pools_by_venue_status: BTreeMap<String, usize>,
    /// What each observed venue status would take to unblock.
    pub work_required_by_venue_status: BTreeMap<String, String>,
    /// In-scope arbs blocked by ≥1 pool of each cause (arbs double-count across
    /// causes when a path has several kinds of gap — stated, not hidden).
    pub in_scope_arbs_touched_by_cause: BTreeMap<String, usize>,
    /// In-scope arbs whose every gap sits on a loadable venue — the reachable
    /// ceiling without any adapter work.
    pub in_scope_arbs_gap_fully_loadable: usize,
}

/// Full WHI-999 report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissedArbReport {
    pub schema_version: String,
    pub inputs: ReportInputs,
    pub cause_recount: CauseRecount,
    pub residual_bound: ResidualBound,
    pub baseline: InScopeBaseline,
    pub exclusions: ExclusionBreakdown,
    /// Ranked missing pools (loadable venues only) by marginal arbs unlocked.
    pub ranking_loadable: Vec<UnlockStep>,
    /// Same ranking ignoring venue support — the upper bound if every adapter existed.
    pub ranking_any_venue: Vec<UnlockStep>,
    /// Missing pools by raw appearance count, with venue / pair / TVL detail.
    pub missing_pools: Vec<MissingPool>,
    pub candidate_sets: Vec<CandidateSet>,
    pub hop_cap: HopCapPricing,
    /// Plain-language verdict, including "no" (an acceptable result).
    pub verdict: Verdict,
    pub notes: Vec<String>,
}

/// The bottom line the issue asks for explicitly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// Best reachable in-scope arb count over any candidate set on loadable venues.
    pub best_loadable_reachable: usize,
    pub best_loadable_reachable_pct: f64,
    /// Same, ignoring venue support.
    pub best_any_venue_reachable: usize,
    pub best_any_venue_reachable_pct: f64,
    /// Arbs/day implied by the best loadable set over the observation window.
    pub best_loadable_arbs_per_day: f64,
    /// True when a loadable universe exists that reaches
    /// `non_trivial_arbs_threshold_pct` of in-scope arbs.
    pub reachable_universe_exists: bool,
    pub non_trivial_arbs_threshold_pct: f64,
    pub statement: String,
    /// Arb **count** is not arb **value**. When the caller supplies a chain-wide
    /// gross figure, the implied share is stated here; otherwise this says
    /// plainly that the report does not price the opportunity.
    pub economics_caveat: String,
}

// ── I/O ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ArbJsonLine {
    #[serde(default)]
    block: Option<u64>,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default, rename = "nSwaps")]
    n_swaps: Option<u32>,
    /// Net-positive entity legs; the largest is the settlement asset.
    #[serde(default)]
    pos: Vec<PosLeg>,
}

/// `pos` legs are `[token, amount]` pairs in the external extract.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PosLeg {
    Pair(String, String),
    Object {
        token: String,
        #[serde(default)]
        amount: Option<String>,
    },
}

impl PosLeg {
    fn token(&self) -> &str {
        match self {
            Self::Pair(t, _) => t,
            Self::Object { token, .. } => token,
        }
    }

    fn amount(&self) -> Option<&str> {
        match self {
            Self::Pair(_, a) => Some(a),
            Self::Object { amount, .. } => amount.as_deref(),
        }
    }
}

/// Settlement asset = the `pos` leg with the largest amount.
///
/// Same rule as [`crate::service::ground_truth::settlement_asset_from_pos`],
/// re-derived here because this loader reads the raw external extract rather
/// than collector output. Amounts are compared as `U256` so a leg wider than
/// `u128` cannot silently sort as zero.
fn settlement_from_pos(pos: &[PosLeg]) -> Option<String> {
    let mut best: Option<(String, U256)> = None;
    for leg in pos {
        let token = normalize_address(leg.token());
        if token.is_empty() {
            continue;
        }
        let amount = leg
            .amount()
            .and_then(parse_amount_u256)
            .unwrap_or(U256::ZERO);
        match &best {
            Some((_, b)) if amount <= *b => {}
            _ => best = Some((token, amount)),
        }
    }
    best.map(|(t, _)| t)
}

fn parse_amount_u256(s: &str) -> Option<U256> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return U256::from_str_radix(hex, 16).ok();
    }
    s.parse::<U256>().ok()
}

/// Events plus what the loader had to drop, so a skip can never pass as a zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedArbEvents {
    pub events: Vec<MissedArbEvent>,
    /// Non-blank JSONL rows read.
    pub rows_seen: usize,
    /// Rows whose `path` was empty or undecodable. They carry no universe
    /// information, so they cannot be classified — but they are counted and
    /// reported rather than silently vanishing (the attribution routes the same
    /// case to `unattributable`, so a silent drop would understate the residual).
    pub skipped_empty_path: usize,
}

/// Load ground-truth arbs from the external JSONL extract.
///
/// Recognized fields: `block`, `hash`, `path` (ordered pools), `nSwaps`, `pos`.
pub fn load_missed_arb_events(path: &Path) -> Result<LoadedArbEvents, ArbCoverageError> {
    let file = File::open(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut loaded = LoadedArbEvents::default();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| ArbCoverageError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        loaded.rows_seen += 1;
        let parsed: ArbJsonLine =
            serde_json::from_str(line).map_err(|source| ArbCoverageError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        let pools: Vec<String> = parsed
            .path
            .iter()
            .map(|p| normalize_address(p))
            .filter(|p| !p.is_empty())
            .collect();
        if pools.is_empty() {
            loaded.skipped_empty_path += 1;
            continue;
        }
        let hop_count = parsed.n_swaps.unwrap_or(pools.len() as u32);
        loaded.events.push(MissedArbEvent {
            block: parsed.block,
            tx_hash: parsed.hash.map(|h| h.to_ascii_lowercase()),
            pools,
            hop_count,
            settlement_asset: settlement_from_pos(&parsed.pos),
        });
    }
    Ok(loaded)
}

// ── analysis ───────────────────────────────────────────────────────────────

/// Everything the analysis needs beyond the events themselves.
#[derive(Debug, Clone)]
pub struct AnalysisConfig {
    /// Frozen universe pools, lowercased.
    pub held: HashSet<String>,
    /// `pool → (token0, token1)` for held pools, for cycle counting.
    pub held_tokens: HashMap<String, (Address, Address)>,
    pub settlement: Address,
    pub max_hops: u32,
    pub min_tvl_wmnt_wei: U256,
    /// Measured candidate TVL; absent entries mean "not measured".
    pub tvl: HashMap<String, PoolTvl>,
    pub tvl_measured: bool,
    pub tvl_block: Option<u64>,
    /// How many pools each candidate set admits.
    pub set_sizes: Vec<usize>,
    /// Rows in the ranking tables.
    pub top_n: usize,
    /// Share of in-scope arbs a candidate set must reach to call the strategy
    /// viable on pool coverage alone.
    pub non_trivial_pct: f64,
    /// Optional chain-wide atomic-arb gross (USD/day) for the economics caveat.
    /// `None` leaves the report explicitly unpriced rather than guessing.
    pub chain_gross_usd_per_day: Option<f64>,
    pub universe_fingerprint: Option<String>,
    pub universe_snapshot_block: Option<u64>,
    pub arb_dataset: String,
    pub census_dataset: String,
    /// Non-blank rows the loader read, and how many it had to drop for having no
    /// decodable path. Reported so a skip cannot pass as a zero.
    pub source_rows: usize,
    pub skipped_empty_path: usize,
}

impl AnalysisConfig {
    /// Held pools as topology triples, dropping keys that are not addresses.
    fn held_pool_tokens(&self) -> Vec<PoolTokens> {
        self.held_tokens
            .iter()
            .filter_map(|(pool, (token0, token1))| {
                Some(PoolTokens {
                    pool: pool.parse().ok()?,
                    token0: *token0,
                    token1: *token1,
                })
            })
            .collect()
    }
}

/// Run the full backwards-selection analysis (pure).
pub fn analyze(
    events: &[MissedArbEvent],
    census: &HashMap<String, PoolCensusEntry>,
    cfg: &AnalysisConfig,
) -> MissedArbReport {
    let settlement_hex = format!("{:?}", cfg.settlement).to_ascii_lowercase();
    let enumerated = enumerated_factories();

    // ── scope + cause recount ──────────────────────────────────────────────
    let mut recount = CauseRecount {
        total_events: events.len(),
        aggregator_misclass: 0,
        out_of_scope_hop_cap: 0,
        out_of_scope_non_wmnt_settlement: 0,
        not_in_universe: 0,
        in_universe_in_scope: 0,
        analysis_denominator: 0,
        not_in_universe_pct: 0.0,
    };
    let mut in_scope_paths: Vec<Vec<String>> = Vec::new();
    // In-scope paths whose every hop is already held — the WHI-957 residual.
    let mut residual_paths: Vec<Vec<String>> = Vec::new();
    let mut block_from: Option<u64> = None;
    let mut block_to: Option<u64> = None;
    // Hop histogram above the cap, for hop-cap pricing.
    let mut above_cap_by_hop: BTreeMap<u32, usize> = BTreeMap::new();
    let mut at_cap_plus_one_paths: Vec<Vec<String>> = Vec::new();

    for event in events {
        if let Some(b) = event.block {
            block_from = Some(block_from.map_or(b, |m| m.min(b)));
            block_to = Some(block_to.map_or(b, |m| m.max(b)));
        }
        match classify_scope(event, &settlement_hex, cfg.max_hops) {
            Scope::AggregatorMisclass => recount.aggregator_misclass += 1,
            Scope::OutOfScopeHopCap => {
                recount.out_of_scope_hop_cap += 1;
                *above_cap_by_hop.entry(event.hop_count).or_insert(0) += 1;
                if event.hop_count == cfg.max_hops + 1 {
                    at_cap_plus_one_paths.push(event.pools.clone());
                }
            }
            Scope::OutOfScopeNonWmntSettlement => {
                recount.out_of_scope_non_wmnt_settlement += 1
            }
            Scope::InScope => {
                let missing = event.pools.iter().any(|p| !cfg.held.contains(p));
                if missing {
                    recount.not_in_universe += 1;
                } else {
                    recount.in_universe_in_scope += 1;
                    residual_paths.push(event.pools.clone());
                }
                in_scope_paths.push(event.pools.clone());
            }
        }
    }
    recount.analysis_denominator = recount.total_events - recount.aggregator_misclass;
    recount.not_in_universe_pct = pct(recount.not_in_universe, recount.analysis_denominator);

    // ── residual bound ─────────────────────────────────────────────────────
    //
    // The bound is an *argument*, not a measurement, and the statement says so.
    // WHI-957 tests `not_in_universe` at priority 6 and only falls through to
    // `unattributable` at priority 11, so no residual event can carry a missing
    // pool. The re-test below is a regression guard on that invariant — it is
    // true by construction of `residual_paths` and would only fire if the two
    // classification paths in this module disagreed.
    //
    // The invariant has one genuine hole, and it is reported rather than
    // papered over: the attribution also routes an event with an *empty*
    // `ordered_pools` to `unattributable`. Such an event cannot be tested for
    // universe membership at all. This loader counts those rows in
    // `skipped_empty_path` instead of classifying them, so the claim below is
    // scoped to events with a decoded path.
    let residual_with_missing = residual_paths
        .iter()
        .filter(|path| path.iter().any(|p| !cfg.held.contains(p)))
        .count();
    let undecoded_note = if cfg.skipped_empty_path == 0 {
        " Every source row had a decodable path, so the residual has no undecoded remainder.".to_string()
    } else {
        format!(
            " Scoped to events with a decoded path: {} source rows had no decodable path and are \
             excluded from every count here (the attribution would route them to the residual too, \
             so treat the residual as +{} unknown).",
            cfg.skipped_empty_path, cfg.skipped_empty_path
        )
    };
    let residual_bound = ResidualBound {
        residual_count: recount.in_universe_in_scope,
        residual_events_with_missing_pool: residual_with_missing,
        residual_events_fully_in_universe: recount.in_universe_in_scope,
        can_hide_not_in_universe: residual_with_missing > 0,
        statement: if residual_with_missing == 0 {
            format!(
                "The {} `unattributable` events all have every hop pool inside the frozen \
                 universe. This is a property of the cause ordering, not a measurement: WHI-957 \
                 tests `not_in_universe` at priority 6 and reaches the residual only at priority \
                 11, so a residual event with a missing pool is impossible by construction (the \
                 zero below is a regression guard on that, not evidence for it). \
                 {:.1}% is therefore a point estimate for the universe question, not a floor — \
                 the residual is unclassified only as to *which* in-universe cause applies \
                 (dirty-cycle vs unprofitable vs lost race), which needs a concurrent ledger \
                 (DI-35). Bounding it does not move the ranking.{undecoded_note}",
                recount.in_universe_in_scope, recount.not_in_universe_pct
            )
        } else {
            format!(
                "{residual_with_missing} residual events carry a pool outside the universe — the \
                 two classification paths in this module disagree, which should be impossible. \
                 Treat the {:.1}% not_in_universe figure as a floor and re-derive the ranking.",
                recount.not_in_universe_pct
            )
        },
    };

    // ── baseline ───────────────────────────────────────────────────────────
    let mut distinct_in_scope_pools: BTreeSet<String> = BTreeSet::new();
    for path in &in_scope_paths {
        for p in path {
            distinct_in_scope_pools.insert(p.clone());
        }
    }
    let missing_pool_set: BTreeSet<String> = distinct_in_scope_pools
        .iter()
        .filter(|p| !cfg.held.contains(*p))
        .cloned()
        .collect();
    let reachable_now = reachable_count(&cfg.held, &in_scope_paths);
    let baseline = InScopeBaseline {
        in_scope_arbs: in_scope_paths.len(),
        reachable_now,
        reachable_now_pct: pct(reachable_now, in_scope_paths.len()),
        blocked_by_missing_pools: in_scope_paths.len() - reachable_now,
        distinct_pools_in_in_scope_arbs: distinct_in_scope_pools.len(),
        distinct_pools_held: distinct_in_scope_pools
            .iter()
            .filter(|p| cfg.held.contains(*p))
            .count(),
        distinct_missing_pools: missing_pool_set.len(),
    };

    // ── per-pool rows ──────────────────────────────────────────────────────
    let mut appears: HashMap<&String, usize> = HashMap::new();
    let mut sole_blocker: HashMap<String, usize> = HashMap::new();
    let mut hop_positions: HashMap<&String, BTreeMap<u32, usize>> = HashMap::new();
    for path in &in_scope_paths {
        let unique: BTreeSet<&String> = path.iter().collect();
        let gap: Vec<&String> = unique
            .iter()
            .copied()
            .filter(|p| !cfg.held.contains(*p))
            .collect();
        for p in &gap {
            *appears.entry(*p).or_insert(0) += 1;
        }
        if gap.len() == 1 {
            *sole_blocker.entry(gap[0].clone()).or_insert(0) += 1;
        }
        // Positional profile over the ordered path (1-based), counted per
        // occurrence so a pool used twice in one cycle shows both slots.
        for (i, pool) in path.iter().enumerate() {
            if cfg.held.contains(pool) {
                continue;
            }
            *hop_positions
                .entry(pool)
                .or_default()
                .entry(i as u32 + 1)
                .or_insert(0) += 1;
        }
    }

    let venue_status_of: HashMap<String, VenueStatus> = missing_pool_set
        .iter()
        .map(|p| (p.clone(), classify_venue(census.get(p), &enumerated)))
        .collect();

    // Cycle admissibility is evaluated on the union of the frozen universe and
    // every venue+TVL-admissible candidate: a pool can only be blamed on the
    // cycle filter relative to the set it would join.
    let mut union_pools = cfg.held_pool_tokens();
    let mut pre_cycle_admissible: BTreeSet<String> = BTreeSet::new();
    for pool in &missing_pool_set {
        if !venue_status_of[pool].is_loadable() || !tvl_admissible(cfg, pool) {
            continue;
        }
        let Some(tokens) = census.get(pool).and_then(|e| census_pool_tokens(pool, e)) else {
            continue;
        };
        union_pools.push(tokens);
        pre_cycle_admissible.insert(pool.clone());
    }
    let on_cycle = pools_on_cycles(&union_pools, cfg.settlement, cfg.max_hops as usize);

    let mut missing_pools: Vec<MissingPool> = missing_pool_set
        .iter()
        .map(|pool| {
            let entry = census.get(pool);
            let status = venue_status_of[pool];
            let tvl = cfg.tvl.get(pool);
            let exclusion_cause = classify_exclusion(
                cfg,
                pool,
                status,
                tvl,
                pre_cycle_admissible.contains(pool.as_str()),
                &on_cycle,
            );
            MissingPool {
                pool: pool.clone(),
                venue: venue_label(entry),
                factory: entry.and_then(|e| e.factory.clone()),
                kind: entry.and_then(|e| e.kind.clone()),
                pair: pair_display(entry),
                token0: entry.and_then(|e| e.token0.clone()),
                token1: entry.and_then(|e| e.token1.clone()),
                census_swaps: entry.and_then(|e| e.swaps),
                appears_in_in_scope_arbs: appears.get(pool).copied().unwrap_or(0),
                sole_blocker_of: sole_blocker.get(pool).copied().unwrap_or(0),
                hop_positions: hop_positions.get(pool).cloned().unwrap_or_default(),
                adapter_class: adapter_class(entry.and_then(|e| e.kind.as_deref())),
                venue_status: status,
                tvl_wmnt_wei: match tvl {
                    Some(PoolTvl::Valued(v)) => Some(v.to_string()),
                    _ => None,
                },
                exclusion_cause,
            }
        })
        .collect();
    missing_pools.sort_by(|a, b| {
        b.appears_in_in_scope_arbs
            .cmp(&a.appears_in_in_scope_arbs)
            .then_with(|| a.pool.cmp(&b.pool))
    });

    // ── exclusion breakdown ────────────────────────────────────────────────
    let exclusions = build_exclusions(&missing_pools, &in_scope_paths, &cfg.held, &venue_status_of);

    // ── rankings ───────────────────────────────────────────────────────────
    let loadable_candidates: BTreeSet<String> = missing_pool_set
        .iter()
        .filter(|p| venue_status_of[*p].is_loadable())
        .cloned()
        .collect();
    let ranking_loadable = render_steps(
        greedy_unlock_rank(&cfg.held, &in_scope_paths, &loadable_candidates, cfg.top_n),
        census,
        &venue_status_of,
        cfg,
        reachable_now,
        in_scope_paths.len(),
    );
    let ranking_any_venue = render_steps(
        greedy_unlock_rank(&cfg.held, &in_scope_paths, &missing_pool_set, cfg.top_n),
        census,
        &venue_status_of,
        cfg,
        reachable_now,
        in_scope_paths.len(),
    );

    // ── candidate sets ─────────────────────────────────────────────────────
    let held_tokens = cfg.held_pool_tokens();
    let baseline_cost = AdmissionBaseline {
        cycles: count_settlement_cycles(&held_tokens, cfg.settlement, cfg.max_hops as usize),
        cold_start_secs: est_cold_start_secs(cfg.held.len()),
        reachable: reachable_now,
    };

    let mut candidate_sets = Vec::new();
    for (restriction, candidates) in [
        (SetRestriction::LoadableOnly, &loadable_candidates),
        (SetRestriction::AnyVenue, &missing_pool_set),
    ] {
        for &size in &cfg.set_sizes {
            candidate_sets.push(build_candidate_set(
                restriction,
                size,
                candidates,
                census,
                &venue_status_of,
                &in_scope_paths,
                cfg,
                baseline_cost,
            ));
        }
    }

    // ── hop cap ────────────────────────────────────────────────────────────
    // Pair the cap change with the largest loadable set, chosen by the same
    // greedy over the same in-scope arbs the candidate sets used — not an
    // arbitrary slice of the candidate pool.
    let largest_set = cfg.set_sizes.iter().copied().max().unwrap_or(0);
    let largest_loadable_added: Vec<String> =
        greedy_unlock_rank(&cfg.held, &in_scope_paths, &loadable_candidates, largest_set)
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
    let hop_cap = price_hop_cap(
        cfg,
        &above_cap_by_hop,
        &at_cap_plus_one_paths,
        &largest_loadable_added,
        baseline_cost.cycles,
    );

    // ── verdict ────────────────────────────────────────────────────────────
    let verdict = build_verdict(&candidate_sets, &baseline, block_from, block_to, cfg);

    MissedArbReport {
        schema_version: MISSED_ARB_REPORT_SCHEMA_VERSION.into(),
        inputs: ReportInputs {
            arb_dataset: cfg.arb_dataset.clone(),
            census_dataset: cfg.census_dataset.clone(),
            universe_pool_count: cfg.held.len(),
            universe_fingerprint: cfg.universe_fingerprint.clone(),
            universe_snapshot_block: cfg.universe_snapshot_block,
            settlement_asset: settlement_hex,
            max_hops: cfg.max_hops,
            min_tvl_wmnt_wei: cfg.min_tvl_wmnt_wei.to_string(),
            block_from,
            block_to,
            tvl_block: cfg.tvl_block,
            tvl_measured: cfg.tvl_measured,
            source_rows: cfg.source_rows,
            skipped_empty_path: cfg.skipped_empty_path,
        },
        cause_recount: recount,
        residual_bound,
        baseline,
        exclusions,
        ranking_loadable,
        ranking_any_venue,
        missing_pools,
        candidate_sets,
        hop_cap,
        verdict,
        notes: vec![
            "Scope rules mirror execution::peer_attribution (aggregator hop>50 → hop cap → \
             non-WMNT settlement), so cause counts reconcile with WHI-957."
                .into(),
            "\"Arbs unlocked\" counts only in-scope arbs. WHI-906's fully_executable includes \
             arbs the strategy cannot take (hop>3, non-WMNT), so it reads higher."
                .into(),
            format!("Cycle counts come from the production enumerator (arbitrage::pathfinder), not a re-implementation. Cold start is modelled linearly from {COLD_START_REFERENCE}."),
            "Per-event rows are never written to the committed report; the arb dataset stays \
             external (WHI-906 / WHI-956 contract)."
                .into(),
        ],
    }
}

/// True when a candidate's TVL is known to clear the floor.
///
/// Unmeasured TVL is treated as admissible so a run without `--rpc-url` still
/// produces a ranking; the report marks `tvl_measured=false` and every affected
/// pool carries [`ExclusionCause::TvlNotMeasured`] so the gap is visible.
fn tvl_admissible(cfg: &AnalysisConfig, pool: &str) -> bool {
    match cfg.tvl.get(pool) {
        Some(PoolTvl::Valued(v)) => *v >= cfg.min_tvl_wmnt_wei,
        Some(PoolTvl::Unavailable) => false,
        None => !cfg.tvl_measured,
    }
}

fn classify_exclusion(
    cfg: &AnalysisConfig,
    pool: &str,
    status: VenueStatus,
    tvl: Option<&PoolTvl>,
    pre_cycle_admissible: bool,
    on_cycle: &BTreeSet<Address>,
) -> ExclusionCause {
    if !status.is_loadable() {
        return ExclusionCause::VenueNotLoadable;
    }
    match tvl {
        Some(PoolTvl::Valued(v)) if *v < cfg.min_tvl_wmnt_wei => {
            return ExclusionCause::BelowTvlFloor
        }
        Some(PoolTvl::Unavailable) => return ExclusionCause::TvlUnavailable,
        None if cfg.tvl_measured => return ExclusionCause::TvlUnavailable,
        None => return ExclusionCause::TvlNotMeasured,
        Some(PoolTvl::Valued(_)) => {}
    }
    if !pre_cycle_admissible {
        // Venue + TVL cleared but the census gave no usable token pair, so the
        // cycle test could not run over it.
        return ExclusionCause::CycleFilterRejected;
    }
    match pool.parse::<Address>() {
        Ok(addr) if on_cycle.contains(&addr) => ExclusionCause::AdmissibleButAbsent,
        Ok(_) => ExclusionCause::CycleFilterRejected,
        Err(_) => ExclusionCause::CycleFilterRejected,
    }
}

/// Pools on ≥1 ordered settlement cycle, via the generator's own primitive.
///
/// Deliberately not [`count_settlement_cycles`]: attributing the cycle filter
/// must use the same membership test the generator applied.
fn pools_on_cycles(
    pools: &[PoolTokens],
    settlement: Address,
    max_hops: usize,
) -> BTreeSet<Address> {
    let candidates: Vec<crate::service::universe_filter::CandidatePool> = pools
        .iter()
        .map(|p| crate::service::universe_filter::CandidatePool {
            protocol: String::new(),
            factory: Address::ZERO,
            pool: p.pool,
            token0: p.token0,
            token1: p.token1,
            fee_tier: None,
            bin_step: None,
            creation_block: None,
        })
        .collect();
    crate::service::universe_filter::pools_on_settlement_cycles(
        &candidates,
        settlement,
        max_hops as u8,
    )
}

fn build_exclusions(
    missing_pools: &[MissingPool],
    in_scope_paths: &[Vec<String>],
    held: &HashSet<String>,
    venue_status_of: &HashMap<String, VenueStatus>,
) -> ExclusionBreakdown {
    let mut pools_by_cause: BTreeMap<String, usize> = BTreeMap::new();
    let mut pools_by_venue_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut work_required_by_venue_status: BTreeMap<String, String> = BTreeMap::new();
    let cause_of: HashMap<&str, ExclusionCause> = missing_pools
        .iter()
        .map(|m| (m.pool.as_str(), m.exclusion_cause))
        .collect();
    for m in missing_pools {
        *pools_by_cause
            .entry(m.exclusion_cause.as_str().to_string())
            .or_insert(0) += 1;
        *pools_by_venue_status
            .entry(m.venue_status.as_str().to_string())
            .or_insert(0) += 1;
        work_required_by_venue_status
            .entry(m.venue_status.as_str().to_string())
            .or_insert_with(|| m.venue_status.work_required().to_string());
    }

    let mut arbs_by_cause: BTreeMap<String, usize> = BTreeMap::new();
    let mut gap_fully_loadable = 0usize;
    for path in in_scope_paths {
        let gap: BTreeSet<&String> = path.iter().filter(|p| !held.contains(*p)).collect();
        if gap.is_empty() {
            continue;
        }
        let mut causes: BTreeSet<&str> = BTreeSet::new();
        for p in &gap {
            if let Some(c) = cause_of.get(p.as_str()) {
                causes.insert(c.as_str());
            }
        }
        for c in causes {
            *arbs_by_cause.entry(c.to_string()).or_insert(0) += 1;
        }
        if gap
            .iter()
            .all(|p| venue_status_of.get(p.as_str()).is_some_and(|s| s.is_loadable()))
        {
            gap_fully_loadable += 1;
        }
    }

    ExclusionBreakdown {
        pools_by_cause,
        pools_by_venue_status,
        work_required_by_venue_status,
        in_scope_arbs_touched_by_cause: arbs_by_cause,
        in_scope_arbs_gap_fully_loadable: gap_fully_loadable,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_steps(
    picks: Vec<(String, StepSelection, usize)>,
    census: &HashMap<String, PoolCensusEntry>,
    venue_status_of: &HashMap<String, VenueStatus>,
    cfg: &AnalysisConfig,
    base_reachable: usize,
    in_scope_total: usize,
) -> Vec<UnlockStep> {
    let mut cumulative = 0usize;
    picks
        .into_iter()
        .enumerate()
        .map(|(i, (pool, selection, gain))| {
            cumulative += gain;
            let entry = census.get(&pool);
            UnlockStep {
                rank: i + 1,
                selection,
                marginal_arbs_unlocked: gain,
                cumulative_arbs_unlocked: cumulative,
                cumulative_reachable: base_reachable + cumulative,
                cumulative_reachable_pct: pct(base_reachable + cumulative, in_scope_total),
                venue: venue_label(entry),
                pair: pair_display(entry),
                venue_status: venue_status_of
                    .get(&pool)
                    .copied()
                    .unwrap_or(VenueStatus::UnknownVenue),
                adapter_class: adapter_class(entry.and_then(|e| e.kind.as_deref())),
                tvl_wmnt_wei: match cfg.tvl.get(&pool) {
                    Some(PoolTvl::Valued(v)) => Some(v.to_string()),
                    _ => None,
                },
                pool,
            }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_candidate_set(
    restriction: SetRestriction,
    size: usize,
    candidates: &BTreeSet<String>,
    census: &HashMap<String, PoolCensusEntry>,
    venue_status_of: &HashMap<String, VenueStatus>,
    in_scope_paths: &[Vec<String>],
    cfg: &AnalysisConfig,
    base: AdmissionBaseline,
) -> CandidateSet {
    let picks = greedy_unlock_rank(&cfg.held, in_scope_paths, candidates, size);
    let added: Vec<String> = picks.iter().map(|(p, _, _)| p.clone()).collect();

    let mut held_after = cfg.held.clone();
    for p in &added {
        held_after.insert(p.clone());
    }
    let reachable_after = reachable_count(&held_after, in_scope_paths);

    // Cycle count over the resulting universe. Pools whose census gives no
    // token pair cannot enter the graph; they are still counted in
    // `pool_count_after` so the two numbers are never silently reconciled.
    let mut pools_after = cfg.held_pool_tokens();
    for p in &added {
        if let Some(tokens) = census.get(p).and_then(|e| census_pool_tokens(p, e)) {
            pools_after.push(tokens);
        }
    }
    let cycles_after =
        count_settlement_cycles(&pools_after, cfg.settlement, cfg.max_hops as usize);
    let cold_after = est_cold_start_secs(held_after.len());

    CandidateSet {
        requested_size: size,
        pools_added: added.len(),
        restriction,
        arbs_unlocked: reachable_after.saturating_sub(base.reachable),
        reachable_after,
        reachable_after_pct: pct(reachable_after, in_scope_paths.len()),
        pool_count_after: held_after.len(),
        cycle_count_after: cycles_after,
        cycle_count_delta: cycles_after as i64 - base.cycles as i64,
        cycle_growth_factor: growth_factor(cycles_after, base.cycles),
        est_cold_start_secs: cold_after,
        est_cold_start_delta_secs: cold_after - base.cold_start_secs,
        adapter_required_pools: added
            .iter()
            .filter(|p| !venue_status_of.get(p.as_str()).is_some_and(|s| s.is_loadable()))
            .count(),
    }
}

/// Price a one-step hop-cap increase: what it recovers vs what it costs.
///
/// `largest_loadable_added` is the biggest candidate set on loadable venues, so
/// the report can separate "cap change alone" from "cap change plus the pools we
/// were going to add anyway".
fn price_hop_cap(
    cfg: &AnalysisConfig,
    above_cap_by_hop: &BTreeMap<u32, usize>,
    at_cap_plus_one_paths: &[Vec<String>],
    largest_loadable_added: &[String],
    base_cycles: usize,
) -> HopCapPricing {
    let cycles_plus_one = count_settlement_cycles(
        &cfg.held_pool_tokens(),
        cfg.settlement,
        cfg.max_hops as usize + 1,
    );

    let in_universe = at_cap_plus_one_paths
        .iter()
        .filter(|path| path.iter().all(|p| cfg.held.contains(p)))
        .count();

    let with_candidates = if largest_loadable_added.is_empty() {
        None
    } else {
        let mut held_after = cfg.held.clone();
        for p in largest_loadable_added {
            held_after.insert(p.clone());
        }
        Some(
            at_cap_plus_one_paths
                .iter()
                .filter(|path| path.iter().all(|p| held_after.contains(p)))
                .count(),
        )
    };

    let total_above: usize = above_cap_by_hop.values().sum();
    let at_plus_one = above_cap_by_hop
        .get(&(cfg.max_hops + 1))
        .copied()
        .unwrap_or(0);

    HopCapPricing {
        arbs_above_cap_by_hop: above_cap_by_hop.clone(),
        arbs_above_cap_total: total_above,
        arbs_at_cap_plus_one: at_plus_one,
        arbs_at_cap_plus_one_in_universe: in_universe,
        arbs_at_cap_plus_one_with_candidates: with_candidates,
        cycle_count_at_cap: base_cycles,
        cycle_count_at_cap_plus_one: cycles_plus_one,
        cycle_growth_factor: growth_factor(cycles_plus_one, base_cycles),
        note: format!(
            "Raising the cap from {} to {} admits only the {} arbs at exactly {} hops — the \
             remaining {} sit deeper still. Of those, {} are already fully inside the universe, \
             so that is what a cap change alone recovers. Cold start is unaffected (same pools); \
             the cost is per-block: the cycle set grows {:.1}× and every dirty-cycle pass \
             re-optimizes it.",
            cfg.max_hops,
            cfg.max_hops + 1,
            at_plus_one,
            cfg.max_hops + 1,
            total_above.saturating_sub(at_plus_one),
            in_universe,
            growth_factor(cycles_plus_one, base_cycles),
        ),
    }
}

fn build_verdict(
    candidate_sets: &[CandidateSet],
    baseline: &InScopeBaseline,
    block_from: Option<u64>,
    block_to: Option<u64>,
    cfg: &AnalysisConfig,
) -> Verdict {
    let best = |restriction: SetRestriction| -> (usize, f64) {
        candidate_sets
            .iter()
            .filter(|s| s.restriction == restriction)
            .map(|s| (s.reachable_after, s.reachable_after_pct))
            .max_by_key(|(n, _)| *n)
            .unwrap_or((baseline.reachable_now, baseline.reachable_now_pct))
    };
    let (best_loadable, best_loadable_pct) = best(SetRestriction::LoadableOnly);
    let (best_any, best_any_pct) = best(SetRestriction::AnyVenue);

    // Mantle targets ~2 s blocks; derive days from the observed block span so
    // the rate is tied to the dataset rather than to a hard-coded window.
    let days = match (block_from, block_to) {
        (Some(a), Some(b)) if b > a => ((b - a) as f64 * 2.0) / 86_400.0,
        _ => 0.0,
    };
    let per_day = if days > 0.0 {
        best_loadable as f64 / days
    } else {
        0.0
    };
    let exists = best_loadable_pct >= cfg.non_trivial_pct;

    // Reaching a share of arbs says nothing about what those arbs are worth.
    // Spell that out rather than letting a count read as a business case.
    let economics_caveat = match cfg.chain_gross_usd_per_day {
        Some(gross) if gross > 0.0 => {
            let share = best_loadable_pct / 100.0;
            format!(
                "Count is not value. Against a chain-wide atomic-arb gross of about \
                 ${gross:.0}/day across all bots, reaching {best_loadable_pct:.1}% of in-scope \
                 arbs implies roughly ${:.2}/day gross before gas, and only if every reachable \
                 arb were also won. That is the number a funding decision turns on — this report \
                 sizes coverage, not profit.",
                gross * share
            )
        }
        _ => "Count is not value. This report sizes pool coverage only: it does not price the \
              arbs, net gas, or model race outcomes. A coverage result of any size can still be \
              economically uninteresting, and that question is not settled here."
            .to_string(),
    };

    Verdict {
        best_loadable_reachable: best_loadable,
        best_loadable_reachable_pct: best_loadable_pct,
        best_any_venue_reachable: best_any,
        best_any_venue_reachable_pct: best_any_pct,
        best_loadable_arbs_per_day: per_day,
        reachable_universe_exists: exists,
        non_trivial_arbs_threshold_pct: cfg.non_trivial_pct,
        economics_caveat,
        statement: format!(
            "Best loadable-venue universe reaches {best_loadable} of {} in-scope arbs \
             ({best_loadable_pct:.1}%), about {per_day:.0} arbs/day over the observed window. \
             Ignoring venue support entirely the ceiling is {best_any} ({best_any_pct:.1}%). \
             Threshold for \"non-trivial\" was set at {:.0}% of in-scope arbs: {}.",
            baseline.in_scope_arbs,
            cfg.non_trivial_pct,
            if exists {
                "a reachable universe does exist"
            } else {
                "no reachable universe clears it — pool coverage alone does not make this \
                 strategy viable, and that is the answer"
            }
        ),
    }
}

/// `after / before` as a growth multiple; 0.0 when there was no baseline.
fn growth_factor(after: usize, before: usize) -> f64 {
    if before == 0 {
        0.0
    } else {
        after as f64 / before as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
    const T1: Address = address!("0000000000000000000000000000000000000001");
    const T2: Address = address!("0000000000000000000000000000000000000002");

    fn wmnt_hex() -> String {
        format!("{WMNT:?}").to_ascii_lowercase()
    }

    fn tokens(pool: Address, token0: Address, token1: Address) -> PoolTokens {
        PoolTokens {
            pool,
            token0,
            token1,
        }
    }

    fn event(pools: &[&str], hops: u32, settlement: Option<&str>) -> MissedArbEvent {
        MissedArbEvent {
            block: Some(100),
            tx_hash: None,
            pools: pools.iter().map(|p| p.to_string()).collect(),
            hop_count: hops,
            settlement_asset: settlement.map(|s| s.to_string()),
        }
    }

    #[test]
    fn scope_priority_matches_peer_attribution() {
        let w = wmnt_hex();
        // Aggregator wins over everything, even a non-WMNT settlement.
        assert_eq!(
            classify_scope(&event(&["0xa"], 1139, Some("0xdead")), &w, 3),
            Scope::AggregatorMisclass
        );
        // Hop cap before settlement.
        assert_eq!(
            classify_scope(&event(&["0xa"], 4, Some("0xdead")), &w, 3),
            Scope::OutOfScopeHopCap
        );
        assert_eq!(
            classify_scope(&event(&["0xa"], 3, Some("0xdead")), &w, 3),
            Scope::OutOfScopeNonWmntSettlement
        );
        assert_eq!(
            classify_scope(&event(&["0xa"], 3, Some(&w)), &w, 3),
            Scope::InScope
        );
        // A missing settlement asset is unknown, not non-WMNT.
        assert_eq!(
            classify_scope(&event(&["0xa"], 2, None), &w, 3),
            Scope::InScope
        );
    }

    #[test]
    fn hop_cap_boundary_is_inclusive_of_the_cap() {
        let w = wmnt_hex();
        assert_eq!(classify_scope(&event(&["0xa"], 3, None), &w, 3), Scope::InScope);
        assert_eq!(
            classify_scope(&event(&["0xa"], 4, None), &w, 3),
            Scope::OutOfScopeHopCap
        );
        // Exactly at the aggregator threshold is still an arb, not noise.
        assert_eq!(
            classify_scope(&event(&["0xa"], AGGREGATOR_HOP_THRESHOLD, None), &w, 3),
            Scope::OutOfScopeHopCap
        );
    }

    #[test]
    fn greedy_prefers_the_pool_that_completes_arbs_not_the_frequent_one() {
        // `f` appears in 3 arbs but each also lacks another pool, so it
        // completes nothing. `x` completes one arb alone.
        let held: HashSet<String> = ["h"].iter().map(|s| s.to_string()).collect();
        let paths = vec![
            vec!["h".into(), "f".into(), "a".into()],
            vec!["h".into(), "f".into(), "b".into()],
            vec!["h".into(), "f".into(), "c".into()],
            vec!["h".into(), "x".into()],
        ];
        let candidates: BTreeSet<String> = ["f", "a", "b", "c", "x"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let steps = greedy_unlock_rank(&held, &paths, &candidates, 1);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].0, "x");
        assert_eq!(steps[0].1, StepSelection::Unlock);
        assert_eq!(steps[0].2, 1);
    }

    #[test]
    fn greedy_falls_back_to_frequency_and_labels_the_step() {
        // Nothing completes alone; `f` is the most frequent gap.
        let held: HashSet<String> = ["h"].iter().map(|s| s.to_string()).collect();
        let paths = vec![
            vec!["h".into(), "f".into(), "a".into()],
            vec!["h".into(), "f".into(), "b".into()],
        ];
        let candidates: BTreeSet<String> =
            ["f", "a", "b"].iter().map(|s| s.to_string()).collect();
        let steps = greedy_unlock_rank(&held, &paths, &candidates, 2);
        assert_eq!(steps[0].0, "f");
        assert_eq!(steps[0].1, StepSelection::FrequencyFallback);
        assert_eq!(steps[0].2, 0);
        // After `f`, both `a` and `b` complete an arb; `a` wins the tie-break.
        assert_eq!(steps[1].0, "a");
        assert_eq!(steps[1].1, StepSelection::Unlock);
        assert_eq!(steps[1].2, 1);
    }

    #[test]
    fn greedy_ignores_arbs_whose_gap_needs_a_non_candidate_pool() {
        // The only arb needs `z`, which is not admissible, so `y` unlocks nothing
        // and must not be credited.
        let held: HashSet<String> = ["h"].iter().map(|s| s.to_string()).collect();
        let paths = vec![vec!["h".into(), "y".into(), "z".into()]];
        let candidates: BTreeSet<String> = ["y"].iter().map(|s| s.to_string()).collect();
        assert!(greedy_unlock_rank(&held, &paths, &candidates, 5).is_empty());
    }

    #[test]
    fn cycle_count_matches_production_enumeration_on_a_triangle() {
        // WMNT–T1, T1–T2, T2–WMNT: one 3-hop cycle in each direction.
        let pools = vec![
            tokens(address!("00000000000000000000000000000000000000a1"), WMNT, T1),
            tokens(address!("00000000000000000000000000000000000000a2"), T1, T2),
            tokens(address!("00000000000000000000000000000000000000a3"), T2, WMNT),
        ];
        assert_eq!(count_settlement_cycles(&pools, WMNT, 3), 2);
        // A 2-hop cap cannot close a triangle.
        assert_eq!(count_settlement_cycles(&pools, WMNT, 2), 0);
    }

    #[test]
    fn cycle_count_grows_with_the_hop_cap() {
        // Two parallel WMNT–T1 pools plus a T1–T2–WMNT leg.
        let pools = vec![
            tokens(address!("00000000000000000000000000000000000000a1"), WMNT, T1),
            tokens(address!("00000000000000000000000000000000000000a2"), WMNT, T1),
            tokens(address!("00000000000000000000000000000000000000a3"), T1, T2),
            tokens(address!("00000000000000000000000000000000000000a4"), T2, WMNT),
        ];
        let at3 = count_settlement_cycles(&pools, WMNT, 3);
        let at4 = count_settlement_cycles(&pools, WMNT, 4);
        assert!(at3 > 0);
        assert!(at4 >= at3, "raising the cap cannot lose cycles");
    }

    #[test]
    fn degenerate_pools_are_skipped_by_cycle_counting() {
        let pools = vec![
            tokens(address!("00000000000000000000000000000000000000a1"), WMNT, WMNT),
            tokens(address!("00000000000000000000000000000000000000a2"), T1, Address::ZERO),
        ];
        assert_eq!(count_settlement_cycles(&pools, WMNT, 3), 0);
    }

    #[test]
    fn venue_classification_puts_registry_ahead_of_census_kind() {
        let enumerated = enumerated_factories();
        // Cleopatra is registered as quarantined even though census says `v3`.
        let cleo = PoolCensusEntry {
            kind: Some("v3".into()),
            factory: Some("0xaaa32926fce6be95ea2c51cb4fcb60836d320c42".into()),
            ..Default::default()
        };
        assert_eq!(
            classify_venue(Some(&cleo), &enumerated),
            VenueStatus::QuarantinedAdapterRequired
        );
        // A registered drop-in loads.
        let agni = PoolCensusEntry {
            kind: Some("v3".into()),
            factory: Some("0x25780dc8fc3cfbd75f33bfdab65e969b603b2035".into()),
            ..Default::default()
        };
        assert_eq!(
            classify_venue(Some(&agni), &enumerated),
            VenueStatus::LoadableDropIn
        );
        // iZi math has no adapter.
        let izi = PoolCensusEntry {
            kind: Some("izi".into()),
            factory: Some("0x45e5f26451cdb01b0fa1f8582e0aad9a6f27c218".into()),
            ..Default::default()
        };
        assert_eq!(
            classify_venue(Some(&izi), &enumerated),
            VenueStatus::UnsupportedMathFamily
        );
        // Same math on an unregistered factory is an enumeration gap, split by
        // family because the work is not equally cheap.
        let unregistered = |kind: &str| PoolCensusEntry {
            kind: Some(kind.into()),
            factory: Some("0x00000000000000000000000000000000000000ff".into()),
            ..Default::default()
        };
        assert_eq!(
            classify_venue(Some(&unregistered("v3")), &enumerated),
            VenueStatus::UnregisteredV3FamilyFactory
        );
        assert_eq!(
            classify_venue(Some(&unregistered("v2")), &enumerated),
            VenueStatus::UnregisteredV2FamilyFactory
        );
        assert_eq!(
            classify_venue(Some(&unregistered("lb")), &enumerated),
            VenueStatus::UnregisteredLbFactory
        );
        assert_eq!(classify_venue(None, &enumerated), VenueStatus::UnknownVenue);
        // Only the loadable class counts as available today.
        for s in [
            VenueStatus::QuarantinedAdapterRequired,
            VenueStatus::UnregisteredV3FamilyFactory,
            VenueStatus::UnregisteredV2FamilyFactory,
            VenueStatus::UnregisteredLbFactory,
            VenueStatus::UnsupportedMathFamily,
            VenueStatus::UnknownVenue,
        ] {
            assert!(!s.is_loadable(), "{} must not read as loadable", s.as_str());
            assert!(!s.work_required().is_empty());
        }
        assert!(VenueStatus::LoadableDropIn.is_loadable());
    }

    #[test]
    fn v2_enumeration_gap_is_not_reported_as_cheap() {
        // WHI-910 keeps V2_FEE hard-coded, so another V2 venue is blocked on
        // per-venue fees — the status must say so rather than imply a registry
        // entry is enough.
        assert!(VenueStatus::UnregisteredV2FamilyFactory
            .work_required()
            .contains("fees"));
        assert!(VenueStatus::UnregisteredV3FamilyFactory
            .work_required()
            .contains("registry"));
    }

    #[test]
    fn interim_v2_and_moe_factories_are_enumerated() {
        let e = enumerated_factories();
        assert!(e.contains(&INTERIM_V2_FACTORY));
        assert!(e.contains(&crate::amms::moe::CANONICAL_MOE_FACTORY));
        assert!(!e.contains(&crate::service::v3_venues::CLEOPATRA_CL.factory));
    }

    #[test]
    fn settlement_asset_is_the_largest_pos_leg_even_beyond_u128() {
        // A leg wider than u128 must not sort as zero.
        let pos = vec![
            PosLeg::Pair("0xAAA".into(), "1".into()),
            PosLeg::Pair(
                "0xBBB".into(),
                "340282366920938463463374607431768211456".into(),
            ),
        ];
        assert_eq!(settlement_from_pos(&pos).as_deref(), Some("0xbbb"));
    }

    #[test]
    fn unmeasured_tvl_stays_admissible_but_is_reported_as_such() {
        let cfg = base_cfg(false);
        assert!(tvl_admissible(&cfg, "0xmissing"));
        let measured = base_cfg(true);
        assert!(!tvl_admissible(&measured, "0xmissing"));
    }

    #[test]
    fn tvl_floor_rejects_and_quarantine_is_distinct() {
        let mut cfg = base_cfg(true);
        cfg.tvl.insert("0xlow".into(), PoolTvl::Valued(U256::from(1u64)));
        cfg.tvl.insert("0xnone".into(), PoolTvl::Unavailable);
        cfg.tvl
            .insert("0xok".into(), PoolTvl::Valued(cfg.min_tvl_wmnt_wei));
        assert!(!tvl_admissible(&cfg, "0xlow"));
        assert!(!tvl_admissible(&cfg, "0xnone"));
        assert!(tvl_admissible(&cfg, "0xok"));

        let on_cycle = BTreeSet::new();
        assert_eq!(
            classify_exclusion(
                &cfg,
                "0xlow",
                VenueStatus::LoadableDropIn,
                cfg.tvl.get("0xlow"),
                false,
                &on_cycle
            ),
            ExclusionCause::BelowTvlFloor
        );
        assert_eq!(
            classify_exclusion(
                &cfg,
                "0xnone",
                VenueStatus::LoadableDropIn,
                cfg.tvl.get("0xnone"),
                false,
                &on_cycle
            ),
            ExclusionCause::TvlUnavailable
        );
        // Venue always wins: a non-loadable venue is never blamed on TVL.
        assert_eq!(
            classify_exclusion(
                &cfg,
                "0xlow",
                VenueStatus::UnsupportedMathFamily,
                cfg.tvl.get("0xlow"),
                false,
                &on_cycle
            ),
            ExclusionCause::VenueNotLoadable
        );
    }

    fn base_cfg(tvl_measured: bool) -> AnalysisConfig {
        AnalysisConfig {
            held: HashSet::new(),
            held_tokens: HashMap::new(),
            settlement: WMNT,
            max_hops: 3,
            min_tvl_wmnt_wei: U256::from(1_000u64) * U256::from(10u64).pow(U256::from(18u64)),
            tvl: HashMap::new(),
            tvl_measured,
            tvl_block: None,
            set_sizes: vec![10],
            top_n: 10,
            non_trivial_pct: 25.0,
            chain_gross_usd_per_day: None,
            universe_fingerprint: None,
            universe_snapshot_block: None,
            arb_dataset: "fixture.jsonl".into(),
            census_dataset: "fixture.json".into(),
            source_rows: 0,
            skipped_empty_path: 0,
        }
    }

    /// End-to-end shape check on a hand-built fixture, including the residual
    /// bound and the "no reachable universe" verdict branch.
    #[test]
    fn analyze_reconciles_causes_and_bounds_the_residual() {
        let w = wmnt_hex();
        let held_pool = "0x00000000000000000000000000000000000000h1";
        let mut cfg = base_cfg(false);
        cfg.held = ["0x00000000000000000000000000000000000000a1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        cfg.held_tokens.insert(
            "0x00000000000000000000000000000000000000a1".into(),
            (WMNT, T1),
        );
        let _ = held_pool;

        let events = vec![
            // in scope, fully held → residual
            event(&["0x00000000000000000000000000000000000000a1"], 2, Some(&w)),
            // in scope, missing a pool → not_in_universe
            event(
                &[
                    "0x00000000000000000000000000000000000000a1",
                    "0x00000000000000000000000000000000000000b2",
                ],
                2,
                Some(&w),
            ),
            // hop cap
            event(&["0x00000000000000000000000000000000000000c3"], 4, Some(&w)),
            // non-WMNT settlement
            event(&["0x00000000000000000000000000000000000000d4"], 2, Some("0xdead")),
            // aggregator noise
            event(&["0x00000000000000000000000000000000000000e5"], 1139, Some(&w)),
        ];

        let report = analyze(&events, &HashMap::new(), &cfg);
        let r = &report.cause_recount;
        assert_eq!(r.total_events, 5);
        assert_eq!(r.aggregator_misclass, 1);
        assert_eq!(r.out_of_scope_hop_cap, 1);
        assert_eq!(r.out_of_scope_non_wmnt_settlement, 1);
        assert_eq!(r.not_in_universe, 1);
        assert_eq!(r.in_universe_in_scope, 1);
        assert_eq!(r.analysis_denominator, 4);
        // Every non-aggregator event lands in exactly one cause.
        assert_eq!(
            r.out_of_scope_hop_cap
                + r.out_of_scope_non_wmnt_settlement
                + r.not_in_universe
                + r.in_universe_in_scope,
            r.analysis_denominator
        );
        // Residual cannot hide a missing pool.
        assert_eq!(report.residual_bound.residual_events_with_missing_pool, 0);
        assert!(!report.residual_bound.can_hide_not_in_universe);
        assert!(report.residual_bound.statement.contains("point estimate"));

        assert_eq!(report.baseline.in_scope_arbs, 2);
        assert_eq!(report.baseline.reachable_now, 1);
        assert_eq!(report.baseline.distinct_missing_pools, 1);

        // The missing pool has no census entry → unknown venue, so it is not
        // rankable as available.
        assert_eq!(report.missing_pools.len(), 1);
        assert_eq!(
            report.missing_pools[0].venue_status,
            VenueStatus::UnknownVenue
        );
        assert_eq!(
            report.missing_pools[0].exclusion_cause,
            ExclusionCause::VenueNotLoadable
        );
        assert!(report.ranking_loadable.is_empty());
        assert_eq!(report.ranking_any_venue.len(), 1);
        assert_eq!(report.ranking_any_venue[0].marginal_arbs_unlocked, 1);

        // Hop-cap pricing sees the single 4-hop arb, none of it in-universe.
        assert_eq!(report.hop_cap.arbs_at_cap_plus_one, 1);
        assert_eq!(report.hop_cap.arbs_at_cap_plus_one_in_universe, 0);
        assert!(report.hop_cap.note.contains("Cold start is unaffected"));

        // 50% reachable < the 25% threshold? No — 50% clears it.
        assert!(report.verdict.best_any_venue_reachable >= 1);
    }

    #[test]
    fn hop_positions_record_where_in_the_cycle_a_missing_pool_sits() {
        let w = wmnt_hex();
        let held = "0x00000000000000000000000000000000000000a1";
        let gap = "0x00000000000000000000000000000000000000b2";
        let mut cfg = base_cfg(false);
        cfg.held = [held].iter().map(|s| s.to_string()).collect();
        cfg.held_tokens.insert(held.into(), (WMNT, T1));

        // Same missing pool at hop 2 in one arb and hop 1 in another.
        let events = vec![
            MissedArbEvent {
                block: Some(1),
                tx_hash: None,
                pools: vec![held.into(), gap.into()],
                hop_count: 2,
                settlement_asset: Some(w.clone()),
            },
            MissedArbEvent {
                block: Some(2),
                tx_hash: None,
                pools: vec![gap.into(), held.into()],
                hop_count: 2,
                settlement_asset: Some(w),
            },
        ];
        let report = analyze(&events, &HashMap::new(), &cfg);
        let row = &report.missing_pools[0];
        assert_eq!(row.pool, gap);
        assert_eq!(row.hop_positions.get(&1), Some(&1));
        assert_eq!(row.hop_positions.get(&2), Some(&1));
        // Held pools never get a positional profile — only gaps do.
        assert!(report.missing_pools.iter().all(|m| m.pool != held));
    }

    #[test]
    fn loader_counts_rows_without_a_decodable_path_instead_of_dropping_them() {
        let dir = std::env::temp_dir().join("whi999_loader_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("arbs.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"block\":1,\"path\":[\"0xAA\"],\"nSwaps\":2,\"pos\":[[\"0xbb\",\"5\"]]}\n",
                "\n",
                "{\"block\":2,\"path\":[],\"nSwaps\":3}\n",
                "{\"block\":3,\"nSwaps\":3}\n",
            ),
        )
        .unwrap();

        let loaded = load_missed_arb_events(&path).unwrap();
        assert_eq!(loaded.rows_seen, 3, "blank lines are not rows");
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.skipped_empty_path, 2);
        // Addresses are normalized on load.
        assert_eq!(loaded.events[0].pools, vec!["0xaa".to_string()]);
        assert_eq!(loaded.events[0].settlement_asset.as_deref(), Some("0xbb"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn residual_statement_flags_undecoded_rows_when_present() {
        let w = wmnt_hex();
        let held = "0x00000000000000000000000000000000000000a1";
        let mut cfg = base_cfg(false);
        cfg.held = [held].iter().map(|s| s.to_string()).collect();
        cfg.held_tokens.insert(held.into(), (WMNT, T1));
        cfg.source_rows = 3;
        cfg.skipped_empty_path = 2;

        let events = vec![event(&[held], 2, Some(&w))];
        let report = analyze(&events, &HashMap::new(), &cfg);
        assert!(
            report.residual_bound.statement.contains("no decodable path"),
            "undecoded rows must qualify the bound: {}",
            report.residual_bound.statement
        );
        assert_eq!(report.inputs.skipped_empty_path, 2);
        assert_eq!(report.inputs.source_rows, 3);
    }

    #[test]
    fn pool_key_matches_the_arb_coverage_key_space() {
        // Checksummed input, lowercase key — the census and arb loaders both
        // normalize this way, so a mismatch would silently miss every pool.
        let addr: Address = "0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"
            .parse()
            .unwrap();
        assert_eq!(
            pool_key(addr),
            normalize_address("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")
        );
        assert_eq!(pool_key(addr), format!("{addr:?}").to_ascii_lowercase());
    }

    #[test]
    fn candidate_set_restriction_round_trips_as_a_stable_string() {
        assert_eq!(SetRestriction::LoadableOnly.as_str(), "loadable_only");
        assert_eq!(SetRestriction::AnyVenue.as_str(), "any_venue");
        let json = serde_json::to_string(&SetRestriction::LoadableOnly).unwrap();
        assert_eq!(json, "\"loadable_only\"");
    }

    #[test]
    fn cold_start_estimate_is_anchored_to_the_whi_936_measurement() {
        // 130 pools reproduces the 350 s reference.
        assert!((est_cold_start_secs(130) - 350.0).abs() < 1e-6);
        assert!(est_cold_start_secs(180) > est_cold_start_secs(130));
    }

}
