use alloy::consensus::BlockHeader;
use alloy::primitives::{Address, I256, U256};
use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, FilterSet, Log},
    sol_types::SolEvent,
    transports::ws::WsConnect,
};
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    moe::{IMoeLBPairEvents, MoeFactory, MoeLbPair},
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
use eyre::WrapErr;
use futures::{stream, StreamExt};
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tracing::{error, info};

#[derive(Debug, Deserialize)]
struct PoolRow {
    #[allow(dead_code)]
    Protocol: String,
    #[serde(rename = "Pair Address")]
    Pair_Address: String,
}

#[derive(Clone, PartialEq, Eq)]
struct PositiveCandidate {
    index: usize,
    profit: I256,
    input: U256,
    output: U256,
    roi: String,
    signature: String,
    pools: Vec<Address>,
}

static LOGGED_PATHS: OnceLock<Mutex<HashMap<String, (I256, U256, U256)>>> = OnceLock::new();

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

    let ws_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());
    info!(target = "monitor", ws = %ws_endpoint, "Using WebSocket endpoint");

    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_endpoint)).await?;
    let latest_block = BlockId::from(provider.get_block_number().await?);

    // Load Moe pools from CSV (filtering by protocol contains "moe")
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file = File::open(&csv_path)
        .with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    let mut pools: HashMap<Address, MoeLbPair> = HashMap::new();
    let mut init_jobs = Vec::new();
    for result in rdr.deserialize::<PoolRow>() {
        let row = result?;
        if !row.Protocol.to_lowercase().contains("moe") {
            continue;
        }
        let addr: Address = row.Pair_Address.parse()?;
        init_jobs.push(addr);
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let provider_for_init = provider.clone();
    let block_for_init = latest_block;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|addr| {
        let provider = provider_for_init.clone();
        let block = block_for_init;
        async move { (addr, MoeLbPair::new(addr).init(block, provider).await) }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, result)) = init_stream.next().await {
        match result {
            Ok(pool) => {
                info!(target = "monitor", address = ?addr, bin_step = pool.bin_step, "Initialized Moe pool");
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(target = "monitor", address = ?addr, error = ?err, "Failed to initialize Moe pool");
            }
        }
    }

    if pools.is_empty() {
        info!(target = "monitor", "No Moe pools loaded. Exiting.");
        return Ok(());
    }

    // Precompute arbitrage paths
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state.state.insert(pool.address(), AMM::MoeLbPair(pool.clone()));
    }
    let graph = build_graph(&state)?;
    let constraints = PathConstraints { max_length: 4, ..Default::default() };
    let finder = PathFinder::new(&graph, constraints);
    let mut paths = finder.find_cycles();
    paths.extend(finder.find_two_pool_misprices());

    // Build event filter
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IMoeLBPairEvents::Swap::SIGNATURE_HASH,
        IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH,
        IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH,
    ]));
    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target = "monitor", "Subscribed to blocks over WS");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 { continue; }
        let target_number = number - 1;

        let windowed = filter.clone().select(target_number);
        match provider.get_logs(&windowed).await {
            Ok(logs) => {
                if logs.is_empty() { continue; }
                let changed = apply_logs(&mut pools, &logs)?;
                if !changed.is_empty() {
                    log_path_simulations(&pools, &paths)?;
                }
            }
            Err(e) => {
                error!(target = "monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

fn apply_logs(pools: &mut HashMap<Address, MoeLbPair>, logs: &[Log]) -> eyre::Result<HashSet<Address>> {
    let mut changed = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let before_active = pool.active_id;
            if let Err(e) = pool.sync(log) {
                error!(target = "monitor.pool", address = ?addr, error = ?e, "sync error");
                continue;
            }
            if pool.active_id != before_active { changed.insert(addr); }
        }
    }
    Ok(changed)
}

fn log_path_simulations(pools: &HashMap<Address, MoeLbPair>, paths: &[ArbitragePath]) -> eyre::Result<()> {
    if pools.is_empty() { return Ok(()); }

    let mut state = StateSpace::default();
    for pool in pools.values() {
        state.state.insert(pool.address(), AMM::MoeLbPair(pool.clone()));
    }
    let state_pools: Vec<AMM> = state.state.values().cloned().collect();

    let results: Vec<Option<PositiveCandidate>> = paths
        .par_iter()
        .enumerate()
        .map(|(idx, path)| {
            let pools_for_path = match pools_for_path(path, &state_pools) {
                Ok(p) => p,
                Err(err) => { error!(target = "monitor", error = ?err, "gather pools for path failed"); return None; }
            };
            let min_input = U256::from(1_000_000_000_000u128);
            let max_input = U256::from(1_000_000_000_000_000_000_000_000u128);
            let best = best_path_simulation(path, &pools_for_path, min_input, max_input);
            if let Some((input, output, profit)) = best {
                if profit > I256::ZERO {
                    let roi = format_roi_percent(profit, input).unwrap_or_else(|| "-".to_string());
                    return Some(PositiveCandidate { index: idx, profit, input, output, roi, signature: path_signature(path), pools: path.hops.iter().map(|h| h.pool_address).collect() });
                }
            }
            None
        })
        .collect();

    let best: Vec<PositiveCandidate> = results.into_iter().flatten().collect();
    if best.is_empty() { return Ok(()); }

    let log_path = std::env::var("POSITIVE_PATH_SIM_LOG").unwrap_or_else(|_| "logs/positive_path_simulations.csv".to_string());
    ensure_log_headers(&log_path, &["block_number", "path_index", "path_signature", "input_amount", "output_amount", "profit", "roi_percent"]) ?;
    let mut writer = WriterBuilder::new().has_headers(false).from_writer(OpenOptions::new().create(true).append(true).open(&log_path)?);

    let mut any = false;
    for c in &best {
        let ok_to_log = should_log_path(c, 5.0);
        if !ok_to_log { continue; }
        writer.write_record([
            "-1".to_string(),
            c.index.to_string(),
            c.signature.clone(),
            c.input.to_string(),
            c.output.to_string(),
            c.profit.to_string(),
            c.roi.clone(),
        ])?;
        any = true;
    }
    if any { writer.flush()?; }

    Ok(())
}

fn best_path_simulation(
    path: &ArbitragePath,
    pools: &[AMM],
    min_input: U256,
    max_input: U256,
) -> Option<(U256, U256, I256)> {
    if min_input.is_zero() || max_input.is_zero() || min_input > max_input { return None; }
    let evaluate = |amount: U256| -> Option<(U256, U256, I256)> {
        if amount < min_input || amount > max_input { return None; }
        simulate_path_raw(path, pools, amount).ok().map(|(out, profit)| (amount, out, profit))
    };
    let mut candidates = vec![min_input];
    if max_input > min_input { candidates.push(max_input); candidates.push(min_input + (max_input - min_input) / U256::from(2)); }
    let mut best: Option<(U256, U256, I256)> = None;
    for amount in candidates.into_iter().filter(|a| *a >= min_input) {
        if let Some(candidate) = evaluate(amount) {
            match &best { Some((_, _, bp)) if candidate.2 <= *bp => {}, _ => best = Some(candidate) }
        }
    }
    let mut best = best?;
    let mut current_input = best.0;
    let range = max_input - min_input;
    if range.is_zero() { return Some(best); }
    let mut step = range / U256::from(4); if step.is_zero() { step = U256::from(1); }
    for _ in 0..64 {
        if step.is_zero() { break; }
        let mut improved = false;
        if let Some(next_input) = current_input.checked_add(step) { if next_input <= max_input { if let Some(candidate) = evaluate(next_input) { if candidate.2 > best.2 { best = candidate; current_input = best.0; improved = true; } } } }
        if !improved { if let Some(prev_input) = current_input.checked_sub(step) { if prev_input >= min_input { if let Some(candidate) = evaluate(prev_input) { if candidate.2 > best.2 { best = candidate; current_input = best.0; improved = true; } } } } }
        if !improved { step = step.checked_div(U256::from(2)).unwrap_or(U256::ZERO); if step.is_zero() { break; } }
    }
    Some(best)
}

fn simulate_path_raw(path: &ArbitragePath, pools: &[AMM], amount_in: U256) -> eyre::Result<(U256, I256)> {
    if path.hops.is_empty() { return Ok((U256::ZERO, I256::ZERO)); }
    let mut current = amount_in;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let output = amm.simulate_swap(hop.token_in, hop.token_out, current)?;
        current = output;
    }
    let profit = I256::from_raw(current) - I256::from_raw(amount_in);
    Ok((current, profit))
}

fn path_signature(path: &ArbitragePath) -> String {
    path.hops
        .iter()
        .map(|hop| format!("{:#x}->{:#x}@{:#x}(fee_bps={})", hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps))
        .collect::<Vec<_>>()
        .join("|")
}

fn format_roi_percent(profit: I256, input: U256) -> Option<String> {
    if input.is_zero() { return None; }
    let profit_i128: i128 = profit.try_into().ok()?;
    let input_u128: u128 = input.try_into().ok()?;
    if input_u128 == 0 { return None; }
    let ratio = (profit_i128 as f64) / (input_u128 as f64) * 100.0;
    Some(format!("{ratio:.4}"))
}

fn ensure_log_headers(path: &str, header: &[&str]) -> eyre::Result<()> {
    use std::fs;
    if let Some(parent) = std::path::Path::new(path).parent() { if !parent.as_os_str().is_empty() { fs::create_dir_all(parent)?; } }
    let writer = OpenOptions::new().create(true).append(true).open(path)?;
    let metadata = writer.metadata()?; drop(writer);
    if metadata.len() == 0 {
        let mut csv = WriterBuilder::new().has_headers(false).from_writer(OpenOptions::new().create(true).append(true).open(path)?);
        csv.write_record(header)?; csv.flush()?;
    }
    Ok(())
}

fn should_log_path(candidate: &PositiveCandidate, profit_change_threshold_percent: f64) -> bool {
    let logged_paths_mutex = LOGGED_PATHS.get_or_init(|| Mutex::new(HashMap::new()));
    let logged_paths = logged_paths_mutex.lock().unwrap();
    if let Some((last_profit, _, _)) = logged_paths.get(&candidate.signature) {
        let profit_diff = (candidate.profit - *last_profit).abs();
        let last_profit_abs = last_profit.abs();
        if last_profit_abs.is_zero() { return !candidate.profit.is_zero(); }
        let diff = profit_diff.to_string().parse::<f64>().unwrap_or(0.0);
        let base = last_profit_abs.to_string().parse::<f64>().unwrap_or(1.0);
        let change_percent = (diff / base) * 100.0;
        change_percent >= profit_change_threshold_percent
    } else { true }
}
// TODO: reimplement Moe LB arbitrage monitor.
