//! Multi-protocol arbitrage bot (WHI-728 / WHI-527.3 / WHI-739 / WHI-741).
//!
//! Runs Agni-V2, Agni-V3, and Moe **concurrently in one process** over a single
//! merged pool graph. Signerless: never reads a private key; production send
//! remains fail-closed via [`amms::service::production_send_allowed`].
//!
//! ## Modes
//!
//! * **Offline fixture** (`--offline`): replays the built-in cross-protocol
//!   fixture. No RPC. Used by the WHI-527 acceptance criterion.
//!   **`--ledger` is rejected** with offline mode — fixture rows would pollute
//!   a gate corpus with synthetic data (WHI-739).
//! * **Live one-shot** (default / `--once`): loads frozen CSV pool universes,
//!   syncs once via `StateSpaceBuilder`, runs a single merged discovery pass,
//!   and exits. With `--ledger`, builds a
//!   [`amms::execution::ShadowExecutionContext`] and appends run-header +
//!   attempt rows (including `ProductionGateBlocked`).
//! * **Live continuous** (`--watch`): after the initial sync + one-shot pass,
//!   opens **one** WS block subscription that drives all selected protocols
//!   against the shared `StateSpace` (WHI-741 / closes DI-27). SIGINT/SIGTERM
//!   exit zero so the shadow ledger is flushed via normal drop paths.
//!
//! The three legacy `*_monitor_executor_service` examples stay untouched.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::Address;
use alloy::providers::Provider;
use amms::amms::amm::AMM;
use amms::amms::factory::Factory;
use amms::amms::moe::{CANONICAL_MOE_FACTORY, CANONICAL_MOE_FACTORY_CREATION_BLOCK};
use amms::execution::{ShadowExecutionContext, ShadowOverrideTarget};
use amms::service::{
    assert_signerless_invariant, attempt_discovered_via_job_slot, build_shadow_execution_context,
    AttemptJobContext,
    connect_http_provider, connect_ws_provider, cross_protocol_fixture_pools,
    discover_for_protocols, discover_opportunities, factories_for_selection,
    filter_pools_by_protocols, parse_protocols_flag, production_send_allowed,
    run_multi_protocol_watch_loop, subscribe_heads_once, validate_max_hops,
    validate_settlement_asset, validate_settlement_asset_config, wait_for_shutdown_signal,
    AgniV2Protocol, AgniV3Protocol, BlockTick, CsvPoolUniverseSource, DiscoveryConfig,
    DiscoveredOpportunity, ExecutionAttempt, MoeCsvPoolUniverseSource, MoeProtocol,
    PoolUniverseSource, Protocol, RpcProviderConfig, SelectedProtocol, ServiceConfig,
    ServiceConfigOpts, WatchLoopConfig, WatchLoopHooks, WatchLoopState, DEFAULT_MAX_HOPS,
    DEFAULT_WMNT, MERGED_BOT_SHADOW_SERVICE,
};
use amms::state_space::{
    BlockHeaderContext, PoolProtocol, PoolUniverseRow, SnapshotId, StateSpaceBuilder,
};
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

    /// Live mode: after the initial sync + one-shot discovery, run the continuous
    /// multi-protocol block loop (one WS subscription → shared StateSpace →
    /// per-protocol tip refresh → merged discovery). Handles SIGINT/SIGTERM for
    /// clean ledger flush (WHI-741).
    #[arg(long, default_value_t = false)]
    watch: bool,

    /// Shadow ledger path. Live mode only: builds a real
    /// [`ShadowExecutionContext`] and appends run-header + attempt rows.
    /// Requires `MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH` and pinned
    /// `config/gas_profiles/` artifacts (fail-closed on misconfiguration).
    /// Incompatible with `--offline`.
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

    /// Prometheus scrape bind address. Loopback only unless explicitly overridden.
    #[arg(
        long = "metrics-addr",
        env = "BOT_METRICS_ADDR",
        default_value = amms::metrics::DEFAULT_METRICS_BIND
    )]
    metrics_addr: String,

    /// Disable the metrics endpoint entirely.
    #[arg(long = "no-metrics", env = "BOT_NO_METRICS", default_value_t = false)]
    no_metrics: bool,

    /// Allow a non-loopback metrics bind. The endpoint is UNAUTHENTICATED —
    /// only set this behind an authenticating reverse proxy.
    #[arg(
        long = "metrics-allow-public-bind",
        env = "BOT_METRICS_ALLOW_PUBLIC_BIND",
        default_value_t = false
    )]
    metrics_allow_public_bind: bool,

    /// After the one-shot run, keep serving /metrics until SIGINT (for scraping a
    /// completed discovery pass, and for the WHI-535 shadow window).
    #[arg(long = "metrics-hold", env = "BOT_METRICS_HOLD", default_value_t = false)]
    metrics_hold: bool,

    /// Print the rendered registry to stdout on exit.
    #[arg(long = "metrics-dump", default_value_t = false)]
    metrics_dump: bool,
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
    if args.once && args.watch {
        bail!("--once and --watch are mutually exclusive");
    }
    // Fail closed on mode conflicts *before* opening a scrape socket so a
    // rejected invocation never leaves /metrics listening (WHI-532 / WHI-739).
    if args.offline && args.ledger.is_some() {
        bail!(
            "--ledger is not supported with --offline: fixture rows would pollute \
             a shadow gate corpus with synthetic data. Run live mode with --ledger, \
             or drop --ledger for the offline fixture acceptance path."
        );
    }
    if args.offline && args.watch {
        bail!("--watch is not supported with --offline (no block subscription)");
    }
    validate_max_hops(args.max_hops, args.allow_long_paths)?;

    // Metrics install happens *after* signerless guards + mode validation so a
    // guard rejection cannot leave a listening socket behind (WHI-532).
    let metrics_handle = install_bot_metrics(&args)?;
    let protocols_label = selected
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(",");
    amms::metrics::record_build_info(
        env!("CARGO_PKG_VERSION"),
        option_env!("GIT_SHA").unwrap_or("unknown"),
        &protocols_label,
        production_send_allowed(),
    );

    info!(
        target: "bot",
        protocols = %protocols_label,
        offline = args.offline,
        once = args.once,
        watch = args.watch,
        "starting multi-protocol bot"
    );

    let result = if args.offline {
        run_offline(&selected, args.max_hops)
    } else {
        run_live(&args, &selected).await
    };

    if let Some(handle) = metrics_handle.as_ref() {
        if args.metrics_dump {
            print!("{}", handle.render());
        }
        if args.metrics_hold {
            info!(
                target: "bot.metrics",
                "metrics-hold: serving /metrics until SIGINT/SIGTERM"
            );
            wait_for_shutdown_signal().await;
        }
    }

    result
}

fn install_bot_metrics(
    args: &Args,
) -> Result<Option<metrics_exporter_prometheus::PrometheusHandle>> {
    amms::metrics::describe_all();
    if args.no_metrics {
        if args.metrics_dump {
            let handle = amms::metrics::build_handle_without_listener()
                .context("install metrics recorder for --metrics-dump")?;
            return Ok(Some(handle));
        }
        return Ok(None);
    }
    let bind = amms::metrics::parse_metrics_bind(
        Some(args.metrics_addr.as_str()),
        args.metrics_allow_public_bind,
    )
    .map_err(|e| eyre::eyre!("{e}"))?;
    let handle = amms::metrics::install_recorder(bind)
        .map_err(|e| eyre::eyre!("{e}"))
        .with_context(|| format!("install metrics recorder on {bind}"))?;
    info!(
        target: "bot.metrics",
        %bind,
        "Prometheus /metrics endpoint listening"
    );
    Ok(Some(handle))
}

fn run_offline(selected: &[SelectedProtocol], max_hops: usize) -> Result<()> {
    let all_pools = cross_protocol_fixture_pools();
    let pools = filter_pools_by_protocols(&all_pools, selected);
    for proto in selected {
        let count = pools
            .iter()
            .filter(|p| proto.matches_amm(p))
            .count();
        amms::metrics::record_discovery_pools_loaded(proto.as_str(), count);
    }
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
            AttemptJobContext::default(),
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

    // WHI-786: throttle + retry-backoff + per-request timeout. Bare
    // ProviderBuilder::connect_http is forbidden here — shared helper lives in
    // `amms::service::rpc_provider` so future binaries cannot reintroduce one.
    let rpc_cfg = RpcProviderConfig::from_env();
    info!(
        target: "bot.live",
        throttle_rps = rpc_cfg.throttle_rps,
        max_retries = rpc_cfg.max_retries,
        initial_backoff_ms = rpc_cfg.initial_backoff_ms,
        request_timeout_ms = rpc_cfg.request_timeout.as_millis() as u64,
        "building production HTTP provider with throttle/retry/timeout layers"
    );
    let http = connect_http_provider(&config.http_endpoint, &rpc_cfg)
        .context("connect production HTTP provider")?;
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

    // Fail closed: --ledger constructs a real ShadowExecutionContext or exits.
    // Misconfiguration (missing thresholds path / gas profiles) must not fall
    // back to the old log-only no-op (WHI-739).
    let shadow_ctx = match args.ledger.as_ref() {
        Some(ledger_path) => {
            let mut executor_config = config.executor_config.clone();
            executor_config.chain_id = chain_id;
            let ctx = build_shadow_execution_context(
                (*http).clone(),
                ShadowOverrideTarget {
                    executor_contract: config.executor_address,
                    wmnt_address: config.wmnt_address,
                },
                executor_config,
                ledger_path,
                MERGED_BOT_SHADOW_SERVICE,
            )
            .with_context(|| {
                format!(
                    "failed to build shadow execution context for --ledger {} \
                     (set MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH and ensure \
                     config/gas_profiles/ pinned artifacts exist)",
                    ledger_path.display()
                )
            })?;
            info!(
                target: "bot.live",
                ledger = %ledger_path.display(),
                service = MERGED_BOT_SHADOW_SERVICE,
                "shadow ledger context ready"
            );
            Some(ctx)
        }
        None => None,
    };

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
                let before = rows.len();
                load_v2_rows(args, v2_factory, chain_id, config.wmnt_address, &mut rows).await;
                amms::metrics::record_discovery_pools_loaded(
                    proto.as_str(),
                    rows.len().saturating_sub(before),
                );
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
                        amms::metrics::record_discovery_pools_loaded(
                            proto.as_str(),
                            loaded.rows.len(),
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
                        amms::metrics::record_discovery_pools_loaded(
                            proto.as_str(),
                            loaded.rows.len(),
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
    let mut tip_header: Option<BlockHeaderContext> = None;
    if let Ok(tip) = http.get_block_number().await {
        if let Ok(Some(block)) = http
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Number(tip))
            .await
        {
            let header = block.header();
            discovery.snapshot_id = SnapshotId::new(chain_id, tip, header.hash());
            discovery.block_timestamp = header.timestamp();
            tip_header = Some(BlockHeaderContext::new(
                header.parent_hash(),
                header.timestamp(),
            ));
        }
    }

    if let (Some(shadow), Some(header)) = (shadow_ctx.as_ref(), tip_header) {
        shadow
            .record_canonical_observation(discovery.snapshot_id, header)
            .context("failed to record canonical observation in shadow ledger")?;
    }

    let found = discover_opportunities(&pools, &discovery)?;
    print_discovery_report(selected, &found);

    if let Some(best) = found.first() {
        let attempt = attempt_discovered_via_job_slot(best, discovery.block_timestamp, AttemptJobContext::default()).await?;
        info!(
            target: "bot.live",
            ?attempt,
            signature = %best.candidate.signature,
            "signerless attempt_execution via job slot"
        );
        if let Some(ref shadow) = shadow_ctx {
            record_attempt_in_shadow_ledger(shadow, best, &attempt)?;
        }
    }

    if !args.watch {
        info!(target: "bot.live", "one-shot live discovery complete");
        return Ok(());
    }

    // --- Continuous multi-protocol watch (WHI-741) ---------------------------------
    // One WS subscription drives every selected protocol against the shared
    // StateSpace already held by `manager`. Reorgs use StateChangeCache (shallow)
    // + SnapshotPublisher halt; deep recovery is WHI-533.
    info!(
        target: "bot.live",
        ws = %config.ws_endpoint,
        protocols = ?selected,
        "entering multi-protocol --watch loop (single shared block subscription)"
    );

    // WS: retry + timeout only (no throttle). Subscriptions are long-lived and
    // low-rate; throttling heads would only add latency. See rpc_provider module.
    let ws = connect_ws_provider(&config.ws_endpoint, &rpc_cfg)
        .await
        .context("connect production WS provider for multi-protocol --watch subscription")?;
    let head_sub = subscribe_heads_once(&ws, chain_id)
        .await
        .context("subscribe_blocks (single multi-protocol subscription)")?;
    // Enforce the single-subscription invariant at the call site (AC).
    if head_sub.subscription_count != 1 {
        bail!(
            "multi-protocol --watch opened {} block subscriptions; expected exactly 1",
            head_sub.subscription_count
        );
    }

    let loop_state = WatchLoopState {
        state: manager.state.clone(),
        latest_block: manager.latest_block.clone(),
        snapshots: manager.snapshots.clone(),
        block_filter: manager.block_filter.clone(),
        chain_id: manager.chain_id,
    };
    let watch_config = WatchLoopConfig {
        discovery: discovery.clone(),
        selected: selected.to_vec(),
        attempt_execution: true,
        refresh_tip_state: true,
    };
    // DynProvider is already type-erased; clone for the watch loop.
    let http_erased = (*http).clone();
    let hooks = BotWatchHooks {
        shadow: shadow_ctx.as_ref(),
    };
    let shutdown = Box::pin(wait_for_shutdown_signal());

    let stats = run_multi_protocol_watch_loop(
        http_erased,
        loop_state,
        watch_config,
        head_sub.stream,
        shutdown,
        hooks,
        head_sub.subscription_count,
    )
    .await
    .context("multi-protocol watch loop")?;

    info!(
        target: "bot.live",
        blocks_processed = stats.blocks_processed,
        opportunities_found = stats.opportunities_found,
        attempts = stats.attempts,
        block_subscriptions = stats.block_subscriptions,
        halted_or_skipped = stats.halted_or_skipped,
        "multi-protocol --watch loop exited cleanly"
    );
    Ok(())
}

/// Shadow-ledger hooks for the continuous watch path (WHI-741 + WHI-739).
struct BotWatchHooks<'a> {
    shadow: Option<&'a ShadowExecutionContext>,
}

impl WatchLoopHooks for BotWatchHooks<'_> {
    fn on_block_ready(&mut self, tick: &BlockTick) -> Result<()> {
        if let Some(shadow) = self.shadow {
            shadow
                .record_canonical_observation(tick.snapshot_id, tick.header)
                .context("watch: record_canonical_observation")?;
        }
        Ok(())
    }

    fn on_attempt(
        &mut self,
        opp: &DiscoveredOpportunity,
        attempt: &ExecutionAttempt,
    ) -> Result<()> {
        if let Some(shadow) = self.shadow {
            record_attempt_in_shadow_ledger(shadow, opp, attempt)?;
        }
        Ok(())
    }
}

/// Append attempt evidence for a live `--ledger` run. Gate-blocked outcomes
/// are the primary shape while production send remains hard-false (WHI-739).
fn record_attempt_in_shadow_ledger(
    shadow: &ShadowExecutionContext,
    opp: &DiscoveredOpportunity,
    attempt: &ExecutionAttempt,
) -> Result<()> {
    match attempt {
        ExecutionAttempt::ProductionGateBlocked {
            amount_in,
            min_profit,
        } => {
            shadow
                .record_production_gate_blocked(
                    &opp.candidate.signature,
                    *amount_in,
                    *min_profit,
                )
                .context("failed to record ProductionGateBlocked in shadow ledger")?;
            info!(
                target: "bot.live",
                signature = %opp.candidate.signature,
                amount_in = %amount_in,
                min_profit = %min_profit,
                "recorded ProductionGateBlocked shadow ledger row"
            );
        }
        ExecutionAttempt::Submitted(tx) => {
            // Unreachable while production_send_allowed is hard-false; log if it
            // ever becomes reachable so evidence is not silently dropped.
            warn!(
                target: "bot.live",
                tx = %tx,
                "Submitted attempt under --ledger is not yet recorded as a shadow row"
            );
        }
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
