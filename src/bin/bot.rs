//! Multi-protocol arbitrage bot (WHI-728 / WHI-527.3).
//!
//! Runs Agni-V2, Agni-V3, and Moe **concurrently in one process** over a single
//! merged pool graph. Signerless: never reads a private key; production send
//! remains fail-closed via [`amms::service::production_send_allowed`].
//!
//! ## Modes
//!
//! * **Offline fixture** (`--offline`): replays the built-in cross-protocol
//!   fixture. No RPC. Used by the WHI-527 acceptance criterion.
//! * **Live** (default without `--offline`): loads frozen CSV pool universes
//!   for the selected protocols, syncs once via `StateSpaceBuilder`, and
//!   runs a single merged discovery pass. Continuous `--watch` notes the
//!   shared job-slot scaffold; full multi-protocol log application reuses
//!   `StateSpaceManager` (legacy services remain the production-disabled
//!   references until M3-9).
//!
//! The three legacy `*_monitor_executor_service` examples stay untouched.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use amms::amms::amm::AMM;
use amms::amms::factory::Factory;
use amms::amms::moe::{CANONICAL_MOE_FACTORY, CANONICAL_MOE_FACTORY_CREATION_BLOCK};
use amms::service::{
    assert_signerless_invariant, attempt_discovered_via_job_slot, cross_protocol_fixture_pools,
    discover_for_protocols, discover_opportunities, factories_for_selection,
    filter_pools_by_protocols, parse_protocols_flag, production_send_allowed,
    validate_settlement_asset, validate_settlement_asset_config, AgniV2Protocol, AgniV3Protocol,
    CsvPoolUniverseSource, DiscoveryConfig, DiscoveredOpportunity, MoeCsvPoolUniverseSource,
    MoeProtocol, PoolUniverseSource, Protocol, SelectedProtocol, ServiceConfig, ServiceConfigOpts,
    DEFAULT_MAX_HOPS, DEFAULT_WMNT,
};
use amms::state_space::{PoolProtocol, PoolUniverseRow, SnapshotId, StateSpaceBuilder};
use clap::Parser;
use eyre::{bail, Context, Result};
use tracing::{info, warn};

/// Signerless multi-protocol Mantle arbitrage bot.
#[derive(Debug, Parser)]
#[command(
    name = "bot",
    about = "Run Agni-V2 / Agni-V3 / Moe concurrently over one merged pool graph (signerless)"
)]
struct Args {
    /// Comma-separated protocols to enable. Default: all three.
    ///
    /// Accepted names: `agni-v2`, `agni-v3`, `moe` (aliases: `v2`, `v3`, `agni`).
    #[arg(long, default_value = "agni-v2,agni-v3,moe", env = "BOT_PROTOCOLS")]
    protocols: String,

    /// Offline fixture mode — no RPC. Demonstrates cross-protocol discovery.
    #[arg(long, default_value_t = false)]
    offline: bool,

    /// Live mode: sync once and exit after a single discovery pass.
    #[arg(long, default_value_t = false)]
    once: bool,

    /// Live mode: note continuous job-slot watch scaffold (one-shot discovery still runs).
    #[arg(long, default_value_t = false)]
    watch: bool,

    /// Optional shadow ledger path (forwarded for WHI-526 runbook compatibility).
    #[arg(long, env = "SHADOW_LEDGER_PATH")]
    ledger: Option<PathBuf>,

    /// CSV pool list for Agni-V2 (live mode).
    #[arg(long, env = "BOT_V2_POOL_LIST", default_value = "data/poolLists.csv")]
    v2_pool_list: PathBuf,

    /// CSV pool list for Agni-V3 (live mode).
    #[arg(long, env = "BOT_V3_POOL_LIST", default_value = "data/poolLists.csv")]
    v3_pool_list: PathBuf,

    /// CSV pool list for Moe (live mode).
    #[arg(long, env = "BOT_MOE_POOL_LIST", default_value = "data/poolLists_moe.csv")]
    moe_pool_list: PathBuf,

    /// Agni V2 factory address (live mode).
    #[arg(long, env = "AGNI_V2_FACTORY_ADDRESS")]
    v2_factory: Option<String>,

    /// Agni V3 factory address (live mode).
    #[arg(long, env = "AGNI_FACTORY_ADDRESS")]
    v3_factory: Option<String>,

    /// Max path hops for discovery (strategy default 3; ARB_PATHS_MANTLE.md §4).
    #[arg(long, default_value_t = DEFAULT_MAX_HOPS)]
    max_hops: usize,

    /// Opt-in to allow `--max-hops` above the strategy cap of 3 (WHI-529).
    ///
    /// Without this flag, values 0 or >3 are rejected with an error citing
    /// ARB_PATHS_MANTLE.md §4.
    #[arg(long, default_value_t = false)]
    allow_long_paths: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    // Fail closed: never construct or read a production signer in this binary.
    amms::execution::guard_shadow_env(&amms::execution::e2e::ProcessEnvSource)
        .context("shadow-mode env guard rejected startup")?;
    assert_signerless_invariant()?;
    if production_send_allowed() {
        bail!("bot binary must remain signerless (production_send_allowed == false)");
    }

    let args = Args::parse();
    let selected = parse_protocols_flag(&args.protocols)?;
    info!(
        target: "bot",
        protocols = %selected
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(","),
        offline = args.offline,
        "starting multi-protocol bot"
    );

    if let Some(ref ledger) = args.ledger {
        info!(
            target: "bot",
            ledger = %ledger.display(),
            "shadow ledger path noted"
        );
    }

    validate_max_hops(args.max_hops, args.allow_long_paths)?;

    if args.offline {
        return run_offline(&selected, args.max_hops);
    }

    run_live(&args, &selected).await
}

/// Reject hop caps outside the strategy range unless `--allow-long-paths`.
///
/// Evidence: ARB_PATHS_MANTLE.md §4 — 93.5% of arb is 2–3 pools; do not optimize
/// for long paths by default (WHI-529).
fn validate_max_hops(max_hops: usize, allow_long_paths: bool) -> Result<()> {
    if max_hops == 0 {
        bail!("--max-hops must be >= 1 (got 0)");
    }
    if max_hops > DEFAULT_MAX_HOPS && !allow_long_paths {
        bail!(
            "--max-hops {max_hops} exceeds strategy cap {DEFAULT_MAX_HOPS} \
             (ARB_PATHS_MANTLE.md §4: 93.5% of arbitrage is 2–3 pools; \
             solidify 2-hop and 3-hop; do not optimize for long paths). \
             Pass --allow-long-paths to override."
        );
    }
    Ok(())
}

fn run_offline(selected: &[SelectedProtocol], max_hops: usize) -> Result<()> {
    let all_pools = cross_protocol_fixture_pools();
    let pools = filter_pools_by_protocols(&all_pools, selected);
    info!(
        target: "bot.offline",
        selected = selected.len(),
        pools = pools.len(),
        "loaded offline multi-protocol fixture"
    );

    // Offline: no executor to query — config-level settlement equality only
    // (executor.WMNT check skipped; logged below) (WHI-529).
    validate_settlement_asset_config(DEFAULT_WMNT, DEFAULT_WMNT)
        .map_err(|e| eyre::eyre!("{e}"))?;
    info!(
        target: "bot.offline",
        settlement = %DEFAULT_WMNT,
        "settlement validation: config equality ok; executor.WMNT check skipped (offline, no RPC)"
    );

    let mut config = DiscoveryConfig::for_settlement(DEFAULT_WMNT);
    config.max_hops = max_hops;
    // Screening gas is free for the offline acceptance run so any gross-positive
    // cross-protocol cycle is reported (the gate still never broadcasts).
    config.gas.gas_price_wei = 0;

    let found = discover_opportunities(&pools, &config)?;
    print_discovery_report(selected, &found);

    // Per-protocol negative baseline (acceptance: no single protocol can find the cycle).
    for proto in selected {
        let subset = discover_for_protocols(&all_pools, &[*proto], &config)?;
        info!(
            target: "bot.offline",
            protocol = %proto,
            opportunities = subset.len(),
            "single-protocol baseline"
        );
        if selected.len() == SelectedProtocol::all().len() && !subset.is_empty() {
            warn!(
                target: "bot.offline",
                protocol = %proto,
                "unexpected single-protocol opportunities on the cross-protocol-only fixture"
            );
        }
    }

    let cross = found.iter().filter(|o| o.is_cross_protocol).count();
    if selected.len() == SelectedProtocol::all().len() && cross == 0 {
        bail!(
            "offline multi-protocol run found no cross-protocol cycle \
             (fixture regression — WHI-728 acceptance failed)"
        );
    }

    // Job-slot + Protocol::attempt_execution (or mixed gate-closed path).
    if let Some(best) = found.first() {
        let attempt = block_on_async(attempt_discovered_via_job_slot(
            best,
            config.block_timestamp,
        ))?;
        info!(
            target: "bot.offline",
            attempt = ?attempt,
            "signerless attempt_execution via job slot"
        );
        assert!(!production_send_allowed());
    }

    Ok(())
}

fn block_on_async<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("offline runtime");
            rt.block_on(fut)
        }
    }
}

fn print_discovery_report(selected: &[SelectedProtocol], found: &[DiscoveredOpportunity]) {
    println!("=== multi-protocol discovery report ===");
    println!(
        "protocols: {}",
        selected
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!("opportunities: {}", found.len());
    let cross = found.iter().filter(|o| o.is_cross_protocol).count();
    println!("cross_protocol_opportunities: {cross}");
    for (i, opp) in found.iter().enumerate() {
        let kinds = opp
            .protocol_kinds
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join("+");
        println!(
            "  [{i}] cross={} protocols={kinds} hops={} input={} net_profit={} signature={}",
            opp.is_cross_protocol,
            opp.candidate.hops,
            opp.candidate.input,
            opp.candidate.net_profit,
            opp.candidate.signature
        );
        for (hop_i, (hop, pool)) in opp
            .candidate
            .path
            .hops
            .iter()
            .zip(opp.candidate.pools.iter())
            .enumerate()
        {
            println!(
                "      hop {hop_i}: kind={:?} pool={:#x} {} -> {}",
                amms::service::protocol_kind_of_amm(pool),
                hop.pool_address,
                hop.token_in,
                hop.token_out
            );
        }
    }
    println!("production_send_allowed: {}", production_send_allowed());
    println!("=== end report ===");
}

async fn run_live(args: &Args, selected: &[SelectedProtocol]) -> Result<()> {
    let config = ServiceConfig::from_env(ServiceConfigOpts::agni_v3())
        .or_else(|_| ServiceConfig::from_env(ServiceConfigOpts::agni_v2()))
        .context("ServiceConfig::from_env (set executor address env vars for live mode)")?;

    let http = ProviderBuilder::new().connect_http(
        config
            .http_endpoint
            .parse()
            .context("parse HTTP endpoint")?,
    );
    let http = Arc::new(http);
    let chain_id = http.get_chain_id().await.context("eth_chainId")?;
    info!(target: "bot.live", chain_id, "connected HTTP provider");

    // Fail closed if settlement ≠ gas asset ≠ executor.WMNT (WHI-529 / B9).
    validate_settlement_asset(
        config.settlement_asset,
        config.wmnt_address,
        config.executor_address,
        http.as_ref(),
    )
    .await
    .map_err(|e| eyre::eyre!("{e}"))?;
    info!(
        target: "bot.live",
        settlement = %config.settlement_asset,
        "settlement asset validated against gas asset and executor.WMNT()"
    );

    let v2_factory = args
        .v2_factory
        .as_deref()
        .map(Address::from_str)
        .transpose()
        .context("parse v2 factory")?
        .unwrap_or(Address::ZERO);
    let v3_factory = args
        .v3_factory
        .as_deref()
        .map(Address::from_str)
        .transpose()
        .context("parse v3 factory")?
        .unwrap_or(Address::ZERO);

    let mut rows: Vec<PoolUniverseRow> = Vec::new();
    let mut factories: Vec<Factory> = factories_for_selection(
        selected,
        v2_factory,
        v3_factory,
        CANONICAL_MOE_FACTORY,
        CANONICAL_MOE_FACTORY_CREATION_BLOCK,
    );

    for proto in selected {
        match proto {
            SelectedProtocol::AgniV2 => {
                load_v2_rows(args, v2_factory, chain_id, config.wmnt_address, &mut rows).await;
            }
            SelectedProtocol::AgniV3 => {
                let source = CsvPoolUniverseSource::new(
                    &args.v3_pool_list,
                    PoolProtocol::Agni,
                    v3_factory,
                )
                .with_protocol_filter("agni");
                match source.load(chain_id, config.wmnt_address).await {
                    Ok(loaded) => {
                        info!(
                            target: "bot.live",
                            protocol = %proto,
                            pools = loaded.rows.len(),
                            "loaded pool universe"
                        );
                        rows.extend(loaded.rows);
                    }
                    Err(e) => warn!(target: "bot.live", error = %e, "V3 pool list unavailable"),
                }
            }
            SelectedProtocol::Moe => {
                let source = MoeCsvPoolUniverseSource::new(&args.moe_pool_list);
                match source.load(chain_id, config.wmnt_address).await {
                    Ok(loaded) => {
                        info!(
                            target: "bot.live",
                            protocol = %proto,
                            pools = loaded.rows.len(),
                            "loaded moe pool universe"
                        );
                        rows.extend(loaded.rows);
                    }
                    Err(e) => warn!(target: "bot.live", error = %e, "Moe pool list unavailable"),
                }
            }
        }
    }

    let v2 = AgniV2Protocol::new(v2_factory);
    let v3 = AgniV3Protocol::new(v3_factory);
    let moe = MoeProtocol::new();
    let mut amms: Vec<AMM> = Vec::new();
    for row in &rows {
        let built = match row.protocol {
            PoolProtocol::UniswapV2 => v2.build_amm(row),
            PoolProtocol::Agni => v3.build_amm(row),
            PoolProtocol::MoeLb => moe.build_amm(row),
            other => {
                warn!(target: "bot.live", ?other, "skipping unsupported pool-universe protocol");
                continue;
            }
        };
        match built {
            Ok(amm) => amms.push(amm),
            Err(e) => warn!(target: "bot.live", error = %e, "build_amm failed"),
        }
    }

    if amms.is_empty() {
        bail!(
            "live mode loaded zero pools for protocols {:?}; check CSV paths / factory env",
            selected
        );
    }

    info!(
        target: "bot.live",
        amms = amms.len(),
        factories = factories.len(),
        "syncing merged multi-protocol state space"
    );

    factories.retain(|f| f.address() != Address::ZERO);

    let manager = StateSpaceBuilder::new(http.clone())
        .chain_id(chain_id)
        .with_factories(factories)
        .with_amms(amms)
        .sync()
        .await
        .context("StateSpaceBuilder::sync over merged multi-protocol set")?;

    let pools: Vec<AMM> = {
        let guard = manager.state.read().await;
        guard.state.values().cloned().collect()
    };
    info!(target: "bot.live", pools = pools.len(), "synced pool state");

    let mut discovery = DiscoveryConfig::for_settlement(config.settlement_asset);
    discovery.max_hops = args.max_hops;
    discovery.min_profit = config.min_net_profit;
    // Stamp tip identity when available so Moe fee evolution uses live time.
    if let Ok(tip) = http.get_block_number().await {
        if let Ok(Some(block)) = http
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(tip))
            .await
        {
            let header = block.header();
            discovery.snapshot_id = SnapshotId::new(chain_id, tip, header.hash());
            discovery.block_timestamp = header.timestamp();
        }
    }

    let found = discover_opportunities(&pools, &discovery)?;
    print_discovery_report(selected, &found);

    if args.watch {
        // Continuous multi-protocol log application is intentionally not claimed
        // here: job-slot + attempt_execution are exercised once below; the three
        // production-disabled example services remain the continuous references
        // until post-merge (M3-9). Fail closed rather than pretend to loop.
        bail!(
            "--watch continuous multi-protocol block loop is not enabled in this PR \
             (one-shot discovery completed above). Re-run without --watch, or use the \
             production-disabled example services for continuous single-protocol loops."
        );
    }
    if !args.once {
        info!(target: "bot.live", "one-shot live discovery complete");
    }

    if let Some(best) = found.first() {
        let attempt = attempt_discovered_via_job_slot(best, discovery.block_timestamp).await?;
        info!(
            target: "bot.live",
            ?attempt,
            signature = %best.candidate.signature,
            "signerless attempt_execution via job slot"
        );
    }

    Ok(())
}

async fn load_v2_rows(
    args: &Args,
    v2_factory: Address,
    chain_id: u64,
    wmnt: Address,
    rows: &mut Vec<PoolUniverseRow>,
) {
    let source =
        CsvPoolUniverseSource::new(&args.v2_pool_list, PoolProtocol::UniswapV2, v2_factory)
            .with_protocol_filter("v2");
    match source.load(chain_id, wmnt).await {
        Ok(loaded) if !loaded.rows.is_empty() => {
            info!(
                target: "bot.live",
                protocol = "agni-v2",
                pools = loaded.rows.len(),
                "loaded pool universe"
            );
            rows.extend(loaded.rows);
            return;
        }
        _ => {}
    }
    let source =
        CsvPoolUniverseSource::new(&args.v2_pool_list, PoolProtocol::UniswapV2, v2_factory);
    match source.load(chain_id, wmnt).await {
        Ok(loaded) => {
            info!(
                target: "bot.live",
                protocol = "agni-v2",
                pools = loaded.rows.len(),
                "loaded pool universe (unfiltered)"
            );
            rows.extend(loaded.rows);
        }
        Err(e) => warn!(
            target: "bot.live",
            path = %args.v2_pool_list.display(),
            error = %e,
            "V2 pool list unavailable"
        ),
    }
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
