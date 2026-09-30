//! Incremental path index for multi-protocol discovery (WHI-940 / WHI-543 / WHI-538 kernel).
//!
//! On a frozen pool universe the topology (graph + settlement cycles) is a pure
//! function of the address set. Build it **once** per universe load, index
//! pool → cycles, and per block optimize only the cycles that touch a dirty
//! pool (or every cycle on [`TipRefreshScope::Full`]).
//!
//! Non-dirty cycles keep their previous gross-quote result; gas screening and
//! candidate materialization still use the current [`DiscoveryConfig`] so a
//! fee-factor change (base fee, priority policy, block gas limit / reserve —
//! WHI-949) can flip net profitability without re-running AMM math.

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::arbitrage::error::ArbitrageError;
use crate::arbitrage::gas::net_profit_after_gas_cost;
use crate::arbitrage::graph::build_graph;
use crate::arbitrage::optimizer::{
    pools_for_path, simulate_path, ConstantFeeCost, OptimizationConfig, PathOptimizer, ZeroFeeCost,
};
use crate::arbitrage::pathfinder::{ArbitragePath, PathConstraints, PathFinder};
use crate::execution::{FeeScoreKey, ProtocolKind, RouteKey};
use crate::service::discovery::{
    path_is_cross_protocol, protocol_mix_label, simulate_mixed_path_with_features,
    DiscoveryConfig, DiscoveredOpportunity,
};
use crate::service::fee_scoring::{
    discovery_fee_reject_reason, topology_discovery_support, DiscoverySupport, ProfileSupport,
    ProfileSupportError,
};
use crate::service::gas_estimate::{
    price_discovery_gas, venue_labels, CandidateGasEvidence, DiscoveryGasEstimator, GasFeatures,
    GasTier, PricedGas,
};
use crate::service::gas::default_gas_safety_margin;
use crate::service::protocol::TipRefreshScope;
use crate::service::select::protocol_kind_of_amm;
use crate::service::shadow_row::{
    collect_expected_states, format_roi_percent, hops_description, Candidate,
};
use crate::state_space::StateSpace;
use alloy::primitives::{Address, I256, U256};
use eyre::{Context, Result};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Topology-only path cache: cycles + pool→path inverted index.
///
/// Built once per universe load. Invalidated only when the pool address set
/// changes (which, on the live frozen-universe path, means a restart).
#[derive(Debug)]
pub struct PathIndex {
    paths: Vec<ArbitragePath>,
    /// Pool address → path indices containing that pool (deduped, sorted).
    pool_to_path_indices: HashMap<Address, Vec<usize>>,
    settlement_asset: Address,
    max_hops: usize,
    /// Universe membership used to detect a topology epoch change.
    universe_addrs: HashSet<Address>,
    /// How many times `build_graph` ran while constructing this index (always 1).
    build_graph_calls: u64,
    /// How many times `find_cycles` ran while constructing this index (always 1).
    find_cycles_calls: u64,
}

impl PathIndex {
    /// Build graph + enumerate cycles + invert pool→path (WHI-940 step 1–2).
    pub fn build(
        pools: &[AMM],
        settlement_asset: Address,
        max_hops: usize,
    ) -> Result<Self> {
        let mut state = StateSpace::default();
        let mut universe_addrs = HashSet::with_capacity(pools.len());
        for pool in pools {
            let addr = pool.address();
            universe_addrs.insert(addr);
            state.state.insert(addr, pool.clone());
        }

        let graph = build_graph(&state).context("building multi-protocol pool graph")?;
        let build_graph_calls = 1u64;

        let constraints = PathConstraints::settlement_cycle(settlement_asset, max_hops);
        let finder = PathFinder::new(&graph, constraints);
        let raw_paths = finder.find_cycles();
        let find_cycles_calls = 1u64;

        // Deduplicate by hop signature; sort so path indices are stable across
        // rebuilds (HashMap iteration order is not).
        let mut unique: HashMap<String, ArbitragePath> = HashMap::new();
        for path in raw_paths {
            let sig = topology_signature(&path);
            unique.entry(sig).or_insert(path);
        }
        let mut paths: Vec<ArbitragePath> = unique.into_values().collect();
        paths.sort_by_key(|p| topology_signature(p));

        let mut pool_to_path_indices: HashMap<Address, Vec<usize>> = HashMap::new();
        for (idx, path) in paths.iter().enumerate() {
            let mut seen_on_path = HashSet::with_capacity(path.hops.len());
            for hop in &path.hops {
                if seen_on_path.insert(hop.pool_address) {
                    pool_to_path_indices
                        .entry(hop.pool_address)
                        .or_default()
                        .push(idx);
                }
            }
        }
        for indices in pool_to_path_indices.values_mut() {
            indices.sort_unstable();
            indices.dedup();
        }

        Ok(Self {
            paths,
            pool_to_path_indices,
            settlement_asset,
            max_hops,
            universe_addrs,
            build_graph_calls,
            find_cycles_calls,
        })
    }

    pub fn paths(&self) -> &[ArbitragePath] {
        &self.paths
    }

    pub fn cycles_total(&self) -> usize {
        self.paths.len()
    }

    pub fn pool_to_path_indices(&self) -> &HashMap<Address, Vec<usize>> {
        &self.pool_to_path_indices
    }

    pub fn settlement_asset(&self) -> Address {
        self.settlement_asset
    }

    pub fn max_hops(&self) -> usize {
        self.max_hops
    }

    pub fn build_graph_calls(&self) -> u64 {
        self.build_graph_calls
    }

    pub fn find_cycles_calls(&self) -> u64 {
        self.find_cycles_calls
    }

    /// True when `pools` has the same address set as the one used at build time.
    pub fn matches_universe(&self, pools: &[AMM]) -> bool {
        if pools.len() != self.universe_addrs.len() {
            return false;
        }
        pools.iter().all(|p| self.universe_addrs.contains(&p.address()))
    }

    /// Path indices affected by `dirty` (union of inverted-index hits).
    pub fn affected_path_indices(&self, dirty: &HashSet<Address>) -> Vec<usize> {
        let mut set = HashSet::new();
        for addr in dirty {
            if let Some(indices) = self.pool_to_path_indices.get(addr) {
                set.extend(indices.iter().copied());
            }
        }
        let mut out: Vec<usize> = set.into_iter().collect();
        out.sort_unstable();
        out
    }
}

/// Breakdown of discovery rejections for operator visibility and liveness monitoring (WHI-1411).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryRejectCounts {
    /// Topology route key absent from gas profile (contract mismatch).
    pub unknown_route: u64,
    /// Route key present but unsupported/unapproved by policy.
    pub unapproved_route: u64,
    /// Pool lookup failed for path hop.
    pub pool_lookup: u64,
    /// Binary search optimizer found no strictly positive net sample.
    pub no_optimum: u64,
    /// Optimizer returned zero expected profit.
    pub zero_profit: u64,
    /// All other rejections (e.g. gas_reserve, gas_screen, hop_cap, mixed_sim_error, optimize_error, etc.).
    pub other: u64,
}

impl DiscoveryRejectCounts {
    /// Record a rejection reason into the corresponding bucket.
    pub fn record(&mut self, reason: &str) {
        match reason {
            crate::metrics::reject_reason::UNKNOWN_ROUTE => {
                self.unknown_route = self.unknown_route.saturating_add(1);
            }
            crate::metrics::reject_reason::UNAPPROVED_ROUTE => {
                self.unapproved_route = self.unapproved_route.saturating_add(1);
            }
            crate::metrics::reject_reason::POOL_LOOKUP => {
                self.pool_lookup = self.pool_lookup.saturating_add(1);
            }
            crate::metrics::reject_reason::NO_OPTIMUM => {
                self.no_optimum = self.no_optimum.saturating_add(1);
            }
            crate::metrics::reject_reason::ZERO_PROFIT => {
                self.zero_profit = self.zero_profit.saturating_add(1);
            }
            _ => {
                self.other = self.other.saturating_add(1);
            }
        }
    }

    /// Bucket-wise saturating sum (WHI-1544 streak accounting).
    pub fn add(&mut self, other: &Self) {
        self.unknown_route = self.unknown_route.saturating_add(other.unknown_route);
        self.unapproved_route = self.unapproved_route.saturating_add(other.unapproved_route);
        self.pool_lookup = self.pool_lookup.saturating_add(other.pool_lookup);
        self.no_optimum = self.no_optimum.saturating_add(other.no_optimum);
        self.zero_profit = self.zero_profit.saturating_add(other.zero_profit);
        self.other = self.other.saturating_add(other.other);
    }

    /// Total rejections across all buckets.
    pub fn total(&self) -> u64 {
        self.unknown_route
            .saturating_add(self.unapproved_route)
            .saturating_add(self.pool_lookup)
            .saturating_add(self.no_optimum)
            .saturating_add(self.zero_profit)
            .saturating_add(self.other)
    }
}

/// Why a zero-completion window (`paths_quoted == 0`) completed nothing (WHI-1544).
///
/// Explanation only — the liveness trigger never reads it. Only `amm_quotes` is
/// positive evidence that the optimizer ran: in a zero-completion window quotes can
/// come only from searches that ended in `Error`. The `other` bucket is ambiguous —
/// it holds `optimize_error` but also pre-simulation prefilter rejects
/// (`gas_screen` on poisoned profile state, `route_key_construction_error`) — so it
/// rules out the all-pre-simulation claim without establishing optimizer work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroCompletionCause {
    /// Every rejected path sits in a pre-simulation bucket (`unknown_route`,
    /// `unapproved_route`, `pool_lookup`) and no optimizer quote was spent.
    PreSimulationOnly,
    /// Optimizer quote work was spent (`amm_quotes > 0`): searches entered the
    /// optimizer and ended in `Error`, so not every cycle was rejected before
    /// simulation.
    OptimizerQuoteWorkPresent,
    /// No quote was spent, but some rejects are outside the pre-simulation buckets
    /// (typically `other`, which may include optimizer `Error` searches or
    /// pre-simulation prefilter failures): no cause can be established either way.
    OtherRejectsAmbiguous,
    /// No reject was recorded over the window, so no cause can be stated.
    Undetermined,
}

impl ZeroCompletionCause {
    /// Stable machine label for the structured `cause` log field.
    pub fn label(self) -> &'static str {
        match self {
            Self::PreSimulationOnly => "pre_simulation_only",
            Self::OptimizerQuoteWorkPresent => "optimizer_quote_work_present",
            Self::OtherRejectsAmbiguous => "other_rejects_ambiguous",
            Self::Undetermined => "undetermined",
        }
    }

    /// Human explanation used in the alarm message.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::PreSimulationOnly => {
                "all evaluated cycles rejected pre-simulation: \
                 unknown_route/unapproved_route/pool_lookup only"
            }
            Self::OptimizerQuoteWorkPresent => {
                "NOT solely pre-simulation rejection: optimizer quote work was spent \
                 (amm_quotes > 0) by searches that ended in Error"
            }
            Self::OtherRejectsAmbiguous => {
                "`other` rejects may include optimizer Error searches or pre-simulation \
                 failures; cannot establish an all-pre-simulation cause"
            }
            Self::Undetermined => "cause undetermined: no rejects recorded over the window",
        }
    }
}

/// Classify a zero-completion window from its reject buckets and quote work
/// (WHI-1544). The pre-simulation claim is made only when the buckets show
/// pre-simulation rejects alone **and** no quote was spent. Optimizer work is
/// claimed only on positive `amm_quotes` evidence; a nonzero `other` (or
/// `no_optimum` / `zero_profit`) without quotes is reported as ambiguous.
pub fn zero_completion_cause(
    rejects: &DiscoveryRejectCounts,
    amm_quotes: u64,
) -> ZeroCompletionCause {
    if amm_quotes > 0 {
        ZeroCompletionCause::OptimizerQuoteWorkPresent
    } else if rejects.other > 0 || rejects.no_optimum > 0 || rejects.zero_profit > 0 {
        ZeroCompletionCause::OtherRejectsAmbiguous
    } else if rejects.total() > 0 {
        ZeroCompletionCause::PreSimulationOnly
    } else {
        ZeroCompletionCause::Undetermined
    }
}

/// Default number of consecutive discovery passes (heads/tip-refreshes) that must each
/// evaluate cycles yet complete zero Ok/NoOptimum optimizer searches (`paths_quoted == 0`)
/// before the **sustained-window**
/// liveness alarm fires (WHI-1411). Counts *passes*, not raw path/topology count, so the
/// threshold does not scale with (and therefore is not trivially tripped by) universe size.
///
/// This is independent of the **exhaustive** branch: a single `Full`-scope pass that
/// evaluates the whole universe and completes zero searches is already conclusive proof (not
/// a sample) and alarms immediately regardless of this threshold.
///
/// Zero completed searches is **not** by itself proof of pre-simulation rejection: an
/// optimizer search that ends in `Error` spends quotes but is not counted in
/// `paths_quoted` (WHI-1544). The alarm text therefore states its cause only from the
/// reject buckets — see [`zero_completion_cause`].
pub const DEFAULT_LIVENESS_DEAD_HEADS_THRESHOLD: usize = 10;

/// The one state a completed (Ok/NoOptimum) search falls into (WHI-1572 binding E).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTier {
    /// The optimizer never called `fee_cost` (every quote failed or was
    /// gross-unprofitable before pricing, `optimizer.rs` `consider_point`).
    NoFeeRequested,
    /// At least one successful Estimated resolution.
    EstimatedUsed,
    /// At least one successful Measured resolution and no Estimated one.
    MeasuredOnly,
    /// Fee was requested but no request resolved.
    Unresolved,
}

impl SearchTier {
    fn classify(work: &OptimizeWork) -> Self {
        if work.fee_requests == 0 {
            Self::NoFeeRequested
        } else if work.estimated_resolutions > 0 {
            Self::EstimatedUsed
        } else if work.measured_resolutions > 0 {
            Self::MeasuredOnly
        } else {
            Self::Unresolved
        }
    }

    /// Bounded metric label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoFeeRequested => "no_fee_requested",
            Self::EstimatedUsed => "estimated_used",
            Self::MeasuredOnly => "measured_only",
            Self::Unresolved => "unresolved",
        }
    }
}

/// Completed-search partition (WHI-1572). Invariant: `total() == paths_quoted`
/// whenever measured fee scoring is active. Error searches and pre-search
/// rejections are outside this denominator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct SearchTierCounts {
    pub no_fee_requested: u64,
    pub estimated_used: u64,
    pub measured_only: u64,
    pub unresolved: u64,
}

impl SearchTierCounts {
    pub fn record(&mut self, tier: SearchTier) {
        let slot = match tier {
            SearchTier::NoFeeRequested => &mut self.no_fee_requested,
            SearchTier::EstimatedUsed => &mut self.estimated_used,
            SearchTier::MeasuredOnly => &mut self.measured_only,
            SearchTier::Unresolved => &mut self.unresolved,
        };
        *slot = slot.saturating_add(1);
    }

    pub fn total(&self) -> u64 {
        self.no_fee_requested
            .saturating_add(self.estimated_used)
            .saturating_add(self.measured_only)
            .saturating_add(self.unresolved)
    }
}

/// Per-block discovery counters for operator logs (WHI-940 step 5 / WHI-952).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiscoveryStats {
    pub cycles_total: usize,
    pub cycles_optimized: usize,
    pub dirty_pools: usize,
    /// Completed Ok/NoOptimum searches: paths whose optimizer binary search
    /// completed (`Ok` or `NoOptimum`).
    ///
    /// **Completed-search coverage**, neither optimizer-entry nor successful
    /// fee-pricing coverage: a path counts here even when every one of its
    /// samples failed fee resolution (see [`Self::fee_resolution_failures`]).
    /// Paths rejected before simulation are not counted, and neither are
    /// `optimize_error` paths — those *did* enter the optimizer and spent
    /// quotes (see [`Self::amm_quotes`]), so zero here does not mean zero
    /// optimizer entries (WHI-1411 / WHI-1424 / WHI-1544).
    pub paths_quoted: u64,
    /// Candidate inputs evaluated this pass (WHI-952 `amm_quotes`): one per
    /// optimizer quote-closure call on **every** optimize outcome that ran a
    /// search — `Ok`, `NoOptimum` (WHI-976) and `Error` (WHI-1424) — plus one
    /// mixed simulation per non-zero optimum. With the WHI-1409 memo it is an
    /// upper bound on simulations actually issued. Pre-simulation rejects and
    /// pool-lookup skips never quote.
    pub amm_quotes: u64,
    /// Sample-level count (WHI-1424): profitable candidate inputs whose real
    /// route could not be fee-priced (unapproved/unknown bucket, fee-policy
    /// rejection, or route-key simulation failure), so the sample was scored
    /// as not viable. **Samples, not paths** — it is never part of `rejects`
    /// and never enters the path-count conservation check
    /// (`rejects.total()` vs paths evaluated). A path whose every profitable
    /// sample failed here is still counted once as `no_optimum` in `rejects`.
    pub fee_resolution_failures: u64,
    /// Cached paths re-screened because fee factors changed (WHI-949).
    pub gas_rescores: u64,
    /// `"full"` or `"touched"` — same labels as [`TipRefreshScope::as_metric_label`].
    /// Empty when discovery was skipped (e.g. inventory precondition, WHI-950).
    pub scope: &'static str,
    /// Breakdown of rejected paths by cause (WHI-1411).
    pub rejects: DiscoveryRejectCounts,
    /// True when the liveness invariant detected a sustained zero-quoted pipeline (WHI-1411).
    pub liveness_alarm: bool,
    /// Completed-search partition (WHI-1572); `Some` exactly when measured fee
    /// scoring is active, with `total() == paths_quoted`.
    pub search_tiers: Option<SearchTierCounts>,
    /// Sample-level successful Measured / Estimated fee resolutions (WHI-1572).
    /// Samples, not paths; never summed with path counts.
    pub measured_resolutions: u64,
    pub estimated_resolutions: u64,
    /// AMM route-key simulations actually issued: optimizer memo misses plus one
    /// post-optimum mixed simulation per non-zero optimum (WHI-1572 replay).
    pub simulations: u64,
    /// Candidates materialized this pass, by gas tier (WHI-1572).
    pub candidates_measured: u64,
    pub candidates_estimated: u64,
}

/// Gross quote cached after optimize + mixed simulation (pre gas-screen).
///
/// `route_key` and `crossings` are evidence, not authorization (WHI-1572
/// binding B): every materialization and fee rescore re-resolves them against
/// the current profile status and bounds, without AMM re-simulation.
#[derive(Debug, Clone)]
struct CachedGross {
    optimal_input: U256,
    amounts_out: Vec<U256>,
    final_out: U256,
    route_key: RouteKey,
    crossings: Vec<u32>,
}

/// Stateful discovery engine: static path index + per-path gross-quote cache.
#[derive(Debug)]
pub struct DiscoveryEngine {
    index: PathIndex,
    /// Index-aligned with `index.paths`. `None` = last optimize found no optimum.
    cache: Vec<Option<CachedGross>>,
    /// True after at least one Full (or cold) optimize pass has populated the cache.
    primed: bool,
    /// Fee factors used on the previous materialize pass (gas re-score trigger).
    last_fee_score_key: Option<FeeScoreKey>,
    /// Stats from the most recent [`Self::discover`] call (for watch-path asserts).
    last_stats: Option<DiscoveryStats>,
    /// Consecutive discovery passes (one call to [`Self::discover`]) evaluated where zero
    /// Ok/NoOptimum searches completed despite `cycles_optimized > 0` (WHI-1411). Reset
    /// to 0 the moment any pass completes at least one search. Counts *passes*, not
    /// paths — see [`DEFAULT_LIVENESS_DEAD_HEADS_THRESHOLD`].
    consecutive_dead_heads: usize,
    /// Reject buckets and quote work summed over the current `consecutive_dead_heads`
    /// streak (WHI-1544). Explanation-only: feeds the alarm's stated cause, never the
    /// trigger. Reset together with `consecutive_dead_heads`.
    dead_streak_rejects: DiscoveryRejectCounts,
    dead_streak_amm_quotes: u64,
    /// Fixed default sustained-window threshold, set once at construction and never
    /// mutated afterward: fire the liveness alarm once `consecutive_dead_heads` reaches
    /// this many passes. Per-call callers can override this for a single call via
    /// `DiscoveryConfig::liveness_dead_heads_threshold` without altering this default for
    /// any other call.
    liveness_dead_heads_threshold: usize,
}

impl DiscoveryEngine {
    pub fn build(
        pools: &[AMM],
        settlement_asset: Address,
        max_hops: usize,
    ) -> Result<Self> {
        let index = PathIndex::build(pools, settlement_asset, max_hops)?;
        let n = index.cycles_total();
        Ok(Self {
            index,
            cache: vec![None; n],
            primed: false,
            last_fee_score_key: None,
            last_stats: None,
            consecutive_dead_heads: 0,
            dead_streak_rejects: DiscoveryRejectCounts::default(),
            dead_streak_amm_quotes: 0,
            liveness_dead_heads_threshold: DEFAULT_LIVENESS_DEAD_HEADS_THRESHOLD,
        })
    }

    pub fn index(&self) -> &PathIndex {
        &self.index
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    pub fn last_stats(&self) -> Option<DiscoveryStats> {
        self.last_stats
    }

    /// Rebuild if the pool address set drifted (defensive; live bot freezes universe).
    pub fn ensure_universe(&mut self, pools: &[AMM], config: &DiscoveryConfig) -> Result<()> {
        if self.index.matches_universe(pools)
            && self.index.settlement_asset() == config.settlement_asset
            && self.index.max_hops() == config.max_hops
        {
            return Ok(());
        }
        *self = Self::build(pools, config.settlement_asset, config.max_hops)?;
        Ok(())
    }

    /// Discover opportunities under `scope`.
    ///
    /// * [`TipRefreshScope::Full`] (or unprimed engine) → re-optimize every cycle.
    /// * [`TipRefreshScope::Touched`] → re-optimize only cycles touching dirty pools;
    ///   all other cycles keep their previous gross quote.
    ///
    /// Gas screening always uses the current `config` so fee changes apply without
    /// re-running AMM math on clean paths. Fee-factor identity covers base fee,
    /// priority policy, and block gas limit / reserve (WHI-949).
    pub fn discover(
        &mut self,
        pools: &[AMM],
        config: &DiscoveryConfig,
        scope: &TipRefreshScope,
    ) -> Result<(Vec<DiscoveredOpportunity>, DiscoveryStats)> {
        use crate::metrics::{self, reject_reason, stage};

        if pools.is_empty() {
            let stats = DiscoveryStats {
                cycles_total: 0,
                cycles_optimized: 0,
                dirty_pools: 0,
                paths_quoted: 0,
                amm_quotes: 0,
                fee_resolution_failures: 0,
                gas_rescores: 0,
                scope: scope.as_metric_label(),
                rejects: DiscoveryRejectCounts::default(),
                liveness_alarm: false,
                ..DiscoveryStats::default()
            };
            self.last_stats = Some(stats);
            // No cycles to evaluate this pass — not an alarming state; keep the gauge in
            // sync rather than leaving it latched at whatever it last reported.
            metrics::record_discovery_liveness_alarm(false);
            return Ok((Vec::new(), stats));
        }

        self.ensure_universe(pools, config)?;

        let force_full = !self.primed || matches!(scope, TipRefreshScope::Full);
        let (to_optimize, dirty_pools, scope_label) = if force_full {
            // Unprimed first pass on a Touched scope still optimizes all and
            // reports scope=full so operators do not mistake a cold prime for
            // a dirty-set miss.
            let dirty_pools = match scope {
                TipRefreshScope::Full => pools.len(),
                TipRefreshScope::Touched(d) => d.len(),
            };
            (
                (0..self.index.cycles_total()).collect::<Vec<_>>(),
                dirty_pools,
                "full",
            )
        } else {
            match scope {
                TipRefreshScope::Full => unreachable!("force_full covers Full"),
                TipRefreshScope::Touched(dirty) => {
                    let indices = self.index.affected_path_indices(dirty);
                    (indices, dirty.len(), "touched")
                }
            }
        };

        let reopt: HashSet<usize> = to_optimize.iter().copied().collect();

        // Quiet-block / non-Moe dirty: Moe tip refresh is skipped, so snapshot
        // timestamps lag the announced tip. Re-emitting a cached Moe gross quote
        // lets discovery rank it, then attempt re-sim with the new tip hits
        // SnapshotTimestampMismatch (hard head failure). Drop Moe-path cache
        // unless this pass re-optimizes that path (Full or dirty Moe).
        if !force_full {
            for (idx, path) in self.index.paths.iter().enumerate() {
                if reopt.contains(&idx) {
                    continue;
                }
                if path_includes_moe(path, pools) {
                    self.cache[idx] = None;
                }
            }
        }

        // WHI-948: optimizer maximises net score; it no longer consumes
        // `min_profit`. Admission floor (`config.min_profit` = bot
        // `min_net_profit`) applies only at materialize.
        //
        // WHI-949: measured fee uses `fee_plan_cost(route_key, fee_context)` for
        // materialize (send-identical). WHI-1409 (G-1) reconciled optimize to the
        // same contract: `optimize_path` below evaluates the real per-sample
        // route key (actual V3 tick / Moe bin crossings), not a topology-guessed
        // constant, so the two stages can never price a candidate on different
        // (and possibly differently-approved) route keys.
        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: config.max_input,
            ..OptimizationConfig::default()
        });

        let discovery_start = Instant::now();
        let cycles_optimized = to_optimize.len();
        let mut amm_quotes = 0u64;
        // Paths whose optimizer search completed (Ok / NoOptimum). Fee
        // Rejected / pool-lookup failures never quote and are not part of the
        // WHI-976 work invariant; Error paths add their quotes to `amm_quotes`
        // but are counted as `optimize_error`, not here.
        let mut paths_quoted = 0u64;
        let mut fee_resolution_failures = 0u64;
        let mut rejects = DiscoveryRejectCounts::default();
        let mut search_tiers = config
            .measured_fee
            .as_ref()
            .map(|_| SearchTierCounts::default());
        let mut measured_resolutions = 0u64;
        let mut estimated_resolutions = 0u64;
        let mut simulations = 0u64;

        for path_idx in &to_optimize {
            let path = &self.index.paths[*path_idx];
            let path_pools = match pools_for_path(path, pools) {
                Ok(p) => p,
                Err(_) => {
                    metrics::record_discovery_rejected(reject_reason::POOL_LOOKUP);
                    rejects.record(reject_reason::POOL_LOOKUP);
                    self.cache[*path_idx] = None;
                    continue;
                }
            };

            let optimize_start = Instant::now();
            let outcome = optimize_path(&optimizer, path, &path_pools, config);
            // WHI-976 / WHI-1424: every outcome that ran a search contributes
            // its quotes — including NoOptimum (pre-WHI-976 discarded) and
            // Error (pre-WHI-1424 discarded).
            if let OptimizeOutcome::Ok { work, .. }
            | OptimizeOutcome::NoOptimum { work }
            | OptimizeOutcome::Error { work, .. } = &outcome
            {
                amm_quotes = amm_quotes.saturating_add(work.quotes);
                fee_resolution_failures =
                    fee_resolution_failures.saturating_add(work.fee_resolution_failures);
                measured_resolutions =
                    measured_resolutions.saturating_add(work.measured_resolutions);
                estimated_resolutions =
                    estimated_resolutions.saturating_add(work.estimated_resolutions);
                simulations = simulations.saturating_add(work.simulations);
            }
            // WHI-1572: classify each completed search into exactly one state.
            if let (
                Some(tiers),
                OptimizeOutcome::Ok { work, .. } | OptimizeOutcome::NoOptimum { work },
            ) = (search_tiers.as_mut(), &outcome)
            {
                let tier = SearchTier::classify(work);
                tiers.record(tier);
                metrics::record_discovery_search_tier(tier.as_str());
            }
            let opt = match outcome {
                OptimizeOutcome::Ok { result, .. } => {
                    paths_quoted = paths_quoted.saturating_add(1);
                    result
                }
                OptimizeOutcome::NoOptimum { .. } => {
                    paths_quoted = paths_quoted.saturating_add(1);
                    metrics::record_discovery_rejected(reject_reason::NO_OPTIMUM);
                    rejects.record(reject_reason::NO_OPTIMUM);
                    self.cache[*path_idx] = None;
                    continue;
                }
                OptimizeOutcome::Rejected { reason } => {
                    metrics::record_discovery_rejected(reason);
                    rejects.record(reason);
                    self.cache[*path_idx] = None;
                    continue;
                }
                OptimizeOutcome::Error { error, .. } => {
                    // Debug not warn: per-path failures can be thousands/block
                    // (WHI-952 RUST_LOG=info bound). Counters still record OPTIMIZE_ERROR.
                    tracing::debug!(
                        target: "bot.discovery",
                        error = %error,
                        "optimize failed; skipping path (not aborting discovery)"
                    );
                    metrics::record_discovery_rejected(reject_reason::OPTIMIZE_ERROR);
                    rejects.record(reject_reason::OPTIMIZE_ERROR);
                    self.cache[*path_idx] = None;
                    continue;
                }
            };
            metrics::record_pipeline_stage(stage::OPTIMIZE, "merged", optimize_start.elapsed());

            if opt.expected_profit.is_zero() {
                metrics::record_discovery_rejected(reject_reason::ZERO_PROFIT);
                rejects.record(reject_reason::ZERO_PROFIT);
                self.cache[*path_idx] = None;
                continue;
            }

            amm_quotes = amm_quotes.saturating_add(1);
            simulations = simulations.saturating_add(1);
            let (amounts_out, final_out, route_key, crossings) = match simulate_mixed_path_with_features(
                path,
                &path_pools,
                opt.optimal_input,
                config.block_timestamp,
            ) {
                Ok(v) => v,
                Err(e) => {
                    tracing::debug!(
                        target: "bot.discovery",
                        error = %e,
                        "mixed simulation failed; skipping path"
                    );
                    metrics::record_discovery_rejected(reject_reason::MIXED_SIM_ERROR);
                    rejects.record(reject_reason::MIXED_SIM_ERROR);
                    self.cache[*path_idx] = None;
                    continue;
                }
            };

            self.cache[*path_idx] = Some(CachedGross {
                optimal_input: opt.optimal_input,
                amounts_out,
                final_out,
                route_key,
                crossings,
            });
        }

        // Topology work is paid at build time; discovery stage here is the
        // optimize + materialize pass for the selected subset.
        metrics::record_pipeline_stage(stage::DISCOVERY, "merged", discovery_start.elapsed());
        metrics::record_discovery_cycles_found(self.index.cycles_total());

        let current_fee_key = fee_score_key_of(config);
        let fee_factors_changed = self
            .last_fee_score_key
            .map(|prev| prev != current_fee_key)
            .unwrap_or(false);

        let mut gas_rescores = 0u64;
        let mut found = Vec::new();
        for (path_idx, cached) in self.cache.iter().enumerate() {
            let Some(cached) = cached else {
                continue;
            };
            let path = &self.index.paths[path_idx];
            let is_reopt = reopt.contains(&path_idx);
            let path_pools = match pools_for_path(path, pools) {
                Ok(p) => p,
                Err(_) => {
                    metrics::record_discovery_rejected(reject_reason::POOL_LOOKUP);
                    if is_reopt {
                        rejects.record(reject_reason::POOL_LOOKUP);
                    }
                    continue;
                }
            };

            // Re-score = re-screen a cached gross quote because fee factors
            // changed, without re-running AMM optimize.
            let rescored = !is_reopt && self.primed && fee_factors_changed;
            if rescored {
                gas_rescores = gas_rescores.saturating_add(1);
            }

            match materialize_from_cache(path, &path_pools, cached, config) {
                Ok(opp) => {
                    let mix = protocol_mix_label(opp.is_cross_protocol, &opp.protocol_kinds);
                    metrics::record_discovery_candidate(mix);
                    found.push(opp);
                }
                Err(reason) => {
                    if is_reopt {
                        rejects.record(reason);
                    }
                }
            }
        }

        found.sort_by(|a, b| b.candidate.net_profit.cmp(&a.candidate.net_profit));
        let mut candidates_measured = 0u64;
        let mut candidates_estimated = 0u64;
        for opp in &found {
            if let Some(gas) = opp.gas.as_ref() {
                metrics::record_discovery_candidate_gas_tier(gas.tier.as_str());
                match gas.tier {
                    GasTier::Measured => candidates_measured += 1,
                    GasTier::Estimated => candidates_estimated += 1,
                }
            }
        }
        if let Some(best) = found.first() {
            let mix = protocol_mix_label(best.is_cross_protocol, &best.protocol_kinds);
            metrics::record_discovery_best_net_profit(mix, best.candidate.net_profit);
        }

        self.primed = true;
        self.last_fee_score_key = Some(current_fee_key);

        // WHI-976: any path that reached the optimizer binary search must have
        // recorded ≥1 amm quote. Scope is `paths_quoted` (Ok / NoOptimum), not
        // raw `cycles_optimized` — fee Rejected / pool-lookup skips never quote
        // by design and must not false-alarm as a dead counter.
        if paths_quoted > 0 && amm_quotes == 0 {
            debug_assert!(
                false,
                "WHI-976 invariant: paths_quoted={paths_quoted} but amm_quotes=0"
            );
            tracing::error!(
                target: "bot.discovery",
                cycles_optimized,
                paths_quoted,
                amm_quotes,
                dirty_pools,
                scope = scope_label,
                "WHI-976 invariant violated: optimizer ran with zero amm_quotes \
                 (counter dead or simulation short-circuited)"
            );
        }

        // WHI-1411: distinguish "priced everything and found nothing" (paths_quoted > 0)
        // from "completed no search" (paths_quoted == 0 — zero completed Ok/NoOptimum
        // searches; Error outcomes excluded, so this is not by itself pre-simulation
        // rejection, WHI-1544). Two independent triggers:
        //
        // 1. Exhaustive: a `Full`-scope pass evaluates the *entire* universe in one shot, so
        //    zero completed Ok/NoOptimum searches is already conclusive proof of a dead
        //    pipeline (not a sample) — fires immediately, with no window needed.
        // 2. Sustained window: a `Touched` pass only samples the dirty subset, so one dead
        //    pass alone is not conclusive. Count *consecutive discovery passes* (each call to
        //    `discover()` — in the watch loop, one call per processed head; a stateless
        //    one-shot caller like `discover_opportunities` builds a fresh engine per call and
        //    so can never accumulate past 1 here, which is expected: only the exhaustive
        //    branch above is meaningful for a single-call caller). Counting passes, not raw
        //    path/topology count, means the threshold does not scale with (and is not
        //    trivially tripped by) universe size.
        //
        // `config.liveness_dead_heads_threshold` is read fresh on every call rather than
        // latched into engine state, so an override only ever applies to the call that
        // supplied it — a later call with `None` falls back to the engine's fixed default,
        // it never silently inherits a prior call's override.
        let effective_dead_heads_threshold = config
            .liveness_dead_heads_threshold
            .unwrap_or(self.liveness_dead_heads_threshold);

        if paths_quoted > 0 {
            self.consecutive_dead_heads = 0;
            self.dead_streak_rejects = DiscoveryRejectCounts::default();
            self.dead_streak_amm_quotes = 0;
        } else if cycles_optimized > 0 {
            self.consecutive_dead_heads = self.consecutive_dead_heads.saturating_add(1);
            self.dead_streak_rejects.add(&rejects);
            self.dead_streak_amm_quotes = self.dead_streak_amm_quotes.saturating_add(amm_quotes);
        }

        let liveness_alarm = (force_full && cycles_optimized > 0 && paths_quoted == 0)
            || (self.consecutive_dead_heads >= effective_dead_heads_threshold);

        metrics::record_discovery_liveness_alarm(liveness_alarm);

        if liveness_alarm {
            // WHI-1544: the cause is stated over the whole zero-completion streak (which
            // includes this pass whenever it evaluated anything), never assumed.
            let cause =
                zero_completion_cause(&self.dead_streak_rejects, self.dead_streak_amm_quotes);
            tracing::error!(
                target: "bot.discovery",
                cycles_optimized,
                paths_quoted,
                amm_quotes,
                consecutive_dead_heads = self.consecutive_dead_heads,
                unknown_route = rejects.unknown_route,
                unapproved_route = rejects.unapproved_route,
                pool_lookup = rejects.pool_lookup,
                no_optimum = rejects.no_optimum,
                zero_profit = rejects.zero_profit,
                other = rejects.other,
                scope = scope_label,
                streak_other = self.dead_streak_rejects.other,
                streak_amm_quotes = self.dead_streak_amm_quotes,
                cause = cause.label(),
                "WHI-1411 liveness invariant violated: zero completed Ok/NoOptimum optimizer \
                 searches (discovery pipeline dead; {})",
                cause.explanation()
            );
        }

        let stats = DiscoveryStats {
            cycles_total: self.index.cycles_total(),
            cycles_optimized,
            dirty_pools,
            paths_quoted,
            amm_quotes,
            fee_resolution_failures,
            gas_rescores,
            scope: scope_label,
            rejects,
            liveness_alarm,
            search_tiers,
            measured_resolutions,
            estimated_resolutions,
            simulations,
            candidates_measured,
            candidates_estimated,
        };
        debug_assert!(stats
            .search_tiers
            .map_or(true, |t| t.total() == stats.paths_quoted));
        self.last_stats = Some(stats);

        Ok((found, stats))
    }
}

/// Work one optimizer search spent, whatever its outcome (WHI-1424).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct OptimizeWork {
    /// Quote-closure calls (candidate inputs evaluated) — feeds `amm_quotes`.
    quotes: u64,
    /// Profitable samples whose fee could not be resolved — see
    /// [`DiscoveryStats::fee_resolution_failures`].
    fee_resolution_failures: u64,
    /// `fee_cost` calls (WHI-1572 search partition).
    fee_requests: u64,
    /// Successful Measured / Estimated fee resolutions (samples).
    measured_resolutions: u64,
    estimated_resolutions: u64,
    /// AMM simulations actually issued (memo misses).
    simulations: u64,
}

#[derive(Debug)]
enum OptimizeOutcome {
    Ok {
        result: crate::arbitrage::optimizer::OptimizationResult,
        work: OptimizeWork,
    },
    /// Optimizer ran quotes but found no strictly positive net sample.
    ///
    /// Carries its work so the WHI-952 / WHI-976 `amm_quotes` counter records
    /// the common unprofitable path (not only a found optimum).
    NoOptimum {
        work: OptimizeWork,
    },
    Rejected {
        reason: &'static str,
    },
    /// A sample hard-failed. Still carries the work spent before and after it
    /// (the search continues past a failed sample) so `amm_quotes` covers all
    /// simulation work, not only Ok / NoOptimum (WHI-1424).
    Error {
        error: ArbitrageError,
        work: OptimizeWork,
    },
}

/// Run one search, counting quote-closure calls here rather than trusting the
/// optimizer's own count, which is lost when the search returns `Err`
/// (WHI-1424). `consider_point` bumps its counter exactly once per closure
/// call, so on success the two agree.
fn run_search<F, Q>(
    optimizer: &PathOptimizer,
    path: &ArbitragePath,
    fee: &F,
    mut quote: Q,
    fee_work: impl FnOnce() -> OptimizeWork,
) -> OptimizeOutcome
where
    F: crate::arbitrage::optimizer::FeeCostModel,
    Q: FnMut(U256) -> Result<Option<(U256, U256)>, ArbitrageError>,
{
    let mut quotes = 0u64;
    let outcome = optimizer.optimize_with_quote_and_fee(path, fee, |amount_in| {
        quotes = quotes.saturating_add(1);
        quote(amount_in)
    });
    debug_assert!(outcome.as_ref().map_or(true, |(_, q)| *q == quotes));
    let work = OptimizeWork {
        quotes,
        ..fee_work()
    };
    match outcome {
        Ok((Some(result), _)) => OptimizeOutcome::Ok { result, work },
        Ok((None, _)) => OptimizeOutcome::NoOptimum { work },
        Err(error) => OptimizeOutcome::Error { error, work },
    }
}

/// Fee-factor identity for both measured and offline scoring modes.
///
/// Offline encodes `gas_price_wei` into `base_fee_per_gas` so a price-only
/// change still triggers gas re-scores (WHI-949 invalidation contract).
fn fee_score_key_of(config: &DiscoveryConfig) -> FeeScoreKey {
    if let Some(m) = config.measured_fee.as_ref() {
        m.fee_score_key()
    } else {
        FeeScoreKey {
            base_fee_per_gas: config.gas.gas_price_wei,
            priority_fee_per_gas: 0,
            block_gas_limit: 0,
            block_gas_reserve: 0,
        }
    }
}

/// WHI-1409 pre-simulation reject gate: the shared discovery-support predicate
/// ([`crate::service::fee_scoring::topology_discovery_support`]; without an
/// estimator exactly the WHI-1421 `topology_profile_support`) mapped to a
/// discovery reject reason.
///
/// Pure profile lookups, zero AMM simulation, so a topology that no crossing
/// bucket can ever price is rejected before spending quote budget (the WHI-1411
/// pre-simulation zero-quote liveness invariant). `Ok(None)` means some bucket
/// is actively approved (or, with the WHI-1572 estimator, estimation-eligible),
/// so the real per-sample search in [`optimize_path`] runs. The startup gate
/// classifies with the very same predicate, so the two cannot disagree.
fn topology_never_approved_reason(
    measured: &crate::service::fee_scoring::MeasuredFeeScoring,
    estimator: Option<&DiscoveryGasEstimator>,
    protocols: &[ProtocolKind],
) -> Result<Option<&'static str>, ProfileSupportError> {
    use crate::metrics::reject_reason;

    Ok(
        match topology_discovery_support(&measured.gas_profile, estimator, protocols)? {
            DiscoverySupport::Profile(ProfileSupport::Supported) | DiscoverySupport::Estimated => {
                None
            }
            // With an estimator every absent / evidence-withheld bucket is
            // estimable, so this means every bucket is invalidated, research-only
            // or policy-ineligible — never "absent measured evidence".
            DiscoverySupport::Profile(_) if estimator.is_some() => {
                Some(reject_reason::DISCOVERY_POLICY_REJECTED)
            }
            DiscoverySupport::Profile(ProfileSupport::Unapproved) => {
                Some(reject_reason::UNAPPROVED_ROUTE)
            }
            DiscoverySupport::Profile(ProfileSupport::Unknown) => {
                Some(reject_reason::UNKNOWN_ROUTE)
            }
        },
    )
}

/// Per-sample measured fee cost using the **real** simulated route key
/// (WHI-1409 / G-1), not a topology guess.
///
/// Derives the route key for `amount_in` via the *same*
/// `simulate_mixed_path_with_route_key` the paired quote closure (and
/// materialize) use, **memoized on `amount_in`** for the lifetime of one
/// `optimize_path` call.
///
/// Memoization — rather than the stash/consume hand-off an earlier revision
/// used — is what keeps this ordering-independent while still costing one
/// simulation per sample: calling [`FeeCostModel::fee_cost`] before, after, or
/// without ever calling the quote closure for a given `amount_in` is always
/// correct, because the cache computes-or-reuses rather than requiring a
/// prime. `consider_point` (`arbitrage::optimizer`) evaluates a candidate by
/// calling the quote closure and then — only when that quote is profitable —
/// `fee_cost` for the *same* `amount_in`; both route through
/// [`Self::simulate`], so that pair costs **one** simulation, not two. Inputs
/// that recur later in the search (the coarse samples that become ternary
/// interval endpoints, plus `max_input` and the domain floor, are all
/// re-evaluated by construction) cost none.
///
/// Soundness: `path`, `path_pools` and `block_timestamp` are immutable for
/// this struct's whole lifetime (one `optimize_path` call over a locally
/// cloned pool vector), so `simulate(amount_in)` is a pure function of
/// `amount_in` and a memo of it cannot go stale.
///
/// Bound: the cache cannot outgrow the optimizer's own
/// [`OptimizationConfig::max_quotes`] budget (default 96) — `consider_point`
/// refuses to evaluate past it, so at most that many distinct inputs can ever
/// be inserted — and the whole cache is dropped when `optimize_path` returns.
///
/// A route whose real bucket is not `Approved` (or whose simulation fails for
/// any other reason) prices as `U256::MAX`, which `net_score`'s checked
/// subtraction turns into "not viable" (never a false cheap price) — the same
/// fail-closed semantics `MeasuredFeeScoring::fee_plan_cost` already applies
/// at materialize.
struct RouteAwareFeeCost<'a> {
    measured: &'a crate::service::fee_scoring::MeasuredFeeScoring,
    /// WHI-1572: when set, unmeasured classes are priced by the estimator.
    estimator: Option<&'a DiscoveryGasEstimator>,
    path: &'a ArbitragePath,
    path_pools: &'a [AMM],
    block_timestamp: u64,
    /// `amount_in` → `(final output amount, real route key, per-hop crossing
    /// counts)`, all from one simulation. The per-hop output vector is
    /// re-derived by materialize from its own simulation, so caching it here
    /// would be dead weight.
    cache: RefCell<HashMap<U256, (U256, RouteKey, Vec<u32>)>>,
    /// Count of `simulate_mixed_path_with_route_key` calls actually issued
    /// (i.e. cache misses). Makes the cost bound this type claims
    /// *measurable* instead of merely asserted — WHI-1409's AC-5 asks that any
    /// latency work be a proven bound, and this is the surface the tests prove
    /// it against.
    simulations: Cell<u64>,
    /// `fee_cost` calls that priced as `U256::MAX` because the sample's real
    /// route could not be fee-resolved (WHI-1424). Without it a topology that
    /// simulates but can never be priced is indistinguishable from an
    /// unprofitable one in the `NoOptimum` outcome.
    fee_resolution_failures: Cell<u64>,
    /// WHI-1572 search partition inputs: `fee_cost` calls and their successes.
    fee_requests: Cell<u64>,
    measured_resolutions: Cell<u64>,
    estimated_resolutions: Cell<u64>,
}

impl<'a> RouteAwareFeeCost<'a> {
    fn new(
        measured: &'a crate::service::fee_scoring::MeasuredFeeScoring,
        estimator: Option<&'a DiscoveryGasEstimator>,
        path: &'a ArbitragePath,
        path_pools: &'a [AMM],
        block_timestamp: u64,
    ) -> Self {
        Self {
            measured,
            estimator,
            path,
            path_pools,
            block_timestamp,
            cache: RefCell::new(HashMap::new()),
            simulations: Cell::new(0),
            fee_resolution_failures: Cell::new(0),
            fee_requests: Cell::new(0),
            measured_resolutions: Cell::new(0),
            estimated_resolutions: Cell::new(0),
        }
    }

    /// Work counters for [`run_search`] (quotes are counted there).
    fn work(&self) -> OptimizeWork {
        OptimizeWork {
            quotes: 0,
            fee_resolution_failures: self.fee_resolution_failures.get(),
            fee_requests: self.fee_requests.get(),
            measured_resolutions: self.measured_resolutions.get(),
            estimated_resolutions: self.estimated_resolutions.get(),
            simulations: self.simulations.get(),
        }
    }

    /// Price one simulated sample: measured-only mode is the unchanged
    /// `fee_plan_cost`; with an estimator, the shared WHI-1572 resolution that
    /// materialization also uses.
    fn price(&self, route_key: &RouteKey, crossings: &[u32]) -> Result<(GasTier, U256), String> {
        match self.estimator {
            None => self
                .measured
                .fee_plan_cost(route_key)
                .map(|cost| (GasTier::Measured, cost))
                .map_err(|e| e.to_string()),
            Some(estimator) => price_discovery_gas(
                &self.measured.gas_profile,
                estimator,
                route_key,
                &GasFeatures::new(route_key.protocols.clone(), crossings.to_vec()),
                &self.measured.fee_context,
                self.measured.policy(),
            )
            .map(|p| (p.tier, p.cost))
            .map_err(|r| r.label().to_string()),
        }
    }

    /// Memoized single simulation entry point shared by
    /// [`FeeCostModel::fee_cost`] and `optimize_path`'s quote closure, so
    /// there is exactly one place that calls
    /// `simulate_mixed_path_with_route_key` with this candidate's
    /// `(path, path_pools, block_timestamp)` — the two call sites cannot drift
    /// apart on which path/pools/timestamp they simulate against, and the
    /// second call for a given `amount_in` is served from the memo.
    ///
    /// Failures are deliberately **not** memoized: `consider_point`
    /// short-circuits on a failed or unprofitable quote and never calls
    /// `fee_cost` for that input, so a failing input costs one simulation per
    /// evaluation either way, and `ProtocolError` is not `Clone`.
    fn simulate(
        &self,
        amount_in: U256,
    ) -> Result<(U256, RouteKey, Vec<u32>), crate::service::error::ProtocolError> {
        {
            let cache = self.cache.borrow();
            if let Some((final_out, route_key, crossings)) = cache.get(&amount_in) {
                return Ok((*final_out, route_key.clone(), crossings.clone()));
            }
        }
        self.simulations.set(self.simulations.get().saturating_add(1));
        let (_, final_out, route_key, crossings) = simulate_mixed_path_with_features(
            self.path,
            self.path_pools,
            amount_in,
            self.block_timestamp,
        )?;
        self.cache
            .borrow_mut()
            .insert(amount_in, (final_out, route_key.clone(), crossings.clone()));
        Ok((final_out, route_key, crossings))
    }

    /// Real `simulate_mixed_path_with_route_key` calls issued so far (cache
    /// misses only). Read by `optimize_path`'s trace-level memo report and by
    /// the memoization tests. See [`Self::simulations`].
    fn simulations_performed(&self) -> u64 {
        self.simulations.get()
    }
}

impl<'a> crate::arbitrage::optimizer::FeeCostModel for RouteAwareFeeCost<'a> {
    fn fee_cost(&self, amount_in: U256) -> U256 {
        self.fee_requests
            .set(self.fee_requests.get().saturating_add(1));
        match self.simulate(amount_in) {
            Ok((_, route_key, crossings)) => match self.price(&route_key, &crossings) {
                Ok((tier, cost)) => {
                    let slot = match tier {
                        GasTier::Measured => &self.measured_resolutions,
                        GasTier::Estimated => &self.estimated_resolutions,
                    };
                    slot.set(slot.get().saturating_add(1));
                    return cost;
                }
                Err(e) => {
                    // Unapproved/unknown bucket or FeePolicy rejection at this
                    // specific candidate size — fail closed (U256::MAX makes
                    // `net_score` reject it). Counted below (WHI-1424) since
                    // it is otherwise indistinguishable from ordinary
                    // unprofitability in the NoOptimum outcome.
                    tracing::trace!(
                        target: "bot.discovery",
                        %amount_in,
                        route_key = %route_key.key_string(),
                        error = %e,
                        "measured fee_plan_cost rejected a candidate input"
                    );
                }
            },
            Err(e) => {
                tracing::trace!(
                    target: "bot.discovery",
                    %amount_in,
                    error = %e,
                    "route-key simulation failed while pricing a candidate input"
                );
            }
        }
        self.fee_resolution_failures
            .set(self.fee_resolution_failures.get().saturating_add(1));
        U256::MAX
    }
}

fn optimize_path(
    optimizer: &PathOptimizer,
    path: &ArbitragePath,
    path_pools: &[AMM],
    config: &DiscoveryConfig,
) -> OptimizeOutcome {
    if let Some(measured) = config.measured_fee.as_ref() {
        let protocols: Vec<ProtocolKind> = path_pools.iter().map(protocol_kind_of_amm).collect();
        let estimator = config.gas_estimator.as_deref();
        match topology_never_approved_reason(measured, estimator, &protocols) {
            Ok(Some(reason)) => return OptimizeOutcome::Rejected { reason },
            Ok(None) => {}
            Err(ProfileSupportError::RouteKey(..)) => {
                return OptimizeOutcome::Rejected {
                    reason: crate::metrics::reject_reason::ROUTE_KEY_CONSTRUCTION_ERROR,
                };
            }
            // Poisoned profile state: fail closed with the label `fee_plan_cost`
            // (`quote`) failures of the same kind get.
            Err(ProfileSupportError::Profile(_)) => {
                return OptimizeOutcome::Rejected {
                    reason: crate::metrics::reject_reason::GAS_SCREEN,
                };
            }
        }

        let fee_model =
            RouteAwareFeeCost::new(measured, estimator, path, path_pools, config.block_timestamp);
        // WHI-1409: `consider_point` calls this quote closure and then, only
        // when the quote is profitable, `fee_model.fee_cost(amount_in)` for the
        // same input. Both route through the memoizing
        // `RouteAwareFeeCost::simulate`, so a candidate costs **one**
        // simulation, and inputs the search revisits cost none. `amm_quotes`
        // counts this closure's invocations, i.e. candidate inputs *evaluated*;
        // with the memo that is an upper bound on simulations actually issued
        // (it was an ~2x undercount before the cache landed).
        let quote = |amount_in: U256| -> Result<Option<(U256, U256)>, ArbitrageError> {
            match fee_model.simulate(amount_in) {
                Ok((final_out, _, _)) => match final_out.checked_sub(amount_in) {
                    Some(gross) if !gross.is_zero() => Ok(Some((gross, final_out))),
                    _ => Ok(None),
                },
                Err(e) if e.is_incomplete_state() => Ok(None),
                Err(e) => Err(ArbitrageError::Simulation(e.to_string())),
            }
        };

        let outcome = run_search(optimizer, path, &fee_model, quote, || fee_model.work());
        // Makes the memo's cost bound observable in a real run, not just in
        // tests: `simulations` is what the hot path actually paid for, and it
        // can never exceed `quotes` (the candidate inputs evaluated), which is
        // itself capped by `OptimizationConfig::max_quotes`. Trace level so the
        // documented RUST_LOG=info bound (WHI-952) stays unaffected.
        if tracing::enabled!(target: "bot.discovery", tracing::Level::TRACE) {
            let quotes = match &outcome {
                OptimizeOutcome::Ok { work, .. }
                | OptimizeOutcome::NoOptimum { work }
                | OptimizeOutcome::Error { work, .. } => work.quotes,
                OptimizeOutcome::Rejected { .. } => 0,
            };
            tracing::trace!(
                target: "bot.discovery",
                simulations = fee_model.simulations_performed(),
                quotes,
                "route-key simulation memo: simulations issued vs candidate inputs evaluated"
            );
        }
        outcome
    } else {
        // Offline fixture path: fixed hop table. Same quote source as
        // `PathOptimizer::optimize_with_fee_quote_count`, routed through
        // `run_search` so an Err keeps its quote count (WHI-1424).
        if path_pools.len() != path.hops.len() {
            return OptimizeOutcome::Error {
                error: ArbitrageError::Optimization("Mismatch between path hops and pools".into()),
                work: OptimizeWork::default(),
            };
        }
        let quote = |amount_in: U256| {
            simulate_path(path, path_pools, amount_in)
                .map(|r| r.map(|r| (r.expected_profit, r.output_amount)))
        };
        // gas_price_wei = 0 → ZeroFeeCost semantics (net = gross).
        if config.gas.gas_price_wei == 0 {
            run_search(optimizer, path, &ZeroFeeCost, quote, OptimizeWork::default)
        } else {
            let fee = ConstantFeeCost(config.gas.calculate_gas_cost(path.hops.len()));
            run_search(optimizer, path, &fee, quote, OptimizeWork::default)
        }
    }
}

/// Screen gross → net using measured FeePolicy path or offline GasConfig.
///
/// Re-resolves the cached `(route_key, crossings)` against the **current**
/// profile status and fee bounds on every call — materialization and fee-factor
/// rescores alike — without any AMM re-simulation (WHI-1572 binding B). Returns
/// the priced gas for measured scoring (`None` offline).
fn gas_screen_net(
    gross: U256,
    hops: usize,
    cached: &CachedGross,
    config: &DiscoveryConfig,
) -> Result<(U256, Option<PricedGas>), &'static str> {
    use crate::metrics::reject_reason;

    if let Some(measured) = config.measured_fee.as_ref() {
        let priced = match config.gas_estimator.as_deref() {
            // Measured-only mode: the unchanged `quote` + `fee_plan_cost` path.
            None => match measured.quote_and_cost(&cached.route_key) {
                Ok((quote, cost)) => PricedGas {
                    tier: GasTier::Measured,
                    expected_gas_used: quote.expected_gas_used,
                    gas_limit: quote.gas_limit,
                    cost,
                    model_digest: None,
                    extrapolated: Default::default(),
                },
                Err(e) => return Err(discovery_fee_reject_reason(&e)),
            },
            Some(estimator) => price_discovery_gas(
                &measured.gas_profile,
                estimator,
                &cached.route_key,
                &GasFeatures::new(cached.route_key.protocols.clone(), cached.crossings.clone()),
                &measured.fee_context,
                measured.policy(),
            )
            .map_err(|r| r.label())?,
        };
        match net_profit_after_gas_cost(gross, priced.cost) {
            Some(net) => Ok((net, Some(priced))),
            None => Err(reject_reason::NET_PROFIT),
        }
    } else {
        if !config
            .gas
            .is_profitable_after_gas(gross, hops, default_gas_safety_margin())
        {
            return Err(reject_reason::GAS_SCREEN);
        }
        match config.gas.net_profit(gross, hops) {
            Some(n) => Ok((n, None)),
            None => Err(reject_reason::NET_PROFIT),
        }
    }
}

fn materialize_from_cache(
    path: &ArbitragePath,
    path_pools: &[AMM],
    cached: &CachedGross,
    config: &DiscoveryConfig,
) -> Result<DiscoveredOpportunity, &'static str> {
    use crate::metrics::{self, reject_reason};

    let gross = match cached.final_out.checked_sub(cached.optimal_input) {
        Some(g) if !g.is_zero() => g,
        _ => {
            metrics::record_discovery_rejected(reject_reason::GROSS_UNDERFLOW);
            return Err(reject_reason::GROSS_UNDERFLOW);
        }
    };

    let hops = path.hops.len();
    if hops > config.max_hops {
        tracing::debug!(
            target: "bot.discovery",
            hops,
            max_hops = config.max_hops,
            "skipping path above strategy hop cap"
        );
        metrics::record_discovery_rejected(reject_reason::HOP_CAP);
        return Err(reject_reason::HOP_CAP);
    }

    // WHI-949: measured path uses FeePolicy::build / fee_plan_cost (send-identical).
    // Offline fixtures keep the hop-table GasConfig screen.
    let (net_profit, priced) = match gas_screen_net(gross, hops, cached, config) {
        Ok(n) => n,
        Err(reason) => {
            metrics::record_discovery_rejected(reason);
            return Err(reason);
        }
    };

    // WHI-948: min_net_profit admission on **net**, not on optimizer gross.
    if net_profit < config.min_profit {
        metrics::record_discovery_rejected(reject_reason::NET_PROFIT);
        return Err(reject_reason::NET_PROFIT);
    }

    let is_cross = path_is_cross_protocol(path_pools);
    let protocol_kinds: Vec<ProtocolKind> = path_pools.iter().map(protocol_kind_of_amm).collect();

    let mut token_path: Vec<Address> = path.hops.iter().map(|h| h.token_in).collect();
    if let Some(last) = path.hops.last() {
        token_path.push(last.token_out);
    }

    let signature = path_signature(path, &protocol_kinds);
    let profit = I256::from_raw(gross);
    let expected_states = match collect_expected_states(path_pools) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(
                target: "bot.discovery",
                error = %e,
                "expected_states collection failed; skipping path"
            );
            metrics::record_discovery_rejected(reject_reason::EXPECTED_STATES);
            return Err(reject_reason::EXPECTED_STATES);
        }
    };
    let log_hops = hops_description(path);
    let roi = format_roi_percent(profit, cached.optimal_input).unwrap_or_else(|| "-".to_string());

    let candidate = Candidate {
        snapshot_id: config.snapshot_id,
        signature,
        hops,
        input: cached.optimal_input,
        output: cached.final_out,
        profit,
        net_profit,
        pool_addresses: path.hops.iter().map(|h| h.pool_address).collect(),
        token_path,
        amounts_out: cached.amounts_out.clone(),
        expected_states,
        path: path.clone(),
        pools: path_pools.to_vec(),
        log_hops,
        roi,
    };

    let gas = priced.map(|priced| {
        let pools: Vec<Address> = path.hops.iter().map(|h| h.pool_address).collect();
        let venues = venue_labels(
            &pools,
            &protocol_kinds,
            &cached.route_key,
            config.pool_venues.as_deref(),
            config.gas_estimator.as_deref(),
        );
        let mut extrapolated = priced.extrapolated;
        if priced.tier == GasTier::Estimated {
            if let Some(estimator) = config.gas_estimator.as_deref() {
                extrapolated.venue_family =
                    venues.iter().any(|v| !estimator.venue_family_seen(v.factory));
            }
        }
        CandidateGasEvidence {
            tier: priced.tier,
            expected_gas_used: priced.expected_gas_used,
            gas_limit: priced.gas_limit,
            model_digest: priced.model_digest,
            extrapolated,
            crossings: cached.crossings.clone(),
            venues,
        }
    });

    Ok(DiscoveredOpportunity {
        candidate,
        route_key: cached.route_key.clone(),
        is_cross_protocol: is_cross,
        protocol_kinds,
        gas,
    })
}

fn topology_signature(path: &ArbitragePath) -> String {
    path.hops
        .iter()
        .map(|h| format!("{:#x}:{}->{}", h.pool_address, h.token_in, h.token_out))
        .collect::<Vec<_>>()
        .join("|")
}

/// True when any hop resolves to a Moe LB pool in the live universe.
fn path_includes_moe(path: &ArbitragePath, pools: &[AMM]) -> bool {
    let Ok(path_pools) = pools_for_path(path, pools) else {
        return false;
    };
    path_pools.iter().any(|p| matches!(p, AMM::MoeLbPair(_)))
}

fn path_signature(path: &ArbitragePath, kinds: &[ProtocolKind]) -> String {
    let hops: Vec<String> = path
        .hops
        .iter()
        .zip(kinds.iter())
        .map(|(hop, kind)| {
            format!(
                "{}:{}->{}/{:#x}",
                kind.as_str(),
                hop.token_in,
                hop.token_out,
                hop.pool_address
            )
        })
        .collect();
    hops.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        BlockFeeContext, RuntimeGasProfile, RuntimeProfileConfig, TickCrossingBucket,
    };
    use crate::service::fixture::{
        cross_protocol_fixture_pools, fixture_agni_pool_address, fixture_settlement_asset,
        fixture_v2_pool_address,
    };
    use alloy::primitives::U256;

    fn engine() -> DiscoveryEngine {
        let pools = cross_protocol_fixture_pools();
        DiscoveryEngine::build(&pools, fixture_settlement_asset(), 3).expect("build")
    }

    #[test]
    fn build_graph_and_find_cycles_once_per_engine() {
        let eng = engine();
        assert_eq!(eng.index().build_graph_calls(), 1);
        assert_eq!(eng.index().find_cycles_calls(), 1);
        assert!(eng.index().cycles_total() >= 1);
    }

    #[test]
    fn pool_index_maps_dirty_pool_to_affected_cycles() {
        let eng = engine();
        let v2 = fixture_v2_pool_address();
        let agni = fixture_agni_pool_address();
        let affected_v2 = eng.index().affected_path_indices(&HashSet::from([v2]));
        let affected_agni = eng.index().affected_path_indices(&HashSet::from([agni]));
        assert!(
            !affected_v2.is_empty(),
            "v2 pool must participate in at least one cycle"
        );
        assert!(
            !affected_agni.is_empty(),
            "agni pool must participate in at least one cycle"
        );
        // Cross-protocol fixture cycle uses both venues.
        let both = eng
            .index()
            .affected_path_indices(&HashSet::from([v2, agni]));
        assert!(both.len() >= affected_v2.len().max(affected_agni.len()));
    }

    #[test]
    fn full_scope_optimizes_all_cycles() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;
        let (_found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");
        assert_eq!(stats.scope, "full");
        assert_eq!(stats.cycles_optimized, stats.cycles_total);
        assert!(eng.is_primed());
        // WHI-976: evaluating cycles implies AMM quotes ran.
        assert!(
            stats.amm_quotes > 0,
            "full optimize must record amm_quotes (got 0 with cycles_optimized={})",
            stats.cycles_optimized
        );
        // Second Full still optimizes all; topology counters stay at 1.
        let (_found2, stats2) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover2");
        assert_eq!(stats2.cycles_optimized, stats2.cycles_total);
        assert!(stats2.amm_quotes > 0);
        assert_eq!(eng.index().build_graph_calls(), 1);
        assert_eq!(eng.index().find_cycles_calls(), 1);
    }

    /// WHI-976: `cycles_evaluated > 0 ⇒ amm_quotes > 0`.
    ///
    /// The pre-fix counter only incremented on a found optimum, so a pass of
    /// all-unprofitable cycles reported `amm_quotes=0` while still counting
    /// every cycle as evaluated. Prove the unprofitable (NoOptimum) path still
    /// records the binary-search quote work.
    #[test]
    fn cycles_optimized_implies_amm_quotes_even_when_no_optimum() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        // Absurd gas price → every sample fails net fee screen → NoOptimum for
        // every cycle, which is the production shape of a quiet market.
        config.gas.gas_price_wei = u128::MAX;

        let (found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");
        assert!(
            stats.cycles_optimized > 0,
            "fixture must select cycles to optimize"
        );
        assert!(
            found.is_empty(),
            "absurd gas must yield zero candidates (got {})",
            found.len()
        );
        assert!(
            stats.amm_quotes > 0,
            "WHI-976: NoOptimum path must still count amm_quotes \
             (cycles_optimized={}, amm_quotes=0)",
            stats.cycles_optimized
        );
        // At least one quote per optimized cycle (search always samples when
        // max_input > 0). Stronger than `> 0` and bounds the dead-counter bug:
        // a pass of K cycles that each ran the binary search reports ≥K quotes.
        assert!(
            stats.amm_quotes >= stats.cycles_optimized as u64,
            "expected ≥1 quote per optimized cycle: cycles_optimized={}, amm_quotes={}",
            stats.cycles_optimized,
            stats.amm_quotes
        );
        // Mapped watch-path stats use the same invariant.
        let pass = crate::service::discovery::DiscoveryPassStats::from(stats);
        assert!(pass.cycles_evaluated > 0);
        assert!(
            pass.amm_quotes > 0,
            "cycles_evaluated > 0 must imply amm_quotes > 0 when optimizer ran"
        );
        assert!(pass.amm_quotes >= pass.cycles_evaluated);
    }

    #[test]
    fn dirty_touched_pass_records_amm_quotes() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;
        eng.discover(&pools, &config, &TipRefreshScope::Full)
            .expect("prime");

        let dirty = HashSet::from([fixture_v2_pool_address()]);
        let (_found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Touched(dirty))
            .expect("touched");
        assert!(stats.cycles_optimized > 0);
        assert!(
            stats.amm_quotes > 0,
            "dirty-touched reopt must record amm_quotes (got 0)"
        );
    }

    #[test]
    fn touched_empty_dirty_optimizes_zero_after_prime() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;

        let (full_found, full_stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("full");
        assert_eq!(full_stats.cycles_optimized, full_stats.cycles_total);
        assert!(!full_found.is_empty());

        let (touched_found, touched_stats) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("touched empty");
        assert_eq!(touched_stats.scope, "touched");
        assert_eq!(touched_stats.cycles_optimized, 0);
        assert_eq!(touched_stats.dirty_pools, 0);
        assert_eq!(touched_found.len(), full_found.len());
        assert_eq!(
            touched_found
                .iter()
                .map(|o| o.candidate.signature.as_str())
                .collect::<Vec<_>>(),
            full_found
                .iter()
                .map(|o| o.candidate.signature.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn touched_dirty_optimizes_only_affected_union() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;

        eng.discover(&pools, &config, &TipRefreshScope::Full)
            .expect("prime");

        let dirty = HashSet::from([fixture_v2_pool_address()]);
        let expected = eng.index().affected_path_indices(&dirty).len();
        let (_found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Touched(dirty))
            .expect("touched dirty");
        assert_eq!(stats.scope, "touched");
        assert_eq!(stats.cycles_optimized, expected);
        assert!(expected > 0, "dirty v2 must hit at least one cycle");
        // Fixture topology is small: every cycle may touch the dirty pool, in
        // which case optimized == total. The invariant is equality with the
        // inverted-index union, not a strict subset of the universe.
        assert_eq!(stats.dirty_pools, 1);
    }

    #[test]
    fn incremental_matches_full_scan_on_static_fixture() {
        let pools = cross_protocol_fixture_pools();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;

        let mut full_eng = engine();
        let (full_a, _) = full_eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("full a");
        let (full_b, _) = full_eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("full b");

        let mut inc = engine();
        let (inc_a, _) = inc
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("inc prime");
        let (inc_b, stats) = inc
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("inc empty dirty");

        assert_eq!(stats.cycles_optimized, 0);
        assert_eq!(opportunity_keys(&full_a), opportunity_keys(&inc_a));
        assert_eq!(opportunity_keys(&full_b), opportunity_keys(&inc_b));
        assert_eq!(opportunity_keys(&full_a), opportunity_keys(&full_b));
    }

    #[test]
    fn unprimed_touched_still_optimizes_all() {
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;
        let (_found, stats) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("unprimed");
        assert_eq!(stats.scope, "full");
        assert_eq!(stats.cycles_optimized, stats.cycles_total);
    }

    #[test]
    fn fee_factor_change_triggers_gas_rescores_without_reopt() {
        // Offline GasConfig path encodes price into FeeScoreKey; changing it
        // with an empty dirty set must re-screen cached gross quotes.
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;

        let (found, _) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("prime");
        assert!(
            !found.is_empty(),
            "zero-gas offline fixture must produce candidates"
        );

        config.gas.gas_price_wei = 1; // fee factor only
        let (found2, stats) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("rescore");
        assert_eq!(stats.cycles_optimized, 0);
        assert!(
            stats.gas_rescores > 0,
            "gas_price change must re-score cached paths (got {})",
            stats.gas_rescores
        );
        // Higher gas may drop candidates; re-score still ran.
        let _ = found2;
    }

    #[test]
    fn measured_priority_only_change_triggers_gas_rescores() {
        use crate::execution::{
            BlockFeeContext, RuntimeGasProfile, RuntimeProfileConfig,
        };
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
        use std::path::PathBuf;
        use std::sync::Arc;

        // Prime offline (zero gas) so the cache has gross quotes, attach
        // measured scoring, then flip priority only (base fee fixed).
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;
        eng.discover(&pools, &config, &TipRefreshScope::Full)
            .expect("offline prime");

        let profile = Arc::new(
            RuntimeGasProfile::load(
                &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("config/gas_profiles/mantle_mainnet_v1.json"),
                RuntimeProfileConfig::mantle_mainnet(Vec::new()),
            )
            .expect("profile"),
        );
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        config.measured_fee = Some(MeasuredFeeScoring::new(
            Arc::clone(&profile),
            0,
            1,
            fee_ctx.clone(),
        ));
        let (_f1, stats1) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("attach measured");
        assert_eq!(stats1.cycles_optimized, 0);
        assert!(
            stats1.gas_rescores > 0,
            "switching to measured must re-score cache"
        );
        let key_lo = config.measured_fee.as_ref().unwrap().fee_score_key();

        config.measured_fee = Some(MeasuredFeeScoring::new(
            profile,
            1, // priority only
            1,
            fee_ctx,
        ));
        let key_hi = config.measured_fee.as_ref().unwrap().fee_score_key();
        assert_ne!(key_lo, key_hi);
        assert_eq!(key_lo.base_fee_per_gas, key_hi.base_fee_per_gas);

        let (_f2, stats2) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("priority rescore");
        assert_eq!(stats2.cycles_optimized, 0);
        assert!(
            stats2.gas_rescores > 0,
            "priority-only policy change must re-score cached paths (got {})",
            stats2.gas_rescores
        );
    }

    #[test]
    fn quiet_touched_clears_moe_path_cache_without_reopt() {
        // Fixture Moe pool is not on the WMNT cycle, so this only asserts the
        // helper + empty-dirty path: non-Moe caches survive; topology counters
        // stay at 1. Moe-on-cycle coverage is enforced by the drop logic when
        // a Moe hop is present.
        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.gas.gas_price_wei = 0;
        eng.discover(&pools, &config, &TipRefreshScope::Full)
            .expect("prime");
        config.block_timestamp = config.block_timestamp + 2;
        let (found, stats) = eng
            .discover(
                &pools,
                &config,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .expect("quiet");
        assert_eq!(stats.cycles_optimized, 0);
        // Cross-protocol V2+V3 fixture cycle has no Moe hop — still rediscovered.
        assert!(
            found.iter().any(|o| o.is_cross_protocol),
            "non-Moe cached paths must survive a quiet block"
        );
        assert_eq!(eng.index().build_graph_calls(), 1);
    }

    fn opportunity_keys(found: &[DiscoveredOpportunity]) -> Vec<(String, U256, U256)> {
        found
            .iter()
            .map(|o| {
                (
                    o.candidate.signature.clone(),
                    o.candidate.input,
                    o.candidate.net_profit,
                )
            })
            .collect()
    }

    /// WHI-1411 test fixture: loads the pinned mainnet gas profile and invalidates the two
    /// routes the cross-protocol fixture pools produce (`v2+v2` and `v2+v3` at zero tick
    /// crossings), forcing every discovery attempt against [`cross_protocol_fixture_pools`]
    /// to reject pre-simulation. Invalidation is in-memory only (no `invalidation_path` on
    /// the loaded profile), so this never touches disk.
    fn gas_profile_with_fixture_routes_invalidated() -> std::sync::Arc<RuntimeGasProfile> {
        use std::path::PathBuf;
        use std::sync::Arc;

        let artifact = crate::execution::gas_profile::load_artifact(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/mantle_mainnet_v1.json"),
        )
        .expect("artifact");
        let profile = Arc::new(
            RuntimeGasProfile::from_artifact_with_identity(
                artifact,
                RuntimeProfileConfig::mantle_mainnet(Vec::new()),
                crate::execution::gas_runtime::mainnet_verified_identity(),
            )
            .expect("profile"),
        );
        let r_v2_v2 = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let r_v2_v3 = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3])
            .unwrap()
            .with_v3_ticks(TickCrossingBucket::Zero);
        let _ = profile.invalidate(&r_v2_v2);
        let _ = profile.invalidate(&r_v2_v3);
        profile
    }

    /// WHI-1411 acceptance: a test drives a 100%-rejection configuration and asserts
    /// the liveness alarm fires (the current invariant does not).
    #[test]
    fn liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
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

        let buf = BufferWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::ERROR)
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());

        let profile = gas_profile_with_fixture_routes_invalidated();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        config.measured_fee = Some(MeasuredFeeScoring::new(
            Arc::clone(&profile),
            0,
            1,
            fee_ctx,
        ));

        let (found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");

        assert!(stats.cycles_optimized > 0, "fixture has cycles to optimize");
        assert_eq!(stats.paths_quoted, 0, "zero paths must reach the optimizer");
        assert_eq!(stats.amm_quotes, 0, "zero quotes spent when all routes rejected pre-sim");
        assert!(found.is_empty(), "no candidates found under 100% rejection");

        // WHI-1411 invariant: liveness alarm must fire!
        assert!(stats.liveness_alarm, "liveness alarm must fire on 100% pre-sim rejection");

        // Rejection counts must sum to paths considered
        assert_eq!(stats.rejects.total(), stats.cycles_optimized as u64);
        assert!(stats.rejects.unknown_route + stats.rejects.unapproved_route > 0);

        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        // WHI-1411 alarm fired:
        assert!(
            text.contains("WHI-1411 liveness invariant violated"),
            "expected WHI-1411 liveness error in logs; got: {text}"
        );
        // WHI-976 invariant did NOT fire (because paths_quoted was 0):
        assert!(
            !text.contains("WHI-976 invariant violated"),
            "WHI-976 invariant must not fire when paths_quoted=0"
        );
        // WHI-1544 truly pre-rejected control: the pre-simulation explanation is
        // accurate here (only route buckets, zero quotes) and is kept.
        assert_eq!(
            stats.rejects.other, 0,
            "control premise: no `other` rejects"
        );
        assert!(
            text.contains("all evaluated cycles rejected pre-simulation")
                && text.contains("pre_simulation_only"),
            "pre-rejected control must keep its pre-simulation explanation; got: {text}"
        );
    }

    /// WHI-1544: the alarm's cause for an Error-only zero-completion window (the
    /// release review's reproduction: four Error searches, `paths_quoted = 0`,
    /// quotes spent) and for the committed live trigger window (2,950 unapproved +
    /// 18 optimizer errors, 468 AMM quotes) must not be pre-simulation rejection,
    /// while a truly pre-rejected control keeps that explanation. Both are
    /// unhealthy; only the explanation differs.
    #[test]
    fn zero_completion_cause_never_claims_pre_simulation_when_optimizer_errored() {
        let error_only = DiscoveryRejectCounts {
            other: 4,
            ..Default::default()
        };
        let live_trigger_window = DiscoveryRejectCounts {
            unapproved_route: 2_950,
            other: 18,
            ..Default::default()
        };
        let pre_rejected = DiscoveryRejectCounts {
            unknown_route: 3,
            unapproved_route: 1_364,
            pool_lookup: 1,
            ..Default::default()
        };
        for (label, rejects, quotes, expected) in [
            (
                "error-only",
                error_only,
                104,
                ZeroCompletionCause::OptimizerQuoteWorkPresent,
            ),
            // Without quote evidence `other > 0` still rules the pre-simulation
            // claim out, but proves nothing either way (see the GAS_SCREEN test).
            (
                "error-only, no quotes",
                error_only,
                0,
                ZeroCompletionCause::OtherRejectsAmbiguous,
            ),
            (
                "live trigger window",
                live_trigger_window,
                468,
                ZeroCompletionCause::OptimizerQuoteWorkPresent,
            ),
        ] {
            let cause = zero_completion_cause(&rejects, quotes);
            assert_eq!(cause, expected, "{label}");
            assert!(
                !cause
                    .explanation()
                    .contains("all evaluated cycles rejected pre-simulation"),
                "{label}: {}",
                cause.explanation()
            );
        }
        let control = zero_completion_cause(&pre_rejected, 0);
        assert_eq!(control, ZeroCompletionCause::PreSimulationOnly);
        assert!(control
            .explanation()
            .contains("all evaluated cycles rejected pre-simulation"));
        // Pre-simulation buckets but quotes spent: not provably pre-simulation.
        assert_eq!(
            zero_completion_cause(&pre_rejected, 1),
            ZeroCompletionCause::OptimizerQuoteWorkPresent
        );
        assert_eq!(
            zero_completion_cause(&DiscoveryRejectCounts::default(), 0),
            ZeroCompletionCause::Undetermined
        );
    }

    /// WHI-1544 (review of the first fix): `other` also holds pre-simulation
    /// prefilter rejects — `gas_screen` on poisoned profile state and
    /// `route_key_construction_error` — which spend no quote. A zero-completion
    /// window made only of those must claim neither all-pre-simulation rejection
    /// nor optimizer work; it is reported as ambiguous.
    #[test]
    fn zero_completion_cause_does_not_claim_optimizer_work_from_pre_simulation_other_rejects() {
        use crate::metrics::reject_reason;

        for reason in [
            reject_reason::GAS_SCREEN,
            reject_reason::ROUTE_KEY_CONSTRUCTION_ERROR,
        ] {
            let mut rejects = DiscoveryRejectCounts::default();
            rejects.record(reason);
            assert_eq!(rejects.other, 1, "premise: {reason} lands in `other`");
            let cause = zero_completion_cause(&rejects, 0);
            let (label, text) = (cause.label(), cause.explanation());
            assert_ne!(label, "optimizer_work_present", "{reason}");
            assert_ne!(label, "optimizer_quote_work_present", "{reason}");
            assert_ne!(label, "pre_simulation_only", "{reason}");
            assert!(
                !text.contains("NOT solely pre-simulation"),
                "{reason}: {text}"
            );
            assert!(
                !text.contains("all evaluated cycles rejected pre-simulation"),
                "{reason}: {text}"
            );
            assert!(
                text.contains("cannot establish an all-pre-simulation cause"),
                "{reason}: {text}"
            );
        }
    }

    /// WHI-1411 acceptance: a test drives a sustained-window configuration (three
    /// consecutive discovery passes each resolving zero paths to the optimizer, none
    /// of them a `Full`/exhaustive pass) and asserts the liveness alarm only fires once
    /// the sustained-window threshold is actually reached — not on the first dead pass.
    #[test]
    fn liveness_alarm_fires_on_sustained_zero_quoted_window() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
        use std::sync::Arc;

        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut prime_config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        prime_config.gas.gas_price_wei = 0;
        eng.discover(&pools, &prime_config, &TipRefreshScope::Full)
            .expect("prime");

        let profile = gas_profile_with_fixture_routes_invalidated();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let mut config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config.measured_fee = Some(MeasuredFeeScoring::new(
            Arc::clone(&profile),
            0,
            1,
            fee_ctx,
        ));
        config.liveness_dead_heads_threshold = Some(3);

        let dirty = HashSet::from([fixture_v2_pool_address()]);

        // Passes 1-2 (Touched, not Full) each resolve zero paths but have not yet reached
        // the 3-pass sustained-window threshold, and are not exhaustive (Touched only samples
        // the dirty subset) — the alarm must stay quiet.
        for pass in 1..=2 {
            let (_found, stats) = eng
                .discover(&pools, &config, &TipRefreshScope::Touched(dirty.clone()))
                .unwrap_or_else(|e| panic!("touched pass {pass}: {e}"));
            assert!(stats.cycles_optimized > 0, "pass {pass} must evaluate cycles");
            assert_eq!(stats.paths_quoted, 0, "pass {pass} must resolve zero paths");
            assert!(
                !stats.liveness_alarm,
                "pass {pass}/3 must not yet trip the sustained-window alarm"
            );
        }

        // Pass 3 reaches the threshold.
        let (_found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Touched(dirty))
            .expect("touched pass 3");
        assert_eq!(stats.paths_quoted, 0);
        assert!(
            stats.liveness_alarm,
            "3rd consecutive dead pass must trip the sustained-window liveness alarm"
        );
    }

    /// WHI-1411 round-3: a per-call `liveness_dead_heads_threshold` override must apply only
    /// to the call that supplied it, never latch into engine state and silently apply to a
    /// later call that passes `None`. Two dead passes under an override of 3 must NOT trip the
    /// alarm on a third dead pass that reverts to the engine's real default (10) — under the
    /// old (buggy) latched behaviour this test would fail because the override would still be
    /// in effect on pass 3.
    #[test]
    fn liveness_dead_heads_threshold_override_does_not_latch_into_later_calls_without_override() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
        use std::sync::Arc;

        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut prime_config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        prime_config.gas.gas_price_wei = 0;
        eng.discover(&pools, &prime_config, &TipRefreshScope::Full)
            .expect("prime");

        let profile = gas_profile_with_fixture_routes_invalidated();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let mut config_with_override = DiscoveryConfig::offline_default(fixture_settlement_asset());
        config_with_override.measured_fee = Some(MeasuredFeeScoring::new(
            Arc::clone(&profile),
            0,
            1,
            fee_ctx.clone(),
        ));
        config_with_override.liveness_dead_heads_threshold = Some(3);

        let dirty = HashSet::from([fixture_v2_pool_address()]);

        // Two dead passes under the override (threshold 3) — not yet tripped.
        for pass in 1..=2 {
            let (_found, stats) = eng
                .discover(
                    &pools,
                    &config_with_override,
                    &TipRefreshScope::Touched(dirty.clone()),
                )
                .unwrap_or_else(|e| panic!("override pass {pass}: {e}"));
            assert_eq!(stats.paths_quoted, 0, "pass {pass} must resolve zero paths");
            assert!(!stats.liveness_alarm, "pass {pass}/3 must not yet trip");
        }

        // Third dead pass, but this call supplies `None` — must fall back to the engine's
        // fixed default threshold (10), not silently inherit the prior call's override of 3.
        let mut config_without_override =
            DiscoveryConfig::offline_default(fixture_settlement_asset());
        config_without_override.measured_fee =
            Some(MeasuredFeeScoring::new(Arc::clone(&profile), 0, 1, fee_ctx.clone()));
        assert_eq!(config_without_override.liveness_dead_heads_threshold, None);

        let (_found, stats) = eng
            .discover(
                &pools,
                &config_without_override,
                &TipRefreshScope::Touched(dirty),
            )
            .expect("unset-override pass 3");
        assert_eq!(stats.paths_quoted, 0);
        assert!(
            !stats.liveness_alarm,
            "a 3rd dead pass must not trip the alarm once the override no longer applies \
             — the default threshold (10) has not been reached"
        );
    }

    /// WHI-1411 round-3: the liveness gauge must not be left latched at a stale value when
    /// `discover()` takes the empty-pools early return — it must be explicitly reset to false.
    #[test]
    fn empty_pools_pass_resets_the_liveness_gauge_to_false() {
        use crate::metrics::render_with_local;

        let mut eng = engine();
        let config = DiscoveryConfig::offline_default(fixture_settlement_asset());

        let rendered = render_with_local(|| {
            let (_found, stats) = eng
                .discover(&[], &config, &TipRefreshScope::Full)
                .expect("empty-pools pass");
            assert!(!stats.liveness_alarm);
        });

        assert!(
            rendered.contains("arbbot_discovery_liveness_alarm 0"),
            "empty-pools pass must explicitly report the gauge as false, not leave it unset:\n{rendered}"
        );
    }

    // -- WHI-1409: route-key contract reconciliation -----------------------

    /// WHI-1409 flagship regression: a topology whose profile entries sit
    /// **only** at nonzero crossing buckets (all Unsupported, no zero-bucket
    /// entry at all). The old `topology_route_key()` always guessed zero, so
    /// such a class could never resolve to anything but `UnknownRoute`,
    /// independent of which pools were in the universe. The reconciled check
    /// enumerates every real bucket and must find the profile's actual (if
    /// Unsupported) entry instead.
    ///
    /// WHI-1422 gave every 2..=3-hop v3/moe topology an explicit entry at
    /// **every** bucket, so the pinned artifact no longer contains a
    /// nonzero-only topology. To keep that premise explicit (review PR108-F6),
    /// this test's artifact is the pinned one with the zero-bucket entries of
    /// `[v3,v3,v3]` and `[moe,moe,moe]` removed; the premise is asserted before
    /// the classification checks. Every other entry is the pinned one.
    #[test]
    fn topology_never_approved_reason_finds_existing_nonzero_bucket_entries() {
        use crate::execution::gas_profile::{BinCrossingBucket, ProfileStatus};
        use std::path::PathBuf;

        let mut artifact = crate::execution::gas_profile::load_artifact(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/mantle_mainnet_v1.json"),
        )
        .expect("artifact");

        let v3v3v3 = vec![ProtocolKind::V3, ProtocolKind::V3, ProtocolKind::V3];
        let moemoemoe = vec![ProtocolKind::Moe, ProtocolKind::Moe, ProtocolKind::Moe];
        let zero_keys = [
            RouteKey::new(v3v3v3.clone())
                .unwrap()
                .with_v3_ticks(TickCrossingBucket::Zero),
            RouteKey::new(moemoemoe.clone())
                .unwrap()
                .with_moe_bins(BinCrossingBucket::Zero),
        ];
        let before = artifact.profiles.len();
        artifact
            .profiles
            .retain(|p| !zero_keys.contains(&p.route_key));
        assert_eq!(
            artifact.profiles.len(),
            before - zero_keys.len(),
            "fixture premise: the pinned artifact has both zero-bucket entries to remove"
        );
        for protocols in [&v3v3v3, &moemoemoe] {
            let entries: Vec<_> = artifact
                .profiles
                .iter()
                .filter(|p| &p.route_key.protocols == protocols)
                .collect();
            assert!(
                !entries.is_empty()
                    && entries
                        .iter()
                        .all(|p| p.status == ProfileStatus::Unsupported && p.reason.is_some()),
                "fixture premise: {protocols:?} keeps only nonzero-bucket Unsupported entries"
            );
        }
        artifact.content_digest =
            crate::execution::gas_profile::compute_content_digest(&artifact).unwrap();
        let mut config = RuntimeProfileConfig::mantle_mainnet(Vec::new());
        config.expected_content_digest = artifact.content_digest.clone();

        let profile = std::sync::Arc::new(
            RuntimeGasProfile::from_artifact_with_identity(
                artifact,
                config,
                crate::execution::gas_runtime::mainnet_verified_identity(),
            )
            .expect("profile"),
        );
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: alloy::primitives::B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let measured = crate::service::fee_scoring::MeasuredFeeScoring::new(profile, 0, 1, fee_ctx);

        // Approved at ticks=0 — must proceed to the real search (`None`).
        assert_eq!(
            topology_never_approved_reason(&measured, None, &[ProtocolKind::V2, ProtocolKind::V3])
                .unwrap(),
            None
        );

        // 3-hop v3/v3/v3: no zero-bucket entry in this fixture, but 1-5, 6-20
        // and 21+ exist, all Unsupported — must resolve to UNAPPROVED_ROUTE,
        // never UNKNOWN_ROUTE.
        assert_eq!(
            topology_never_approved_reason(&measured, None, &v3v3v3).unwrap(),
            Some(crate::metrics::reject_reason::UNAPPROVED_ROUTE)
        );

        // moe/moe/moe: same story (only the 1-3, 4-10 and 11+ bin entries
        // remain, all Unsupported).
        assert_eq!(
            topology_never_approved_reason(&measured, None, &moemoemoe).unwrap(),
            Some(crate::metrics::reject_reason::UNAPPROVED_ROUTE)
        );

        // v3/moe/v3 (unmodified pinned entries): WHI-1422 gave every bucket an
        // explicit entry (it had none before), none approved (`ticks=0:bins=0`
        // is withheld by review PR108-F2/F3) — UNAPPROVED_ROUTE, never
        // UNKNOWN_ROUTE.
        assert_eq!(
            topology_never_approved_reason(&measured, None,
                &[ProtocolKind::V3, ProtocolKind::Moe, ProtocolKind::V3]
            )
            .unwrap(),
            Some(crate::metrics::reject_reason::UNAPPROVED_ROUTE)
        );

        // A 4-hop mixed class has no entry at any bucket (WHI-1422 covers
        // 2..=3 hops) — a genuine profile *coverage* gap, not a route-key
        // construction bug. UNKNOWN_ROUTE is the correct, honest answer here.
        assert_eq!(
            topology_never_approved_reason(&measured, None,
                &[
                    ProtocolKind::V3,
                    ProtocolKind::Moe,
                    ProtocolKind::V3,
                    ProtocolKind::Moe
                ]
            )
            .unwrap(),
            Some(crate::metrics::reject_reason::UNKNOWN_ROUTE)
        );
    }

    /// WHI-1409 fixture: promotes the pinned artifact's existing (Unsupported)
    /// `['v3','v3'] ticks=1-5` entry to `Approved`, reusing the same
    /// stats/holdout shape as the real `['v2','v3'] ticks=0` approval. This
    /// simulates "the profile has been extended to cover a real nonzero-bucket
    /// class" without touching the checked-in artifact, so the test can prove
    /// the *consumer* (optimize) can actually reach and price such a class.
    fn gas_profile_with_v3v3_low_tick_approved() -> std::sync::Arc<RuntimeGasProfile> {
        gas_profile_with_v3v3_tick_approved(TickCrossingBucket::Low)
    }

    /// [`gas_profile_with_v3v3_low_tick_approved`] generalised to any
    /// `['v3','v3']` tick bucket (WHI-1424 approves one the fixture never reaches).
    fn gas_profile_with_v3v3_tick_approved(
        bucket: TickCrossingBucket,
    ) -> std::sync::Arc<RuntimeGasProfile> {
        use crate::execution::gas_profile::{DistributionStats, HoldoutResult, ProfileStatus};
        use std::path::PathBuf;

        let mut artifact = crate::execution::gas_profile::load_artifact(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/mantle_mainnet_v1.json"),
        )
        .expect("artifact");

        let target = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3])
            .unwrap()
            .with_v3_ticks(bucket);
        let entry = artifact
            .profiles
            .iter_mut()
            .find(|p| p.route_key == target)
            .expect("v3+v3 tick entry must already exist (Unsupported) in the pinned artifact");
        assert_eq!(
            entry.status,
            ProfileStatus::Unsupported,
            "fixture assumes this class starts Unsupported (WHI-1409 drift guard)"
        );
        entry.status = ProfileStatus::Approved;
        entry.reason = None;
        entry.stats = Some(DistributionStats {
            sample_count: 12,
            min: 231_740,
            p50: 231_740,
            p95: 231_740,
            p99: 231_740,
            max: 231_740,
        });
        entry.expected_gas_used = Some(231_740);
        entry.gas_limit = Some(328_088);
        entry.holdout = Some(HoldoutResult {
            holdout_count: 2,
            holdout_max: 231_740,
            holdout_below_limit: true,
            train_below_limit: true,
            holdout_below_block_gas_limit: true,
            train_below_block_gas_limit: true,
            all_below_limit: true,
            all_below_block_gas_limit: true,
        });

        artifact.content_digest =
            crate::execution::gas_profile::compute_content_digest(&artifact).unwrap();

        let mut config = RuntimeProfileConfig::mantle_mainnet(Vec::new());
        config.expected_content_digest = artifact.content_digest.clone();

        std::sync::Arc::new(
            RuntimeGasProfile::from_artifact_with_identity(
                artifact,
                config,
                crate::execution::gas_runtime::mainnet_verified_identity(),
            )
            .expect("profile with promoted v3+v3 low-tick route"),
        )
    }

    /// WHI-1409 acceptance: a 2-hop V3+V3 cycle whose *real* simulated route
    /// key is `ticks=1-5` (not zero) must reach the optimizer and be found,
    /// with the discovered opportunity's route key matching the real bucket —
    /// not the old always-zero topology guess. Under the pre-fix code this
    /// path was **permanently** `Rejected` before ever running a single quote
    /// (`topology_route_key` always queried `ticks=0`, which is Unsupported
    /// for `['v3','v3']` and would stay so even after a real profile
    /// expansion at a nonzero bucket like this fixture's).
    #[test]
    fn measured_fee_optimize_resolves_real_nonzero_bucket_and_materializes_the_same_key() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;

        let (wmnt, pools) = v3_v3_crossing_fixture_pools();
        let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).expect("build engine");

        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.max_input = U256::from(50_000u128 * V3_V3_FIXTURE_SCALE);

        let profile = gas_profile_with_v3v3_low_tick_approved();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        config.measured_fee = Some(MeasuredFeeScoring::new(profile, 0, 1, fee_ctx));

        let (found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");

        assert!(stats.cycles_optimized > 0, "fixture must produce a v3-v3 cycle");
        assert!(
            stats.paths_quoted > 0,
            "WHI-1409: the real per-sample route key must let this path reach the \
             optimizer instead of being pre-simulation rejected on a guessed zero-bucket key"
        );
        assert_eq!(
            stats.rejects.unknown_route, 0,
            "the real bucket must resolve against the pinned profile's existing \
             (if unsupported) entries, never fall through as UnknownRoute"
        );
        assert_eq!(
            found.len(),
            1,
            "expected exactly one v3-v3 opportunity, got {found:?}"
        );
        assert_eq!(
            found[0].route_key.v3_tick_crossings,
            Some(TickCrossingBucket::Low),
            "materialize's route key must reflect the real (nonzero) tick crossing \
             count, not the old always-zero topology guess"
        );
    }

    /// WHI-1409 acceptance (direct guard, round 1 fix): assert `RouteAwareFeeCost`
    /// — the exact fee model `optimize_path` prices every search sample with —
    /// prices a candidate input at *exactly* `fee_plan_cost` of the route key an
    /// independent `simulate_mixed_path_with_route_key` call (the function
    /// materialize itself calls) derives for that same input. This is the literal
    /// "optimize's route key == materialize's route key" guard the acceptance
    /// criteria ask for, not just an end-to-end proxy: it holds at a small input
    /// (zero crossings, both pools) and at a large one (pool_b crosses two
    /// ticks), i.e. across a bucket transition, not only at the eventual winner.
    #[test]
    fn route_aware_fee_cost_prices_exactly_the_route_key_materialize_would_derive() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;

        let (_wmnt, pools) = v3_v3_crossing_fixture_pools();
        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).expect("pools for path");

        let profile = gas_profile_with_v3v3_low_tick_approved();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let measured = MeasuredFeeScoring::new(profile, 0, 1, fee_ctx);
        let block_timestamp = 0;
        let fee_model = RouteAwareFeeCost::new(&measured, None, &path, &path_pools, block_timestamp);

        let small = U256::from(1_000u128 * V3_V3_FIXTURE_SCALE); // zero crossings on both legs
        let large = U256::from(20_000u128 * V3_V3_FIXTURE_SCALE); // pool_b crosses 2 ticks
        for amount_in in [small, large] {
            let (_, _, real_key) =
                crate::service::discovery::simulate_mixed_path_with_route_key(&path, &path_pools, amount_in, block_timestamp)
                    .expect("materialize-equivalent simulation");
            let expected_cost = measured
                .fee_plan_cost(&real_key)
                .expect("promoted profile approves both the zero and low-tick buckets");
            use crate::arbitrage::optimizer::FeeCostModel;
            assert_eq!(
                fee_model.fee_cost(amount_in),
                expected_cost,
                "optimize's per-sample fee model must price amount_in={amount_in} at exactly \
                 fee_plan_cost(materialize's real route key {real_key:?}), not a stale or \
                 differently-derived key"
            );
        }
    }

    /// WHI-1409 Opus-escalation fix (closing DI-44's "cache or bound the
    /// hot-path cost" half): the per-sample route-key simulation is memoized on
    /// `amount_in`, so the quote-closure + `fee_cost` pair `consider_point`
    /// issues for one candidate costs **one** `simulate_mixed_path_with_route_key`
    /// call rather than two, and an input the search revisits costs none.
    ///
    /// Asserted through the model's own cache-miss counter rather than a
    /// wrapper, because the counter is exactly the surface that turns "bounded
    /// hot-path cost" from a claim into a measurement (WHI-1409 AC-5).
    #[test]
    fn route_aware_fee_cost_memoizes_one_simulation_per_distinct_amount_in() {
        use crate::arbitrage::optimizer::FeeCostModel;
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;

        let (_wmnt, pools) = v3_v3_crossing_fixture_pools();
        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).expect("pools for path");

        let profile = gas_profile_with_v3v3_low_tick_approved();
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let measured = MeasuredFeeScoring::new(profile, 0, 1, fee_ctx);
        let fee_model = RouteAwareFeeCost::new(&measured, None, &path, &path_pools, 0);

        let a = U256::from(1_000u128 * V3_V3_FIXTURE_SCALE); // zero crossings
        let b = U256::from(20_000u128 * V3_V3_FIXTURE_SCALE); // pool_b crosses 2 ticks

        assert_eq!(fee_model.simulations_performed(), 0, "nothing simulated yet");

        // Leg 1 of what `consider_point` does for candidate `a`: the quote.
        let (_, key_a, _) = fee_model.simulate(a).expect("simulate a");
        assert_eq!(fee_model.simulations_performed(), 1);

        // Leg 2 for the *same* candidate: pricing. Pre-cache this was a second
        // full simulation; it must now be a memo hit.
        let cost_a = fee_model.fee_cost(a);
        assert_eq!(
            fee_model.simulations_performed(),
            1,
            "fee_cost(amount_in) must reuse the quote's simulation of that same \
             input, not issue a second one"
        );
        assert_eq!(
            cost_a,
            measured.fee_plan_cost(&key_a).expect("approved bucket"),
            "the memoized key must still price identically to the freshly \
             simulated one"
        );

        // A genuinely new input does pay for one simulation...
        let _ = fee_model.fee_cost(b);
        assert_eq!(fee_model.simulations_performed(), 2);

        // ...and revisiting `a` later in the search (coarse samples recur as
        // ternary interval endpoints, and `max_input`/domain floor always do)
        // is free, in either call order — memoization has no "must be primed
        // first" ordering requirement.
        let _ = fee_model.fee_cost(a);
        let (_, key_a_again, _) = fee_model.simulate(a).expect("simulate a again");
        assert_eq!(
            fee_model.simulations_performed(),
            2,
            "a revisited amount_in must be served entirely from the memo"
        );
        assert_eq!(key_a_again, key_a, "memo must return the same route key");

        // Exactly one simulation per distinct input touched.
        assert_eq!(fee_model.simulations_performed(), 2);
    }

    /// WHI-1409 round-1 fix: the measured-fee quote closure must soft-skip a
    /// candidate whose AMM state is transiently incomplete (matching the
    /// pre-existing `AMMError::is_incomplete_state` convention the offline path
    /// already relies on via `simulate_path`), not hard-abort the whole search.
    #[test]
    fn measured_fee_quote_soft_skips_incomplete_moe_state_instead_of_erroring() {
        use crate::amms::moe::MoeLbPair;
        use crate::amms::uniswap_v2::UniswapV2Pool;
        use crate::amms::Token;
        use crate::arbitrage::pathfinder::{ArbitragePath, PathHop};
        use crate::execution::gas_runtime::mainnet_verified_identity;
        use crate::execution::{RuntimeGasProfile, RuntimeProfileConfig};
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::{address, B256};
        use std::path::PathBuf;

        let wmnt = fixture_settlement_asset();
        let token = address!("00000000000000000000000000000000000000ce");
        let token2 = address!("00000000000000000000000000000000000000cf");

        // v2/moe/moe (bins=0) is Approved in the real pinned artifact, so the
        // WHI-1409 pre-check (`topology_never_approved_reason`) lets this
        // topology through to the real per-sample search — unlike the
        // single-hop-Moe case, which has no hop=1 profile entry at any bucket
        // and would be rejected pre-simulation before ever reaching the quote
        // closure this test means to exercise. (WHI-1520: the 2-hop v2/moe
        // bins=0 class is withheld on the 124-pool universe, PR109-F1.)
        let mut v2_pool =
            UniswapV2Pool::new(address!("00000000000000000000000000000000000000a7"), 300);
        v2_pool.token_a = Token::new_with_decimals(wmnt, 18);
        v2_pool.token_b = Token::new_with_decimals(token, 18);
        v2_pool.reserve_0 = 1_000_000_000_000_000_000_000;
        v2_pool.reserve_1 = 1_000_000_000_000_000_000_000;

        let mut moe_pair = MoeLbPair::new(address!("00000000000000000000000000000000000000a6"));
        moe_pair.token_x = Token::new_with_decimals(token, 18);
        moe_pair.token_y = Token::new_with_decimals(token2, 18);
        moe_pair.bin_step = 20;
        moe_pair.active_id = 8_388_608;
        // No snapshot installed — simulate_swap returns MoeError::IncompleteState.
        assert!(moe_pair.snapshot.is_none());

        let mut moe_pair_2 = MoeLbPair::new(address!("00000000000000000000000000000000000000a5"));
        moe_pair_2.token_x = Token::new_with_decimals(token2, 18);
        moe_pair_2.token_y = Token::new_with_decimals(wmnt, 18);
        moe_pair_2.bin_step = 20;
        moe_pair_2.active_id = 8_388_608;

        let path = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: v2_pool.address,
                    token_in: wmnt,
                    token_out: token,
                    fee_bps: 300,
                },
                PathHop {
                    pool_address: moe_pair.address,
                    token_in: token,
                    token_out: token2,
                    fee_bps: 20,
                },
                PathHop {
                    pool_address: moe_pair_2.address,
                    token_in: token2,
                    token_out: wmnt,
                    fee_bps: 20,
                },
            ],
        };
        let path_pools = vec![
            AMM::UniswapV2Pool(v2_pool),
            AMM::MoeLbPair(moe_pair),
            AMM::MoeLbPair(moe_pair_2),
        ];

        let profile = std::sync::Arc::new(
            RuntimeGasProfile::from_artifact_with_identity(
                crate::execution::gas_profile::load_artifact(
                    &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("config/gas_profiles/mantle_mainnet_v1.json"),
                )
                .expect("artifact"),
                RuntimeProfileConfig::mantle_mainnet(Vec::new()),
                mainnet_verified_identity(),
            )
            .expect("profile"),
        );
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.measured_fee = Some(MeasuredFeeScoring::new(profile, 0, 1, fee_ctx));

        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: U256::from(1_000_000_000_000_000_000u128),
            ..OptimizationConfig::default()
        });
        let outcome = optimize_path(&optimizer, &path, &path_pools, &config);
        assert!(
            matches!(outcome, OptimizeOutcome::NoOptimum { .. }),
            "incomplete Moe state must soft-skip to NoOptimum, not hard-error: {outcome:?}"
        );
    }

    // -- WHI-1421: one profile-support predicate for both call sites --------

    fn mainnet_gas_profile() -> std::sync::Arc<RuntimeGasProfile> {
        std::sync::Arc::new(
            RuntimeGasProfile::from_artifact_with_identity(
                crate::execution::gas_profile::load_artifact(
                    &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                        .join("config/gas_profiles/mantle_mainnet_v1.json"),
                )
                .expect("artifact"),
                RuntimeProfileConfig::mantle_mainnet(Vec::new()),
                crate::execution::gas_runtime::mainnet_verified_identity(),
            )
            .expect("profile"),
        )
    }

    fn v3v3_low_tick_key() -> RouteKey {
        RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3])
            .unwrap()
            .with_v3_ticks(TickCrossingBucket::Low)
    }

    /// [`gas_profile_with_v3v3_low_tick_approved`] with its only `[v3,v3]`
    /// approval (ticks=1-5) invalidated in memory.
    fn gas_profile_with_v3v3_low_tick_invalidated() -> std::sync::Arc<RuntimeGasProfile> {
        let profile = gas_profile_with_v3v3_low_tick_approved();
        profile
            .invalidate(&v3v3_low_tick_key())
            .expect("in-memory invalidation");
        profile
    }

    /// Verdicts of every WHI-1408 startup-gate entry point (synced pools, which is
    /// also the first-live-block re-check in `block_loop`, and loaded universe) and
    /// of the WHI-1409 pre-simulation filter at its real call site
    /// ([`optimize_path`]) over the same [`v3_v3_crossing_fixture_pools`] and
    /// `profile`. Returns `(pools_gate_ok, universe_gate_ok, prefilter_reject)`.
    fn v3v3_gate_verdicts(
        profile: std::sync::Arc<RuntimeGasProfile>,
    ) -> (bool, bool, Option<&'static str>) {
        use crate::service::fee_scoring::{
            assert_pools_gas_profile_compatibility, assert_universe_gas_profile_compatibility,
            MeasuredFeeScoring,
        };
        use crate::service::pool_universe::LoadedPoolUniverse;
        use crate::state_space::{PoolProtocol, PoolUniverseRow};
        use alloy::primitives::B256;

        let (wmnt, pools) = v3_v3_crossing_fixture_pools();
        let pools_gate = assert_pools_gas_profile_compatibility(B256::ZERO, &pools, &profile, 3);
        let universe = LoadedPoolUniverse {
            rows: pools
                .iter()
                .map(|p| PoolUniverseRow {
                    protocol: PoolProtocol::Agni,
                    factory: Address::ZERO,
                    pool: p.address(),
                    token0: wmnt,
                    token1: v3_v3_fixture_token(),
                })
                .collect(),
            fingerprint: B256::ZERO,
            addresses: pools.iter().map(|p| p.address()).collect(),
            snapshot_block: None,
        };
        let universe_gate = assert_universe_gas_profile_compatibility(&universe, &profile, 3);

        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).expect("pools for path");
        let mut config = DiscoveryConfig::offline_default(wmnt);
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        config.measured_fee = Some(MeasuredFeeScoring::new(profile, 0, 1, fee_ctx));
        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: U256::from(50_000u128 * V3_V3_FIXTURE_SCALE),
            ..OptimizationConfig::default()
        });
        let prefilter_reject = match optimize_path(&optimizer, &path, &path_pools, &config) {
            OptimizeOutcome::Rejected { reason } => Some(reason),
            _ => None,
        };
        (pools_gate.is_ok(), universe_gate.is_ok(), prefilter_reject)
    }

    /// WHI-1421 AC-1 (the issue's probe, inverted): the profile approves
    /// `[v3,v3]` only at ticks=1-5, never at zero. Before the fix the startup gate
    /// asked only about the zero bucket and refused to start while the
    /// pre-simulation filter let the same topology through.
    #[test]
    fn nonzero_bucket_only_approval_passes_startup_gate_and_prefilter() {
        let profile = gas_profile_with_v3v3_low_tick_approved();
        let zero = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
        assert!(
            profile.quote(&zero).is_err(),
            "fixture premise: [v3,v3] ticks=0 must not be approved"
        );
        assert_eq!(v3v3_gate_verdicts(profile), (true, true, None));
    }

    /// WHI-1421 AC-2: no approval for the topology at any bucket fails both.
    #[test]
    fn topology_without_any_approval_fails_startup_gate_and_prefilter() {
        assert_eq!(
            v3v3_gate_verdicts(mainnet_gas_profile()),
            (
                false,
                false,
                Some(crate::metrics::reject_reason::UNAPPROVED_ROUTE)
            )
        );
    }

    /// WHI-1421 AC-3: the topology's only approval has been invalidated.
    #[test]
    fn invalidated_only_approval_fails_startup_gate_and_prefilter() {
        assert_eq!(
            v3v3_gate_verdicts(gas_profile_with_v3v3_low_tick_invalidated()),
            (
                false,
                false,
                Some(crate::metrics::reject_reason::UNAPPROVED_ROUTE)
            )
        );
    }

    /// WHI-1421 AC-5 drift guard: independently enumerate every 2..=4-hop
    /// topology over {v2, v3, moe} and assert that the startup gate's census
    /// (what decides pass/fail at startup and on the first live block) and the
    /// pre-simulation filter classify each one identically, over several
    /// fixture profiles, including a nonzero-bucket-only approval and
    /// invalidations.
    #[test]
    fn startup_gate_and_prefilter_agree_on_every_topology() {
        use crate::metrics::reject_reason::{UNAPPROVED_ROUTE, UNKNOWN_ROUTE};
        use crate::service::fee_scoring::{
            evaluate_universe_gas_profile_compatibility, topology_label, MeasuredFeeScoring,
        };
        use alloy::primitives::B256;

        const MAX_HOPS: usize = 4;
        let kinds = [ProtocolKind::V2, ProtocolKind::V3, ProtocolKind::Moe];
        let mut topologies = Vec::new();
        for len in 2..=MAX_HOPS {
            for mut n in 0..kinds.len().pow(len as u32) {
                let mut topo = Vec::with_capacity(len);
                for _ in 0..len {
                    topo.push(kinds[n % kinds.len()]);
                    n /= kinds.len();
                }
                topologies.push(topo);
            }
        }
        assert_eq!(topologies.len(), 9 + 27 + 81);
        // Enough pools of every protocol for the gate to generate all of them.
        let counts: HashMap<ProtocolKind, usize> = kinds.iter().map(|&k| (k, MAX_HOPS)).collect();

        let fixtures = [
            ("pinned mainnet", mainnet_gas_profile()),
            (
                "v3v3 nonzero-bucket-only approval",
                gas_profile_with_v3v3_low_tick_approved(),
            ),
            (
                "v3v3 only approval invalidated",
                gas_profile_with_v3v3_low_tick_invalidated(),
            ),
            (
                "fixture routes invalidated",
                gas_profile_with_fixture_routes_invalidated(),
            ),
        ];
        for (name, profile) in fixtures {
            let census =
                evaluate_universe_gas_profile_compatibility(B256::ZERO, &counts, &profile, MAX_HOPS)
                    .expect("census");
            assert_eq!(census.topologies_total, topologies.len(), "{name}");
            let fee_ctx = BlockFeeContext {
                block_number: 1,
                block_hash: B256::ZERO,
                base_fee_per_gas: 1,
                block_gas_limit: 30_000_000,
            };
            let measured = MeasuredFeeScoring::new(profile, 0, 1, fee_ctx);
            let mut supported = 0;
            for topo in &topologies {
                let label = topology_label(topo);
                let startup = if census.topologies_supported.contains(&label) {
                    supported += 1;
                    None
                } else if census.topologies_unapproved.contains(&label) {
                    Some(UNAPPROVED_ROUTE)
                } else if census.topologies_unknown.contains(&label) {
                    Some(UNKNOWN_ROUTE)
                } else {
                    panic!("{name}: startup gate never classified {label}");
                };
                let prefilter = topology_never_approved_reason(&measured, None, topo).expect("prefilter");
                assert_eq!(startup, prefilter, "{name}: call sites disagree on {label}");
            }
            assert_eq!(supported, census.topologies_supported.len(), "{name}");
        }
    }

    const V3_V3_FIXTURE_SCALE: u128 = 1_000_000_000_000_000;

    /// Shared WHI-1409 fixture addresses — defined once so
    /// [`v3_v3_crossing_fixture_pools`] and [`v3_v3_crossing_fixture_path`]
    /// cannot silently drift apart on which pool/token each literal means.
    fn v3_v3_fixture_token() -> Address {
        alloy::primitives::address!("00000000000000000000000000000000000000cd")
    }
    fn v3_v3_fixture_pool_a_address() -> Address {
        alloy::primitives::address!("00000000000000000000000000000000000000a4")
    }
    fn v3_v3_fixture_pool_b_address() -> Address {
        alloy::primitives::address!("00000000000000000000000000000000000000a5")
    }

    /// Shared WHI-1409 fixture: two Agni V3 pools on the same synthetic
    /// WMNT/TOKEN pair, priced so a WMNT→TOKEN→WMNT round trip is profitable,
    /// with `pool_b` primed with two initialized ticks so a moderate trade
    /// crosses them (`TickCrossingBucket::Low`) while `pool_a` never crosses.
    /// Liquidity/amounts are scaled by [`V3_V3_FIXTURE_SCALE`] off a
    /// hand-verified small-integer base (see the crossing/profit derivation in
    /// the WHI-1409 PR) so gross profit clears the ~231_740 wei measured-fee
    /// cost — tick movement only depends on the *ratio* of amount_in to
    /// liquidity, so scaling both preserves the crossing behavior.
    fn v3_v3_crossing_fixture_pools() -> (Address, Vec<AMM>) {
        use crate::amms::agni::{AgniPool, Info};
        use crate::amms::Token;

        let wmnt = fixture_settlement_asset();
        let token = v3_v3_fixture_token();

        let mut pool_a = AgniPool {
            address: v3_v3_fixture_pool_a_address(),
            token_a: Token::new_with_decimals(wmnt, 18),
            token_b: Token::new_with_decimals(token, 18),
            liquidity: 1_000_000 * V3_V3_FIXTURE_SCALE,
            sqrt_price: uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(0).unwrap(),
            fee: 3_000,
            tick: 0,
            tick_spacing: 1,
            ..Default::default()
        };
        pool_a.tick_bitmap_coverage.extend(-1200i16..=200i16);

        let mut pool_b = AgniPool {
            address: v3_v3_fixture_pool_b_address(),
            token_a: Token::new_with_decimals(wmnt, 18),
            token_b: Token::new_with_decimals(token, 18),
            liquidity: 1_000_000 * V3_V3_FIXTURE_SCALE,
            sqrt_price: uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(-1000).unwrap(),
            fee: 3_000,
            tick: -1000,
            tick_spacing: 1,
            ..Default::default()
        };
        pool_b.tick_bitmap_coverage.extend(-1200i16..=200i16);
        for t in [-998i32, -996i32] {
            pool_b.flip_tick(t);
            pool_b.ticks.insert(t, Info::new(1, 0, true));
        }

        (wmnt, vec![AMM::AgniPool(pool_a), AMM::AgniPool(pool_b)])
    }

    /// The single 2-hop WMNT→TOKEN→WMNT cycle [`v3_v3_crossing_fixture_pools`]
    /// forms, in the profitable direction (pool_a buy leg, pool_b sell leg).
    fn v3_v3_crossing_fixture_path() -> ArbitragePath {
        use crate::arbitrage::pathfinder::PathHop;

        let wmnt = fixture_settlement_asset();
        let token = v3_v3_fixture_token();
        ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: v3_v3_fixture_pool_a_address(),
                    token_in: wmnt,
                    token_out: token,
                    fee_bps: 3_000,
                },
                PathHop {
                    pool_address: v3_v3_fixture_pool_b_address(),
                    token_in: token,
                    token_out: wmnt,
                    fee_bps: 3_000,
                },
            ],
        }
    }

    // -- WHI-1424: evaluation-coverage counters --------------------------------

    /// WHI-1424 AC: a topology that passes the pre-simulation filter (some
    /// `['v3','v3']` bucket is approved) and simulates profitably, but whose
    /// *real* per-sample bucket is never approved, must be counted as
    /// fee-resolution failures — not reported only as `no_optimum`. The
    /// failures are sample-level and stay out of the path-count conservation.
    #[test]
    fn simulated_but_never_fee_resolved_topology_counts_fee_resolution_failures() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;

        let (wmnt, pools) = v3_v3_crossing_fixture_pools();
        // The fixture only ever crosses 0 or 2 ticks; approve 6-20 alone.
        let profile = gas_profile_with_v3v3_tick_approved(TickCrossingBucket::Mid);
        for bucket in [TickCrossingBucket::Zero, TickCrossingBucket::Low] {
            let key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3])
                .unwrap()
                .with_v3_ticks(bucket);
            assert!(
                profile.quote(&key).is_err(),
                "fixture premise: {bucket:?} must not be approved"
            );
        }
        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.max_input = U256::from(50_000u128 * V3_V3_FIXTURE_SCALE);
        let fee_ctx = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1,
            block_gas_limit: 30_000_000,
        };
        config.measured_fee = Some(MeasuredFeeScoring::new(profile, 0, 1, fee_ctx));

        // Per path: the profitable direction reaches the search, every
        // profitable sample fails fee resolution, and the outcome is NoOptimum.
        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).expect("pools for path");
        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: config.max_input,
            ..OptimizationConfig::default()
        });
        let work = match optimize_path(&optimizer, &path, &path_pools, &config) {
            OptimizeOutcome::NoOptimum { work } => work,
            other => panic!("expected NoOptimum, got {other:?}"),
        };
        assert!(
            work.fee_resolution_failures > 0,
            "profitable samples that cannot be priced must be counted: {work:?}"
        );

        let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).expect("build engine");
        let (found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");
        assert!(found.is_empty());
        assert_eq!(
            stats.rejects.unknown_route + stats.rejects.unapproved_route,
            0,
            "premise: the topology passes the pre-simulation filter"
        );
        assert!(
            stats.paths_quoted > 0,
            "premise: the path reaches the optimizer"
        );
        assert!(
            stats.fee_resolution_failures >= work.fee_resolution_failures,
            "discovery must surface the sample-level fee failures, got {stats:?}"
        );
        assert_eq!(
            stats.rejects.total(),
            stats.cycles_optimized as u64,
            "path-count conservation must not include sample-level failures"
        );
        // WHI-1572: a search whose every fee request failed is `unresolved`, and
        // the four states still partition the completed searches.
        let tiers = stats.search_tiers.expect("measured scoring records the partition");
        assert_eq!(tiers.total(), stats.paths_quoted);
        assert!(tiers.unresolved >= 1, "{tiers:?}");
        assert_eq!(tiers.estimated_used + tiers.measured_only, 0, "{tiers:?}");
    }

    /// WHI-1424 AC: an `OptimizeOutcome::Error` path contributes the quotes it
    /// spent to `amm_quotes` (pre-fix the Error branch dropped them).
    #[test]
    fn optimize_error_path_contributes_its_quotes_to_amm_quotes() {
        use crate::amms::agni::Info;

        let (wmnt, mut pools) = v3_v3_crossing_fixture_pools();
        // Crossing tick -996 on the sell leg would drive liquidity below zero,
        // so large samples hard-fail (LiquidityUnderflow, not incomplete
        // state) while small samples quote normally.
        let AMM::AgniPool(pool_b) = &mut pools[1] else {
            unreachable!("fixture pool_b is Agni")
        };
        pool_b.ticks.insert(
            -996,
            Info::new(1, -((2_000_000 * V3_V3_FIXTURE_SCALE) as i128), true),
        );
        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.max_input = U256::from(50_000u128 * V3_V3_FIXTURE_SCALE);
        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: config.max_input,
            ..OptimizationConfig::default()
        });

        let error_path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&error_path, &pools).expect("pools for path");
        let error_quotes = match optimize_path(&optimizer, &error_path, &path_pools, &config) {
            OptimizeOutcome::Error { work, .. } => work.quotes,
            other => panic!("expected Error, got {other:?}"),
        };
        assert!(error_quotes > 0, "the search quoted before failing");

        let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).expect("build engine");
        // Every other cycle in the fixture finds no optimum, so its quotes are
        // exactly its search's quotes (no mixed-sim +1).
        let other_quotes: u64 = eng
            .index()
            .paths
            .iter()
            .filter(|p| topology_signature(p) != topology_signature(&error_path))
            .map(|p| {
                let pp = pools_for_path(p, &pools).expect("pools for path");
                match optimize_path(&optimizer, p, &pp, &config) {
                    OptimizeOutcome::NoOptimum { work } => work.quotes,
                    other => panic!("expected NoOptimum on the other cycles, got {other:?}"),
                }
            })
            .sum();

        let (_, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Full)
            .expect("discover");
        assert_eq!(
            stats.rejects.other, 1,
            "the error path counts as optimize_error"
        );
        assert_eq!(
            stats.amm_quotes,
            other_quotes + error_quotes,
            "amm_quotes must include the Error path's quotes"
        );
        // WHI-1572: the Error search stays outside the completed-search partition.
        if let Some(tiers) = stats.search_tiers {
            assert_eq!(tiers.total(), stats.paths_quoted);
        }
    }

    // -- WHI-1572: discovery-only gas estimation ----------------------------------

    fn fixture_fee_ctx(base_fee: u128, block_gas_limit: u64) -> BlockFeeContext {
        BlockFeeContext {
            block_number: 1,
            block_hash: alloy::primitives::B256::ZERO,
            base_fee_per_gas: base_fee,
            block_gas_limit,
        }
    }

    fn mainnet_estimator(
        profile: &RuntimeGasProfile,
    ) -> std::sync::Arc<crate::service::gas_estimate::DiscoveryGasEstimator> {
        std::sync::Arc::new(
            crate::service::gas_estimate::DiscoveryGasEstimator::load_mainnet(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
                profile,
            )
            .expect("mainnet estimator"),
        )
    }

    fn estimated_config(
        wmnt: Address,
        profile: std::sync::Arc<RuntimeGasProfile>,
        estimator: std::sync::Arc<crate::service::gas_estimate::DiscoveryGasEstimator>,
        fee_ctx: BlockFeeContext,
    ) -> DiscoveryConfig {
        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.max_input = U256::from(50_000u128 * V3_V3_FIXTURE_SCALE);
        config.measured_fee = Some(crate::service::fee_scoring::MeasuredFeeScoring::new(
            profile, 0, 1, fee_ctx,
        ));
        config.gas_estimator = Some(estimator);
        config
    }

    /// AC4 / AC6: an estimation-only topology (every `[v3,v3]` bucket is
    /// Unsupported in the pinned profile) passes the pre-simulation filter only
    /// with the estimator, and each input is priced from its own single
    /// simulation's raw per-hop counts with unchanged bucket semantics.
    #[test]
    fn estimator_prices_an_estimation_only_topology_from_the_same_simulation() {
        use crate::arbitrage::optimizer::FeeCostModel;
        use crate::metrics::reject_reason;
        use crate::service::discovery::{
            simulate_mixed_path_with_features, simulate_mixed_path_with_route_key,
        };
        use crate::service::gas_estimate::{price_discovery_gas, GasFeatures, GasTier};

        let (_wmnt, pools) = v3_v3_crossing_fixture_pools();
        let profile = mainnet_gas_profile();
        let estimator = mainnet_estimator(&profile);
        let fee_ctx = fixture_fee_ctx(1, 30_000_000);
        let measured =
            crate::service::fee_scoring::MeasuredFeeScoring::new(profile.clone(), 0, 1, fee_ctx.clone());
        let v3v3 = [ProtocolKind::V3, ProtocolKind::V3];
        assert_eq!(
            topology_never_approved_reason(&measured, None, &v3v3).unwrap(),
            Some(reject_reason::UNAPPROVED_ROUTE),
            "measured-only mode keeps rejecting the unmeasured topology"
        );
        assert_eq!(
            topology_never_approved_reason(&measured, Some(&estimator), &v3v3).unwrap(),
            None
        );

        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).expect("pools");
        let fee_model = RouteAwareFeeCost::new(&measured, Some(&estimator), &path, &path_pools, 0);
        let small = U256::from(V3_V3_FIXTURE_SCALE);
        let large = U256::from(20_000u128 * V3_V3_FIXTURE_SCALE);
        for (amount_in, expected_crossings) in [(small, vec![0, 0]), (large, vec![0, 2])] {
            let (_, _, key, crossings) =
                simulate_mixed_path_with_features(&path, &path_pools, amount_in, 0).unwrap();
            assert_eq!(crossings, expected_crossings);
            let (_, _, legacy_key) =
                simulate_mixed_path_with_route_key(&path, &path_pools, amount_in, 0).unwrap();
            assert_eq!(key, legacy_key, "route-key semantics unchanged");
            let priced = price_discovery_gas(
                &profile,
                &estimator,
                &key,
                &GasFeatures::new(key.protocols.clone(), crossings),
                &fee_ctx,
                measured.policy(),
            )
            .unwrap();
            assert_eq!(priced.tier, GasTier::Estimated);
            let cost = fee_model.fee_cost(amount_in);
            assert_eq!(cost, priced.cost, "finite per-input pricing");
            assert!(cost < U256::MAX);
            // The paired quote for the same input is served from the memo.
            fee_model.simulate(amount_in).unwrap();
        }
        assert_eq!(fee_model.simulations_performed(), 2, "one simulation per input");
        assert_eq!(fee_model.estimated_resolutions.get(), 2);
        assert_eq!(fee_model.fee_resolution_failures.get(), 0);
    }

    /// AC4 / AC6 / AC9: a full estimator-enabled pass on the fixture has zero
    /// absent-evidence rejects, materializes the estimated candidate at exactly
    /// the optimize-side price, labels V2-free extrapolation and keeps the
    /// completed-search partition equal to `paths_quoted`.
    #[test]
    fn estimator_enabled_discovery_materializes_the_optimize_price_and_partitions_searches() {
        use crate::service::discovery::simulate_mixed_path_with_features;
        use crate::service::gas_estimate::{price_discovery_gas, GasFeatures, GasTier};

        let (wmnt, pools) = v3_v3_crossing_fixture_pools();
        let profile = mainnet_gas_profile();
        let estimator = mainnet_estimator(&profile);
        let fee_ctx = fixture_fee_ctx(1, 30_000_000);
        let config = estimated_config(wmnt, profile.clone(), estimator.clone(), fee_ctx.clone());
        let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).expect("engine");
        let (found, stats) = eng.discover(&pools, &config, &TipRefreshScope::Full).unwrap();

        assert_eq!(stats.rejects.unknown_route, 0);
        assert_eq!(stats.rejects.unapproved_route, 0);
        let tiers = stats.search_tiers.expect("partition recorded");
        assert_eq!(tiers.total(), stats.paths_quoted);
        assert!(tiers.estimated_used >= 1, "{tiers:?}");
        assert!(stats.estimated_resolutions > 0);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(stats.candidates_estimated, 1);

        let opp = &found[0];
        let gas = opp.gas.as_ref().expect("gas evidence");
        assert_eq!(gas.tier, GasTier::Estimated);
        assert!(opp.is_gas_estimated());
        assert!(gas.extrapolated.v2_free, "V2-free estimate is labeled");
        assert!(gas.extrapolated.venue_family, "unattributed venue is unseen");
        assert!(gas
            .venues
            .iter()
            .all(|v| v.venue == "unattributed"
                && v.qualification == crate::service::gas_estimate::VenueQualification::Unverified));
        // Optimize-side price at the chosen input == materialized price.
        let (_, _, key, crossings) =
            simulate_mixed_path_with_features(&opp.candidate.path, &opp.candidate.pools, opp.candidate.input, config.block_timestamp)
                .unwrap();
        assert_eq!(crossings, gas.crossings);
        let priced = price_discovery_gas(
            &profile,
            &estimator,
            &key,
            &GasFeatures::new(key.protocols.clone(), crossings),
            &fee_ctx,
            config.measured_fee.as_ref().unwrap().policy(),
        )
        .unwrap();
        assert_eq!(priced.expected_gas_used, gas.expected_gas_used);
        assert_eq!(priced.gas_limit, gas.gas_limit);
        let gross = opp.candidate.output - opp.candidate.input;
        assert_eq!(opp.candidate.net_profit, gross - priced.cost);
    }

    /// AC5: a cached Measured class invalidated later is rejected at the next
    /// materialization — never downgraded to Estimated, with no re-simulation.
    /// Same for poisoned state and a changed available block gas.
    #[test]
    fn cached_resolution_is_revalidated_without_resimulation() {
        use crate::metrics::reject_reason;
        use crate::service::gas_estimate::{DiscoveryGasEstimator, GasTier};

        let low = v3v3_low_tick_key();
        let run = |mutate: &dyn Fn(&RuntimeGasProfile, &mut DiscoveryConfig), expected: &str| {
            let (wmnt, pools) = v3_v3_crossing_fixture_pools();
            let profile = gas_profile_with_v3v3_low_tick_approved();
            let estimator =
                std::sync::Arc::new(DiscoveryGasEstimator::for_profile_with(&profile, |_| {}).unwrap());
            let mut config =
                estimated_config(wmnt, profile.clone(), estimator, fixture_fee_ctx(1, 30_000_000));
            let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).expect("engine");
            let (found, _) = eng.discover(&pools, &config, &TipRefreshScope::Full).unwrap();
            assert_eq!(found.len(), 1);
            let gas = found[0].gas.as_ref().unwrap();
            assert_eq!(gas.tier, GasTier::Measured, "optimum sits in the approved Low bucket");
            assert_eq!(found[0].route_key, low);

            mutate(&profile, &mut config);
            let (found, stats) = eng
                .discover(&pools, &config, &TipRefreshScope::Touched(HashSet::new()))
                .unwrap();
            assert!(found.is_empty(), "{expected}: {found:?}");
            assert_eq!(stats.cycles_optimized, 0);
            assert_eq!(stats.simulations, 0, "no AMM re-simulation");
            assert_eq!(stats.amm_quotes, 0);
            let (idx, cached) = eng
                .cache
                .iter()
                .enumerate()
                .find_map(|(i, c)| c.as_ref().map(|c| (i, c)))
                .expect("gross quote stays cached as evidence");
            let path = &eng.index.paths[idx];
            let path_pools = pools_for_path(path, &pools).unwrap();
            let err = materialize_from_cache(path, &path_pools, cached, &config).unwrap_err();
            assert_eq!(err, expected);
            stats
        };
        run(&|profile, _| profile.invalidate(&low).unwrap(), reject_reason::ROUTE_INVALIDATED);
        run(&|profile, _| profile.poison_invalidation_lock_for_test(), reject_reason::GAS_SCREEN);
        let stats = run(
            &|_, config| {
                let m = config.measured_fee.as_mut().unwrap();
                m.fee_context.block_gas_limit = 300_000;
            },
            reject_reason::GAS_RESERVE,
        );
        assert_eq!(stats.gas_rescores, 1, "fee-factor rescore re-checks bounds");
    }

    /// AC9: the four states over real fee-model work — no fee requested, both
    /// tiers used (estimated wins), measured-only with partial failures, and
    /// unresolved.
    #[test]
    fn search_partition_classifies_no_fee_both_tiers_partial_and_unresolved() {
        use crate::arbitrage::optimizer::FeeCostModel;
        use crate::service::gas_estimate::DiscoveryGasEstimator;

        assert_eq!(SearchTier::classify(&OptimizeWork::default()), SearchTier::NoFeeRequested);

        let (_wmnt, pools) = v3_v3_crossing_fixture_pools();
        let path = v3_v3_crossing_fixture_path();
        let path_pools = pools_for_path(&path, &pools).unwrap();
        let small = U256::from(V3_V3_FIXTURE_SCALE); // ticks=0: Unsupported
        let large = U256::from(20_000u128 * V3_V3_FIXTURE_SCALE); // ticks=1-5: approved
        let profile = gas_profile_with_v3v3_low_tick_approved();
        let measured = crate::service::fee_scoring::MeasuredFeeScoring::new(
            profile.clone(),
            0,
            1,
            fixture_fee_ctx(1, 30_000_000),
        );

        let estimator = DiscoveryGasEstimator::for_profile_with(&profile, |_| {}).unwrap();
        let both = RouteAwareFeeCost::new(&measured, Some(&estimator), &path, &path_pools, 0);
        both.fee_cost(small);
        both.fee_cost(large);
        let work = both.work();
        assert_eq!((work.measured_resolutions, work.estimated_resolutions), (1, 1));
        assert_eq!(SearchTier::classify(&work), SearchTier::EstimatedUsed);

        let measured_only = RouteAwareFeeCost::new(&measured, None, &path, &path_pools, 0);
        assert_eq!(measured_only.fee_cost(small), U256::MAX);
        measured_only.fee_cost(large);
        let work = measured_only.work();
        assert_eq!(work.fee_resolution_failures, 1, "partial failure stays a sample count");
        assert_eq!(SearchTier::classify(&work), SearchTier::MeasuredOnly);

        let unresolved = RouteAwareFeeCost::new(&measured, None, &path, &path_pools, 0);
        unresolved.fee_cost(small);
        assert_eq!(SearchTier::classify(&unresolved.work()), SearchTier::Unresolved);

        // Pre-search rejection stays outside the denominator.
        let (wmnt, pools) = v3_v3_crossing_fixture_pools();
        let mut config = DiscoveryConfig::offline_default(wmnt);
        config.measured_fee = Some(crate::service::fee_scoring::MeasuredFeeScoring::new(
            mainnet_gas_profile(),
            0,
            1,
            fixture_fee_ctx(1, 30_000_000),
        ));
        let mut eng = DiscoveryEngine::build(&pools, wmnt, 3).unwrap();
        let (_, stats) = eng.discover(&pools, &config, &TipRefreshScope::Full).unwrap();
        assert!(stats.rejects.unapproved_route > 0);
        assert_eq!(stats.paths_quoted, 0);
        assert_eq!(stats.search_tiers, Some(SearchTierCounts::default()));
    }

    /// AC6: every structurally supported topology family of the committed
    /// universe has an explicit discovery path with the estimator (V2-free ones
    /// labeled), the estimation-only `[v3,v3]` pool set passes the WHI-1408
    /// synced-pools / first-live-block gate, and measured-only mode is unchanged.
    #[test]
    fn committed_universe_topologies_all_have_a_discovery_path_with_the_estimator() {
        use crate::service::fee_scoring::{
            assert_pools_discovery_policy_compatibility, evaluate_universe_discovery_policy,
        };
        use crate::service::gas_estimate::GasFeatures;

        let profile = mainnet_gas_profile();
        let estimator = mainnet_estimator(&profile);
        let csv = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/pool_universe.csv"),
        )
        .unwrap();
        let mut counts: HashMap<ProtocolKind, usize> = HashMap::new();
        for line in csv.lines().skip(1) {
            let kind = match line.split(',').next().unwrap() {
                "agni-v2" => ProtocolKind::V2,
                "agni-v3" => ProtocolKind::V3,
                "moe" => ProtocolKind::Moe,
                other => panic!("unexpected protocol {other}"),
            };
            *counts.entry(kind).or_default() += 1;
        }
        let measured_only = evaluate_universe_discovery_policy(
            alloy::primitives::B256::ZERO, &counts, &profile, None, 3,
        )
        .unwrap();
        assert!(measured_only.topologies_estimated.is_empty());
        assert!(!measured_only.topologies_unapproved.is_empty());

        let census = evaluate_universe_discovery_policy(
            alloy::primitives::B256::ZERO, &counts, &profile, Some(&estimator), 3,
        )
        .unwrap();
        assert!(census.topologies_unapproved.is_empty(), "{census}");
        assert!(census.topologies_unknown.is_empty(), "{census}");
        assert_eq!(
            census.topologies_supported.len() + census.topologies_estimated.len(),
            census.topologies_total
        );
        for label in &census.topologies_estimated {
            let protocols: Vec<ProtocolKind> = label
                .split(':')
                .nth(1)
                .unwrap()
                .split('+')
                .map(|p| match p {
                    "v2" => ProtocolKind::V2,
                    "v3" => ProtocolKind::V3,
                    _ => ProtocolKind::Moe,
                })
                .collect();
            let n = protocols.len();
            let est = estimator
                .estimate(&GasFeatures::new(protocols.clone(), vec![0; n]))
                .unwrap_or_else(|e| panic!("{label}: {e:?}"));
            assert_eq!(
                est.extrapolated.v2_free,
                !protocols.contains(&ProtocolKind::V2),
                "{label}"
            );
        }

        let (_, pools) = v3_v3_crossing_fixture_pools();
        assert!(assert_pools_discovery_policy_compatibility(
            alloy::primitives::B256::ZERO, &pools, &profile, None, 3
        )
        .is_err());
        assert_pools_discovery_policy_compatibility(
            alloy::primitives::B256::ZERO, &pools, &profile, Some(&estimator), 3,
        )
        .expect("estimation-only topology passes the discovery-policy gate");
    }
}
