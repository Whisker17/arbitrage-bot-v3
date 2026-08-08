//! Multi-protocol opportunity discovery (WHI-728 / WHI-527.3 / WHI-940).
//!
//! One merged pool set → one shared [`crate::service::path_index::DiscoveryEngine`]
//! that builds the graph and enumerates settlement cycles **once**, then per
//! block re-optimizes only cycles touching dirty pools (or all cycles on a Full
//! tip refresh). Per-hop dispatch to the owning protocol only happens *after*
//! a concrete path is found (simulation + route-key construction). This is the
//! mechanism that enables cross-DEX cycles (V2 hop + Agni hop + Moe hop in one
//! path).

use crate::amms::amm::AMM;
use crate::arbitrage::pathfinder::{ArbitragePath, DEFAULT_MAX_HOPS};
use crate::execution::{BinCrossingBucket, ProtocolKind, RouteKey, TickCrossingBucket};
use crate::service::error::ProtocolError;
use crate::service::fee_scoring::MeasuredFeeScoring;
use crate::service::gas::GasConfig;
use crate::service::path_index::DiscoveryEngine;
use crate::service::protocol::{Candidate, ExecutionAttempt, TipRefreshScope};
use crate::service::select::{protocol_kind_of_amm, SelectedProtocol};
use crate::state_space::SnapshotId;
use alloy::primitives::{Address, B256, U256};
use eyre::{eyre, Context, Result};

/// Knobs for a single multi-protocol discovery pass.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub settlement_asset: Address,
    pub max_hops: usize,
    pub min_profit: U256,
    pub max_input: U256,
    /// Offline-fixture hop table. Ignored when [`Self::measured_fee`] is `Some`.
    pub gas: GasConfig,
    /// Live measured fee scoring (WHI-949). When set, discovery and send share
    /// [`crate::execution::fee_plan_cost`] / [`crate::execution::FeePolicy::build`].
    pub measured_fee: Option<MeasuredFeeScoring>,
    /// Block timestamp used for Moe fee evolution during mixed simulation.
    pub block_timestamp: u64,
    /// Snapshot identity stamped onto candidates (offline fixtures use synthetic ids).
    pub snapshot_id: SnapshotId,
}

impl DiscoveryConfig {
    /// Default discovery knobs for a given settlement asset (offline or live).
    pub fn for_settlement(settlement_asset: Address) -> Self {
        Self {
            settlement_asset,
            max_hops: DEFAULT_MAX_HOPS,
            min_profit: U256::ZERO,
            max_input: U256::from(10u128.pow(21)),
            gas: GasConfig::default(),
            measured_fee: None,
            block_timestamp: 1_700_000_000,
            snapshot_id: SnapshotId::new(5000, 1, B256::ZERO),
        }
    }

    /// Alias used by offline fixture tests.
    pub fn offline_default(settlement_asset: Address) -> Self {
        Self::for_settlement(settlement_asset)
    }
}

/// Reject hop caps outside the strategy range unless `allow_long_paths`.
///
/// Evidence: ARB_PATHS_MANTLE.md §4 — 93.5% of arb is 2–3 pools; do not optimize
/// for long paths by default (WHI-529). Used by `bot` CLI for both offline and live.
pub fn validate_max_hops(max_hops: usize, allow_long_paths: bool) -> Result<()> {
    if max_hops == 0 {
        return Err(eyre!("--max-hops must be >= 1 (got 0)"));
    }
    if max_hops > DEFAULT_MAX_HOPS && !allow_long_paths {
        return Err(eyre!(
            "--max-hops {max_hops} exceeds strategy cap {DEFAULT_MAX_HOPS} \
             (ARB_PATHS_MANTLE.md §4: 93.5% of arbitrage is 2–3 pools; \
             solidify 2-hop and 3-hop; do not optimize for long paths). \
             Pass --allow-long-paths to override."
        ));
    }
    Ok(())
}

/// One opportunity discovered on the merged multi-protocol graph.
#[derive(Debug, Clone)]
pub struct DiscoveredOpportunity {
    pub candidate: Candidate,
    pub route_key: RouteKey,
    pub is_cross_protocol: bool,
    pub protocol_kinds: Vec<ProtocolKind>,
}

/// Work counters for one discovery pass (WHI-952 per-block summary).
///
/// Mapped from [`crate::service::path_index::DiscoveryStats`] on the watch path
/// (WHI-940 incremental engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiscoveryPassStats {
    /// Cycles re-optimized this pass (dirty / full set — not the static topology size).
    pub cycles_evaluated: u64,
    /// AMM quote / simulation calls (`simulate_path` + mixed sim) this pass.
    pub amm_quotes: u64,
    /// Gas re-scores of cached gross quotes when fee factors change (WHI-949).
    pub gas_rescores: u64,
}

impl From<crate::service::path_index::DiscoveryStats> for DiscoveryPassStats {
    fn from(s: crate::service::path_index::DiscoveryStats) -> Self {
        Self {
            cycles_evaluated: s.cycles_optimized as u64,
            amm_quotes: s.amm_quotes,
            gas_rescores: s.gas_rescores,
        }
    }
}

/// Opportunities plus the work counters that produced them (WHI-952).
#[derive(Debug, Clone, Default)]
pub struct DiscoveryPass {
    pub opportunities: Vec<DiscoveredOpportunity>,
    pub stats: DiscoveryPassStats,
}

/// True when the path hops span more than one [`ProtocolKind`].
pub fn path_is_cross_protocol(pools: &[AMM]) -> bool {
    let mut kinds = pools.iter().map(protocol_kind_of_amm);
    let Some(first) = kinds.next() else {
        return false;
    };
    kinds.any(|k| k != first)
}

/// Per-hop mixed-protocol simulation.
///
/// After a path is found, each hop is dispatched to the owning [`Protocol`]
/// impl via a single-hop `simulate_path_with_route_key` call (V2 / V3 / Moe).
/// Route-key buckets are merged across hops so a V2+Agni (or V2+Moe, etc.)
/// cycle gets a mixed [`RouteKey`].
pub fn simulate_mixed_path_with_route_key(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
    block_timestamp: u64,
) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError> {
    use crate::service::protocol::{AgniV2Protocol, AgniV3Protocol, MoeProtocol, Protocol};

    if path.hops.is_empty() {
        return Err(ProtocolError::Simulation("empty path".into()));
    }
    if path.hops.len() != pools.len() {
        return Err(ProtocolError::Simulation(format!(
            "path hops ({}) / pools ({}) length mismatch",
            path.hops.len(),
            pools.len()
        )));
    }

    let v2 = AgniV2Protocol::new(Address::ZERO);
    let v3 = AgniV3Protocol::new(Address::ZERO);
    let moe = MoeProtocol::new();

    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    let mut protocols = Vec::with_capacity(path.hops.len());
    let mut v3_crossings = TickCrossingBucket::Zero;
    let mut moe_crossings = BinCrossingBucket::Zero;
    let mut has_v3 = false;
    let mut has_moe = false;

    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let single_path = ArbitragePath {
            hops: vec![*hop],
        };
        let single_pools = [amm.clone()];
        let (hop_outs, hop_out, hop_key) = match protocol_kind_of_amm(amm) {
            ProtocolKind::V2 => {
                v2.simulate_path_with_route_key(&single_path, &single_pools, current, block_timestamp)?
            }
            ProtocolKind::V3 => {
                v3.simulate_path_with_route_key(&single_path, &single_pools, current, block_timestamp)?
            }
            ProtocolKind::Moe => {
                moe.simulate_path_with_route_key(&single_path, &single_pools, current, block_timestamp)?
            }
        };
        let kind = hop_key
            .protocols
            .first()
            .copied()
            .unwrap_or_else(|| protocol_kind_of_amm(amm));
        protocols.push(kind);
        if kind == ProtocolKind::V3 {
            has_v3 = true;
            if let Some(bucket) = hop_key.v3_tick_crossings {
                v3_crossings = max_tick_bucket(v3_crossings, bucket);
            }
        }
        if kind == ProtocolKind::Moe {
            has_moe = true;
            if let Some(bucket) = hop_key.moe_bin_crossings {
                moe_crossings = max_bin_bucket(moe_crossings, bucket);
            }
        }
        let out = hop_outs.last().copied().unwrap_or(hop_out);
        outputs.push(out);
        current = hop_out;
    }

    let mut route_key = RouteKey::new(protocols).map_err(|e| ProtocolError::RouteKey(e.to_string()))?;
    if has_v3 {
        route_key = route_key.with_v3_ticks(v3_crossings);
    }
    if has_moe {
        route_key = route_key.with_moe_bins(moe_crossings);
    }
    Ok((outputs, current, route_key))
}

fn max_tick_bucket(a: TickCrossingBucket, b: TickCrossingBucket) -> TickCrossingBucket {
    use TickCrossingBucket::*;
    match (a, b) {
        (High, _) | (_, High) => High,
        (Mid, _) | (_, Mid) => Mid,
        (Low, _) | (_, Low) => Low,
        _ => Zero,
    }
}

fn max_bin_bucket(a: BinCrossingBucket, b: BinCrossingBucket) -> BinCrossingBucket {
    use BinCrossingBucket::*;
    match (a, b) {
        (High, _) | (_, High) => High,
        (Mid, _) | (_, Mid) => Mid,
        (Low, _) | (_, Low) => Low,
        _ => Zero,
    }
}

/// Discover profitable closed settlement cycles over a **merged** multi-protocol pool set.
///
/// One-shot full scan: builds a fresh [`DiscoveryEngine`] and optimizes every
/// cycle ([`TipRefreshScope::Full`]). Prefer a long-lived engine on the watch
/// path so topology is built once and dirty cycles are optimized incrementally
/// (WHI-940).
pub fn discover_opportunities(
    pools: &[AMM],
    config: &DiscoveryConfig,
) -> Result<Vec<DiscoveredOpportunity>> {
    Ok(discover_pass(pools, config)?.opportunities)
}

/// Like [`discover_opportunities`] but also returns WHI-952 work counters.
pub fn discover_pass(pools: &[AMM], config: &DiscoveryConfig) -> Result<DiscoveryPass> {
    let (opportunities, stats) =
        discover_opportunities_with_scope(pools, config, &TipRefreshScope::Full)?;
    Ok(DiscoveryPass {
        opportunities,
        stats: stats.into(),
    })
}

/// Same as [`discover_opportunities`] with an explicit tip-refresh scope.
///
/// One-shot: builds a throwaway engine. Watch loops should hold a
/// [`DiscoveryEngine`] across blocks instead.
pub fn discover_opportunities_with_scope(
    pools: &[AMM],
    config: &DiscoveryConfig,
    scope: &TipRefreshScope,
) -> Result<(Vec<DiscoveredOpportunity>, crate::service::path_index::DiscoveryStats)> {
    if pools.is_empty() {
        return Ok((
            Vec::new(),
            crate::service::path_index::DiscoveryStats {
                cycles_total: 0,
                cycles_optimized: 0,
                dirty_pools: 0,
                amm_quotes: 0,
                gas_rescores: 0,
                scope: scope.as_metric_label(),
            },
        ));
    }
    let mut engine = DiscoveryEngine::build(pools, config.settlement_asset, config.max_hops)?;
    engine.discover(pools, config, scope)
}

/// Discover opportunities restricted to a protocol subset (for drift / negative tests).
pub fn discover_for_protocols(
    pools: &[AMM],
    selected: &[SelectedProtocol],
    config: &DiscoveryConfig,
) -> Result<Vec<DiscoveredOpportunity>> {
    let filtered = crate::service::select::filter_pools_by_protocols(pools, selected);
    discover_opportunities(&filtered, config)
}

/// Stable `protocol_mix` / attempt label for metrics (WHI-532).
pub(crate) fn protocol_mix_label(is_cross: bool, kinds: &[ProtocolKind]) -> &'static str {
    if is_cross {
        return "cross";
    }
    match kinds.first() {
        Some(ProtocolKind::V2) => "agni-v2",
        Some(ProtocolKind::V3) => "agni-v3",
        Some(ProtocolKind::Moe) => "moe",
        None => "unknown",
    }
}

/// Build factories for the selected protocols.
///
/// **Do not pass these to `StateSpaceBuilder` on the live bot path (WHI-784).**
/// Factory wiring triggers historical `Factory::discover` and runtime pool
/// auto-add from creation logs, both of which violate the frozen pool-universe
/// invariant. Live mode loads AMMs from committed CSV lists only.
///
/// Kept for offline tooling / legacy single-protocol examples that intentionally
/// discover; the multi-protocol `bot` binary must not call this for sync.
///
/// `v3_factories` is a **set** of UniV3-family factory addresses (WHI-910).
/// `SelectedProtocol::AgniV3` is the shared math family — one entry is emitted
/// per factory so CREATE2 / deployer identity stays per-venue. Pass
/// [`crate::service::drop_in_v3_factories`] for the loadable WHI-765/WHI-938 drop-ins.
/// An empty slice emits no V3 factory (caller must supply the set explicitly).
pub fn factories_for_selection(
    selected: &[SelectedProtocol],
    v2_factory: Address,
    v3_factories: &[Address],
    moe_factory: Address,
    moe_creation_block: u64,
) -> Vec<crate::amms::factory::Factory> {
    use crate::service::protocol::{AgniV2Protocol, AgniV3Protocol, MoeProtocol, Protocol};

    let mut out = Vec::new();
    for s in selected {
        match s {
            SelectedProtocol::AgniV2 => {
                out.push(AgniV2Protocol::new(v2_factory).factory());
            }
            SelectedProtocol::AgniV3 => {
                for &factory in v3_factories {
                    out.push(AgniV3Protocol::new(factory).factory());
                }
            }
            SelectedProtocol::Moe => {
                out.push(
                    MoeProtocol::new()
                        .with_factory(moe_factory, moe_creation_block)
                        .factory(),
                );
            }
        }
    }
    out
}

/// Assert the production-send gate remains closed (default / signerless path).
///
/// When `--enable-sends` arms the gate (WHI-860), callers must skip this check
/// and instead rely on arm-time fail-closed preconditions.
pub fn assert_signerless_invariant() -> Result<()> {
    if crate::service::startup::production_send_allowed() {
        return Err(eyre!(
            "production_send_allowed() is true but signerless invariant was asserted \
             (omit assert_signerless_invariant when the send path is armed — WHI-860)"
        ));
    }
    Ok(())
}

/// Optional head-observation context for block-to-submit latency (WHI-532).
#[derive(Debug, Clone, Copy)]
pub struct AttemptJobContext {
    pub observed_at: std::time::Instant,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

impl Default for AttemptJobContext {
    fn default() -> Self {
        Self {
            observed_at: std::time::Instant::now(),
            base_fee_per_gas: 0,
            block_gas_limit: 0,
        }
    }
}

/// Optional identity context for a real send (header + pool-universe fingerprint).
///
/// When omitted, the send path uses zero placeholders (valid only while the gate
/// is closed). Armed sends should supply the live tip identity.
#[derive(Debug, Clone, Copy)]
pub struct AttemptIdentityContext {
    pub header: crate::state_space::BlockHeaderContext,
    pub pool_universe_fingerprint: B256,
}

impl Default for AttemptIdentityContext {
    fn default() -> Self {
        Self {
            header: crate::state_space::BlockHeaderContext::new(B256::ZERO, 0),
            pool_universe_fingerprint: B256::ZERO,
        }
    }
}

/// Run a discovered candidate through the shared job-slot + `Protocol::attempt_execution`
/// (or mixed-path gate-closed path) so the bot exercises `service::block_loop` primitives.
///
/// Pure-protocol candidates dispatch to the owning [`Protocol`] impl when the send
/// gate is closed. When the gate is armed and `send` is `Some`, pure-protocol
/// candidates are submitted via [`crate::service::send_path::SendRuntime`].
/// Mixed-protocol candidates remain gate-blocked / refused (canary is one pure path).
pub async fn attempt_discovered_via_job_slot(
    opp: &DiscoveredOpportunity,
    block_timestamp: u64,
    job_ctx: AttemptJobContext,
) -> Result<ExecutionAttempt> {
    attempt_discovered_via_job_slot_with_send(
        opp,
        block_timestamp,
        job_ctx,
        None,
        AttemptIdentityContext::default(),
        None,
    )
    .await
}

/// Result of walking the WHI-951 attempt plan for one head / one-shot pass.
#[derive(Debug, Default)]
pub struct AttemptWalkResult {
    pub attempts: Vec<(DiscoveredOpportunity, ExecutionAttempt)>,
    /// Set when no typed attempt was recorded but the walk ended for a counted reason.
    pub outcome_override: Option<&'static str>,
}

/// Walk the eligibility-aware attempt plan (WHI-951).
///
/// * Gate **closed**: historical top-1; errors propagate.
/// * Gate **armed**: try statically eligible candidates under `budget`; dynamic
///   `Err` advances without a per-candidate info log; stop on
///   [`ExecutionAttempt::Submitted`].
///
/// `pinned_balance` is the WHI-950 strategy-A pin for this head (reused by send).
pub async fn walk_attempt_plan(
    opportunities: &[DiscoveredOpportunity],
    eligibility: &crate::service::eligibility::EligibilityView,
    production_send_armed: bool,
    budget: std::time::Duration,
    block_timestamp: u64,
    job_ctx: AttemptJobContext,
    send: Option<&crate::service::send_path::SendRuntime>,
    identity: AttemptIdentityContext,
    pinned_balance: Option<crate::state_space::SnapshotBoundBalance>,
) -> Result<AttemptWalkResult> {
    use crate::service::eligibility::{
        candidates_for_attempt, next_attempt_decision, AttemptBudget, AttemptSelectionOutcome,
    };
    use tracing::debug;

    let plan = candidates_for_attempt(opportunities, eligibility, production_send_armed);
    let mut out = AttemptWalkResult::default();

    if !production_send_armed {
        if let Some(best) = plan.first().copied() {
            debug!(
                target: "service.eligibility",
                signature = %best.candidate.signature,
                "dispatching top-1 candidate (gate closed)"
            );
            let attempt = attempt_discovered_via_job_slot_with_send(
                best,
                block_timestamp,
                job_ctx,
                send,
                identity,
                pinned_balance,
            )
            .await
            .context("attempt_discovered_via_job_slot")?;
            out.attempts.push((best.clone(), attempt));
        }
        return Ok(out);
    }

    let budget = AttemptBudget::from_now(budget);
    let mut tried = 0u64;
    let mut next_idx = 0usize;
    loop {
        match next_attempt_decision(plan.len(), next_idx, tried, &budget) {
            AttemptSelectionOutcome::NoCandidate => break,
            AttemptSelectionOutcome::BudgetExhausted { tried: t } => {
                debug!(
                    target: "service.eligibility",
                    tried = t,
                    "attempt budget exhausted; abandoning block (WHI-951)"
                );
                out.outcome_override = Some("budget_exhausted");
                break;
            }
            AttemptSelectionOutcome::ExhaustedEligible { tried: t } => {
                debug!(
                    target: "service.eligibility",
                    tried = t,
                    "no further eligible candidates after dynamic failures (WHI-951)"
                );
                if out.attempts.is_empty() && t > 0 {
                    out.outcome_override = Some("dynamic_exhausted");
                }
                break;
            }
            AttemptSelectionOutcome::Try { index_in_plan } => {
                let cand = plan[index_in_plan];
                next_idx = index_in_plan + 1;
                tried += 1;
                debug!(
                    target: "service.eligibility",
                    signature = %cand.candidate.signature,
                    plan_index = index_in_plan,
                    "dispatching eligible candidate (armed)"
                );
                match attempt_discovered_via_job_slot_with_send(
                    cand,
                    block_timestamp,
                    job_ctx,
                    send,
                    identity,
                    pinned_balance,
                )
                .await
                {
                    Ok(attempt) => {
                        // Any Ok ends the walk: Submitted is the success stop;
                        // ProductionGateBlocked while armed is unexpected and
                        // must not burn further budget on more candidates.
                        out.attempts.push((cand.clone(), attempt));
                        break;
                    }
                    Err(e) => {
                        // Dynamic preflight failure — advance if budget remains.
                        // debug only (G-5: no per-skipped-candidate info line).
                        debug!(
                            target: "service.eligibility",
                            plan_index = index_in_plan,
                            error = %e,
                            "dynamic preflight failed; considering next eligible (WHI-951)"
                        );
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Same as [`attempt_discovered_via_job_slot`] with an optional armed [`SendRuntime`].
///
/// `pinned_balance` is the WHI-950 strategy-A pin for this head.
pub async fn attempt_discovered_via_job_slot_with_send(
    opp: &DiscoveredOpportunity,
    block_timestamp: u64,
    job_ctx: AttemptJobContext,
    send: Option<&crate::service::send_path::SendRuntime>,
    identity: AttemptIdentityContext,
    pinned_balance: Option<crate::state_space::SnapshotBoundBalance>,
) -> Result<ExecutionAttempt> {
    use crate::service::block_loop::{new_job_slot, ExecutionJob};
    use crate::service::protocol::{
        AgniV2Protocol, AgniV3Protocol, ExecutionAttempt, MoeProtocol, Protocol,
        ServiceExecutionContext,
    };
    use crate::state_space::BlockHeaderContext;

    if job_ctx.base_fee_per_gas > 0 {
        crate::metrics::record_gas_base_fee(job_ctx.base_fee_per_gas);
    }
    let header = if identity.header.block_timestamp == 0 && block_timestamp != 0 {
        BlockHeaderContext::new(identity.header.parent_hash, block_timestamp)
    } else if identity.header.block_timestamp == 0 {
        BlockHeaderContext::new(B256::ZERO, block_timestamp)
    } else {
        identity.header
    };
    let slot = new_job_slot::<ExecutionJob<crate::service::protocol::Candidate>>();
    slot.publish(ExecutionJob {
        candidate: opp.candidate.clone(),
        block_number: opp.candidate.snapshot_id.block_number,
        header,
        pool_universe_fingerprint: identity.pool_universe_fingerprint,
        base_fee_per_gas: job_ctx.base_fee_per_gas,
        block_gas_limit: job_ctx.block_gas_limit,
        observed_at: job_ctx.observed_at,
    });
    let job = slot
        .take()
        .ok_or_else(|| eyre!("job slot lost the published candidate"))?;

    let protocol_label = protocol_mix_label(opp.is_cross_protocol, &opp.protocol_kinds);

    // Re-validate every hop through the owning Protocol (mixed or pure).
    let _ = simulate_mixed_path_with_route_key(
        &job.candidate.path,
        &job.candidate.pools,
        job.candidate.input,
        block_timestamp,
    )?;

    // Armed send path (WHI-860): pure-protocol only, requires SendRuntime.
    if crate::service::startup::production_send_allowed() {
        let Some(runtime) = send else {
            return Err(eyre!(
                "production_send_allowed but no SendRuntime was provided (fail closed)"
            ));
        };
        if opp.is_cross_protocol {
            return Err(eyre!("production send path not enabled for mixed routes"));
        }
        let attempt = runtime
            .submit_opportunity(
                opp,
                block_timestamp,
                job_ctx,
                job.header,
                job.pool_universe_fingerprint,
                pinned_balance,
            )
            .await
            .context("SendRuntime::submit_opportunity")?;
        record_attempt_outcome(protocol_label, &attempt, job.observed_at);
        return Ok(attempt);
    }

    // Pure-protocol candidates also exercise Protocol::attempt_execution.
    // Mixed candidates cannot call a single Protocol::attempt_execution (each
    // impl's simulate_path assumes homogeneous hops), so after hop-level Protocol
    // dispatch above they share the same fail-closed gate outcome.
    if !opp.is_cross_protocol {
        let kind = opp
            .protocol_kinds
            .first()
            .copied()
            .ok_or_else(|| eyre!("pure candidate missing protocol kind"))?;
        let ctx = ServiceExecutionContext::MonitorOnly;
        let attempt = match kind {
            ProtocolKind::V2 => AgniV2Protocol::new(Address::ZERO)
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}"))?,
            ProtocolKind::V3 => AgniV3Protocol::new(Address::ZERO)
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}"))?,
            ProtocolKind::Moe => MoeProtocol::new()
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}"))?,
        };
        record_attempt_outcome(protocol_label, &attempt, job.observed_at);
        return Ok(attempt);
    }

    let attempt = ExecutionAttempt::ProductionGateBlocked {
        amount_in: job.candidate.input,
        min_profit: job.candidate.net_profit,
    };
    record_attempt_outcome(protocol_label, &attempt, job.observed_at);
    Ok(attempt)
}

fn record_attempt_outcome(
    protocol: &str,
    attempt: &crate::service::protocol::ExecutionAttempt,
    observed_at: std::time::Instant,
) {
    use crate::metrics::block_outcome;
    use crate::service::protocol::ExecutionAttempt;
    let outcome = match attempt {
        ExecutionAttempt::ProductionGateBlocked { .. } => block_outcome::GATE_BLOCKED,
        ExecutionAttempt::Submitted(_) => block_outcome::SUBMITTED,
    };
    crate::metrics::record_block_to_submit(protocol, outcome, observed_at.elapsed());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::pathfinder::{PathConstraints, DEFAULT_MAX_HOPS};
    use crate::service::fixture::{cross_protocol_fixture_pools, fixture_settlement_asset};
    use crate::state_space::snapshot::EFFECTIVE_MAX_HOPS;
    // PathConstraints still needed for strategy_max_hops_defaults_aligned.

    #[test]
    fn strategy_max_hops_defaults_aligned() {
        // WHI-529: pathfinder, discovery, and pool-universe fingerprint cap must not drift.
        let settlement = fixture_settlement_asset();
        assert_eq!(DEFAULT_MAX_HOPS, 3);
        assert_eq!(PathConstraints::default().max_length, DEFAULT_MAX_HOPS);
        assert_eq!(
            DiscoveryConfig::for_settlement(settlement).max_hops,
            DEFAULT_MAX_HOPS
        );
        assert_eq!(EFFECTIVE_MAX_HOPS as usize, DEFAULT_MAX_HOPS);
    }

    #[test]
    fn validate_max_hops_rejects_zero_and_over_cap() {
        assert!(validate_max_hops(0, false).is_err());
        assert!(validate_max_hops(4, false).is_err());
        let err = validate_max_hops(4, false).unwrap_err().to_string();
        assert!(
            err.contains("ARB_PATHS_MANTLE") && err.contains("3"),
            "unexpected: {err}"
        );
        assert!(validate_max_hops(3, false).is_ok());
        assert!(validate_max_hops(4, true).is_ok());
    }

    #[test]
    fn mixed_discovery_finds_cross_protocol_cycle() {
        let pools = cross_protocol_fixture_pools();
        let config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        let found = discover_opportunities(&pools, &config).expect("discover");
        assert!(
            found.iter().any(|o| o.is_cross_protocol),
            "expected at least one cross-protocol opportunity; found={}",
            found.len()
        );
    }

    #[test]
    fn single_protocol_subsets_find_no_cycle() {
        let pools = cross_protocol_fixture_pools();
        let config = DiscoveryConfig::offline_default(fixture_settlement_asset());
        for proto in SelectedProtocol::all() {
            let found =
                discover_for_protocols(&pools, &[proto], &config).expect("subset discover");
            assert!(
                found.is_empty(),
                "single-protocol {proto} must not discover the cross-protocol-only fixture; got {}",
                found.len()
            );
        }
    }

    #[test]
    fn production_gate_stays_closed() {
        assert_signerless_invariant().unwrap();
    }

    #[test]
    fn factories_for_selection_emits_one_entry_per_v3_factory() {
        use alloy::primitives::address;
        use crate::amms::factory::Factory;
        use crate::service::drop_in_v3_factories;

        let v3 = drop_in_v3_factories();
        // WHI-938: Cleopatra CL quarantined — six loadable drop-ins.
        assert_eq!(v3.len(), 6);
        let factories = factories_for_selection(
            &[SelectedProtocol::AgniV3],
            address!("00000000000000000000000000000000000000f2"),
            &v3,
            address!("00000000000000000000000000000000000000f3"),
            1,
        );
        assert_eq!(factories.len(), 6);
        let mut seen = std::collections::HashSet::new();
        for f in &factories {
            match f {
                Factory::AgniFactory(af) => {
                    assert!(
                        seen.insert(af.address),
                        "duplicate factory {:?}",
                        af.address
                    );
                    assert!(v3.contains(&af.address));
                }
                other => panic!("expected AgniFactory, got {other:?}"),
            }
        }
    }

    #[test]
    fn factories_for_selection_empty_v3_emits_none() {
        use alloy::primitives::address;
        let factories = factories_for_selection(
            &[SelectedProtocol::AgniV3, SelectedProtocol::AgniV2],
            address!("00000000000000000000000000000000000000f2"),
            &[],
            address!("00000000000000000000000000000000000000f3"),
            1,
        );
        // Only V2 — no V3 when the set is empty.
        assert_eq!(factories.len(), 1);
    }
}
