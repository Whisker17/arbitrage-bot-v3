//! Optimal-input search for arbitrage paths (WHI-948 / M4-4).
//!
//! Objective is **net PnL** (`gross_quote − fee_cost`), found by deterministic
//! log-scale coarse sampling + local refinement over **all** local candidate
//! intervals — no concavity assumption, no 1 WMNT hard ceiling, no binary
//! search that squeezes to the least-profitable edge.
//!
//! Fee cost is injected via [`FeeCostModel`]. G-2 (WHI-949) exposes
//! `fee_plan_cost(route_key, fee_context)` on
//! [`crate::service::fee_scoring::MeasuredFeeScoring`]; discovery materialize
//! uses that for send-identical admission. G-1 (WHI-1409) closed the gap on
//! the optimize side: `PathOptimizer::optimize_with_quote_and_fee` lets the
//! caller supply a route-key-aware quote source so discovery can evaluate the
//! *real* per-sample route key (actual V3 tick / Moe bin crossings) instead of
//! a single topology-guessed constant fee shared by every sample — see
//! `RouteAwareFeeCost` in `service::path_index`.

use alloy::primitives::U256;

use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    moe::MoeError,
};

use super::error::ArbitrageError;
use super::pathfinder::{ArbitragePath, PathHop};

/// True when a hop cannot be quoted because pool state is not fully loaded.
///
/// Moe surfaces this as [`AMMError::MoeError`]`(`[`MoeError::IncompleteState`]`)`
/// via `#[from]`; the top-level [`AMMError::IncompleteState`] is used by other
/// AMM variants. Both must soft-skip a path rather than abort discovery.
pub(crate) fn is_incomplete_amm_state(err: &AMMError) -> bool {
    matches!(
        err,
        AMMError::IncompleteState | AMMError::MoeError(MoeError::IncompleteState)
    )
}

/// Knobs for multi-peak optimal-input search (WHI-948).
///
/// Defaults are the declared sampling/refinement budget used by production and
/// recorded in evidence packages.
#[derive(Debug, Clone)]
pub struct OptimizationConfig {
    /// Local ternary-refine iterations per candidate interval.
    pub max_iterations: usize,
    /// Local refine stops when `(high − low) * 10_000 ≤ mid * tolerance_bps`
    /// (relative window width in basis points of the midpoint).
    pub tolerance_bps: u32,
    /// Feasible-domain upper bound (G-3 supplies the production cap; discovery
    /// currently forwards `DiscoveryConfig::max_input`).
    pub max_input: U256,
    /// Log-scale coarse sample count across `[1, max_input]` (endpoints always
    /// included separately).
    pub coarse_samples: usize,
    /// Hard cap on quote evaluations per path (coarse + refine + endpoints).
    pub max_quotes: u64,
}

impl Default for OptimizationConfig {
    fn default() -> Self {
        Self {
            // Evidence defaults (WHI-948): log coarse + per-interval refine.
            max_iterations: 12,
            tolerance_bps: 5,
            max_input: U256::from(10_u128.pow(24)),
            coarse_samples: 24,
            max_quotes: 96,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptimizationResult {
    pub path: ArbitragePath,
    pub optimal_input: U256,
    /// Gross profit (`output − input`). Materialization re-derives net after gas.
    pub expected_profit: U256,
    pub output_amount: U256,
    /// Net score used for selection (`gross − fee_cost`). Zero fee ⇒ equals gross.
    pub net_profit: U256,
}

#[derive(Clone)]
pub struct PathOptimizer {
    config: OptimizationConfig,
}

/// Fee cost in settlement-asset wei for a candidate input size.
///
/// G-2 (WHI-949) supplies `MeasuredFeeScoring::fee_plan_cost(route_key, …)`.
/// Wire a model that maps `amount_in → route_key(amount_in) → fee_plan_cost`
/// for full input-dependent gas; production discovery currently uses
/// hop-topology [`ConstantFeeCost`] from that API (zero crossing buckets) at
/// optimize time and re-scores with the true route key at materialize.
/// Offline/tests may use [`ZeroFeeCost`] or [`SteppedFeeCost`].
pub trait FeeCostModel {
    fn fee_cost(&self, amount_in: U256) -> U256;
}

/// No fee — net score equals gross. Offline / pure-gross fixtures.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZeroFeeCost;

impl FeeCostModel for ZeroFeeCost {
    fn fee_cost(&self, _amount_in: U256) -> U256 {
        U256::ZERO
    }
}

/// Constant fee independent of input (constant-offset gas models).
#[derive(Debug, Clone, Copy)]
pub struct ConstantFeeCost(pub U256);

impl FeeCostModel for ConstantFeeCost {
    fn fee_cost(&self, _amount_in: U256) -> U256 {
        self.0
    }
}

/// Step-function fee: last `(threshold, cost)` with `amount_in ≥ threshold` wins.
///
/// Models gas-bucket flips as input grows (V3 tick / Moe bin crossings).
/// Construct via [`SteppedFeeCost::new`] so thresholds are sorted ascending.
#[derive(Debug, Clone)]
pub struct SteppedFeeCost {
    /// Sorted ascending by threshold.
    pub steps: Vec<(U256, U256)>,
}

impl SteppedFeeCost {
    /// Build a stepped fee schedule. Steps are sorted by threshold ascending.
    pub fn new(mut steps: Vec<(U256, U256)>) -> Self {
        steps.sort_by(|a, b| a.0.cmp(&b.0));
        Self { steps }
    }
}

impl FeeCostModel for SteppedFeeCost {
    fn fee_cost(&self, amount_in: U256) -> U256 {
        let mut cost = U256::ZERO;
        for (threshold, step_cost) in &self.steps {
            if amount_in >= *threshold {
                cost = *step_cost;
            } else {
                break;
            }
        }
        cost
    }
}

/// Outcome of a pure (quote-fn) search — used by unit tests and the AMM wrapper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOutcome {
    pub optimal_input: U256,
    pub gross_profit: U256,
    pub output_amount: U256,
    pub net_profit: U256,
    pub quotes: u64,
}

impl PathOptimizer {
    pub fn new(config: OptimizationConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &OptimizationConfig {
        &self.config
    }

    pub fn optimize(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
    ) -> Result<Option<OptimizationResult>, ArbitrageError> {
        self.optimize_with_fee(path, pools, &ZeroFeeCost)
    }

    /// Like [`Self::optimize`], but also returns how many quote calls ran
    /// (WHI-952 `amm_quotes` counter — exact, not a budget upper bound).
    pub fn optimize_with_quote_count(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
    ) -> Result<(Option<OptimizationResult>, u64), ArbitrageError> {
        self.optimize_with_fee_quote_count(path, pools, &ZeroFeeCost)
    }

    pub fn optimize_with_fee<F: FeeCostModel>(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        fee: &F,
    ) -> Result<Option<OptimizationResult>, ArbitrageError> {
        self.optimize_with_fee_quote_count(path, pools, fee)
            .map(|(result, _quotes)| result)
    }

    pub fn optimize_with_fee_quote_count<F: FeeCostModel>(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        fee: &F,
    ) -> Result<(Option<OptimizationResult>, u64), ArbitrageError> {
        if pools.len() != path.hops.len() {
            return Err(ArbitrageError::Optimization(
                "Mismatch between path hops and pools".into(),
            ));
        }

        self.optimize_with_quote_and_fee(path, fee, |amount_in| {
            simulate_path_gross(path, pools, amount_in)
        })
    }

    /// Generalized optimize: the caller supplies the quote source (gross
    /// profit + output amount per candidate input) instead of the fixed
    /// [`simulate_path_gross`] wiring [`Self::optimize_with_fee_quote_count`]
    /// uses.
    ///
    /// WHI-1409 / G-1: discovery's measured-fee path plugs in a route-key-aware
    /// quote source here — one that simulates the *real* per-sample V3 tick /
    /// Moe bin crossings (via `simulate_mixed_path_with_route_key`) and prices
    /// each candidate with the matching [`FeeCostModel`] built from that same
    /// route key, instead of a single topology-guessed constant fee shared by
    /// every sample. Offline/test callers keep using
    /// [`Self::optimize_with_fee_quote_count`], which is now a thin wrapper
    /// over this method.
    pub fn optimize_with_quote_and_fee<F, Q>(
        &self,
        path: &ArbitragePath,
        fee: &F,
        mut quote: Q,
    ) -> Result<(Option<OptimizationResult>, u64), ArbitrageError>
    where
        F: FeeCostModel,
        Q: FnMut(U256) -> Result<Option<(U256, U256)>, ArbitrageError>,
    {
        let path_owned = path.clone();
        let mut quote_err: Option<ArbitrageError> = None;
        let (outcome, quotes) = search_optimal_input(&self.config, fee, |amount_in| {
            match quote(amount_in) {
                Ok(Some((gross, output))) => Some((gross, output)),
                Ok(None) => None,
                Err(e) => {
                    quote_err = Some(e);
                    None
                }
            }
        });

        if let Some(err) = quote_err {
            return Err(err);
        }

        let Some(outcome) = outcome else {
            return Ok((None, quotes));
        };

        Ok((
            Some(OptimizationResult {
                path: path_owned,
                optimal_input: outcome.optimal_input,
                expected_profit: outcome.gross_profit,
                output_amount: outcome.output_amount,
                net_profit: outcome.net_profit,
            }),
            quotes,
        ))
    }
}

/// Checked net score: `None` when gross < fee (common across samples).
///
/// Never uses wrapping `U256` subtraction (WHI-937 class).
#[inline]
pub fn net_score(gross: U256, fee: U256) -> Option<U256> {
    gross.checked_sub(fee).filter(|n| !n.is_zero())
}

/// Deterministic multi-peak search over a pure quote function.
///
/// `quote(amount_in) -> Option<(gross_profit, output_amount)>`. `None` means
/// unquotable / unprofitable gross (soft skip). Fee is applied via `fee`.
///
/// Returns `(best, quotes_used)`. `best` is `None` when no sample has strictly
/// positive net score, or when the quote budget is zero / `max_input` is zero.
pub fn search_optimal_input<F, Q>(
    config: &OptimizationConfig,
    fee: &F,
    mut quote: Q,
) -> (Option<SearchOutcome>, u64)
where
    F: FeeCostModel,
    Q: FnMut(U256) -> Option<(U256, U256)>,
{
    if config.max_input.is_zero() || config.max_quotes == 0 {
        return (None, 0);
    }

    let mut quotes: u64 = 0;
    let mut best: Option<SearchOutcome> = None;

    // --- Coarse log-scale samples + domain endpoints ---
    let samples = log_spaced_samples(config.max_input, config.coarse_samples);
    let mut scored: Vec<(U256, Option<U256>)> = Vec::with_capacity(samples.len());

    for &amount_in in &samples {
        let net = consider_point(
            &mut quote,
            fee,
            amount_in,
            config.max_input,
            &mut quotes,
            config.max_quotes,
            &mut best,
        );
        scored.push((amount_in, net));
    }

    // --- Identify local candidate intervals from coarse scores ---
    // Treat None as −∞. A local max is strictly better than a missing neighbour
    // and ≥ the other neighbour (plateaus keep the leftmost peak).
    let n = scored.len();
    let mut intervals: Vec<(U256, U256)> = Vec::new();

    for i in 0..n {
        let (_, s) = scored[i];
        let Some(s) = s else {
            continue;
        };
        let left_ok = i == 0 || scored[i - 1].1.map(|l| s >= l).unwrap_or(true);
        let right_ok = i + 1 >= n || scored[i + 1].1.map(|r| s >= r).unwrap_or(true);
        if left_ok && right_ok {
            let lo = if i == 0 {
                scored[i].0
            } else {
                scored[i - 1].0
            };
            let hi = if i + 1 >= n {
                scored[i].0
            } else {
                scored[i + 1].0
            };
            if hi >= lo {
                intervals.push((lo, hi));
            }
        }
    }

    // Contiguous positive regions that the local-max pass may have under-covered
    // (flat ridges spanning multiple samples).
    {
        let mut start: Option<U256> = None;
        let mut prev_pos: Option<U256> = None;
        for &(amount, score) in &scored {
            if score.is_some() {
                if start.is_none() {
                    start = Some(amount);
                }
                prev_pos = Some(amount);
            } else if let (Some(s), Some(e)) = (start.take(), prev_pos.take()) {
                intervals.push((s, e));
            }
        }
        if let (Some(s), Some(e)) = (start, prev_pos) {
            intervals.push((s, e));
        }
    }

    // Feasible-domain endpoints (WHI-948): lower bound is the smallest coarse
    // sample with score > 0 when any exist; otherwise domain floor 1. Upper is
    // always max_input (G-3 supplies the production cap via config).
    let domain_lo = scored
        .iter()
        .filter_map(|(amount, score)| score.map(|_| *amount))
        .min()
        .unwrap_or(U256::from(1u64));
    intervals.push((domain_lo, domain_lo));
    intervals.push((config.max_input, config.max_input));

    // Dedup intervals (sort by lo, then hi).
    intervals.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    intervals.dedup();

    // --- Local ternary refinement inside each interval ---
    for (mut low, mut high) in intervals {
        if high < low {
            continue;
        }
        // Always evaluate endpoints.
        let _ = consider_point(
            &mut quote,
            fee,
            low,
            config.max_input,
            &mut quotes,
            config.max_quotes,
            &mut best,
        );
        if low != high {
            let _ = consider_point(
                &mut quote,
                fee,
                high,
                config.max_input,
                &mut quotes,
                config.max_quotes,
                &mut best,
            );
        }

        for _ in 0..config.max_iterations {
            if quotes >= config.max_quotes {
                break;
            }
            if high <= low {
                break;
            }
            let span = high - low;
            // tolerance_bps wired: relative window width.
            let mid: U256 = low + (span >> 1);
            if !mid.is_zero() {
                let thr = mid
                    .saturating_mul(U256::from(config.tolerance_bps))
                    / U256::from(10_000u64);
                if span <= thr || span <= U256::from(1u64) {
                    let _ = consider_point(
                        &mut quote,
                        fee,
                        mid,
                        config.max_input,
                        &mut quotes,
                        config.max_quotes,
                        &mut best,
                    );
                    break;
                }
            }

            // Ternary: compare third-points.
            let third = span / U256::from(3u64);
            if third.is_zero() {
                // Span is 1 or 2 — evaluate remaining points.
                let mut x = low + U256::from(1u64);
                while x < high && quotes < config.max_quotes {
                    let _ = consider_point(
                        &mut quote,
                        fee,
                        x,
                        config.max_input,
                        &mut quotes,
                        config.max_quotes,
                        &mut best,
                    );
                    x += U256::from(1u64);
                }
                break;
            }

            let left = low + third;
            let right = high - third;

            let left_net = consider_point(
                &mut quote,
                fee,
                left,
                config.max_input,
                &mut quotes,
                config.max_quotes,
                &mut best,
            );
            let right_net = consider_point(
                &mut quote,
                fee,
                right,
                config.max_input,
                &mut quotes,
                config.max_quotes,
                &mut best,
            );

            // Shrink toward the better third. Missing scores shrink from that side.
            match (left_net, right_net) {
                (Some(ls), Some(rs)) if ls >= rs => high = right,
                (Some(_), Some(_)) => low = left,
                (Some(_), None) => high = right,
                (None, Some(_)) => low = left,
                (None, None) => {
                    // Both dead — shrink from both ends toward centre once.
                    low = left;
                    high = right;
                }
            }
        }
    }

    if let Some(mut b) = best {
        b.quotes = quotes;
        (Some(b), quotes)
    } else {
        (None, quotes)
    }
}

/// Quote `amount_in`, update `best` when net improves, return the net score.
fn consider_point<F, Q>(
    quote: &mut Q,
    fee: &F,
    amount_in: U256,
    max_input: U256,
    quotes: &mut u64,
    max_quotes: u64,
    best: &mut Option<SearchOutcome>,
) -> Option<U256>
where
    F: FeeCostModel,
    Q: FnMut(U256) -> Option<(U256, U256)>,
{
    if *quotes >= max_quotes || amount_in.is_zero() || amount_in > max_input {
        return None;
    }
    *quotes = quotes.saturating_add(1);
    let (gross, output) = quote(amount_in)?;
    let net = net_score(gross, fee.fee_cost(amount_in))?;
    let candidate = SearchOutcome {
        optimal_input: amount_in,
        gross_profit: gross,
        output_amount: output,
        net_profit: net,
        quotes: *quotes,
    };
    let replace = match best {
        None => true,
        Some(cur) => {
            net > cur.net_profit || (net == cur.net_profit && amount_in < cur.optimal_input)
        }
    };
    if replace {
        *best = Some(candidate);
    }
    Some(net)
}

/// Log-spaced samples on `[1, max_input]`, always including both endpoints.
///
/// Pure integer construction (no `f64`): `count` rungs evenly spaced in
/// **bit-index** space (`2^{i·(L−1)/(n−1)}`), then densified with midpoints
/// of the largest gaps until `count` is met. `count` is a hard upper bound on
/// the returned set size (duplicates collapse, so size may be lower).
pub fn log_spaced_samples(max_input: U256, count: usize) -> Vec<U256> {
    if max_input.is_zero() {
        return Vec::new();
    }
    let one = U256::from(1u64);
    if max_input == one || count <= 1 {
        let mut set = std::collections::BTreeSet::new();
        set.insert(one.min(max_input));
        set.insert(max_input);
        return set.into_iter().collect();
    }

    let mut set = std::collections::BTreeSet::new();
    set.insert(one);
    set.insert(max_input);

    let target = count.max(2);
    // Bit-length of max_input (position of highest set bit + 1).
    let bit_len = 256u32.saturating_sub(max_input.leading_zeros() as u32).max(1);
    let max_bit = bit_len.saturating_sub(1);

    // Evenly spaced bit indices → geometric rungs. Caps at `target` inserts.
    for i in 0..target {
        let bit = if target == 1 {
            0
        } else {
            (i as u32 * max_bit) / (target as u32 - 1)
        };
        let rung = if bit >= 255 {
            max_input
        } else {
            (U256::from(1u64) << bit).min(max_input).max(one)
        };
        set.insert(rung);
        if set.len() >= target {
            break;
        }
    }

    // Densify midpoints of largest gaps until we hit `target` (never exceed).
    while set.len() < target {
        let pts: Vec<U256> = set.iter().copied().collect();
        let mut best_gap = U256::ZERO;
        let mut best_mid: Option<U256> = None;
        for w in pts.windows(2) {
            let lo = w[0];
            let hi = w[1];
            if hi <= lo + U256::from(1u64) {
                continue;
            }
            let gap = hi - lo;
            if gap > best_gap {
                best_gap = gap;
                best_mid = Some(lo + (gap >> 1));
            }
        }
        let Some(mid) = best_mid else {
            break;
        };
        if !set.insert(mid) {
            break;
        }
    }

    debug_assert!(set.len() <= target.max(2));
    set.into_iter().collect()
}

/// Gross-only path simulation used by the multi-peak search.
///
/// Returns `(gross_profit, output_amount)` or `None` when the path is
/// unquotable / unprofitable at this size.
fn simulate_path_gross(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> Result<Option<(U256, U256)>, ArbitrageError> {
    match simulate_path(path, pools, amount_in)? {
        Some(r) => Ok(Some((r.expected_profit, r.output_amount))),
        None => Ok(None),
    }
}

pub fn simulate_path(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> Result<Option<OptimizationResult>, ArbitrageError> {
    if path.hops.is_empty() {
        tracing::warn!(target: "simulate.path", "No hops found in arbitrage path");
        return Ok(None);
    }

    // Zero input shows up at the low end of the search; not operational.
    if amount_in.is_zero() {
        tracing::trace!(
            target: "simulate.path",
            "Skipping simulation because input amount is zero"
        );
        return Ok(None);
    }

    let mut current_amount = amount_in;

    for (index, (hop, amm)) in path.hops.iter().zip(pools.iter()).enumerate() {
        tracing::debug!(
            target: "simulate.path",
            hop_index = index,
            pool = %hop.pool_address,
            token_in = %hop.token_in,
            token_out = %hop.token_out,
            input_amount = %current_amount,
            "Simulating hop"
        );

        let output = match simulate_hop(amm, hop, current_amount) {
            Ok(output) => output,
            Err(error) if is_incomplete_amm_state(&error) => {
                tracing::debug!(
                    target: "simulate.path",
                    hop_index = index,
                    pool = %hop.pool_address,
                    error = %error,
                    "Skipping path because AMM state is incomplete"
                );
                return Ok(None);
            }
            Err(error) => return Err(ArbitrageError::Simulation(error.to_string())),
        };
        // Zero hop output is ordinary quote death — empty reserve / no
        // liquidity in range / amount dust after fees — not a simulation
        // malfunction. Real failures already return `Err` above (WHI-969).
        // Soft-skip at TRACE so default logs stay actionable.
        if output.is_zero() {
            tracing::trace!(
                target: "simulate.path",
                hop_index = index,
                pool = %hop.pool_address,
                "Simulation produced zero output; aborting path"
            );
            return Ok(None);
        }
        tracing::debug!(
            target: "simulate.path",
            hop_index = index,
            pool = %hop.pool_address,
            output_amount = %output,
            "Hop simulation succeeded"
        );
        current_amount = output;
    }

    // `final_output < input` is ordinary unprofitability — the common case at
    // every search step on a dead path (WHI-937). Do not treat checked
    // subtraction as an arithmetic "underflow" error: it is a comparison.
    // There is no separate genuine U256 underflow on this path; hard simulation
    // failures surface as `Err(ArbitrageError::Simulation(...))` above.
    if current_amount < amount_in {
        tracing::trace!(
            target: "simulate.path",
            final_output = %current_amount,
            input_amount = %amount_in,
            "path unprofitable"
        );
        return Ok(None);
    }

    let profit = current_amount - amount_in;
    tracing::debug!(
        target: "simulate.path",
        final_output = %current_amount,
        expected_profit = %profit,
        "Simulation completed"
    );
    Ok(Some(OptimizationResult {
        path: path.clone(),
        optimal_input: amount_in,
        expected_profit: profit,
        output_amount: current_amount,
        // Single-point simulate has no fee context; net == gross.
        net_profit: profit,
    }))
}

fn simulate_hop(amm: &AMM, hop: &PathHop, amount_in: U256) -> Result<U256, AMMError> {
    amm.simulate_swap(hop.token_in, hop.token_out, amount_in)
}

pub fn pools_for_path(
    path: &ArbitragePath,
    state_pools: &[AMM],
) -> Result<Vec<AMM>, ArbitrageError> {
    let mut pools = Vec::with_capacity(path.hops.len());

    for hop in &path.hops {
        let pool = state_pools
            .iter()
            .find(|pool| pool.address() == hop.pool_address)
            .ok_or_else(|| {
                ArbitrageError::Simulation(format!(
                    "Pool {:?} not found in state",
                    hop.pool_address
                ))
            })?;

        pools.push(pool.clone());
    }

    Ok(pools)
}

/// Legacy binary-search optimizer preserved only for regression comparison
/// tests (WHI-948 acceptance: multi-peak / large-input / right-endpoint).
///
/// Do **not** call from production. Encodes the three defects: 1 WMNT ceiling,
/// converges to largest profitable input, non-monotone false negatives.
#[cfg(test)]
pub fn legacy_binary_search_optimal_input<Q>(
    max_input: U256,
    min_profit: U256,
    max_iterations: usize,
    mut quote_gross: Q,
) -> Option<(U256, U256)>
where
    Q: FnMut(U256) -> Option<U256>,
{
    let initial_guess = U256::from(10_u128.pow(18));
    let mut low = U256::ZERO;
    let mut high = initial_guess.min(max_input);
    let mut best: Option<(U256, U256)> = None;

    for _ in 0..max_iterations {
        let mid: U256 = (low + high) >> 1;
        if mid.is_zero() {
            high = mid.saturating_sub(U256::from(1));
            continue;
        }
        match quote_gross(mid) {
            Some(profit) if profit > min_profit => {
                best = Some((mid, profit));
                low = mid + U256::from(1);
            }
            _ => {
                high = mid.saturating_sub(U256::from(1));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::{amm::AMM, uniswap_v3::UniswapV3Pool, Token};
    use alloy::primitives::Address;

    fn addr(b: u8) -> Address {
        let mut raw = [0u8; 20];
        raw[19] = b;
        Address::from(raw)
    }

    fn dummy_pool() -> AMM {
        let mut pool = UniswapV3Pool::default();
        pool.address = addr(1);
        pool.token_a = Token::new_with_decimals(addr(2), 6);
        pool.token_b = Token::new_with_decimals(addr(3), 6);
        pool.liquidity = 1_000_000;
        pool.sqrt_price = U256::from(1) << 96;
        pool.fee = 3000;
        AMM::from(pool)
    }

    fn u(n: u64) -> U256 {
        U256::from(n)
    }

    /// Piecewise multi-peak gross: peaks at 30 (profit 100) and 80 (profit 70).
    /// Domain [1, 100]. Old binary search with 1e18 ceiling is irrelevant here
    /// because cap is small; with min_profit=0 it still walks to the right edge.
    fn multi_peak_gross(x: U256) -> Option<(U256, U256)> {
        let x = u64::try_from(x).ok()?;
        let profit = if (20..=40).contains(&x) {
            // Triangle peak at 30 → 100
            100u64.saturating_sub(x.abs_diff(30) * 5)
        } else if (70..=90).contains(&x) {
            // Triangle peak at 80 → 70
            70u64.saturating_sub(x.abs_diff(80) * 3)
        } else {
            0
        };
        if profit == 0 {
            None
        } else {
            Some((u(profit), u(x + profit)))
        }
    }

    /// Unimodal peak at 40, profitable on [10, 80], domain [1, 100].
    fn right_edge_gross(x: U256) -> Option<(U256, U256)> {
        let x = u64::try_from(x).ok()?;
        if !(10..=80).contains(&x) {
            return None;
        }
        let profit = 100u64.saturating_sub(x.abs_diff(40) * 2);
        if profit == 0 {
            None
        } else {
            Some((u(profit), u(x + profit)))
        }
    }

    /// Profitable only above cap/2: [60, 90] on domain [1, 100], peak at 75.
    fn large_input_gross(x: U256) -> Option<(U256, U256)> {
        let x = u64::try_from(x).ok()?;
        if !(60..=90).contains(&x) {
            return None;
        }
        let profit = 50u64.saturating_sub(x.abs_diff(75));
        if profit == 0 {
            None
        } else {
            Some((u(profit), u(x + profit)))
        }
    }

    #[test]
    fn simulate_path_zero_amount_returns_none() {
        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 3000,
            }],
        };

        let result = simulate_path(&path, &[dummy_pool()], U256::ZERO).unwrap();
        assert!(result.is_none());
    }

    /// WHI-969: hop `amount_out == 0` is ordinary quote death (empty reserve /
    /// dust), not a broken simulator. Soft-skip with Ok(None) — same class as
    /// unprofitable paths. Genuine malfunctions surface as `Err` from
    /// `simulate_swap` (token mismatch, incomplete state, math errors).
    #[test]
    fn simulate_path_zero_hop_output_returns_none() {
        use crate::amms::uniswap_v2::UniswapV2Pool;

        let token_a = addr(0x11);
        let token_b = addr(0x22);
        let pool_addr = addr(0xa1);

        let mut pool = UniswapV2Pool::new(pool_addr, 300);
        pool.token_a = Token::new_with_decimals(token_a, 18);
        pool.token_b = Token::new_with_decimals(token_b, 18);
        // reserve_out == 0 → get_amount_out returns zero for any amount_in.
        pool.reserve_0 = 1_000_000_000_000_000_000;
        pool.reserve_1 = 0;
        let pools = vec![AMM::UniswapV2Pool(pool)];

        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: pool_addr,
                token_in: token_a,
                token_out: token_b,
                fee_bps: 30,
            }],
        };

        let result = simulate_path(&path, &pools, U256::from(10u128.pow(18))).expect("soft skip");
        assert!(
            result.is_none(),
            "zero hop output must soft-skip the path, not Err"
        );
    }

    /// WHI-937: `final_output < input` is ordinary unprofitability (fee-only
    /// round-trip), not a simulation failure. Soft-skip with Ok(None).
    #[test]
    fn simulate_path_unprofitable_roundtrip_returns_none() {
        use crate::amms::uniswap_v2::UniswapV2Pool;

        let token_a = addr(0x11);
        let token_b = addr(0x22);
        let pool_addr = addr(0xa1);

        // 300 matches the Agni-V2 service fee unit used by production V2 pools.
        let mut pool = UniswapV2Pool::new(pool_addr, 300);
        pool.token_a = Token::new_with_decimals(token_a, 18);
        pool.token_b = Token::new_with_decimals(token_b, 18);
        pool.reserve_0 = 1_000_000_000_000_000_000_000;
        pool.reserve_1 = 1_000_000_000_000_000_000_000;
        let pools = vec![AMM::UniswapV2Pool(pool.clone()), AMM::UniswapV2Pool(pool)];

        // Same-pool round-trip always loses the V2 fee → unprofitable.
        let path = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: pool_addr,
                    token_in: token_a,
                    token_out: token_b,
                    fee_bps: 30,
                },
                PathHop {
                    pool_address: pool_addr,
                    token_in: token_b,
                    token_out: token_a,
                    fee_bps: 30,
                },
            ],
        };

        let amount_in = U256::from(10u128.pow(18));
        let result = simulate_path(&path, &pools, amount_in).expect("soft skip");
        assert!(
            result.is_none(),
            "fee-only round-trip must soft-skip as unprofitable, not Err"
        );
    }

    #[test]
    fn simulate_path_skips_incomplete_amm_state() {
        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 3000,
            }],
        };

        let result = simulate_path(&path, &[dummy_pool()], U256::from(10_000)).unwrap();
        assert!(result.is_none());
    }

    /// WHI-862: Moe wraps IncompleteState as `AMMError::MoeError(...)` (via
    /// `#[from]`). Discovery must soft-skip those paths, not abort the whole pass.
    #[test]
    fn simulate_path_skips_moe_incomplete_state_variant() {
        use crate::amms::moe::MoeLbPair;

        let mut pair = MoeLbPair::new(addr(1));
        pair.token_x = Token::new_with_decimals(addr(2), 18);
        pair.token_y = Token::new_with_decimals(addr(3), 18);
        pair.bin_step = 20;
        pair.active_id = 8_388_608;
        // No snapshot → simulate_swap returns MoeError::IncompleteState.
        assert!(pair.snapshot.is_none());

        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 20,
            }],
        };
        let result =
            simulate_path(&path, &[AMM::MoeLbPair(pair)], U256::from(10_000)).expect("soft skip");
        assert!(
            result.is_none(),
            "Moe IncompleteState must soft-skip the path, not Err"
        );
    }

    #[test]
    fn net_score_checked_never_underflows() {
        assert_eq!(net_score(u(10), u(3)), Some(u(7)));
        assert_eq!(net_score(u(10), u(10)), None); // zero net rejected
        assert_eq!(net_score(u(3), u(10)), None);
    }

    #[test]
    fn small_domain_exhaustive_oracle_regret_within_tolerance() {
        // Enumerable domain [1, 100]; brute-force net optimum vs search.
        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 20,
            max_iterations: 16,
            tolerance_bps: 500, // 5% of mid — enough for integer domain
            max_quotes: 96,
        };
        let fee = ZeroFeeCost;

        let mut oracle_best: Option<(u64, u64)> = None; // (input, net)
        for x in 1u64..=100 {
            if let Some((gross, _)) = multi_peak_gross(u(x)) {
                if let Some(net) = net_score(gross, fee.fee_cost(u(x))) {
                    let n = u64::try_from(net).unwrap();
                    let replace = match oracle_best {
                        None => true,
                        Some((_, best_n)) => n > best_n,
                    };
                    if replace {
                        oracle_best = Some((x, n));
                    }
                }
            }
        }
        let (oracle_in, oracle_net) = oracle_best.expect("fixture has profit");

        let (outcome, quotes) =
            search_optimal_input(&config, &fee, multi_peak_gross);
        let outcome = outcome.expect("finds peak");
        let found_net = u64::try_from(outcome.net_profit).unwrap();
        let regret = oracle_net.saturating_sub(found_net);
        // Tolerance: regret ≤ max(1, 5% of oracle net) for this coarse grid.
        let tol = (oracle_net / 20).max(1);
        assert!(
            regret <= tol,
            "regret {regret} > tol {tol}: oracle in={oracle_in} net={oracle_net}, \
             found in={} net={found_net}",
            outcome.optimal_input,
        );
        assert!(quotes <= config.max_quotes);
    }

    #[test]
    fn multi_peak_fixture_new_finds_global_old_misses() {
        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 24,
            max_iterations: 12,
            tolerance_bps: 100,
            max_quotes: 96,
        };

        let (new, _) = search_optimal_input(&config, &ZeroFeeCost, multi_peak_gross);
        let new = new.expect("new");
        // Best peak is at 30 with net 100.
        assert!(
            new.net_profit >= u(95),
            "new should find near the tall peak, got net {}",
            new.net_profit
        );
        let new_in = u64::try_from(new.optimal_input).unwrap();
        assert!(
            (25..=35).contains(&new_in),
            "new input should be near 30, got {new_in}"
        );

        // Old binary search: first mid≈50 is unprofitable (gap between peaks), so
        // `high` collapses into the first peak and walks to its right edge (~40),
        // never discovering the second peak. Net at edge is well below the tall peak.
        let old = legacy_binary_search_optimal_input(u(100), u(0), 16, |x| {
            multi_peak_gross(x).map(|(g, _)| g)
        });
        let (old_in, old_gross) = old.expect("old finds something on this domain");
        let old_in_u = u64::try_from(old_in).unwrap();
        // Old is stuck on peak-1's right shoulder, not at the global peak.
        assert!(
            !(25..=35).contains(&old_in_u) || old_gross < u(95),
            "old should not land on the global peak; got in={old_in_u} gross={old_gross}"
        );
        let delta = new.net_profit.saturating_sub(old_gross);
        assert!(
            delta >= u(20),
            "expected material net delta, got {delta} (new={}, old={old_gross})",
            new.net_profit
        );
    }

    #[test]
    fn gas_bucket_flip_prefers_lower_gross_higher_net() {
        // Gross peaks at 80 with profit 200; at 30 profit is 120.
        // Fee jumps from 10 → 150 at input 50, so net at 80 is 50, net at 30 is 110.
        let gross = |x: U256| -> Option<(U256, U256)> {
            let x = u64::try_from(x).ok()?;
            let profit = if (20..=40).contains(&x) {
                120u64.saturating_sub(x.abs_diff(30) * 4)
            } else if (60..=90).contains(&x) {
                200u64.saturating_sub(x.abs_diff(80) * 5)
            } else {
                0
            };
            if profit == 0 {
                None
            } else {
                Some((u(profit), u(x + profit)))
            }
        };
        let fee = SteppedFeeCost::new(vec![(u(0), u(10)), (u(50), u(150))]);
        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 24,
            max_iterations: 14,
            tolerance_bps: 100,
            max_quotes: 96,
        };

        let (outcome, _) = search_optimal_input(&config, &fee, gross);
        let outcome = outcome.expect("finds net peak");
        let inn = u64::try_from(outcome.optimal_input).unwrap();
        assert!(
            inn < 50,
            "must not pick the expensive gas bucket (input={inn}, net={})",
            outcome.net_profit
        );
        assert!(
            outcome.net_profit >= u(100),
            "expected net near 110, got {}",
            outcome.net_profit
        );
        // Gross at the chosen point should be lower than the high-gross peak (~200).
        assert!(
            outcome.gross_profit < u(180),
            "should sacrifice gross for net; gross={}",
            outcome.gross_profit
        );
    }

    #[test]
    fn right_endpoint_returns_near_peak_not_edge() {
        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 24,
            max_iterations: 14,
            tolerance_bps: 100,
            max_quotes: 96,
        };
        let (new, _) = search_optimal_input(&config, &ZeroFeeCost, right_edge_gross);
        let new = new.expect("peak");
        let inn = u64::try_from(new.optimal_input).unwrap();
        assert!(
            (35..=45).contains(&inn),
            "should return near peak 40, not right edge 80; got {inn}"
        );

        let old = legacy_binary_search_optimal_input(u(100), u(0), 16, |x| {
            right_edge_gross(x).map(|(g, _)| g)
        })
        .expect("old finds something");
        let old_in = u64::try_from(old.0).unwrap();
        // Old converges to right edge of profitable interval (~80).
        assert!(
            old_in >= 70,
            "old should squeeze to right edge, got {old_in}"
        );
    }

    #[test]
    fn large_input_false_negative_old_misses_new_finds() {
        // Domain cap = 100; profitable only on [60, 90] which starts above cap/2.
        // Old first mid ≈ 50 fails → high collapses → None.
        let old = legacy_binary_search_optimal_input(u(100), u(0), 16, |x| {
            large_input_gross(x).map(|(g, _)| g)
        });
        assert!(
            old.is_none(),
            "old binary search must false-negative when profitable region starts > cap/2"
        );

        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 24,
            max_iterations: 12,
            tolerance_bps: 100,
            max_quotes: 96,
        };
        let (new, _) = search_optimal_input(&config, &ZeroFeeCost, large_input_gross);
        let new = new.expect("new finds");
        let inn = u64::try_from(new.optimal_input).unwrap();
        assert!(
            (70..=80).contains(&inn),
            "new should find near peak 75, got {inn}"
        );
    }

    #[test]
    fn quote_budget_respected() {
        let config = OptimizationConfig {
            max_input: u(10_000),
            coarse_samples: 32,
            max_iterations: 20,
            tolerance_bps: 5,
            max_quotes: 40,
        };
        let mut calls = 0u64;
        let (outcome, quotes) = search_optimal_input(&config, &ZeroFeeCost, |x| {
            calls += 1;
            // Gentle single peak around 1000.
            let xv = u64::try_from(x).unwrap_or(0);
            let profit = 500u64.saturating_sub(xv.abs_diff(1_000) / 2);
            if profit == 0 {
                None
            } else {
                Some((u(profit), u(xv + profit)))
            }
        });
        assert!(outcome.is_some());
        assert!(
            quotes <= config.max_quotes,
            "quotes {quotes} exceeded budget {}",
            config.max_quotes
        );
        assert!(
            calls <= config.max_quotes,
            "actual quote fn calls {calls} exceeded budget"
        );
    }

    #[test]
    fn tolerance_bps_is_wired_as_convergence_criterion() {
        // Tighter tolerance should not increase beyond max_quotes, but the
        // field must affect the refine stop: with huge tolerance the window
        // collapses immediately (few refine quotes); with tiny tolerance more
        // refine steps fire.
        let base = OptimizationConfig {
            max_input: u(10_000),
            coarse_samples: 16,
            max_iterations: 20,
            max_quotes: 200,
            tolerance_bps: 5,
        };
        let loose = OptimizationConfig {
            tolerance_bps: 5_000, // 50% of mid — stop almost immediately
            ..base.clone()
        };

        let quote = |x: U256| {
            let xv = u64::try_from(x).unwrap_or(0);
            let profit = 1_000u64.saturating_sub(xv.abs_diff(2_000) / 3);
            if profit == 0 {
                None
            } else {
                Some((u(profit), u(xv + profit)))
            }
        };

        let (tight_out, tight_q) = search_optimal_input(&base, &ZeroFeeCost, quote);
        let (loose_out, loose_q) = search_optimal_input(&loose, &ZeroFeeCost, quote);
        let tight_out = tight_out.unwrap();
        let loose_out = loose_out.unwrap();

        assert!(
            tight_q >= loose_q,
            "tighter tolerance should spend ≥ refine quotes (tight={tight_q}, loose={loose_q})"
        );
        // Both should land near the peak; loose may be slightly worse.
        assert!(tight_out.net_profit >= loose_out.net_profit.saturating_sub(u(50)));
    }

    #[test]
    fn no_one_wmnt_ceiling_respects_max_input() {
        // Peak at 5 WMNT; max_input = 10 WMNT. Old code would cap at 1 WMNT.
        let one_wmnt = U256::from(10u128.pow(18));
        let five_wmnt = one_wmnt * U256::from(5u64);
        let ten_wmnt = one_wmnt * U256::from(10u64);

        let config = OptimizationConfig {
            max_input: ten_wmnt,
            coarse_samples: 32,
            max_iterations: 14,
            tolerance_bps: 50,
            max_quotes: 128,
        };

        let quote = |x: U256| -> Option<(U256, U256)> {
            // Profit peaks at 5 WMNT; linear tent with half-width 5 WMNT.
            if x.is_zero() {
                return None;
            }
            let dist = if x > five_wmnt {
                x - five_wmnt
            } else {
                five_wmnt - x
            };
            if dist >= five_wmnt {
                return None;
            }
            // max profit 1e15 at peak → 0 at ±5 WMNT.
            let max_p = U256::from(10u128.pow(15));
            let profit = max_p * (five_wmnt - dist) / five_wmnt;
            if profit.is_zero() {
                None
            } else {
                Some((profit, x + profit))
            }
        };

        let (outcome, _) = search_optimal_input(&config, &ZeroFeeCost, quote);
        let outcome = outcome.expect("finds");
        assert!(
            outcome.optimal_input > one_wmnt,
            "must search above 1 WMNT when max_input allows; got {}",
            outcome.optimal_input
        );
        // Within ~20% of 5 WMNT.
        let lo = five_wmnt * U256::from(80u64) / U256::from(100u64);
        let hi = five_wmnt * U256::from(120u64) / U256::from(100u64);
        assert!(
            outcome.optimal_input >= lo && outcome.optimal_input <= hi,
            "expected near 5 WMNT, got {}",
            outcome.optimal_input
        );
    }

    #[test]
    fn replay_regret_report_on_synthetic_corpus() {
        // Synthetic "replay corpus": several profit shapes. Report regret vs
        // brute-force oracle; do not assert global optimality — only that
        // median regret stays within the declared relative tolerance.
        let corpus: Vec<(&str, Box<dyn Fn(U256) -> Option<(U256, U256)>>)> = vec![
            ("multi_peak", Box::new(multi_peak_gross)),
            ("right_edge", Box::new(right_edge_gross)),
            ("large_input", Box::new(large_input_gross)),
        ];

        let config = OptimizationConfig {
            max_input: u(100),
            coarse_samples: 24,
            max_iterations: 14,
            tolerance_bps: 100,
            max_quotes: 96,
        };

        let mut regrets = Vec::new();
        let mut report = String::from("WHI-948 replay regret report (synthetic corpus)\n");
        report.push_str("shape,oracle_net,found_net,regret,quotes\n");

        for (name, quote_fn) in &corpus {
            let mut oracle_net = 0u64;
            for x in 1u64..=100 {
                if let Some((g, _)) = quote_fn(u(x)) {
                    if let Some(n) = net_score(g, U256::ZERO) {
                        oracle_net = oracle_net.max(u64::try_from(n).unwrap());
                    }
                }
            }
            let (found, quotes) = search_optimal_input(&config, &ZeroFeeCost, |x| quote_fn(x));
            let found_net = match found {
                Some(o) => u64::try_from(o.net_profit).unwrap(),
                None => 0,
            };
            let regret = oracle_net.saturating_sub(found_net);
            regrets.push(regret);
            report.push_str(&format!(
                "{name},{oracle_net},{found_net},{regret},{quotes}\n"
            ));
        }

        // Surface the report in test output (`cargo test -- --nocapture`).
        eprintln!("{report}");

        let max_regret = *regrets.iter().max().unwrap();
        // Relative 10% of typical oracle nets (~50–100) ⇒ allow regret ≤ 15.
        assert!(
            max_regret <= 15,
            "max regret {max_regret} exceeds declared band; report:\n{report}"
        );
    }

    #[test]
    fn optimization_config_default_has_no_min_profit_field() {
        // Compile-time / structural: min_profit is gone; sampling knobs exist.
        let d = OptimizationConfig::default();
        assert!(d.coarse_samples >= 8);
        assert!(d.max_quotes >= d.coarse_samples as u64);
        assert!(d.tolerance_bps > 0);
        assert!(!d.max_input.is_zero());
    }

    #[test]
    fn coarse_samples_caps_grid_on_large_max_input() {
        // Production-scale caps must not dump every power-of-two rung.
        let max = U256::from(10u128.pow(21));
        let n = 24;
        let samples = log_spaced_samples(max, n);
        assert!(
            samples.len() <= n,
            "coarse_samples={n} must hard-cap grid, got {}",
            samples.len()
        );
        assert_eq!(samples.first().copied(), Some(U256::from(1u64)));
        assert_eq!(samples.last().copied(), Some(max));
    }
}
