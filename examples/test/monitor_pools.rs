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
    uniswap_v2::{IUniswapV2Pair, UniswapV2Pool},
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

#[derive(Debug, Deserialize)]
struct AgniPoolRow {
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

#[derive(Debug, Deserialize)]
struct V2PoolRow {
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
    tracing_subscriber::fmt::init();

    // WebSocket endpoint (fallback to a public Mantle WS if not provided)
    let ws_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());
    info!(target: "monitor", ws = %ws_endpoint, "Using WebSocket endpoint");

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_endpoint))
        .await?;

    // Load pools from CSVs (Agni + V2)
    let mut agni_csv = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    agni_csv.push("data/poolLists.csv");
    let agni_file =
        File::open(&agni_csv).with_context(|| format!("Failed to open {}", agni_csv.display()))?;
    let mut agni_rdr = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(agni_file);

    let mut v2_csv = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    v2_csv.push("data/poolLists_v2.csv");
    let v2_file =
        File::open(&v2_csv).with_context(|| format!("Failed to open {}", v2_csv.display()))?;
    let mut v2_rdr = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(v2_file);

    // Initialize pools
    let latest_block = BlockId::from(provider.get_block_number().await?);
    let mut pools: HashMap<Address, AMM> = HashMap::new();
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
    let mut v2_rows = 0usize;
    enum InitJob {
        Agni(Address, Option<u32>),
        V2(Address),
    }
    let mut init_jobs: Vec<InitJob> = Vec::new();

    for result in agni_rdr.deserialize::<AgniPoolRow>() {
        let row = result?;
        total_rows += 1;
        if !row.Protocol.to_lowercase().contains("agni") {
            continue;
        }
        agni_rows += 1;
        let addr = parse_address(&row.Pair_Address)?;
        init_jobs.push(InitJob::Agni(addr, row.Fee_Tier));
    }

    for result in v2_rdr.deserialize::<V2PoolRow>() {
        let row = result?;
        total_rows += 1;
        let protocol_lower = row.Protocol.to_lowercase();

        // Agni v2 is UniV2-style per user's clarification; treat as V2
        let is_supported_v2 = [
            "agni v2",
            "moe",
            "merchantmoe",
            "mantleswap",
            "fusionx",
            "papple",
        ]
        .iter()
        .any(|needle| protocol_lower.contains(needle));

        if !is_supported_v2 {
            continue;
        }

        v2_rows += 1;
        let addr = parse_address(&row.Pair_Address)?;
        init_jobs.push(InitJob::V2(addr));
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let provider_for_init = provider.clone();
    let block_for_init = latest_block;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|job| {
        let provider = provider_for_init.clone();
        let block = block_for_init;
        async move {
            match job {
                InitJob::Agni(addr, fee) => {
                    let result = AgniPool::new(addr)
                        .init_basic(block, provider)
                        .await
                        .map(AMM::from);
                    (addr, fee, result.map_err(|e| (addr, e)))
                }
                InitJob::V2(addr) => {
                    // default v2 fee 0.3% in 1e5 scale = 300
                    let primary_provider = provider.clone();
                    let fallback_provider = provider.clone();

                    let result = UniswapV2Pool::new(addr, 300)
                        .init::<_, _>(block, primary_provider)
                        .await;
                    let mapped = match result {
                        Ok(v2_pool) => Ok(AMM::from(v2_pool)),
                        Err(err) => {
                            warn!(
                                target = "monitor",
                                address = ?addr,
                                error = ?err,
                                "V2 pool init_basic failed, falling back to fallback init"
                            );
                            UniswapV2Pool::new(addr, 300)
                                .init_fallback(fallback_provider, block)
                                .await
                                .map(AMM::from)
                        }
                    };
                    (addr, None, mapped.map_err(|e| (addr, e)))
                }
            }
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, fee_tier, result)) = init_stream.next().await {
        match result {
            Ok(amm) => {
                match &amm {
                    AMM::AgniPool(pool) => {
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
                        info!(target: "monitor", address = ?addr, fee = pool.fee, "Initialized Agni pool");
                        fee_tiers.insert(addr, fee_tier);
                    }
                    AMM::UniswapV2Pool(_) => {
                        info!(target: "monitor", address = ?addr, "Initialized V2 pool");
                    }
                    _ => {}
                }
                log_pool_state_any(
                    &pool_log_path,
                    latest_block.as_u64().unwrap_or_default(),
                    "init",
                    &amm,
                )?;
                pools.insert(addr, amm);
            }
            Err((addr, err)) => {
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
        info!(target: "monitor", total_rows, agni_rows, v2_rows, "No pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "monitor",
        loaded = pools.len(),
        total_rows,
        agni_rows,
        v2_rows,
        "Initialized pools (Agni + V2)"
    );

    // Estimate arbitrage path candidates using the in-memory state
    let arbitrage_paths = {
        let mut state = StateSpace::default();
        for amm in pools.values() {
            state.state.insert(amm.address(), amm.clone());
        }

        match build_graph(&state) {
            Ok(graph) => {
                let constraints = PathConstraints {
                    max_length: 3,
                    required_start_token: Some(address!(
                        "78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"
                    )),
                    required_end_token: Some(address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
                    ..PathConstraints::default()
                };
                let finder = PathFinder::new(&graph, constraints);
                let cycles = finder.find_cycles();
                let two_pool = finder.find_two_pool_misprices();

                let matches_fee = |path: &ArbitragePath| {
                    path.hops
                        .iter()
                        .all(|hop| match fee_tiers.get(&hop.pool_address) {
                            Some(Some(expected_fee)) => pools
                                .get(&hop.pool_address)
                                .map(|amm| match amm {
                                    AMM::AgniPool(pool) => pool.fee == *expected_fee,
                                    _ => true,
                                })
                                .unwrap_or(false),
                            Some(None) | None => true,
                        })
                };

                cycles.iter().filter(|path| matches_fee(path)).count()
                    + two_pool.iter().filter(|path| matches_fee(path)).count()
            }
            Err(err) => {
                error!(target: "monitor", error = ?err, "Failed to build arbitrage graph");
                0
            }
        }
    };

    // Build event filter for only these pools and only relevant events (Agni + V2)
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
        IUniswapV2Pair::Sync::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // Subscribe to new blocks over WS and fetch logs per block
    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target: "monitor", "Subscribed to blocks over WS");
    info!(target: "monitor", candidate_paths = arbitrage_paths, "Pre-computed arbitrage candidate paths");

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
                apply_logs(&mut pools, &logs, target_number, &pool_log_path, &fee_tiers)?;
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
    pools: &mut HashMap<Address, AMM>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
    fee_tiers: &HashMap<Address, Option<u32>>,
) -> eyre::Result<()> {
    for log in logs {
        let addr = log.address();
        if let Some(amm) = pools.get_mut(&addr) {
            let sig = log.topics()[0];
            if let Err(e) = amm.sync(log) {
                error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error");
                continue;
            }

            let event = if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                "swap"
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                "mint"
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                "burn"
            } else if sig == IUniswapV2Pair::Sync::SIGNATURE_HASH {
                "sync"
            } else {
                "unknown"
            };

            log_pool_state_any(log_path, block_number, event, amm)?;
            info!(target: "monitor.pool", address = ?addr, event = event, "Applied");
        }
    }

    // After applying logs, record path simulations for the current block.
    log_path_simulations(pools, block_number, fee_tiers)?;

    Ok(())
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

fn log_pool_state_any(path: &Path, block_number: u64, event: &str, pool: &AMM) -> eyre::Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    match pool {
        AMM::AgniPool(p) => {
            writer.write_record([
                block_number.to_string(),
                format!("{:#x}", p.address()),
                event.to_owned(),
                p.sqrt_price.to_string(),
                p.liquidity.to_string(),
                p.tick.to_string(),
            ])?;
        }
        AMM::UniswapV2Pool(p) => {
            writer.write_record([
                block_number.to_string(),
                format!("{:#x}", p.address()),
                event.to_owned(),
                "0".to_string(),
                p.reserve_0.to_string(),
                p.reserve_1.to_string(),
            ])?;
        }
        _ => {
            writer.write_record([
                block_number.to_string(),
                format!("{:#x}", pool.address()),
                event.to_owned(),
                "0".to_string(),
                "0".to_string(),
                "0".to_string(),
            ])?;
        }
    }

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
    pools: &HashMap<Address, AMM>,
    block_number: u64,
    fee_tiers: &HashMap<Address, Option<u32>>,
) -> eyre::Result<()> {
    if pools.is_empty() {
        return Ok(());
    }

    let mut state = StateSpace::default();
    for amm in pools.values() {
        state.state.insert(amm.address(), amm.clone());
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(
                target: "monitor",
                error = ?err,
                "Failed to build graph for path simulation"
            );
            return Ok(());
        }
    };

    let constraints = PathConstraints {
        max_length: 3,
        required_start_token: Some(address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
        required_end_token: Some(address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")),
        ..PathConstraints::default()
    };
    let finder = PathFinder::new(&graph, constraints);
    let mut unique_paths: HashMap<String, ArbitragePath> = HashMap::new();

    for path in finder
        .find_cycles()
        .into_iter()
        .chain(finder.find_two_pool_misprices())
    {
        let signature = path_signature(&path);
        unique_paths.entry(signature).or_insert(path);
    }

    let mut path_entries: Vec<(String, ArbitragePath)> = unique_paths.into_iter().collect();
    path_entries.sort_by(|a, b| a.0.cmp(&b.0));

    if path_entries.is_empty() {
        return Ok(());
    }

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/positive_path_simulations.csv"));
    let mut positive_writer: Option<csv::Writer<_>> = None;
    let mut positive_candidates = Vec::new();

    let state_pools: Vec<AMM> = state.state.values().cloned().collect();
    const MIN_INPUT: u128 = 1_000_000_000_000; // 10^12
    const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000; // 10^24

    for (idx, (signature, path)) in path_entries.iter().enumerate() {
        if !path
            .hops
            .iter()
            .all(|hop| match fee_tiers.get(&hop.pool_address) {
                Some(Some(expected_fee)) => pools
                    .get(&hop.pool_address)
                    .map(|amm| match amm {
                        AMM::AgniPool(pool) => pool.fee == *expected_fee,
                        _ => true,
                    })
                    .unwrap_or(false),
                Some(None) | None => true,
            })
        {
            continue;
        }

        let pools_for_path = match pools_for_path(path, &state_pools) {
            Ok(p) => p,
            Err(err) => {
                error!(
                    target: "monitor",
                    error = ?err,
                    "Failed to gather pools for path simulation"
                );
                continue;
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
                    positive_candidates.push(PositiveCandidate {
                        index: idx,
                        profit,
                        input,
                        output,
                        roi: roi_str,
                        signature: signature.clone(),
                        hops,
                        pools: path.hops.iter().map(|hop| hop.pool_address).collect(),
                    });
                }
            }
        }
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
            if !is_same_selection {
                info!(
                    target = "monitor.arb.summary",
                    block = block_number,
                    selected_paths = selected_indices.len(),
                    path_indices = ?selected_path_indices,
                    total_profit = %total_profit,
                    "Selected optimal non-conflicting arbitrage paths for block"
                );

                // Persist best path once per selection change
                let best_csv_path = std::env::var("BEST_PATH_LOG")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| PathBuf::from("logs/best_arbitrage_paths.csv"));
                ensure_log_headers(
                    &best_csv_path,
                    &[
                        "block_number",
                        "path_index",
                        "path_signature",
                        "input_amount",
                        "output_amount",
                        "gross_profit",
                        "gas_cost",
                        "net_profit",
                        "roi_percent",
                        "hops",
                        "pool_types",
                    ],
                )?;

                if let Some(&best_sorted_idx) = selected_indices.first() {
                    let best = &unique_candidates[best_sorted_idx];
                    let best_hops_num = best.hops.split('|').count();
                    let gas_config = GasConfig::default();
                    let best_gas_cost = gas_config.calculate_gas_cost(best_hops_num);
                    let best_profit_u256 = U256::from_limbs(*best.profit.as_limbs());
                    let best_net_profit = gas_config
                        .net_profit(best_profit_u256, best_hops_num)
                        .unwrap_or(U256::ZERO);

                    let mut writer = WriterBuilder::new().has_headers(false).from_writer(
                        OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&best_csv_path)?,
                    );
                    let mut rec = StringRecord::new();
                    rec.push_field(&block_number.to_string());
                    rec.push_field(&best.index.to_string());
                    rec.push_field(&best.signature);
                    rec.push_field(&best.input.to_string());
                    rec.push_field(&best.output.to_string());
                    rec.push_field(&best.profit.to_string());
                    rec.push_field(&best_gas_cost.to_string());
                    rec.push_field(&best_net_profit.to_string());
                    rec.push_field(&best.roi);
                    rec.push_field(&best_hops_num.to_string());
                    // poolTypes string aligned with hops order
                    let pool_types_str = best
                        .pools
                        .iter()
                        .map(|addr| match pools.get(addr) {
                            Some(AMM::UniswapV2Pool(_)) => "V2",
                            Some(AMM::AgniPool(_)) => "Agni",
                            Some(AMM::UniswapV3Pool(_)) => "V3",
                            _ => "Unknown",
                        })
                        .collect::<Vec<_>>()
                        .join("|");

                    rec.push_field(&pool_types_str);

                    writer.write_record(&rec)?;
                    writer.flush()?;
                }

                let gas_config = GasConfig::default();
                for &idx in &selected_indices {
                    let candidate = &unique_candidates[idx];
                    let num_hops = candidate.hops.split('|').count();
                    let gas_cost = gas_config.calculate_gas_cost(num_hops);
                    let profit_u256 = U256::from_limbs(*candidate.profit.as_limbs());
                    let net_profit = gas_config
                        .net_profit(profit_u256, num_hops)
                        .unwrap_or(U256::ZERO);

                    info!(
                        target = "monitor.arb.detail",
                        block = block_number,
                        path_index = candidate.index,
                        hops = num_hops,
                        optimal_input = %candidate.input,
                        output_amount = %candidate.output,
                        gross_profit = %candidate.profit,
                        gas_cost = %gas_cost,
                        net_profit = %net_profit,
                        roi = %candidate.roi,
                        path = %candidate.signature,
                        "Arbitrage candidate"
                    );
                }

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
