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
use crate::arbitrage::gas::net_profit_after_gas_cost;
use crate::arbitrage::graph::build_graph;
use crate::arbitrage::optimizer::{
    pools_for_path, ConstantFeeCost, OptimizationConfig, PathOptimizer, ZeroFeeCost,
};
use crate::arbitrage::pathfinder::{ArbitragePath, PathConstraints, PathFinder};
use crate::execution::{
    BinCrossingBucket, FeeScoreKey, ProtocolKind, RouteKey, TickCrossingBucket,
};
use crate::service::discovery::{
    path_is_cross_protocol, protocol_mix_label, simulate_mixed_path_with_route_key,
    DiscoveryConfig, DiscoveredOpportunity,
};
use crate::service::fee_scoring::discovery_fee_reject_reason;
use crate::service::gas::default_gas_safety_margin;
use crate::service::protocol::TipRefreshScope;
use crate::service::select::protocol_kind_of_amm;
use crate::service::shadow_row::{
    collect_expected_states, format_roi_percent, hops_description, Candidate,
};
use crate::state_space::StateSpace;
use alloy::primitives::{Address, I256, U256};
use eyre::{Context, Result};
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

/// Default threshold of consecutive cycles evaluated without reaching the optimizer before alarming (WHI-1411).
pub const DEFAULT_LIVENESS_UNQUOTED_CYCLES_THRESHOLD: usize = 100;

/// Per-block discovery counters for operator logs (WHI-940 step 5 / WHI-952).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiscoveryStats {
    pub cycles_total: usize,
    pub cycles_optimized: usize,
    pub dirty_pools: usize,
    /// Number of paths that reached the optimizer binary search (Ok or NoOptimum).
    ///
    /// Distinguishes paths evaluated and quoted from paths rejected before
    /// simulation (WHI-1411).
    pub paths_quoted: u64,
    /// Exact `simulate_path` + mixed-sim calls this pass (WHI-952 `amm_quotes`).
    ///
    /// Counts quotes on **every** optimize attempt that reaches the binary
    /// search — including `NoOptimum` (unprofitable). Pre-WHI-976 the counter
    /// only incremented on a found optimum, so quiet markets logged
    /// `amm_quotes=0` while `cycles_optimized > 0`. Fee-reject / pool-lookup
    /// skips never quote and are excluded from the dead-counter invariant.
    pub amm_quotes: u64,
    /// Cached paths re-screened because fee factors changed (WHI-949).
    pub gas_rescores: u64,
    /// `"full"` or `"touched"` — same labels as [`TipRefreshScope::as_metric_label`].
    /// Empty when discovery was skipped (e.g. inventory precondition, WHI-950).
    pub scope: &'static str,
    /// Breakdown of rejected paths by cause (WHI-1411).
    pub rejects: DiscoveryRejectCounts,
    /// True when the liveness invariant detected a sustained zero-quoted pipeline (WHI-1411).
    pub liveness_alarm: bool,
}

/// Gross quote cached after optimize + mixed simulation (pre gas-screen).
#[derive(Debug, Clone)]
struct CachedGross {
    optimal_input: U256,
    amounts_out: Vec<U256>,
    final_out: U256,
    route_key: RouteKey,
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
    /// Cumulative consecutive cycles evaluated where zero paths reached the optimizer (WHI-1411).
    consecutive_unquoted_cycles: usize,
    /// Configurable window threshold for firing the liveness alarm (default: 100).
    liveness_unquoted_cycles_threshold: usize,
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
            consecutive_unquoted_cycles: 0,
            liveness_unquoted_cycles_threshold: DEFAULT_LIVENESS_UNQUOTED_CYCLES_THRESHOLD,
        })
    }

    pub fn with_liveness_threshold(mut self, threshold: usize) -> Self {
        self.liveness_unquoted_cycles_threshold = threshold;
        self
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
                gas_rescores: 0,
                scope: scope.as_metric_label(),
                rejects: DiscoveryRejectCounts::default(),
                liveness_alarm: false,
            };
            self.last_stats = Some(stats);
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
        // materialize (send-identical). Optimize uses a topology route key with
        // zero crossing buckets as a constant fee scorer; G-1 can replace that
        // with per-sample route_key evaluation via the same fee_plan_cost API.
        let optimizer = PathOptimizer::new(OptimizationConfig {
            max_input: config.max_input,
            ..OptimizationConfig::default()
        });

        let discovery_start = Instant::now();
        let cycles_optimized = to_optimize.len();
        let mut amm_quotes = 0u64;
        // Paths that entered optimize_with_fee_quote_count (Ok / NoOptimum).
        // Fee Rejected / pool-lookup failures never quote and are not part of
        // the WHI-976 work invariant.
        let mut paths_quoted = 0u64;
        let mut rejects = DiscoveryRejectCounts::default();

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
            let opt = match optimize_path(&optimizer, path, &path_pools, config) {
                OptimizeOutcome::Ok { result, quotes } => {
                    paths_quoted = paths_quoted.saturating_add(1);
                    amm_quotes = amm_quotes.saturating_add(quotes);
                    result
                }
                OptimizeOutcome::NoOptimum { quotes } => {
                    // WHI-976: pre-fix discarded these quotes (`Ok((None, _))`),
                    // so unprofitable cycles left amm_quotes=0 while still
                    // counting as evaluated. Binary search still ran N quotes.
                    paths_quoted = paths_quoted.saturating_add(1);
                    amm_quotes = amm_quotes.saturating_add(quotes);
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
                OptimizeOutcome::Error(e) => {
                    // Debug not warn: per-path failures can be thousands/block
                    // (WHI-952 RUST_LOG=info bound). Counters still record OPTIMIZE_ERROR.
                    tracing::debug!(
                        target: "bot.discovery",
                        error = %e,
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
            let (amounts_out, final_out, route_key) = match simulate_mixed_path_with_route_key(
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
        // from "could not price anything" (paths_quoted == 0). Alarm when sustained
        // window of evaluated cycles resolves zero paths to the optimizer (all rejected pre-simulation).
        if let Some(threshold) = config.liveness_threshold {
            self.liveness_unquoted_cycles_threshold = threshold;
        }

        if paths_quoted > 0 {
            self.consecutive_unquoted_cycles = 0;
        } else if cycles_optimized > 0 {
            self.consecutive_unquoted_cycles = self
                .consecutive_unquoted_cycles
                .saturating_add(cycles_optimized);
        }

        let liveness_alarm = (force_full && cycles_optimized > 0 && paths_quoted == 0)
            || (self.consecutive_unquoted_cycles >= self.liveness_unquoted_cycles_threshold);

        if liveness_alarm {
            tracing::error!(
                target: "bot.discovery",
                cycles_optimized,
                paths_quoted,
                amm_quotes,
                consecutive_unquoted_cycles = self.consecutive_unquoted_cycles,
                unknown_route = rejects.unknown_route,
                unapproved_route = rejects.unapproved_route,
                pool_lookup = rejects.pool_lookup,
                no_optimum = rejects.no_optimum,
                zero_profit = rejects.zero_profit,
                other = rejects.other,
                scope = scope_label,
                "WHI-1411 liveness invariant violated: sustained zero paths reached optimizer \
                 (discovery pipeline dead; all cycles rejected pre-simulation)"
            );
        }

        let stats = DiscoveryStats {
            cycles_total: self.index.cycles_total(),
            cycles_optimized,
            dirty_pools,
            paths_quoted,
            amm_quotes,
            gas_rescores,
            scope: scope_label,
            rejects,
            liveness_alarm,
        };
        self.last_stats = Some(stats);

        Ok((found, stats))
    }
}

enum OptimizeOutcome {
    Ok {
        result: crate::arbitrage::optimizer::OptimizationResult,
        quotes: u64,
    },
    /// Optimizer ran quotes but found no strictly positive net sample.
    ///
    /// Carries `quotes` so the WHI-952 / WHI-976 `amm_quotes` counter records
    /// work on the common unprofitable path (not only on a found optimum).
    NoOptimum {
        quotes: u64,
    },
    Rejected {
        reason: &'static str,
    },
    Error(crate::arbitrage::error::ArbitrageError),
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

/// Topology-only route key (zero V3/Moe crossings) for constant-fee optimize.
///
/// Materialize re-scores with the true mixed-sim route key. G-1 may evaluate
/// `fee_plan_cost(route_key(input), fee_context)` at every sample instead.
fn topology_route_key(path_pools: &[AMM]) -> Result<RouteKey, String> {
    let protocols: Vec<ProtocolKind> = path_pools.iter().map(protocol_kind_of_amm).collect();
    let has_v3 = protocols.contains(&ProtocolKind::V3);
    let has_moe = protocols.contains(&ProtocolKind::Moe);
    let mut key = RouteKey::new(protocols).map_err(|e| e.to_string())?;
    if has_v3 {
        key = key.with_v3_ticks(TickCrossingBucket::Zero);
    }
    if has_moe {
        key = key.with_moe_bins(BinCrossingBucket::Zero);
    }
    Ok(key)
}

fn optimize_path(
    optimizer: &PathOptimizer,
    path: &ArbitragePath,
    path_pools: &[AMM],
    config: &DiscoveryConfig,
) -> OptimizeOutcome {
    if let Some(measured) = config.measured_fee.as_ref() {
        let topo = match topology_route_key(path_pools) {
            Ok(k) => k,
            Err(_) => {
                return OptimizeOutcome::Rejected {
                    reason: crate::metrics::reject_reason::UNKNOWN_ROUTE,
                };
            }
        };
        match measured.fee_plan_cost(&topo) {
            Ok(cost) => {
                match optimizer.optimize_with_fee_quote_count(
                    path,
                    path_pools,
                    &ConstantFeeCost(cost),
                ) {
                    Ok((Some(result), quotes)) => OptimizeOutcome::Ok { result, quotes },
                    Ok((None, quotes)) => OptimizeOutcome::NoOptimum { quotes },
                    Err(e) => OptimizeOutcome::Error(e),
                }
            }
            Err(e) => {
                // Unknown / unapproved topology bucket or FeePolicy rejection —
                // fail closed (no hop-table fallback). Still allow optimize
                // with zero fee only when we will fail at materialize? No:
                // skip entirely so we never rank send-ineligible paths.
                tracing::debug!(
                    target: "bot.discovery",
                    error = %e,
                    "measured fee_plan_cost rejected path at optimize"
                );
                OptimizeOutcome::Rejected {
                    reason: discovery_fee_reject_reason(&e),
                }
            }
        }
    } else {
        // Offline fixture path: fixed hop table.
        let fee = ConstantFeeCost(config.gas.calculate_gas_cost(path.hops.len()));
        // gas_price_wei = 0 → ZeroFeeCost semantics (net = gross).
        if config.gas.gas_price_wei == 0 {
            match optimizer.optimize_with_fee_quote_count(path, path_pools, &ZeroFeeCost) {
                Ok((Some(result), quotes)) => OptimizeOutcome::Ok { result, quotes },
                Ok((None, quotes)) => OptimizeOutcome::NoOptimum { quotes },
                Err(e) => OptimizeOutcome::Error(e),
            }
        } else {
            match optimizer.optimize_with_fee_quote_count(path, path_pools, &fee) {
                Ok((Some(result), quotes)) => OptimizeOutcome::Ok { result, quotes },
                Ok((None, quotes)) => OptimizeOutcome::NoOptimum { quotes },
                Err(e) => OptimizeOutcome::Error(e),
            }
        }
    }
}

/// Screen gross → net using measured FeePolicy path or offline GasConfig.
fn gas_screen_net(
    gross: U256,
    hops: usize,
    route_key: &RouteKey,
    config: &DiscoveryConfig,
) -> Result<U256, &'static str> {
    use crate::metrics::reject_reason;

    if let Some(measured) = config.measured_fee.as_ref() {
        match measured.fee_plan_cost(route_key) {
            Ok(cost) => match net_profit_after_gas_cost(gross, cost) {
                Some(net) => Ok(net),
                None => Err(reject_reason::NET_PROFIT),
            },
            Err(e) => Err(discovery_fee_reject_reason(&e)),
        }
    } else {
        if !config
            .gas
            .is_profitable_after_gas(gross, hops, default_gas_safety_margin())
        {
            return Err(reject_reason::GAS_SCREEN);
        }
        match config.gas.net_profit(gross, hops) {
            Some(n) => Ok(n),
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
    let net_profit = match gas_screen_net(gross, hops, &cached.route_key, config) {
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

    Ok(DiscoveredOpportunity {
        candidate,
        route_key: cached.route_key.clone(),
        is_cross_protocol: is_cross,
        protocol_kinds,
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
    use crate::execution::{BlockFeeContext, RuntimeGasProfile, RuntimeProfileConfig};
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

    /// WHI-1411 acceptance: a test drives a 100%-rejection configuration and asserts
    /// the liveness alarm fires (the current invariant does not).
    #[test]
    fn liveness_alarm_fires_on_100_percent_rejection_while_whi_976_does_not() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
        use std::io::{self, Write};
        use std::path::PathBuf;
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
        // Invalidate in memory only (no invalidation_path on disk) to force 100% pre-sim rejection
        let r_v2_v2 = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let r_v2_v3 = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3])
            .unwrap()
            .with_v3_ticks(TickCrossingBucket::Zero);
        let _ = profile.invalidate(&r_v2_v2);
        let _ = profile.invalidate(&r_v2_v3);

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
    }

    #[test]
    fn liveness_alarm_fires_on_sustained_zero_quoted_window() {
        use crate::service::fee_scoring::MeasuredFeeScoring;
        use alloy::primitives::B256;
        use std::path::PathBuf;
        use std::sync::Arc;

        let pools = cross_protocol_fixture_pools();
        let mut eng = engine();
        let mut prime_config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        prime_config.gas.gas_price_wei = 0;
        eng.discover(&pools, &prime_config, &TipRefreshScope::Full)
            .expect("prime");

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
        config.liveness_threshold = Some(2);

        let dirty = HashSet::from([fixture_v2_pool_address()]);
        let (_found, stats) = eng
            .discover(&pools, &config, &TipRefreshScope::Touched(dirty))
            .expect("touched pass");

        assert!(stats.cycles_optimized >= 2);
        assert_eq!(stats.paths_quoted, 0);
        assert!(
            stats.liveness_alarm,
            "sustained unquoted cycles >= threshold must fire liveness alarm"
        );
    }
}
