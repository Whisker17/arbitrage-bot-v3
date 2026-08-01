//! Multi-protocol opportunity discovery (WHI-728 / WHI-527.3).
//!
//! One merged pool set → one `build_graph` / `PathFinder` pass. Per-hop dispatch
//! to the owning protocol only happens *after* a concrete path is found
//! (simulation + route-key construction). This is the mechanism that enables
//! cross-DEX cycles (V2 hop + Agni hop + Moe hop in one path).

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::arbitrage::graph::build_graph;
use crate::arbitrage::optimizer::{pools_for_path, OptimizationConfig, PathOptimizer};
use crate::arbitrage::pathfinder::{ArbitragePath, PathConstraints, PathFinder};
use crate::execution::{BinCrossingBucket, ProtocolKind, RouteKey, TickCrossingBucket};
use crate::service::error::ProtocolError;
use crate::service::gas::{default_gas_safety_margin, GasConfig};
use crate::service::protocol::{Candidate, ExecutionAttempt};
use crate::service::select::{protocol_kind_of_amm, SelectedProtocol};
use crate::service::shadow_row::{
    collect_expected_states, format_roi_percent, hops_description,
};
use crate::state_space::{SnapshotId, StateSpace};
use alloy::primitives::{Address, B256, I256, U256};
use eyre::{eyre, Context, Result};

/// Knobs for a single multi-protocol discovery pass.
#[derive(Debug, Clone)]
pub struct DiscoveryConfig {
    pub settlement_asset: Address,
    pub max_hops: usize,
    pub min_profit: U256,
    pub max_input: U256,
    pub gas: GasConfig,
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
            max_hops: 3,
            min_profit: U256::ZERO,
            max_input: U256::from(10u128.pow(21)),
            gas: GasConfig::default(),
            block_timestamp: 1_700_000_000,
            snapshot_id: SnapshotId::new(5000, 1, B256::ZERO),
        }
    }

    /// Alias used by offline fixture tests.
    pub fn offline_default(settlement_asset: Address) -> Self {
        Self::for_settlement(settlement_asset)
    }
}

/// One opportunity discovered on the merged multi-protocol graph.
#[derive(Debug, Clone)]
pub struct DiscoveredOpportunity {
    pub candidate: Candidate,
    pub route_key: RouteKey,
    pub is_cross_protocol: bool,
    pub protocol_kinds: Vec<ProtocolKind>,
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
/// Runs a single `build_graph` + `find_cycles` + optimize pass — not one pass per protocol.
pub fn discover_opportunities(
    pools: &[AMM],
    config: &DiscoveryConfig,
) -> Result<Vec<DiscoveredOpportunity>> {
    if pools.is_empty() {
        return Ok(Vec::new());
    }

    let mut state = StateSpace::default();
    for pool in pools {
        state.state.insert(pool.address(), pool.clone());
    }

    let graph = build_graph(&state).context("building multi-protocol pool graph")?;
    let constraints = PathConstraints {
        max_length: config.max_hops,
        required_start_token: Some(config.settlement_asset),
        required_end_token: Some(config.settlement_asset),
        ..PathConstraints::default()
    };
    let finder = PathFinder::new(&graph, constraints);
    let paths = finder.find_cycles();

    let optimizer = PathOptimizer::new(OptimizationConfig {
        min_profit: config.min_profit,
        max_input: config.max_input,
        ..OptimizationConfig::default()
    });

    let mut found = Vec::new();
    for path in &paths {
        let path_pools = match pools_for_path(path, pools) {
            Ok(p) => p,
            Err(_) => continue,
        };

        // Gross-profit size via the protocol-agnostic hop simulator first
        // (works across AMM variants without crossing evidence).
        let Some(opt) = optimizer.optimize(path, &path_pools)? else {
            continue;
        };
        if opt.expected_profit.is_zero() {
            continue;
        }

        // Mixed-protocol route key + hop outputs (crossing evidence).
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
                continue;
            }
        };

        let gross = match final_out.checked_sub(opt.optimal_input) {
            Some(g) if !g.is_zero() => g,
            _ => continue,
        };

        let hops = path.hops.len();
        // WHI-729: gross-quote screening uses the shared safety-margin helper
        // (never a hardcoded 1.2 literal).
        if !config
            .gas
            .is_profitable_after_gas(gross, hops, default_gas_safety_margin())
        {
            continue;
        }
        let Some(net_profit) = config.gas.net_profit(gross, hops) else {
            continue;
        };

        let is_cross = path_is_cross_protocol(&path_pools);
        let protocol_kinds: Vec<ProtocolKind> = path_pools.iter().map(protocol_kind_of_amm).collect();

        let mut token_path: Vec<Address> = path.hops.iter().map(|h| h.token_in).collect();
        if let Some(last) = path.hops.last() {
            token_path.push(last.token_out);
        }

        let signature = path_signature(path, &protocol_kinds);
        let profit = I256::from_raw(gross);
        let expected_states = match collect_expected_states(&path_pools) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(
                    target: "bot.discovery",
                    error = %e,
                    "expected_states collection failed; skipping path"
                );
                continue;
            }
        };
        let log_hops = hops_description(path);
        let roi =
            format_roi_percent(profit, opt.optimal_input).unwrap_or_else(|| "-".to_string());
        let candidate = Candidate {
            snapshot_id: config.snapshot_id,
            signature,
            hops,
            input: opt.optimal_input,
            output: final_out,
            profit,
            net_profit,
            pool_addresses: path.hops.iter().map(|h| h.pool_address).collect(),
            token_path,
            amounts_out,
            expected_states,
            path: path.clone(),
            pools: path_pools,
            log_hops,
            roi,
        };

        found.push(DiscoveredOpportunity {
            candidate,
            route_key,
            is_cross_protocol: is_cross,
            protocol_kinds,
        });
    }

    // Highest net profit first.
    found.sort_by(|a, b| b.candidate.net_profit.cmp(&a.candidate.net_profit));
    Ok(found)
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

/// Build factories for the selected protocols (for `StateSpaceBuilder` live wiring).
pub fn factories_for_selection(
    selected: &[SelectedProtocol],
    v2_factory: Address,
    v3_factory: Address,
    moe_factory: Address,
    moe_creation_block: u64,
) -> Vec<crate::amms::factory::Factory> {
    use crate::service::protocol::{AgniV2Protocol, AgniV3Protocol, MoeProtocol, Protocol};

    selected
        .iter()
        .map(|s| match s {
            SelectedProtocol::AgniV2 => AgniV2Protocol::new(v2_factory).factory(),
            SelectedProtocol::AgniV3 => AgniV3Protocol::new(v3_factory).factory(),
            SelectedProtocol::Moe => MoeProtocol::new()
                .with_factory(moe_factory, moe_creation_block)
                .factory(),
        })
        .collect()
}

/// Assert the production-send gate remains closed (invariant for this PR).
pub fn assert_signerless_invariant() -> Result<()> {
    if crate::service::startup::production_send_allowed() {
        return Err(eyre!(
            "production_send_allowed() must remain false in the multi-protocol bot (WHI-728)"
        ));
    }
    Ok(())
}

/// Run a discovered candidate through the shared job-slot + `Protocol::attempt_execution`
/// (or mixed-path gate-closed path) so the bot exercises `service::block_loop` primitives.
///
/// Pure-protocol candidates dispatch to the owning [`Protocol`] impl. Mixed-protocol
/// candidates re-validate via [`simulate_mixed_path_with_route_key`] then return
/// [`ExecutionAttempt::ProductionGateBlocked`] while the send gate is closed.
pub async fn attempt_discovered_via_job_slot(
    opp: &DiscoveredOpportunity,
    block_timestamp: u64,
) -> Result<ExecutionAttempt> {
    use crate::service::block_loop::{new_job_slot, ExecutionJob};
    use crate::service::protocol::{
        AgniV2Protocol, AgniV3Protocol, ExecutionAttempt, MoeProtocol, Protocol,
        ServiceExecutionContext,
    };
    use crate::state_space::BlockHeaderContext;
    use alloy::primitives::B256;

    let slot = new_job_slot::<ExecutionJob<crate::service::protocol::Candidate>>();
    slot.publish(ExecutionJob {
        candidate: opp.candidate.clone(),
        block_number: opp.candidate.snapshot_id.block_number,
        header: BlockHeaderContext::new(B256::ZERO, block_timestamp),
        pool_universe_fingerprint: B256::ZERO,
        base_fee_per_gas: 0,
        block_gas_limit: 0,
    });
    let job = slot
        .take()
        .ok_or_else(|| eyre!("job slot lost the published candidate"))?;

    // Re-validate every hop through the owning Protocol (mixed or pure).
    let _ = simulate_mixed_path_with_route_key(
        &job.candidate.path,
        &job.candidate.pools,
        job.candidate.input,
        block_timestamp,
    )?;

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
        return match kind {
            ProtocolKind::V2 => AgniV2Protocol::new(Address::ZERO)
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}")),
            ProtocolKind::V3 => AgniV3Protocol::new(Address::ZERO)
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}")),
            ProtocolKind::Moe => MoeProtocol::new()
                .attempt_execution(&job.candidate, ctx)
                .await
                .map_err(|e| eyre!("{e}")),
        };
    }

    if !crate::service::startup::production_send_allowed() {
        return Ok(ExecutionAttempt::ProductionGateBlocked {
            amount_in: job.candidate.input,
            min_profit: job.candidate.net_profit,
        });
    }
    Err(eyre!("production send path not enabled for mixed routes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::fixture::{cross_protocol_fixture_pools, fixture_settlement_asset};

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
}
