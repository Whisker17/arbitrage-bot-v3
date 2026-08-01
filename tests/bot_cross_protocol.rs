//! WHI-728 acceptance: multi-protocol bot discovers cross-DEX cycles offline.
//!
//! 1. Merged agni-v2+agni-v3+moe fixture finds ≥1 cross-protocol cycle.
//! 2. Each single-protocol subset finds none (fixture is cross-protocol-only).
//! 3. Pure-protocol mixed simulator matches `Protocol::simulate_path_with_route_key`
//!    (old-service vs new-binary drift guard for same-protocol paths).
//! 4. `production_send_allowed()` stays hard-false.
//! 5. E2E: the `bot` binary offline path reports a cross-protocol opportunity.

use std::process::Command;

use alloy::primitives::{address, U256};
use amms::amms::amm::AMM;
use amms::amms::uniswap_v2::UniswapV2Pool;
use amms::amms::Token;
use amms::arbitrage::pathfinder::{ArbitragePath, PathHop};
use amms::service::{
    attempt_discovered_via_job_slot, cross_protocol_fixture_pools, discover_for_protocols,
    discover_opportunities, parse_protocols_flag, production_send_allowed,
    simulate_mixed_path_with_route_key, AgniV2Protocol, DiscoveryConfig, ExecutionAttempt,
    Protocol, SelectedProtocol, V2_FEE,
};

#[test]
fn multi_protocol_fixture_discovers_cross_protocol_cycle() {
    let selected = parse_protocols_flag("agni-v2,agni-v3,moe").unwrap();
    assert_eq!(selected, SelectedProtocol::all());

    let pools = cross_protocol_fixture_pools();
    let mut config = DiscoveryConfig::offline_default(amms::service::fixture_settlement_asset());
    config.gas.gas_price_wei = 0;

    let found = discover_opportunities(&pools, &config).expect("discover");
    assert!(
        !found.is_empty(),
        "merged multi-protocol run must find opportunities"
    );
    assert!(
        found.iter().any(|o| o.is_cross_protocol),
        "at least one opportunity must be cross-protocol; got {:?}",
        found
            .iter()
            .map(|o| (o.is_cross_protocol, o.protocol_kinds.clone()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn single_protocol_runs_cannot_discover_fixture_cycle() {
    let pools = cross_protocol_fixture_pools();
    let mut config = DiscoveryConfig::offline_default(amms::service::fixture_settlement_asset());
    config.gas.gas_price_wei = 0;

    for proto in SelectedProtocol::all() {
        let found = discover_for_protocols(&pools, &[proto], &config).expect("subset");
        assert!(
            found.is_empty(),
            "single-protocol {proto} must not discover the cross-protocol-only fixture; got {}",
            found.len()
        );
    }
}

#[test]
fn pure_v2_mixed_simulator_matches_protocol_impl() {
    // Drift guard: pure V2 path through mixed simulator == AgniV2Protocol.
    let token_a = address!("0000000000000000000000000000000000000001");
    let token_b = address!("0000000000000000000000000000000000000002");
    let pool_addr = address!("00000000000000000000000000000000000000a1");

    let mut pool = UniswapV2Pool::new(pool_addr, V2_FEE);
    pool.token_a = Token::new_with_decimals(token_a, 18);
    pool.token_b = Token::new_with_decimals(token_b, 18);
    pool.reserve_0 = 1_000_000_000_000_000_000_000;
    pool.reserve_1 = 2_000_000_000_000_000_000_000;
    let pools = vec![AMM::UniswapV2Pool(pool)];

    let path = ArbitragePath {
        hops: vec![PathHop {
            pool_address: pool_addr,
            token_in: token_a,
            token_out: token_b,
            fee_bps: 30,
        }],
    };
    let amount_in = U256::from(10u128.pow(18));

    let proto = AgniV2Protocol::new(address!("0000000000000000000000000000000000000f01"));
    let (p_outs, p_final, p_rk) = proto
        .simulate_path_with_route_key(&path, &pools, amount_in, 0)
        .expect("protocol simulate");
    let (m_outs, m_final, m_rk) =
        simulate_mixed_path_with_route_key(&path, &pools, amount_in, 0).expect("mixed simulate");

    assert_eq!(p_outs, m_outs);
    assert_eq!(p_final, m_final);
    assert_eq!(p_rk.protocols, m_rk.protocols);
    assert_eq!(p_rk.hop_count, m_rk.hop_count);
}

#[test]
fn pure_v3_mixed_simulator_matches_protocol_impl() {
    use amms::service::AgniV3Protocol;
    use amms::service::fixture::fixture_agni_pool;

    let pools = vec![fixture_agni_pool()];
    let amm = &pools[0];
    let tokens = match amm {
        AMM::AgniPool(p) => (p.token_a.address, p.token_b.address, p.address),
        _ => panic!("expected Agni pool"),
    };
    let path = ArbitragePath {
        hops: vec![PathHop {
            pool_address: tokens.2,
            token_in: tokens.0,
            token_out: tokens.1,
            fee_bps: 30,
        }],
    };
    let amount_in = U256::from(10u128.pow(15));
    let proto = AgniV3Protocol::new(address!("0000000000000000000000000000000000000f02"));
    let (p_outs, p_final, p_rk) = proto
        .simulate_path_with_route_key(&path, &pools, amount_in, 0)
        .expect("v3 protocol");
    let (m_outs, m_final, m_rk) =
        simulate_mixed_path_with_route_key(&path, &pools, amount_in, 0).expect("v3 mixed");
    assert_eq!(p_outs, m_outs);
    assert_eq!(p_final, m_final);
    assert_eq!(p_rk.protocols, m_rk.protocols);
}

#[test]
fn pure_moe_mixed_simulator_matches_protocol_impl() {
    use amms::service::fixture::fixture_moe_pool;
    use amms::service::MoeProtocol;

    let pools = vec![fixture_moe_pool()];
    let (token_in, token_out, pool_addr) = match &pools[0] {
        AMM::MoeLbPair(p) => (p.token_x.address, p.token_y.address, p.address),
        _ => panic!("expected Moe pool"),
    };
    let path = ArbitragePath {
        hops: vec![PathHop {
            pool_address: pool_addr,
            token_in,
            token_out,
            fee_bps: 0,
        }],
    };
    let amount_in = U256::from(1_000_000u64);
    let ts = 1_700_000_000u64;
    let proto = MoeProtocol::new();
    let (p_outs, p_final, p_rk) = proto
        .simulate_path_with_route_key(&path, &pools, amount_in, ts)
        .expect("moe protocol");
    let (m_outs, m_final, m_rk) =
        simulate_mixed_path_with_route_key(&path, &pools, amount_in, ts).expect("moe mixed");
    assert_eq!(p_outs, m_outs);
    assert_eq!(p_final, m_final);
    assert_eq!(p_rk.protocols, m_rk.protocols);
}

#[test]
fn production_send_stays_closed() {
    assert!(!production_send_allowed());
}

#[test]
fn parse_protocols_default_all_three() {
    let p = parse_protocols_flag("").unwrap();
    assert_eq!(p.len(), 3);
    assert!(p.contains(&SelectedProtocol::AgniV2));
    assert!(p.contains(&SelectedProtocol::AgniV3));
    assert!(p.contains(&SelectedProtocol::Moe));
}

#[tokio::test]
async fn job_slot_attempt_blocks_production_send() {
    let pools = cross_protocol_fixture_pools();
    let mut config = DiscoveryConfig::for_settlement(amms::service::fixture_settlement_asset());
    config.gas.gas_price_wei = 0;
    let found = discover_opportunities(&pools, &config).expect("discover");
    let best = found.first().expect("cross-protocol opportunity");
    let attempt = attempt_discovered_via_job_slot(best, config.block_timestamp)
        .await
        .expect("attempt");
    assert!(matches!(
        attempt,
        ExecutionAttempt::ProductionGateBlocked { .. }
    ));
}

/// End-to-end: actually run the `bot` binary against the offline fixture and
/// assert the report contains a cross-protocol opportunity (WHI-728 AC).
#[test]
fn bot_binary_offline_reports_cross_protocol_opportunity() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO_BIN_EXE_bot"))
        .current_dir(manifest_dir)
        .args([
            "--offline",
            "--protocols",
            "agni-v2,agni-v3,moe",
        ])
        .output()
        .expect("spawn bot binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bot --offline failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    assert!(
        stdout.contains("cross_protocol_opportunities: ")
            && !stdout.contains("cross_protocol_opportunities: 0"),
        "binary report missing positive cross-protocol count:\n{stdout}"
    );
    assert!(
        stdout.contains("production_send_allowed: false"),
        "binary must print hard-false production send gate:\n{stdout}"
    );
    assert!(
        stdout.contains("protocols: agni-v2,agni-v3,moe"),
        "binary must echo selected protocols:\n{stdout}"
    );
}
