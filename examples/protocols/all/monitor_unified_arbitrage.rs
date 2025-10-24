use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::primitives::{address, Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::sol_types::SolEvent;
use alloy::transports::ws::WsConnect;
use amms::amms::{
    agni::{AgniPool, IAgniPoolEvents},
    amm::{AutomatedMarketMaker, AMM},
    moe::{sync_active_bins_batch, sync_slot0_batch, IMoeLBPairEvents, MoeLbPair},
};
use amms::arbitrage::{
    gas::GasConfig,
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::state_space::StateSpace;
use csv::{ReaderBuilder, StringRecord, WriterBuilder};
use eyre::{Context, Result};
use futures::{stream, StreamExt};
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};
use tracing::{debug, error, info, warn};

const MAX_HOPS: usize = 4;
const WMNT_ADDRESS: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const BINS_RADIUS: u32 = 50;
const BINS_BATCH_SIZE: u32 = 15;
const PROFIT_CHANGE_THRESHOLD: f64 = 5.0;

#[derive(Debug, Deserialize)]
struct PoolRow {
    #[serde(rename = "Protocol")]
    protocol: String,
    #[serde(rename = "Pair Name")]
    #[allow(dead_code)]
    pair_name: String,
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "TokenA Address")]
    #[allow(dead_code)]
    token_a_address: String,
    #[serde(rename = "TokenB Address")]
    #[allow(dead_code)]
    token_b_address: String,
    #[serde(rename = "Fee Tier")]
    fee_tier: Option<u32>,
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

/// Check if a Moe pool has reasonable reserves for arbitrage
fn is_moe_pool_reasonable(pool: &MoeLbPair) -> bool {
    if pool.reserve_x == 0 || pool.reserve_y == 0 {
        warn!(
            target: "unified.filter",
            address = %pool.address,
            reserve_x = pool.reserve_x,
            reserve_y = pool.reserve_y,
            "Moe pool has zero reserves, excluding from arbitrage"
        );
        return false;
    }
    true
}

fn build_path_cache(
    agni_pools: &HashMap<Address, AgniPool>,
    moe_pools: &HashMap<Address, MoeLbPair>,
    max_hops: usize,
) -> PathCache {
    let mut state = StateSpace::default();
    let mut filtered_count = 0;

    // Add Agni/FusionX pools
    for pool in agni_pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }

    // Add Moe pools (filter out zero-reserve pools)
    for pool in moe_pools.values() {
        if is_moe_pool_reasonable(pool) {
            state
                .state
                .insert(pool.address(), AMM::MoeLbPair(pool.clone()));
        } else {
            filtered_count += 1;
        }
    }

    if filtered_count > 0 {
        info!(
            target: "unified.monitor",
            filtered = filtered_count,
            total_moe = moe_pools.len(),
            "Filtered {} Moe pools with zero reserves from arbitrage",
            filtered_count
        );
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(target: "unified.monitor", error = ?err, "Failed to build arbitrage graph");
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

    let ws_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());
    info!(target: "unified.monitor", ws = %ws_endpoint, "Using WebSocket endpoint");

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_endpoint))
        .await?;

    info!(target: "unified.monitor", max_hops = MAX_HOPS, "Using max hops for path search");

    run_service(provider).await
}

async fn run_service<P>(provider: P) -> Result<()>
where
    P: Provider + Clone,
{
    let pool_log_path = std::env::var("POOL_UPDATE_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/unified_pool_updates.csv"));
    ensure_log_headers(
        &pool_log_path,
        &[
            "block_number",
            "pool_address",
            "protocol",
            "event",
            "field1",
            "field2",
            "field3",
        ],
    )?;

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/unified_positive_path_simulations.csv"));
    let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/unified_best_arbitrage_paths.csv"));
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

    let latest_block = provider.get_block_number().await?;
    let latest_block_id = BlockId::from(latest_block);

    let mut agni_pools: HashMap<Address, AgniPool> = HashMap::new();
    let mut moe_pools: HashMap<Address, MoeLbPair> = HashMap::new();

    initialize_pools(&provider, latest_block_id, &mut agni_pools, &mut moe_pools).await?;

    if agni_pools.is_empty() && moe_pools.is_empty() {
        warn!(target: "unified.monitor", "No pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "unified.monitor",
        agni_pools = agni_pools.len(),
        moe_pools = moe_pools.len(),
        total_pools = agni_pools.len() + moe_pools.len(),
        "Initialized pools"
    );

    // Log initial pool states
    for pool in agni_pools.values() {
        log_agni_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }
    for pool in moe_pools.values() {
        log_moe_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }

    let path_cache = build_path_cache(&agni_pools, &moe_pools, MAX_HOPS);

    // Build combined event filter
    let mut agni_events = vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ];
    let moe_events = vec![
        IMoeLBPairEvents::Swap::SIGNATURE_HASH,
        IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH,
        IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH,
    ];
    agni_events.extend_from_slice(&moe_events);

    let mut filter = Filter::new().event_signature(FilterSet::from(agni_events));

    let mut all_pool_addresses: Vec<Address> = agni_pools.keys().copied().collect();
    all_pool_addresses.extend(moe_pools.keys().copied());
    filter = filter.address(all_pool_addresses);

    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target: "unified.monitor", "Subscribed to blocks over WS");
    info!(target: "unified.monitor", candidate_paths = path_cache.paths.len(), "Pre-computed arbitrage candidate paths");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "unified.monitor.block", block = target_number, "Processing block");

        let windowed = filter.clone().select(target_number);
        match provider.get_logs(&windowed).await {
            Ok(logs) => {
                info!(target: "unified.monitor.block", block = target_number, logs = logs.len(), "Fetched logs");
                if logs.is_empty() {
                    continue;
                }

                let changed = apply_logs(
                    &mut agni_pools,
                    &mut moe_pools,
                    &logs,
                    target_number,
                    &pool_log_path,
                )?;

                // Resync Moe pools if changed
                if !changed.is_empty() {
                    let moe_changed: Vec<Address> = changed
                        .iter()
                        .filter(|&addr| moe_pools.contains_key(addr))
                        .copied()
                        .collect();

                    if !moe_changed.is_empty() {
                        let mut pools_to_resync: Vec<AMM> = moe_changed
                            .iter()
                            .filter_map(|addr| moe_pools.get(addr).map(|p| AMM::MoeLbPair(p.clone())))
                            .collect();

                        if !pools_to_resync.is_empty() {
                            let block_id = BlockId::from(target_number);

                            // Resync reserves
                            if let Err(e) = sync_slot0_batch(&mut pools_to_resync, block_id, provider.clone()).await {
                                warn!(
                                    target: "unified.monitor.block",
                                    block = target_number,
                                    error = ?e,
                                    "Failed to resync Moe reserves after events"
                                );
                            }

                            // Resync bins
                            if let Err(e) = sync_bins_in_batches(
                                &mut pools_to_resync,
                                block_id,
                                provider.clone(),
                                BINS_RADIUS,
                                BINS_BATCH_SIZE,
                            )
                            .await
                            {
                                warn!(
                                    target: "unified.monitor.block",
                                    block = target_number,
                                    error = ?e,
                                    "Failed to resync Moe bins after events"
                                );
                            } else {
                                // Update moe_pools with resynced data
                                for amm in pools_to_resync {
                                    if let AMM::MoeLbPair(p) = amm {
                                        moe_pools.insert(p.address, p);
                                    }
                                }
                            }
                        }
                    }

                    log_path_simulations(
                        &agni_pools,
                        &moe_pools,
                        target_number,
                        &path_cache,
                        &changed,
                    )?;
                }
            }
            Err(e) => {
                error!(target: "unified.monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

async fn sync_bins_in_batches<P: Provider + Clone>(
    amms: &mut Vec<AMM>,
    block_id: BlockId,
    provider: P,
    radius: u32,
    batch_size: u32,
) -> Result<()> {
    let original_active_ids: Vec<u32> = amms
        .iter()
        .map(|amm| {
            if let AMM::MoeLbPair(pair) = amm {
                pair.active_id
            } else {
                0
            }
        })
        .collect();

    let total_range = radius * 2;
    let num_batches = (total_range + batch_size - 1) / batch_size;

    debug!(
        target: "unified.sync",
        radius,
        batch_size,
        num_batches,
        "Syncing bins in {} batches",
        num_batches
    );

    for batch_idx in 0..num_batches {
        let center_offset = batch_idx * batch_size;
        let center_offset_signed = center_offset as i32 - radius as i32 + batch_size as i32;

        for (idx, amm) in amms.iter_mut().enumerate() {
            if let AMM::MoeLbPair(pair) = amm {
                if center_offset_signed >= 0 {
                    pair.active_id = original_active_ids[idx].saturating_add(center_offset_signed as u32);
                } else {
                    pair.active_id = original_active_ids[idx].saturating_sub((-center_offset_signed) as u32);
                }
            }
        }

        sync_active_bins_batch(amms, block_id, provider.clone(), batch_size).await?;
    }

    // Restore original active_ids
    for (idx, amm) in amms.iter_mut().enumerate() {
        if let AMM::MoeLbPair(pair) = amm {
            pair.active_id = original_active_ids[idx];
        }
    }

    Ok(())
}

async fn initialize_pools<P: Provider + Clone>(
    provider: &P,
    block_id: BlockId,
    agni_pools: &mut HashMap<Address, AgniPool>,
    moe_pools: &mut HashMap<Address, MoeLbPair>,
) -> Result<()> {
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists_all.csv");

    if !csv_path.exists() {
        return Err(eyre::eyre!("CSV file not found: {}", csv_path.display()));
    }

    let file = File::open(&csv_path)
        .with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    let mut agni_init_jobs = Vec::new();
    let mut moe_init_jobs = Vec::new();

    for row in rdr.deserialize::<PoolRow>() {
        let row = row?;
        let addr = Address::from_str(row.pair_address.trim()).context("Invalid pool address")?;

        let protocol_lower = row.protocol.to_lowercase();
        if protocol_lower.contains("agni") || protocol_lower.contains("fusionx") {
            agni_init_jobs.push((addr, row.fee_tier));
        } else if protocol_lower.contains("moe") {
            moe_init_jobs.push(addr);
        }
    }

    info!(
        target: "unified.init",
        agni_jobs = agni_init_jobs.len(),
        moe_jobs = moe_init_jobs.len(),
        "Initializing pools"
    );

    // Initialize Agni/FusionX pools
    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut agni_stream = stream::iter(agni_init_jobs.into_iter().map(|(addr, fee_tier)| {
        let provider = provider.clone();
        async move {
            let result = AgniPool::new(addr).init_basic(block_id, provider).await;
            (addr, fee_tier, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, fee_tier, result)) = agni_stream.next().await {
        match result {
            Ok(pool) => {
                if let Some(csv_fee) = fee_tier {
                    if pool.fee != csv_fee {
                        warn!(
                            target: "unified.init",
                            address = %addr,
                            csv_fee,
                            pool_fee = pool.fee,
                            "Fee tier mismatch"
                        );
                    }
                }
                info!(target: "unified.init", address = %addr, fee = pool.fee, protocol = "Agni/FusionX", "Initialized pool");
                agni_pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "unified.init",
                    address = %addr,
                    error = %err,
                    "Failed to initialize Agni/FusionX pool"
                );
            }
        }
    }

    // Initialize Moe pools
    let mut moe_stream = stream::iter(moe_init_jobs.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let result = MoeLbPair::new(addr).init_basic(block_id, provider).await;
            (addr, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, result)) = moe_stream.next().await {
        match result {
            Ok(pool) => {
                info!(target: "unified.init", address = %addr, bin_step = pool.bin_step, protocol = "Moe", "Initialized pool");
                moe_pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "unified.init",
                    address = %addr,
                    error = %err,
                    "Failed to initialize Moe pool"
                );
            }
        }
    }

    // Sync bin data for Moe pools
    if !moe_pools.is_empty() {
        info!(
            target: "unified.init",
            radius = BINS_RADIUS,
            batch_size = BINS_BATCH_SIZE,
            "Syncing bin data for Moe pools"
        );
        let mut pool_vec: Vec<AMM> = moe_pools.values().cloned().map(AMM::MoeLbPair).collect();
        if let Err(e) = sync_bins_in_batches(
            &mut pool_vec,
            block_id,
            provider.clone(),
            BINS_RADIUS,
            BINS_BATCH_SIZE,
        )
        .await
        {
            error!(target: "unified.init", error = ?e, "Failed to sync Moe bin data");
        } else {
            for amm in pool_vec {
                if let AMM::MoeLbPair(p) = amm {
                    moe_pools.insert(p.address, p);
                }
            }
            info!(target: "unified.init", "Moe bin data synced successfully");
        }

        let pools_with_bins = moe_pools.values().filter(|p| !p.bins.is_empty()).count();
        info!(
            target: "unified.init",
            pools_with_bins,
            total_moe_pools = moe_pools.len(),
            coverage_percent = (pools_with_bins as f64 / moe_pools.len() as f64 * 100.0),
            "Moe bins data coverage: {}/{} pools ({:.1}%)",
            pools_with_bins,
            moe_pools.len(),
            pools_with_bins as f64 / moe_pools.len() as f64 * 100.0
        );
    }

    Ok(())
}

fn apply_logs(
    agni_pools: &mut HashMap<Address, AgniPool>,
    moe_pools: &mut HashMap<Address, MoeLbPair>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
) -> Result<HashSet<Address>> {
    let mut changed = HashSet::new();

    for log in logs {
        let addr = log.address();
        let sig = log.topics()[0];

        // Try Agni pools first
        if let Some(pool) = agni_pools.get_mut(&addr) {
            if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Agni Swap)");
                    continue;
                }
                log_agni_pool_state(log_path, block_number, "swap", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Agni",
                    event = "Swap",
                    "Applied event"
                );
                changed.insert(addr);
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Agni Mint)");
                    continue;
                }
                log_agni_pool_state(log_path, block_number, "mint", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Agni",
                    event = "Mint",
                    "Applied event"
                );
                changed.insert(addr);
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Agni Burn)");
                    continue;
                }
                log_agni_pool_state(log_path, block_number, "burn", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Agni",
                    event = "Burn",
                    "Applied event"
                );
                changed.insert(addr);
            }
        }
        // Try Moe pools
        else if let Some(pool) = moe_pools.get_mut(&addr) {
            if sig == IMoeLBPairEvents::Swap::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Moe Swap)");
                    continue;
                }
                log_moe_pool_state(log_path, block_number, "swap", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Moe",
                    event = "Swap",
                    "Applied event"
                );
                changed.insert(addr);
            } else if sig == IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Moe Deposit)");
                    continue;
                }
                log_moe_pool_state(log_path, block_number, "deposit", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Moe",
                    event = "DepositedToBins",
                    "Applied event"
                );
                changed.insert(addr);
            } else if sig == IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "unified.pool", address = %addr, error = ?e, "sync error (Moe Withdraw)");
                    continue;
                }
                log_moe_pool_state(log_path, block_number, "withdraw", pool)?;
                info!(
                    target: "unified.pool",
                    address = %addr,
                    protocol = "Moe",
                    event = "WithdrawnFromBins",
                    "Applied event"
                );
                changed.insert(addr);
            }
        }
    }

    if !changed.is_empty() {
        info!(target: "unified.pool", block = block_number, changed = changed.len(), "Updated pools");
    }

    Ok(changed)
}

fn log_path_simulations(
    agni_pools: &HashMap<Address, AgniPool>,
    moe_pools: &HashMap<Address, MoeLbPair>,
    block_number: u64,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
) -> Result<()> {
    if agni_pools.is_empty() && moe_pools.is_empty() {
        return Ok(());
    }

    // Rebuild state pools on-demand for simulation
    let mut state = StateSpace::default();
    for pool in agni_pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }
    for pool in moe_pools.values() {
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
        .unwrap_or_else(|_| PathBuf::from("logs/unified_positive_path_simulations.csv"));
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
                    error!(target: "unified.monitor", error = ?err, "Failed to gather pools for path simulation");
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

            update_logged_paths(
                &candidates_to_log
                    .iter()
                    .map(|&c| c.clone())
                    .collect::<Vec<_>>(),
            );

            info!(
                target: "unified.monitor.csv",
                block = block_number,
                logged_count = candidates_to_log.len(),
                total_count = unique_candidates.len(),
                "Logged {} new/changed paths out of {} total unique paths",
                candidates_to_log.len(),
                unique_candidates.len()
            );
        } else {
            info!(
                target: "unified.monitor.csv",
                block = block_number,
                total_count = unique_candidates.len(),
                "No new or significantly changed paths to log"
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
            let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("logs/unified_best_arbitrage_paths.csv"));

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
                target: "unified.monitor.arb.summary",
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
                        target: "unified.monitor.arb",
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
                target: "unified.monitor.arb.summary",
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

fn log_agni_pool_state(
    path: &Path,
    block_number: u64,
    event: &str,
    pool: &AgniPool,
) -> Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    writer.write_record([
        block_number.to_string(),
        format!("{:#x}", pool.address()),
        "Agni".to_owned(),
        event.to_owned(),
        pool.sqrt_price.to_string(),
        pool.liquidity.to_string(),
        pool.tick.to_string(),
    ])?;
    writer.flush()?;

    Ok(())
}

fn log_moe_pool_state(path: &Path, block_number: u64, event: &str, pool: &MoeLbPair) -> Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    writer.write_record([
        block_number.to_string(),
        format!("{:#x}", pool.address()),
        "Moe".to_owned(),
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

