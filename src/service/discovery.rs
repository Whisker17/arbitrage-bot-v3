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
use crate::service::gas::GasConfig;
use crate::service::protocol::Candidate;
use crate::service::select::{protocol_kind_of_amm, SelectedProtocol};
use crate::state_space::{SnapshotId, StateSpace};
use alloy::primitives::{Address, B256, U256};
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
    pub fn offline_default(settlement_asset: Address) -> Self {
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

/// Per-hop mixed-protocol simulation (the e2e_run pattern, extended to Moe).
///
/// Dispatches each hop to the AMM-native math / crossing-evidence path so a
/// single cycle can combine V2, Agni-V3, and Moe hops.
pub fn simulate_mixed_path_with_route_key(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
    block_timestamp: u64,
) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError> {
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

    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    let mut protocols = Vec::with_capacity(path.hops.len());
    let mut v3_crossings = 0u32;
    let mut moe_crossings = 0u32;
    let mut has_v3 = false;
    let mut has_moe = false;

    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        match amm {
            AMM::UniswapV2Pool(pool) => {
                protocols.push(ProtocolKind::V2);
                let out = pool
                    .simulate_swap(hop.token_in, hop.token_out, current)
                    .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
                outputs.push(out);
                current = out;
            }
            AMM::AgniPool(pool) => {
                protocols.push(ProtocolKind::V3);
                has_v3 = true;
                let evidence = pool
                    .simulate_swap_with_crossing_evidence(hop.token_in, current)
                    .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
                v3_crossings = v3_crossings.saturating_add(evidence.crossing_count);
                outputs.push(evidence.amount_out);
                current = evidence.amount_out;
            }
            AMM::UniswapV3Pool(pool) => {
                protocols.push(ProtocolKind::V3);
                has_v3 = true;
                let evidence = pool
                    .simulate_swap_with_crossing_evidence(hop.token_in, hop.token_out, current)
                    .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
                v3_crossings = v3_crossings.saturating_add(evidence.crossing_count);
                outputs.push(evidence.amount_out);
                current = evidence.amount_out;
            }
            AMM::MoeLbPair(pool) => {
                protocols.push(ProtocolKind::Moe);
                has_moe = true;
                let swap_for_y = hop.token_in == pool.token_x.address;
                let evidence = pool
                    .simulate_swap_with_crossing_evidence(swap_for_y, current, block_timestamp)
                    .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
                moe_crossings = moe_crossings.saturating_add(evidence.crossing_count);
                outputs.push(evidence.amount_out);
                current = evidence.amount_out;
            }
        }
    }

    let mut route_key = RouteKey::new(protocols.clone())
        .map_err(|e| ProtocolError::RouteKey(e.to_string()))?;
    if has_v3 {
        route_key = route_key.with_v3_ticks(TickCrossingBucket::from_crossings(v3_crossings));
    }
    if has_moe {
        route_key = route_key.with_moe_bins(BinCrossingBucket::from_crossings(moe_crossings));
    }
    Ok((outputs, current, route_key))
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
        let candidate = Candidate {
            snapshot_id: config.snapshot_id,
            signature,
            hops,
            input: opt.optimal_input,
            output: final_out,
            net_profit,
            pool_addresses: path.hops.iter().map(|h| h.pool_address).collect(),
            token_path,
            amounts_out,
            path: path.clone(),
            pools: path_pools,
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
