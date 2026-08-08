//! WHI-728 acceptance: multi-protocol bot discovers cross-DEX cycles offline.
//! WHI-739: shadow ledger emission from the merged binary (library path).
//!
//! 1. Merged agni-v2+agni-v3+moe fixture finds ≥1 cross-protocol cycle.
//! 2. Each single-protocol subset finds none (fixture is cross-protocol-only).
//! 3. Pure-protocol mixed simulator matches `Protocol::simulate_path_with_route_key`
//!    (old-service vs new-binary drift guard for same-protocol paths).
//! 4. `production_send_allowed()` stays closed by default (WHI-860 gate).
//! 5. E2E: the `bot` binary offline path reports a cross-protocol opportunity.
//! 6. Shadow ledger records run_header + ProductionGateBlocked candidate and
//!    round-trips through the shadow_report reader.
//! 7. `--offline --ledger` is rejected (fixture corpus pollution guard).

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::transports::mock::Asserter;
use amms::amms::amm::AMM;
use amms::amms::uniswap_v2::UniswapV2Pool;
use amms::amms::Token;
use amms::arbitrage::pathfinder::{ArbitragePath, PathHop};
use amms::execution::{
    audit_bytes, ledger_header_service, BlockFeeContextCache, ExecutorConfig, RuntimeProfileConfig,
    ShadowConfigPaths, ShadowExecutionContext, ShadowLedgerSetup, ShadowOverrideTarget,
    ShadowPinnedConfig,
};
use amms::service::{
    attempt_discovered_via_job_slot, cross_protocol_fixture_pools, discover_for_protocols,
    discover_opportunities, parse_protocols_flag, production_send_allowed,
    simulate_mixed_path_with_route_key, AgniV2Protocol, AttemptJobContext, DiscoveryConfig,
    ExecutionAttempt, Protocol, SelectedProtocol, V2_FEE, MERGED_BOT_SHADOW_SERVICE,
};
use amms::state_space::{BlockHeaderContext, SnapshotId};

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
    let attempt = attempt_discovered_via_job_slot(best, config.block_timestamp, AttemptJobContext::default())
        .await
        .expect("attempt");
    assert!(matches!(
        attempt,
        ExecutionAttempt::ProductionGateBlocked { .. }
    ));
}

/// WHI-532: offline fixture emits discovery counters via `--metrics-dump`.
#[test]
fn bot_offline_dump_reports_discovery_metrics() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO_BIN_EXE_bot"))
        .current_dir(manifest_dir)
        .args([
            "--offline",
            "--no-metrics",
            "--metrics-dump",
            "--protocols",
            "agni-v2,agni-v3,moe",
        ])
        .output()
        .expect("spawn bot binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bot --offline --metrics-dump failed: status={:?}\nstdout={stdout}\nstderr={stderr}",
        output.status
    );
    let has_cross = stdout.lines().any(|l| {
        l.starts_with("arbbot_discovery_candidates_total{")
            && l.contains("protocol_mix=\"cross\"")
            && l
                .rsplit_once(' ')
                .map(|(_, v)| v.parse::<u64>().map(|n| n >= 1).unwrap_or(false))
                .unwrap_or(false)
    });
    assert!(
        has_cross,
        "missing arbbot_discovery_candidates_total{{protocol_mix=\"cross\"}} >= 1\n{stdout}"
    );
    let has_cycles = stdout.lines().any(|l| {
        l.starts_with("arbbot_discovery_cycles_found_total ")
            && l
                .rsplit_once(' ')
                .map(|(_, v)| v.parse::<u64>().map(|n| n >= 1).unwrap_or(false))
                .unwrap_or(false)
    });
    assert!(
        has_cycles,
        "missing arbbot_discovery_cycles_found_total >= 1\n{stdout}"
    );
}

/// WHI-937 / WHI-969: default-level offline logs must stay small, and ordinary
/// unprofitability / zero-output path deaths must not appear as WARN. A flood
/// here is the same class of bug that filled a VPS disk in ~minutes on live
/// discovery.
///
/// `tracing_subscriber::fmt` defaults to stdout (see `src/bin/bot.rs`), so the
/// budget applies to combined process output, not stderr alone.
#[test]
fn bot_offline_log_volume_stays_under_budget_at_default_level() {
    // Default filter is `info` (see bot.rs). Clear any ambient RUST_LOG so CI
    // and developer shells don't accidentally raise volume.
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO_BIN_EXE_bot"))
        .current_dir(manifest_dir)
        .env_remove("RUST_LOG")
        .args([
            "--offline",
            "--no-metrics",
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

    // Fixed budget at default log level. Offline fixture is tiny; if this
    // regresses toward multi-MB, unprofitable-path noise is back at INFO/WARN.
    const MAX_LOG_BYTES: usize = 256 * 1024;
    let combined_len = output.stdout.len().saturating_add(output.stderr.len());
    assert!(
        combined_len <= MAX_LOG_BYTES,
        "offline log output {} bytes exceeds {}-byte budget (WHI-937 log flood guard)\nstdout_head:\n{}\nstderr_head:\n{}",
        combined_len,
        MAX_LOG_BYTES,
        stdout.chars().take(2_000).collect::<String>(),
        stderr.chars().take(2_000).collect::<String>()
    );

    let combined = format!("{stdout}{stderr}");
    // Legacy WARN phrasing + TRACE-only ordinary path outcomes: none at default.
    let banned = [
        "Simulation failed to compute profit",
        "compute profit (underflow)",
        "path unprofitable",
        "Simulation produced zero output",
    ];
    for needle in banned {
        assert!(
            !combined.contains(needle),
            "default-level offline log must not emit ordinary-path noise ({needle}):\n{combined}"
        );
    }
    // Any remaining WARN on simulate.path is unexpected for the offline fixture.
    for line in combined.lines() {
        let is_warn = line.contains("WARN");
        let is_sim = line.contains("simulate.path");
        assert!(
            !(is_warn && is_sim),
            "unexpected simulate.path WARN at default level:\n{line}"
        );
    }
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
            "--no-metrics",
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
        "binary must print closed production send gate by default:\n{stdout}"
    );
    assert!(
        stdout.contains("protocols: agni-v2,agni-v3,moe"),
        "binary must echo selected protocols:\n{stdout}"
    );
}

/// WHI-860: `--enable-sends` is incompatible with offline fixture mode.
#[test]
fn bot_binary_rejects_enable_sends_with_offline() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output = Command::new(env!("CARGO_BIN_EXE_bot"))
        .current_dir(manifest_dir)
        .args(["--offline", "--enable-sends"])
        .output()
        .expect("spawn bot");
    assert!(
        !output.status.success(),
        "enable-sends + offline must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("enable-sends") || stderr.contains("--offline"),
        "stderr should name the conflict: {stderr}"
    );
}

/// WHI-739: `--offline --ledger` must fail closed (synthetic fixture data must
/// not enter a shadow gate corpus).
#[test]
fn bot_binary_rejects_offline_with_ledger() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let dir = tempfile::tempdir().expect("temp dir");
    let ledger = dir.path().join("should_not_exist.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_bot"))
        .current_dir(manifest_dir)
        .args([
            "--offline",
            "--ledger",
            ledger.to_str().expect("utf8 path"),
            "--protocols",
            "agni-v2,agni-v3,moe",
        ])
        .output()
        .expect("spawn bot binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "bot --offline --ledger must exit non-zero\nstdout={stdout}\nstderr={stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("--ledger is not supported with --offline")
            || combined.contains("not supported with --offline"),
        "error must explain offline+ledger rejection:\n{combined}"
    );
    assert!(
        !ledger.exists(),
        "rejected offline+ledger run must not create a ledger file"
    );
}

/// WHI-739: mock-provider path builds a shadow context, records a gate-blocked
/// attempt from real discovery, and the ledger deserializes via the same
/// reader `shadow_report` uses (`ledger_header_service` + `audit_bytes`).
#[tokio::test]
async fn shadow_ledger_round_trip_records_gate_blocked_attempt() {
    let pools = cross_protocol_fixture_pools();
    let mut config = DiscoveryConfig::for_settlement(amms::service::fixture_settlement_asset());
    config.gas.gas_price_wei = 0;
    let found = discover_opportunities(&pools, &config).expect("discover");
    let best = found
        .iter()
        .find(|o| o.is_cross_protocol)
        .expect("cross-protocol opportunity");
    let attempt = attempt_discovered_via_job_slot(best, config.block_timestamp, AttemptJobContext::default())
        .await
        .expect("attempt");
    let ExecutionAttempt::ProductionGateBlocked {
        amount_in,
        min_profit,
    } = attempt
    else {
        panic!("expected ProductionGateBlocked, got {attempt:?}");
    };

    let ledger_dir = tempfile::tempdir().expect("ledger temp dir");
    let ledger_path = ledger_dir.path().join("bot_shadow.jsonl");
    let context = build_shadow_context_for_test(&ledger_path, MERGED_BOT_SHADOW_SERVICE);

    context
        .record_canonical_observation(
            SnapshotId::new(5000, 1, alloy::primitives::B256::ZERO),
            BlockHeaderContext::new(alloy::primitives::B256::ZERO, 1_700_000_000),
        )
        .expect("observation row");
    context
        .record_production_gate_blocked(&best.candidate.signature, amount_in, min_profit)
        .expect("gate-blocked candidate row");

    let bytes = std::fs::read(&ledger_path).expect("read ledger");
    assert!(!bytes.is_empty(), "ledger must be non-empty");

    let service = ledger_header_service(ledger_path.to_str().unwrap_or("ledger"), &bytes)
        .expect("shadow_report reader must parse run_header.service");
    assert_eq!(service, MERGED_BOT_SHADOW_SERVICE);

    let audit = audit_bytes(&bytes).expect("ledger sequences must audit clean");
    assert!(
        audit.row_count >= 3,
        "expect run_header + observation + candidate, got {}",
        audit.row_count
    );

    let rows: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
        .expect("utf8")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| serde_json::from_str(line).expect("json row"))
        .collect();
    assert_eq!(rows[0]["row_type"], "run_header");
    assert_eq!(rows[0]["service"], MERGED_BOT_SHADOW_SERVICE);
    assert_eq!(rows[0]["send_capability"], "no_send");
    assert!(
        rows.iter().any(|r| r["row_type"] == "observation"),
        "must include observation row: {rows:?}"
    );
    let candidate = rows
        .iter()
        .find(|r| r["row_type"] == "candidate")
        .expect("must include candidate row");
    let detail = candidate["detail"].as_str().unwrap_or("");
    assert!(
        detail.contains("production_gate_blocked"),
        "candidate detail must record gate block: {detail}"
    );
    assert!(
        detail.contains(&best.candidate.signature),
        "candidate detail must include signature"
    );
}

/// WHI-741: multi-block watch path records observations at distinct heights so a
/// `--ledger --watch` run is demonstrably multi-block (library path; no live RPC).
#[tokio::test]
async fn multi_block_watch_ticks_record_distinct_heights_in_ledger() {
    use amms::amms::amm::AutomatedMarketMaker;
    use amms::service::{
        process_observed_head, WatchLoopConfig, WatchLoopState,
    };
    use amms::state_space::{
        MarketSnapshot, ObservedHead, ProtocolCoverage, SnapshotPublisher, StateSpace,
    };
    use alloy::primitives::B256;
    use alloy::rpc::types::{Filter, Log};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use tokio::sync::RwLock;

    let pools = cross_protocol_fixture_pools();
    let mut space = StateSpace::default();
    for amm in &pools {
        space.state.insert(amm.address(), amm.clone());
    }
    space.latest_block.store(10, Ordering::Relaxed);
    let latest_block = Arc::clone(&space.latest_block);
    let state = Arc::new(RwLock::new(space));
    let snapshots = SnapshotPublisher::new();
    snapshots
        .publish(MarketSnapshot::new(
            SnapshotId::new(5000, 10, B256::repeat_byte(0x10)),
            BlockHeaderContext::new(B256::repeat_byte(0x0f), 1_700_000_000),
            HashMap::new(),
            ProtocolCoverage::default(),
        ))
        .await;

    let loop_state = WatchLoopState::new(
        state,
        latest_block,
        snapshots,
        Filter::new(),
        5000,
    );
    let mut discovery = DiscoveryConfig::for_settlement(amms::service::fixture_settlement_asset());
    discovery.gas.gas_price_wei = 0;
    let config = WatchLoopConfig {
        discovery,
        selected: SelectedProtocol::all().to_vec(),
        attempt_execution: true,
        refresh_tip_state: false,
        http_tip_wait: amms::service::DEFAULT_HTTP_TIP_WAIT,
        skip_fatal_window: amms::service::DEFAULT_SKIP_FATAL_WINDOW,
        skip_ratio_window: amms::service::DEFAULT_SKIP_RATIO_WINDOW,
        skip_ratio_threshold: amms::service::DEFAULT_SKIP_RATIO_THRESHOLD,
        send_runtime: None,
        pool_universe_fingerprint: B256::ZERO,
        attempt_budget: amms::service::DEFAULT_ATTEMPT_BUDGET,
    };

    let ledger_dir = tempfile::tempdir().expect("ledger temp");
    let ledger_path = ledger_dir.path().join("watch_multi.jsonl");
    let shadow = build_shadow_context_for_test(&ledger_path, MERGED_BOT_SHADOW_SERVICE);

    let asserter = Asserter::new();
    for _ in 0..8 {
        asserter.push_success(&Vec::<Log>::new());
    }
    let http = ProviderBuilder::new()
        .connect_mocked_client(asserter)
        .erased();

    let mut heights = Vec::new();
    for (n, h, parent) in [
        (11u64, 0x11u8, 0x10u8),
        (12, 0x12, 0x11),
        (13, 0x13, 0x12),
    ] {
        let head = ObservedHead::new(
            5000,
            n,
            B256::repeat_byte(h),
            B256::repeat_byte(parent),
            1_700_000_000 + n,
        );
        let tick = process_observed_head(
            &http,
            &loop_state,
            &config,
            head,
            Some(25),
            30_000_000,
            !heights.is_empty(),
        )
        .await
        .expect("process")
        .tick
        .expect("tick");
        heights.push(tick.block_number);
        shadow
            .record_canonical_observation(tick.snapshot_id, tick.header)
            .expect("observation");
        for (opp, attempt) in &tick.attempts {
            if let ExecutionAttempt::ProductionGateBlocked {
                amount_in,
                min_profit,
            } = attempt
            {
                shadow
                    .record_production_gate_blocked(
                        &opp.candidate.signature,
                        *amount_in,
                        *min_profit,
                    )
                    .expect("gate-blocked row");
            }
        }
    }

    assert_eq!(heights, vec![11, 12, 13]);
    let bytes = std::fs::read(&ledger_path).expect("read ledger");
    let audit = audit_bytes(&bytes).expect("well-formed ledger");
    assert!(audit.row_count >= 1 + 3 + 3, "header + 3 obs + 3 attempts");

    let rows: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
        .expect("utf8")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect();
    // Wire shape: observation.snapshot_id.block_number (see ledger.rs unit tests).
    let obs_heights: std::collections::BTreeSet<u64> = rows
        .iter()
        .filter(|r| r["row_type"] == "observation")
        .filter_map(|r| r["snapshot_id"]["block_number"].as_u64())
        .collect();
    assert_eq!(
        obs_heights,
        [11u64, 12, 13].into_iter().collect(),
        "ledger must span three distinct block heights; rows={rows:?}"
    );
}

/// Wallet-free mock-provider context (mirrors `tests/pipeline_wiring.rs` /
/// `tests/shadow_runtime.rs` pattern). Zero RPC at construction.
///
/// Missing-thresholds fail-closed coverage lives in
/// `service::startup::tests::build_shadow_execution_context_requires_thresholds_path`
/// (unit) rather than duplicating env mutation here.
fn build_shadow_context_for_test(ledger_path: &PathBuf, service: &'static str) -> ShadowExecutionContext {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let gas_profiles = manifest_dir.join("config/gas_profiles");
    let identity_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(manifest_dir.join("config/executor_identity.json"))
            .expect("executor identity"),
    )
    .expect("identity json");
    let target = ShadowOverrideTarget {
        executor_contract: Address::repeat_byte(0xE0),
        wmnt_address: identity_json["wmnt"]
            .as_str()
            .expect("wmnt")
            .parse()
            .expect("wmnt address"),
    };
    let pinned = ShadowPinnedConfig::load(
        ShadowConfigPaths {
            artifact_dir: manifest_dir.join("contracts/executor/artifacts"),
            wmnt_descriptor_path: gas_profiles.join("wmnt_descriptor.mantle_mainnet.json"),
            moe_allowlist_path: gas_profiles.join("moe_allowlist.mantle_mainnet.json"),
            approved_pools_path: gas_profiles.join("approved_pools.mantle_mainnet.json"),
            threshold_config_path: gas_profiles.join("shadow_thresholds_evidence.example.json"),
            gas_profile_artifact_path: gas_profiles.join("mantle_mainnet_v1.json"),
        },
        target,
    )
    .expect("pin shadow config");

    let provider = ProviderBuilder::new()
        .connect_mocked_client(Asserter::new())
        .erased();

    ShadowExecutionContext::new(
        provider,
        pinned,
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
        Arc::new(BlockFeeContextCache::default()),
        ExecutorConfig::default(),
        ShadowLedgerSetup {
            path: ledger_path,
            started_at_unix: 1_700_000_000,
            service,
        },
    )
    .expect("shadow context")
}
