use alloy::consensus::BlockHeader;
use alloy::primitives::{address, Address, I256, U256};
use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, FilterSet, Log},
    sol_types::SolEvent,
    transports::ws::WsConnect,
};
use amms::amms::{
    agni::{AgniPool, IAgniPoolEvents},
    amm::{AutomatedMarketMaker, AMM},
};
use amms::arbitrage::optimizer::pools_for_path;
use amms::arbitrage::{
    gas::GasConfig,
    graph::build_graph,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::state_space::StateSpace;
use csv::{ReaderBuilder, StringRecord, WriterBuilder};
use eyre::Report;
use eyre::WrapErr;
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::{error, info, warn};
use rayon::prelude::*;
fn build_path_cache(
    pools: &HashMap<Address, AgniPool>,
    fee_tiers: &HashMap<Address, Option<u32>>,
    max_hops: usize,
) -> PathCache {
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(target: "monitor", error = ?err, "Failed to build arbitrage graph");
            return PathCache {
                paths: Vec::new(),
                signatures: Vec::new(),
                pool_to_path_indices: HashMap::new(),
            };
        }
    };

    let constraints = PathConstraints {
        max_length: max_hops,
        required_start_token: Some(address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
        required_end_token: Some(address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
        ..PathConstraints::default()
    };
    let finder = PathFinder::new(&graph, constraints);

    let paths_iter = finder
        .find_cycles()
        .into_iter()
        .chain(finder.find_two_pool_misprices());

    let mut unique_paths: HashMap<String, ArbitragePath> = HashMap::new();
    let matches_fee = |path: &ArbitragePath| {
        path.hops
            .iter()
            .all(|hop| match fee_tiers.get(&hop.pool_address) {
                Some(Some(expected_fee)) => pools
                    .get(&hop.pool_address)
                    .map(|pool| pool.fee == *expected_fee)
                    .unwrap_or(false),
                Some(None) | None => true,
            })
    };

    for path in paths_iter {
        if matches_fee(&path) {
            let signature = path_signature(&path);
            unique_paths.entry(signature).or_insert(path);
        }
    }

    let mut signatures_paths: Vec<(String, ArbitragePath)> = unique_paths.into_iter().collect();
    signatures_paths.sort_by(|a, b| a.0.cmp(&b.0));

    let signatures: Vec<String> = signatures_paths.iter().map(|(s, _)| s.clone()).collect();
    let paths: Vec<ArbitragePath> = signatures_paths.into_iter().map(|(_, p)| p).collect();

    let mut pool_to_path_indices: HashMap<Address, Vec<usize>> = HashMap::new();
    for (idx, path) in paths.iter().enumerate() {
        for hop in &path.hops {
            pool_to_path_indices.entry(hop.pool_address).or_default().push(idx);
        }
    }

    PathCache { paths, signatures, pool_to_path_indices }
}

#[derive(Debug, Deserialize)]
struct PoolRow {
    #[allow(dead_code)]
    Protocol: String,
    #[allow(dead_code)]
    #[serde(rename = "Pair Name")]
    Pair_Name: String,
    #[serde(rename = "Pair Address")]
    Pair_Address: String,
    #[allow(dead_code)]
    #[serde(rename = "TokenA Address")]
    TokenA_Address: String,
    #[allow(dead_code)]
    #[serde(rename = "TokenB Address")]
    TokenB_Address: String,
    #[allow(dead_code)]
    #[serde(rename = "Fee Tier")]
    Fee_Tier: Option<u32>,
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

#[derive(Clone, PartialEq, Eq)]
struct SelectionSnapshot {
    signatures: Vec<String>,
    profits: Vec<I256>,
}

#[derive(Clone, Debug)]
struct LoggedPathRecord {
    signature: String,
    profit: I256,
    input: U256,
    output: U256,
    roi: String,
}

static LAST_SELECTION: OnceLock<Mutex<Option<SelectionSnapshot>>> = OnceLock::new();
static LOGGED_PATHS: OnceLock<Mutex<HashMap<String, LoggedPathRecord>>> = OnceLock::new();

#[derive(Clone)]
struct PathCache {
    paths: Vec<ArbitragePath>,
    signatures: Vec<String>,
    pool_to_path_indices: HashMap<Address, Vec<usize>>, // pool -> indices in paths
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
async fn main() -> eyre::Result<()> {
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

    // WebSocket endpoint (fallback to a public Mantle WS if not provided)
    let ws_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());
    info!(target: "monitor", ws = %ws_endpoint, "Using WebSocket endpoint");

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_endpoint))
        .await?;

    let max_hops: usize = 4;
    info!(target: "monitor", max_hops, "Using max hops for path search");

    // Load pools from CSV
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    // Initialize pools
    let latest_block = BlockId::from(provider.get_block_number().await?);
    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();
    let pool_log_path = std::env::var("POOL_UPDATE_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/pool_updates.csv"));
    ensure_log_headers(
        &pool_log_path,
        &[
            "block_number",
            "pool_address",
            "event",
            "sqrt_price_x96",
            "liquidity",
            "tick",
        ],
    )?;
    let mut total_rows = 0usize;
    let mut agni_rows = 0usize;
    let mut init_jobs = Vec::new();

    for result in rdr.deserialize::<PoolRow>() {
        let row = result?;
        total_rows += 1;
        if !row.Protocol.to_lowercase().contains("agni") {
            continue;
        }
        agni_rows += 1;
        let addr = parse_address(&row.Pair_Address)?;
        init_jobs.push((addr, row.Fee_Tier));
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let provider_for_init = provider.clone();
    let block_for_init = latest_block;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|(addr, fee_tier)| {
        let provider = provider_for_init.clone();
        let block = block_for_init;
        async move {
            let result = AgniPool::new(addr).init_basic(block, provider).await;
            (addr, fee_tier, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, fee_tier, result)) = init_stream.next().await {
        match result {
            Ok(pool) => {
                if let Some(csv_fee) = fee_tier {
                    if pool.fee != csv_fee {
                        warn!(
                            target: "monitor",
                            address = ?addr,
                            csv_fee,
                            pool_fee = pool.fee,
                            "Fee tier mismatch"
                        );
                    }
                }
                info!(target: "monitor", address = ?addr, fee = pool.fee, "Initialized pool");
                log_pool_state(
                    &pool_log_path,
                    latest_block.as_u64().unwrap_or_default(),
                    "init",
                    &pool,
                )?;
                fee_tiers.insert(addr, fee_tier);
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "monitor",
                    address = ?addr,
                    error = ?err,
                    "Failed to initialize pool"
                );
            }
        }
    }

    if pools.is_empty() {
        info!(target: "monitor", total_rows, agni_rows, "No pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "monitor",
        loaded = pools.len(),
        total_rows,
        agni_rows,
        "Initialized Agni pools"
    );

    // Precompute arbitrage paths once and reuse
    let path_cache = build_path_cache(&pools, &fee_tiers, max_hops);

    // Build event filter for only these pools and only relevant events
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // Subscribe to new blocks over WS and fetch logs per block
    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target: "monitor", "Subscribed to blocks over WS");
    info!(target: "monitor", candidate_paths = path_cache.paths.len(), "Pre-computed arbitrage candidate paths");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        // Some RPC providers (e.g., Mantle public) may not have logs indexed for the tip yet.
        // Query the previous block to avoid "invalid block range params" errors.
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "monitor.block", block = target_number, "Processing block");
        let windowed = filter.clone().select(target_number);
        match provider.get_logs(&windowed).await {
            Ok(logs) => {
                info!(target: "monitor.block", block = target_number, logs = logs.len(), "Fetched logs");
                if logs.is_empty() {
                    continue;
                }
                let changed = apply_logs(&mut pools, &logs, target_number, &pool_log_path, &fee_tiers)?;
                if !changed.is_empty() {
                    log_path_simulations(&pools, target_number, &path_cache, &changed)?;
                }
            }
            Err(e) => {
                error!(target: "monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

fn parse_address(s: &str) -> eyre::Result<Address> {
    let s = s.trim();
    let addr = s.parse::<Address>()?;
    Ok(addr)
}

fn apply_logs(
    pools: &mut HashMap<Address, AgniPool>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
    _fee_tiers: &HashMap<Address, Option<u32>>,
) -> eyre::Result<HashSet<Address>> {
    let mut changed_pools: HashSet<Address> = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let before_tick = pool.tick;
            let before_liq = pool.liquidity;
            let before_sqrt = pool.sqrt_price;

            let sig = log.topics()[0];
            if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                match IAgniPoolEvents::Swap::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Swap)");
                            continue;
                        }
                        log_pool_state(log_path, block_number, "swap", pool)?;
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Swap",
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            sqrt_from = ?before_sqrt,
                            sqrt_to = ?pool.sqrt_price,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                        changed_pools.insert(addr);
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Swap failed");
                    }
                }
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                match IAgniPoolEvents::Mint::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Mint)");
                            continue;
                        }
                        log_pool_state(log_path, block_number, "mint", pool)?;
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Mint",
                            owner = ?e.owner,
                            tick_lower = ?e.tickLower,
                            tick_upper = ?e.tickUpper,
                            amount = e.amount,
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                        changed_pools.insert(addr);
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Mint failed");
                    }
                }
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                match IAgniPoolEvents::Burn::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Burn)");
                            continue;
                        }
                        log_pool_state(log_path, block_number, "burn", pool)?;
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Burn",
                            owner = ?e.owner,
                            tick_lower = ?e.tickLower,
                            tick_upper = ?e.tickUpper,
                            amount = e.amount,
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                        changed_pools.insert(addr);
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Burn failed");
                    }
                }
            } else {
                // Unknown event (should not happen due to filter)
                if let Err(e) = pool.sync(log) {
                    error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Unknown)");
                } else {
                    log_pool_state(log_path, block_number, "unknown", pool)?;
                    info!(
                        target: "monitor.pool",
                        address = ?addr,
                        event = "Unknown",
                        tick_from = before_tick,
                        tick_to = pool.tick,
                        liq_from = before_liq,
                        liq_to = pool.liquidity,
                        "Applied"
                    );
                    changed_pools.insert(addr);
                }
            }
        }
    }

    Ok(changed_pools)
}

fn ensure_log_headers(path: &Path, header: &[&str]) -> eyre::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let writer = OpenOptions::new().create(true).append(true).open(path)?;
    let metadata = writer.metadata()?;
    drop(writer);

    if metadata.len() == 0 {
        let mut csv = WriterBuilder::new()
            .has_headers(false)
            .from_writer(OpenOptions::new().create(true).append(true).open(path)?);
        csv.write_record(header)?;
        csv.flush()?;
    }

    Ok(())
}

fn log_pool_state(
    path: &Path,
    block_number: u64,
    event: &str,
    pool: &AgniPool,
) -> eyre::Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    writer.write_record([
        block_number.to_string(),
        format!("{:#x}", pool.address()),
        event.to_owned(),
        pool.sqrt_price.to_string(),
        pool.liquidity.to_string(),
        pool.tick.to_string(),
    ])?;
    writer.flush()?;

    Ok(())
}

/// Check if a path should be logged based on whether it's new or has significant changes
/// Returns true if the path should be logged
fn should_log_path(candidate: &PositiveCandidate, profit_change_threshold_percent: f64) -> bool {
    let logged_paths_mutex = LOGGED_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
    let logged_paths = logged_paths_mutex.lock().unwrap();

    if let Some(last_record) = logged_paths.get(&candidate.signature) {
        // Path exists, check if there's significant change
        // Calculate profit change percentage
        let profit_diff = (candidate.profit - last_record.profit).abs();
        let last_profit_abs = last_record.profit.abs();

        if last_profit_abs.is_zero() {
            // If last profit was zero but current is not, log it
            return !candidate.profit.is_zero();
        }

        // Convert to f64 for percentage calculation
        let profit_diff_f64 = profit_diff.to_string().parse::<f64>().unwrap_or(0.0);
        let last_profit_f64 = last_profit_abs.to_string().parse::<f64>().unwrap_or(1.0);
        let change_percent = (profit_diff_f64 / last_profit_f64) * 100.0;

        // Log if change exceeds threshold
        change_percent >= profit_change_threshold_percent
    } else {
        // New path, should log
        true
    }
}

/// Update the logged paths cache with new records
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

fn log_path_simulations(
    pools: &HashMap<Address, AgniPool>,
    block_number: u64,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
) -> eyre::Result<()> {
    if pools.is_empty() {
        return Ok(());
    }

    // Rebuild state pools on-demand for simulation
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
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
        .unwrap_or_else(|_| PathBuf::from("logs/positive_path_simulations.csv"));
    let mut positive_writer: Option<csv::Writer<_>> = None;
    let mut positive_candidates = Vec::new();

    const MIN_INPUT: u128 = 1_000_000_000_000; // 10^12
    const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000; // 10^24

    // Parallelize path simulation
    let sim_results: Vec<Option<PositiveCandidate>> = path_entries
        .par_iter()
        .enumerate()
        .map(|(pos, (original_idx, signature, path))| {
            let pools_for_path = match pools_for_path(path, &state_pools) {
                Ok(p) => p,
                Err(err) => {
                    error!(
                        target: "monitor",
                        error = ?err,
                        "Failed to gather pools for path simulation"
                    );
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
        // Use 5% as the threshold - only log if profit changes by more than 5%
        const PROFIT_CHANGE_THRESHOLD: f64 = 5.0;
        let candidates_to_log: Vec<&PositiveCandidate> = unique_candidates
            .iter()
            .filter(|candidate| should_log_path(candidate, PROFIT_CHANGE_THRESHOLD))
            .collect();

        if !candidates_to_log.is_empty() {
            if positive_writer.is_none() {
                positive_writer = Some(
                    WriterBuilder::new().has_headers(false).from_writer(
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&positive_sim_log_path)?,
                    ),
                );
            }

            if let Some(ref mut pos_writer) = positive_writer {
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

                    pos_writer.write_record(&record)?;
                }
                pos_writer.flush()?;
            }

            // Update the logged paths cache with newly logged candidates
            update_logged_paths(
                &candidates_to_log
                    .iter()
                    .map(|&c| c.clone())
                    .collect::<Vec<_>>(),
            );

            info!(
                target = "monitor.csv",
                block = block_number,
                logged_count = candidates_to_log.len(),
                total_count = unique_candidates.len(),
                "Logged {} new/changed paths out of {} total unique paths",
                candidates_to_log.len(),
                unique_candidates.len()
            );
        } else {
            info!(
                target = "monitor.csv",
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
                .unwrap_or_else(|_| PathBuf::from("logs/best_arbitrage_paths.csv"));
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
            let mut best_writer = WriterBuilder::new()
                .has_headers(false)
                .from_writer(
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
                target = "monitor.arb.summary",
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
                        target = "monitor.arb",
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
                target = "monitor.arb.summary",
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

fn simulate_path_raw(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> eyre::Result<(U256, I256)> {
    if path.hops.is_empty() {
        return Ok((U256::ZERO, I256::ZERO));
    }

    let mut current = amount_in;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let output = amm
            .simulate_swap(hop.token_in, hop.token_out, current)
            .map_err(Report::new)?;
        current = output;
    }

    let profit = I256::from_raw(current) - I256::from_raw(amount_in);
    Ok((current, profit))
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

fn format_roi_percent(profit: I256, input: U256) -> Option<String> {
    if input.is_zero() {
        return None;
    }

    let profit_i128: i128 = profit.try_into().ok()?;
    let input_u128: u128 = input.try_into().ok()?;
    if input_u128 == 0 {
        return None;
    }

    let ratio = (profit_i128 as f64) / (input_u128 as f64) * 100.0;
    Some(format!("{ratio:.4}"))
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
