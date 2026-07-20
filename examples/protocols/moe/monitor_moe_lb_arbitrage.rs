use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::{address, Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::sol_types::SolEvent;
use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};
use alloy::transports::ws::WsConnect;
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    moe::{
        default_moe_pool_list_path, sync_moe_snapshots_batch, IMoeLBPairEvents, MoeLbPair,
        MoePoolList, MoeSnapshotContext, MoeSnapshotSyncConfig, CANONICAL_MOE_FACTORY,
    },
};
use amms::arbitrage::{
    gas::GasConfig,
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::state_space::hash_pinned_logs_filter;
use amms::state_space::StateSpace;
use csv::{StringRecord, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use tracing::{debug, error, info, warn};

const MAX_HOPS: usize = 4;
const WMNT_ADDRESS: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const BINS_RADIUS: u32 = 50; // Sync 50 bins on each side of active bin for better swap coverage
const BINS_BATCH_SIZE: u32 = 15; // Sync bins in batches of ±15 to avoid contract size limits

const POSITIVE_PATH_LOG_HEADERS: &[&str] = &[
    "block_number",
    "path_index",
    "path_signature",
    "input_amount",
    "output_amount",
    "profit",
    "roi_percent",
    "hops",
];

const BEST_PATH_LOG_HEADERS: &[&str] = &[
    "block_number",
    "path_index",
    "path_signature",
    "input_amount",
    "output_amount",
    "profit",
    "roi_percent",
    "hops",
];

fn resolve_ws_endpoint() -> String {
    let raw = std::env::var("RPC_WS_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_WS_URL").ok())
        .unwrap_or_else(|| "wss://mantle.publicnode.com".to_string());
    let normalized = normalize_ws_endpoint(raw.trim());
    if normalized != raw {
        info!(
            target: "moe.config",
            original = %raw,
            normalized = %normalized,
            "Normalized WS endpoint"
        );
    }
    normalized
}

fn resolve_http_endpoint() -> String {
    std::env::var("RPC_HTTP_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_HTTP_URL").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://rpc.mantle.xyz".to_string())
}

fn normalize_ws_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "wss://mantle.publicnode.com".to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    }
}

#[derive(Clone, PartialEq, Eq)]
struct PositiveCandidate {
    index: usize,
    profit: I256,
    input: U256,
    output: U256,
    roi: String,
    signature: String,
    hops: String,
    pools: Vec<Address>,
}

#[derive(Clone, Debug)]
struct LoggedPathRecord {
    signature: String,
    profit: I256,
    input: U256,
    output: U256,
    roi: String,
}

#[derive(Clone, PartialEq, Eq)]
struct SelectionSnapshot {
    signatures: Vec<String>,
    profits: Vec<I256>,
}

static LAST_SELECTION: OnceLock<Mutex<Option<SelectionSnapshot>>> = OnceLock::new();
static LOGGED_PATHS: OnceLock<Mutex<HashMap<String, LoggedPathRecord>>> = OnceLock::new();

#[derive(Clone)]
struct PathCache {
    paths: Vec<ArbitragePath>,
    signatures: Vec<String>,
    pool_to_path_indices: HashMap<Address, Vec<usize>>,
}

/// Check if a pool has reasonable reserves for arbitrage
fn is_pool_reasonable(pool: &MoeLbPair) -> bool {
    // Only filter pools with zero reserves (no liquidity)
    if pool.reserve_x == 0 || pool.reserve_y == 0 {
        warn!(
            target: "moe.filter",
            address = %pool.address,
            reserve_x = pool.reserve_x,
            reserve_y = pool.reserve_y,
            "Pool has zero reserves, excluding from arbitrage"
        );
        return false;
    }

    // Note: We do NOT filter based on reserve ratio because different token decimals
    // can make ratios look extreme even when the pool is perfectly normal.
    // For example, CMETH (18 decimals) vs FBTC (8 decimals) will have extreme raw ratios.
    // The swap simulation with proper fee calculation will handle edge cases.

    true
}

fn build_path_cache(pools: &HashMap<Address, MoeLbPair>, max_hops: usize) -> PathCache {
    let mut state = StateSpace::default();
    let mut filtered_count = 0;

    for pool in pools.values() {
        // Only include reasonable pools in arbitrage graph
        if is_pool_reasonable(pool) {
            state
                .state
                .insert(pool.address(), AMM::MoeLbPair(pool.clone()));
        } else {
            filtered_count += 1;
        }
    }

    if filtered_count > 0 {
        info!(
            target: "moe.monitor",
            filtered = filtered_count,
            total = pools.len(),
            "Filtered {} pools with zero reserves from arbitrage",
            filtered_count
        );
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(target: "moe.monitor", error = ?err, "Failed to build arbitrage graph");
            return PathCache {
                paths: Vec::new(),
                signatures: Vec::new(),
                pool_to_path_indices: HashMap::new(),
            };
        }
    };

    let constraints = PathConstraints {
        max_length: max_hops,
        required_start_token: Some(WMNT_ADDRESS),
        required_end_token: Some(WMNT_ADDRESS),
        ..PathConstraints::default()
    };
    let finder = PathFinder::new(&graph, constraints);

    let paths_iter = finder
        .find_cycles()
        .into_iter()
        .chain(finder.find_two_pool_misprices());

    let mut unique_paths: HashMap<String, ArbitragePath> = HashMap::new();
    for path in paths_iter {
        let signature = path_signature(&path);
        unique_paths.entry(signature).or_insert(path);
    }

    let mut signatures_paths: Vec<(String, ArbitragePath)> = unique_paths.into_iter().collect();
    signatures_paths.sort_by(|a, b| a.0.cmp(&b.0));

    let signatures: Vec<String> = signatures_paths.iter().map(|(s, _)| s.clone()).collect();
    let paths: Vec<ArbitragePath> = signatures_paths.into_iter().map(|(_, p)| p).collect();

    let mut pool_to_path_indices: HashMap<Address, Vec<usize>> = HashMap::new();
    for (idx, path) in paths.iter().enumerate() {
        for hop in &path.hops {
            pool_to_path_indices
                .entry(hop.pool_address)
                .or_default()
                .push(idx);
        }
    }

    PathCache {
        paths,
        signatures,
        pool_to_path_indices,
    }
}

fn select_best_non_conflicting_paths(candidates: &[PositiveCandidate]) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut best_total = I256::ZERO;
    let mut best_combination = Vec::new();
    let mut current_combination = Vec::new();
    let mut used_pools = HashSet::new();

    fn backtrack(
        candidates: &[PositiveCandidate],
        start: usize,
        used_pools: &mut HashSet<Address>,
        current_combination: &mut Vec<usize>,
        best_combination: &mut Vec<usize>,
        best_total: &mut I256,
    ) {
        let current_total = current_combination
            .iter()
            .fold(I256::ZERO, |acc, &idx| acc + candidates[idx].profit);

        if current_total > *best_total {
            *best_total = current_total;
            *best_combination = current_combination.clone();
        }

        for i in start..candidates.len() {
            let candidate = &candidates[i];
            if candidate.pools.iter().any(|pool| used_pools.contains(pool)) {
                continue;
            }

            candidate.pools.iter().for_each(|pool| {
                used_pools.insert(*pool);
            });
            current_combination.push(i);

            backtrack(
                candidates,
                i + 1,
                used_pools,
                current_combination,
                best_combination,
                best_total,
            );

            current_combination.pop();
            candidate.pools.iter().for_each(|pool| {
                used_pools.remove(pool);
            });
        }
    }

    let mut sorted_indices: Vec<usize> = (0..candidates.len()).collect();
    sorted_indices.sort_by(|&a, &b| candidates[b].profit.cmp(&candidates[a].profit));

    let sorted_candidates: Vec<PositiveCandidate> = sorted_indices
        .iter()
        .map(|&idx| candidates[idx].clone())
        .collect();

    backtrack(
        &sorted_candidates,
        0,
        &mut used_pools,
        &mut current_combination,
        &mut best_combination,
        &mut best_total,
    );

    best_combination
        .into_iter()
        .map(|sorted_idx| sorted_indices[sorted_idx])
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    {
        let level = std::env::var("RUST_LOG")
            .ok()
            .and_then(|s| s.parse::<tracing::Level>().ok())
            .unwrap_or(tracing::Level::INFO);
        let _ = tracing_subscriber::fmt()
            .with_target(true)
            .with_level(true)
            .with_line_number(true)
            .with_file(true)
            .compact()
            .with_max_level(level)
            .try_init();
    }

    let ws_endpoint = resolve_ws_endpoint();
    let http_endpoint = resolve_http_endpoint();
    info!(target: "moe.monitor", ws = %ws_endpoint, http = %http_endpoint, "Using endpoints");

    // HTTP with retry/throttle for fail-closed pool-list load + init.
    let http_client = ClientBuilder::default()
        .layer(ThrottleLayer::new(40))
        .layer(RetryBackoffLayer::new(8, 250, 500))
        .http(http_endpoint.parse().context("invalid http endpoint")?);
    let http_provider = ProviderBuilder::new().connect_client(http_client);

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_endpoint))
        .await?;

    info!(target: "moe.monitor", max_hops = MAX_HOPS, "Using max hops for path search");

    run_service(ws_provider, http_provider).await
}

async fn run_service<P, H>(ws_provider: P, http_provider: H) -> Result<()>
where
    P: Provider + Clone,
    H: Provider + Clone,
{
    let pool_log_path = std::env::var("POOL_UPDATE_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_pool_updates.csv"));
    ensure_log_headers(
        &pool_log_path,
        &[
            "block_number",
            "pool_address",
            "event",
            "active_id",
            "reserve_x",
            "reserve_y",
        ],
    )?;

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_positive_path_simulations.csv"));
    let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_best_arbitrage_paths.csv"));
    ensure_log_headers(&positive_sim_log_path, POSITIVE_PATH_LOG_HEADERS)?;
    ensure_log_headers(&best_paths_log_path, BEST_PATH_LOG_HEADERS)?;

    let latest_block = http_provider.get_block_number().await?;
    let mut pools: HashMap<Address, MoeLbPair> = HashMap::new();
    initialize_moe_pools(&http_provider, latest_block, &mut pools).await?;

    info!(
        target: "moe.monitor",
        pools = pools.len(),
        "Initialized Moe LBPairs"
    );

    for pool in pools.values() {
        log_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }

    let path_cache = build_path_cache(&pools, MAX_HOPS);

    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IMoeLBPairEvents::Swap::SIGNATURE_HASH,
        IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH,
        IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "moe.monitor", "Subscribed to blocks over WS");
    info!(target: "moe.monitor", candidate_paths = path_cache.paths.len(), "Pre-computed arbitrage candidate paths");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "moe.monitor.block", block = target_number, "Processing block");

        let target_header = http_provider
            .get_block_by_number(target_number.into())
            .await?
            .ok_or_else(|| eyre!("missing block {target_number}"))?;
        let context = MoeSnapshotContext::new(
            target_header.header().hash(),
            target_header.header().timestamp,
        );
        let windowed = hash_pinned_logs_filter(filter.clone(), context.block_hash);
        match http_provider.get_logs(&windowed).await {
            Ok(logs) => {
                info!(target: "moe.monitor.block", block = target_number, logs = logs.len(), "Fetched logs");
                if logs.is_empty() {
                    continue;
                }

                let mut working_pools = pools.clone();
                let changed = apply_logs(&mut working_pools, &logs, target_number, &pool_log_path)?;

                // Resync reserves and bins for changed pools to ensure accurate simulation
                if !changed.is_empty() {
                    let mut snapshot_amms: Vec<AMM> = working_pools
                        .values()
                        .cloned()
                        .map(AMM::MoeLbPair)
                        .collect();
                    sync_moe_snapshots_at_context(
                        &mut snapshot_amms,
                        http_provider.clone(),
                        context,
                        BINS_RADIUS,
                        BINS_BATCH_SIZE,
                    )
                    .await?;
                    working_pools = snapshot_amms
                        .into_iter()
                        .filter_map(|amm| match amm {
                            AMM::MoeLbPair(pair) => Some((pair.address, pair)),
                            _ => None,
                        })
                        .collect();
                    debug!(
                        target: "moe.monitor.block",
                        block = target_number,
                        count = changed.len(),
                        "Resynced Moe snapshots for all pools"
                    );

                    pools = working_pools;
                    log_path_simulations(&pools, target_number, &path_cache, &changed)?;
                }
            }
            Err(e) => {
                error!(target: "moe.monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

/// Sync bins in batches to avoid "max code size exceeded" error
async fn sync_moe_snapshots_at_context<P: Provider + Clone>(
    amms: &mut Vec<AMM>,
    provider: P,
    context: MoeSnapshotContext,
    radius: u32,
    batch_size: u32,
) -> Result<()> {
    let block_id = BlockId::hash_canonical(context.block_hash);
    sync_moe_snapshots_batch(
        amms,
        block_id,
        provider,
        context,
        MoeSnapshotSyncConfig {
            bins_radius: radius,
            bins_per_request: batch_size,
        },
    )
    .await?;
    Ok(())
}

async fn initialize_moe_pools<P: Provider + Clone>(
    provider: &P,
    block_number: u64,
    pools: &mut HashMap<Address, MoeLbPair>,
) -> Result<()> {
    let block_id = alloy::eips::BlockId::from(block_number);
    let csv_path = default_moe_pool_list_path();
    let list = MoePoolList::load_and_validate_on_chain(
        &csv_path,
        provider.clone(),
        block_id,
        CANONICAL_MOE_FACTORY,
    )
    .await
    .with_context(|| {
        format!(
            "Failed to load/validate dedicated Moe pool list at {} (no Agni fallback)",
            csv_path.display()
        )
    })?;

    info!(
        target: "moe.service",
        path = %csv_path.display(),
        pools = list.len(),
        snapshot_block = list.snapshot_block(),
        "Loaded and on-chain-validated dedicated Moe pool list"
    );

    let init_jobs: Vec<Address> = list.entries.iter().map(|e| e.pool).collect();
    let expected = init_jobs.len();

    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let result = MoeLbPair::new(addr).init_basic(block_id, provider).await;
            (addr, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    let mut init_errors = Vec::new();
    while let Some((addr, result)) = init_stream.next().await {
        match result {
            Ok(pool) => {
                info!(target: "moe.init", address = %addr, bin_step = pool.bin_step, "Initialized Moe pool");
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "moe.init",
                    address = %addr,
                    error = %err,
                    "Failed to initialize Moe pool"
                );
                init_errors.push(format!("{addr}: {err}"));
            }
        }
    }

    if !init_errors.is_empty() {
        return Err(eyre!(
            "fail-closed: {}/{} Moe pools failed to initialize: {}",
            init_errors.len(),
            expected,
            init_errors.join("; ")
        ));
    }
    if pools.len() != expected || pools.is_empty() {
        return Err(eyre!(
            "fail-closed: expected {expected} initialized Moe pools, got {}",
            pools.len()
        ));
    }

    // Sync bin data for all pools using batched approach
    {
        info!(
            target: "moe.init",
            radius = BINS_RADIUS,
            batch_size = BINS_BATCH_SIZE,
            "Syncing bin data for active bins"
        );
        let mut pool_vec: Vec<AMM> = pools.values().cloned().map(AMM::MoeLbPair).collect();
        let header = provider
            .get_block_by_number(block_number.into())
            .await?
            .ok_or_else(|| eyre!("missing block {block_number}"))?;
        let context = MoeSnapshotContext::new(header.header().hash(), header.header().timestamp);
        sync_moe_snapshots_at_context(
            &mut pool_vec,
            provider.clone(),
            context,
            BINS_RADIUS,
            BINS_BATCH_SIZE,
        )
        .await
        .context("Failed to sync Moe snapshot")?;
        for amm in pool_vec {
            if let AMM::MoeLbPair(p) = amm {
                pools.insert(p.address, p);
            }
        }
        info!(target: "moe.init", "Bin data synced successfully");

        // Log bins coverage statistics
        let pools_with_bins = pools.values().filter(|p| !p.bins.is_empty()).count();
        info!(
            target: "moe.init",
            pools_with_bins,
            total_pools = pools.len(),
            coverage_percent = (pools_with_bins as f64 / pools.len() as f64 * 100.0),
            "Bins data coverage: {}/{} pools ({:.1}%)",
            pools_with_bins,
            pools.len(),
            pools_with_bins as f64 / pools.len() as f64 * 100.0
        );
    }

    Ok(())
}

fn apply_logs(
    pools: &mut HashMap<Address, MoeLbPair>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
) -> Result<HashSet<Address>> {
    let mut changed = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let before_active_id = pool.active_id;
            let before_rx = pool.reserve_x;
            let before_ry = pool.reserve_y;

            match log.topics()[0] {
                sig if sig == IMoeLBPairEvents::Swap::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Swap)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "swap", pool)?;
                    info!(
                        target: "moe.pool",
                        address = %addr,
                        event = "Swap",
                        active_id_from = before_active_id,
                        active_id_to = pool.active_id,
                        rx_from = before_rx,
                        rx_to = pool.reserve_x,
                        ry_from = before_ry,
                        ry_to = pool.reserve_y,
                        "Applied swap event"
                    );
                    changed.insert(addr);
                }
                sig if sig == IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Deposit)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "deposit", pool)?;
                    info!(
                        target: "moe.pool",
                        address = %addr,
                        event = "DepositedToBins",
                        "Applied deposit event"
                    );
                    changed.insert(addr);
                }
                sig if sig == IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Withdraw)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "withdraw", pool)?;
                    info!(
                        target: "moe.pool",
                        address = %addr,
                        event = "WithdrawnFromBins",
                        "Applied withdraw event"
                    );
                    changed.insert(addr);
                }
                _ => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Unknown)");
                    }
                    log_pool_state(log_path, block_number, "unknown", pool)?;
                }
            }
        }
    }

    if !changed.is_empty() {
        info!(target: "moe.pool", block = block_number, changed = changed.len(), "Updated Moe pools");
    }

    Ok(changed)
}

fn log_path_simulations(
    pools: &HashMap<Address, MoeLbPair>,
    block_number: u64,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
) -> Result<()> {
    if pools.is_empty() {
        return Ok(());
    }

    // Rebuild state pools on-demand for simulation
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::MoeLbPair(pool.clone()));
    }
    let state_pools: Vec<AMM> = state.state.values().cloned().collect();

    // Filter cached paths to only those touching changed pools
    let mut candidate_indices: HashSet<usize> = HashSet::new();
    for changed in changed_pools {
        if let Some(indices) = path_cache.pool_to_path_indices.get(changed) {
            for &idx in indices {
                candidate_indices.insert(idx);
            }
        }
    }
    if candidate_indices.is_empty() {
        return Ok(());
    }

    let mut path_entries: Vec<(usize, &String, &ArbitragePath)> = candidate_indices
        .iter()
        .map(|&idx| (idx, &path_cache.signatures[idx], &path_cache.paths[idx]))
        .collect();
    path_entries.sort_by(|a, b| a.1.cmp(b.1));

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_positive_path_simulations.csv"));
    let mut positive_candidates = Vec::new();

    const MIN_INPUT: u128 = 1_000_000_000_000;
    const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000;

    // Parallelize path simulation
    let sim_results: Vec<Option<PositiveCandidate>> = path_entries
        .par_iter()
        .map(|(original_idx, signature, path)| {
            let pools_for_path = match pools_for_path(path, &state_pools) {
                Ok(p) => p,
                Err(err) => {
                    error!(target: "moe.monitor", error = ?err, "Failed to gather pools for path simulation");
                    return None;
                }
            };

            let best_simulation = best_path_simulation(
                path,
                &pools_for_path,
                U256::from(MIN_INPUT),
                U256::from(MAX_INPUT),
            );

            if let Some((input, output, profit)) = best_simulation {
                let roi_str = format_roi_percent(profit, input).unwrap_or_else(|| "-".to_string());
                let hops = hops_description(path);
                if profit > I256::ZERO {
                    let gas_config = GasConfig::default();
                    let num_hops = path.hops.len();
                    let profit_u256 = U256::from_limbs(*profit.as_limbs());
                    if gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.2) {
                        return Some(PositiveCandidate {
                            index: *original_idx,
                            profit,
                            input,
                            output,
                            roi: roi_str,
                            signature: (*signature).clone(),
                            hops,
                            pools: path.hops.iter().map(|hop| hop.pool_address).collect(),
                        });
                    }
                }
            }
            None
        })
        .collect();

    for item in sim_results.into_iter().flatten() {
        positive_candidates.push(item);
    }

    if !positive_candidates.is_empty() {
        ensure_log_headers(
            &positive_sim_log_path,
            &[
                "block_number",
                "path_index",
                "path_signature",
                "input_amount",
                "output_amount",
                "profit",
                "roi_percent",
                "hops",
            ],
        )?;

        let mut best_by_signature: HashMap<String, PositiveCandidate> = HashMap::new();
        for candidate in positive_candidates.into_iter() {
            if let Some(existing) = best_by_signature.get_mut(&candidate.signature) {
                if candidate.profit > existing.profit {
                    *existing = candidate;
                }
            } else {
                best_by_signature.insert(candidate.signature.clone(), candidate);
            }
        }

        let unique_candidates: Vec<PositiveCandidate> = best_by_signature.into_values().collect();

        // Filter candidates to only include new paths or paths with significant changes
        const PROFIT_CHANGE_THRESHOLD: f64 = 5.0;
        let candidates_to_log: Vec<&PositiveCandidate> = unique_candidates
            .iter()
            .filter(|candidate| should_log_path(candidate, PROFIT_CHANGE_THRESHOLD))
            .collect();

        if !candidates_to_log.is_empty() {
            let mut writer = WriterBuilder::new().has_headers(false).from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&positive_sim_log_path)?,
            );

            for candidate in &candidates_to_log {
                let mut record = StringRecord::new();
                record.push_field(&block_number.to_string());
                record.push_field(&candidate.index.to_string());
                record.push_field(&candidate.signature);
                record.push_field(&candidate.input.to_string());
                record.push_field(&candidate.output.to_string());
                record.push_field(&candidate.profit.to_string());
                record.push_field(&candidate.roi);
                record.push_field(&candidate.hops);

                writer.write_record(&record)?;
            }
            writer.flush()?;

            // Update the logged paths cache with newly logged candidates
            update_logged_paths(
                &candidates_to_log
                    .iter()
                    .map(|&c| c.clone())
                    .collect::<Vec<_>>(),
            );

            info!(
                target: "moe.monitor.csv",
                block = block_number,
                logged_count = candidates_to_log.len(),
                total_count = unique_candidates.len(),
                "Logged {} new/changed paths out of {} total unique paths",
                candidates_to_log.len(),
                unique_candidates.len()
            );
        } else {
            info!(
                target: "moe.monitor.csv",
                block = block_number,
                total_count = unique_candidates.len(),
                "No new or significantly changed paths to log (all {} paths already recorded)",
                unique_candidates.len()
            );
        }

        let mut selected_indices = select_best_non_conflicting_paths(&unique_candidates);
        selected_indices.sort_by(|&a, &b| {
            unique_candidates[b]
                .profit
                .cmp(&unique_candidates[a].profit)
        });

        let total_profit = selected_indices
            .iter()
            .fold(I256::ZERO, |acc, &idx| acc + unique_candidates[idx].profit);
        let selected_path_indices: Vec<usize> = selected_indices
            .iter()
            .map(|&idx| unique_candidates[idx].index)
            .collect();
        let selected_signatures: Vec<String> = selected_indices
            .iter()
            .map(|&idx| unique_candidates[idx].signature.clone())
            .collect();

        let snapshot = SelectionSnapshot {
            signatures: selected_signatures,
            profits: selected_indices
                .iter()
                .map(|&idx| unique_candidates[idx].profit)
                .collect(),
        };

        let mutex = LAST_SELECTION.get_or_init(|| Mutex::new(None));
        let mut last_snapshot = mutex.lock().unwrap();

        let is_same_selection = last_snapshot.as_ref() == Some(&snapshot);

        if !selected_indices.is_empty() {
            // Always write the best (top-1) arbitrage path per block to CSV
            let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("logs/moe_best_arbitrage_paths.csv"));
            ensure_log_headers(
                &best_paths_log_path,
                &[
                    "block_number",
                    "path_index",
                    "path_signature",
                    "input_amount",
                    "output_amount",
                    "profit",
                    "roi_percent",
                    "hops",
                ],
            )?;

            let best_idx = selected_indices[0];
            let best = &unique_candidates[best_idx];
            let mut best_writer = WriterBuilder::new().has_headers(false).from_writer(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&best_paths_log_path)?,
            );
            let mut best_record = StringRecord::new();
            best_record.push_field(&block_number.to_string());
            best_record.push_field(&best.index.to_string());
            best_record.push_field(&best.signature);
            best_record.push_field(&best.input.to_string());
            best_record.push_field(&best.output.to_string());
            best_record.push_field(&best.profit.to_string());
            best_record.push_field(&best.roi);
            best_record.push_field(&best.hops);
            best_writer.write_record(&best_record)?;
            best_writer.flush()?;

            let summary_message = if is_same_selection {
                "Arbitrage selection unchanged from previous block"
            } else {
                "Selected optimal non-conflicting arbitrage paths for block"
            };

            info!(
                target: "moe.monitor.arb.summary",
                block = block_number,
                selected_paths = selected_indices.len(),
                path_indices = ?selected_path_indices,
                total_profit = %total_profit,
                "{summary_message}"
            );

            if !is_same_selection {
                for &idx in &selected_indices {
                    let candidate = &unique_candidates[idx];
                    info!(
                        target: "moe.monitor.arb",
                        block = block_number,
                        path_index = candidate.index,
                        optimal_input = %candidate.input,
                        output_amount = %candidate.output,
                        profit = %candidate.profit,
                        roi = %candidate.roi,
                        path = %candidate.signature,
                        "Profitable arbitrage path detected in simulation"
                    );
                }
            }

            if !is_same_selection {
                *last_snapshot = Some(snapshot);
            }
        } else {
            info!(
                target: "moe.monitor.arb.summary",
                block = block_number,
                selected_paths = 0usize,
                path_indices = ?selected_path_indices,
                total_profit = %total_profit,
                "No profitable arbitrage paths for block"
            );

            if !is_same_selection {
                *last_snapshot = Some(snapshot);
            }
        }
    }

    Ok(())
}

fn best_path_simulation(
    path: &ArbitragePath,
    pools: &[AMM],
    min_input: U256,
    max_input: U256,
) -> Option<(U256, U256, I256)> {
    if min_input.is_zero() || max_input.is_zero() || min_input > max_input {
        return None;
    }

    let evaluate = |amount: U256| -> Option<(U256, U256, I256)> {
        if amount < min_input || amount > max_input {
            return None;
        }
        simulate_path_raw(path, pools, amount)
            .ok()
            .map(|(output, profit)| (amount, output, profit))
    };

    let mut candidates = Vec::with_capacity(3);
    candidates.push(min_input);
    if max_input > min_input {
        candidates.push(max_input);
        candidates.push(min_input + (max_input - min_input) / U256::from(2));
    }

    let mut best: Option<(U256, U256, I256)> = None;
    for amount in candidates.into_iter().filter(|a| *a >= min_input) {
        if let Some(candidate) = evaluate(amount) {
            match &best {
                Some((_, _, best_profit)) if candidate.2 <= *best_profit => {}
                _ => best = Some(candidate),
            }
        }
    }

    let mut best = best?;
    let mut current_input = best.0;
    let range = max_input - min_input;
    if range.is_zero() {
        return Some(best);
    }

    let mut step = range / U256::from(4);
    if step.is_zero() {
        step = U256::from(1);
    }

    const MAX_ITERATIONS: usize = 64;
    for _ in 0..MAX_ITERATIONS {
        if step.is_zero() {
            break;
        }

        let mut improved = false;

        if let Some(next_input) = current_input.checked_add(step) {
            if next_input <= max_input {
                if let Some(candidate) = evaluate(next_input) {
                    if candidate.2 > best.2 {
                        best = candidate;
                        current_input = best.0;
                        improved = true;
                    }
                }
            }
        }

        if !improved {
            if let Some(prev_input) = current_input.checked_sub(step) {
                if prev_input >= min_input {
                    if let Some(candidate) = evaluate(prev_input) {
                        if candidate.2 > best.2 {
                            best = candidate;
                            current_input = best.0;
                            improved = true;
                        }
                    }
                }
            }
        }

        if !improved {
            step = step.checked_div(U256::from(2)).unwrap_or(U256::ZERO);
            if step.is_zero() {
                break;
            }
        }
    }

    Some(best)
}

fn simulate_path_raw(path: &ArbitragePath, pools: &[AMM], amount_in: U256) -> Result<(U256, I256)> {
    if path.hops.is_empty() {
        return Ok((U256::ZERO, I256::ZERO));
    }

    let mut current = amount_in;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        current = amm.simulate_swap(hop.token_in, hop.token_out, current)?;
    }
    let profit = I256::from_raw(current) - I256::from_raw(amount_in);
    Ok((current, profit))
}

fn ensure_log_headers(path: &Path, headers: &[&str]) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let metadata = file.metadata()?;
    drop(file);

    if metadata.len() == 0 {
        let mut writer = WriterBuilder::new()
            .has_headers(false)
            .from_writer(OpenOptions::new().create(true).append(true).open(path)?);
        writer.write_record(headers)?;
        writer.flush()?;
    }

    Ok(())
}

fn log_pool_state(path: &Path, block_number: u64, event: &str, pool: &MoeLbPair) -> Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    writer.write_record([
        block_number.to_string(),
        format!("{:#x}", pool.address()),
        event.to_owned(),
        pool.active_id.to_string(),
        pool.reserve_x.to_string(),
        pool.reserve_y.to_string(),
    ])?;
    writer.flush()?;

    Ok(())
}

fn should_log_path(candidate: &PositiveCandidate, threshold_percent: f64) -> bool {
    let logged_paths_mutex = LOGGED_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
    let logged_paths = logged_paths_mutex.lock().unwrap();

    if let Some(last_record) = logged_paths.get(&candidate.signature) {
        let profit_diff = (candidate.profit - last_record.profit).abs();
        let last_profit_abs = last_record.profit.abs();

        if last_profit_abs.is_zero() {
            return !candidate.profit.is_zero();
        }

        let profit_diff_f64 = profit_diff.to_string().parse::<f64>().unwrap_or(0.0);
        let last_profit_f64 = last_profit_abs.to_string().parse::<f64>().unwrap_or(1.0);
        let change_percent = (profit_diff_f64 / last_profit_f64) * 100.0;

        change_percent >= threshold_percent
    } else {
        true
    }
}

fn update_logged_paths(candidates: &[PositiveCandidate]) {
    let logged_paths_mutex = LOGGED_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut logged_paths = logged_paths_mutex.lock().unwrap();

    for candidate in candidates {
        logged_paths.insert(
            candidate.signature.clone(),
            LoggedPathRecord {
                signature: candidate.signature.clone(),
                profit: candidate.profit,
                input: candidate.input,
                output: candidate.output,
                roi: candidate.roi.clone(),
            },
        );
    }
}

fn format_roi_percent(profit: I256, input: U256) -> Option<String> {
    if input.is_zero() {
        return None;
    }

    let profit_f64 = profit.to_string().parse::<f64>().ok()?;
    let input_f64 = input.to_string().parse::<f64>().ok()?;
    if input_f64.abs() < f64::EPSILON {
        return None;
    }

    let ratio = (profit_f64 / input_f64) * 100.0;
    Some(format!("{ratio:.4}"))
}

#[allow(dead_code)]
fn build_token_path(path: &ArbitragePath) -> Vec<Address> {
    let mut tokens = Vec::with_capacity(path.hops.len() + 1);
    if let Some(first) = path.hops.first() {
        tokens.push(first.token_in);
        for hop in &path.hops {
            tokens.push(hop.token_out);
        }
    }
    tokens
}

fn path_signature(path: &ArbitragePath) -> String {
    path.hops
        .iter()
        .map(|hop| {
            format!(
                "{:#x}->{:#x}@{:#x}(fee_bps={})",
                hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn hops_description(path: &ArbitragePath) -> String {
    path.hops
        .iter()
        .map(|hop| {
            format!(
                "{:#x}->{:#x}@{:#x}(fee_bps={})",
                hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
