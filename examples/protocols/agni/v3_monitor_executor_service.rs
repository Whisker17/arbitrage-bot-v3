use alloy::consensus::BlockHeader;
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use alloy::transports::ws::WsConnect;
use amms::amms::{
    agni::{AgniPool, IAgniPoolEvents},
    amm::{AutomatedMarketMaker, AMM},
};
use amms::arbitrage::{
    gas::GasConfig,
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::execution::{gas_schedule::gas_limit_for_hops, IArbitrageExecutor, IERC20};
use amms::state_space::StateSpace;
use csv::{ReaderBuilder, StringRecord, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tracing::{error, info, warn};

const MAX_HOPS: usize = 3;
const WMNT_ADDRESS: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const MIN_PROFIT_FLOOR_WEI: &str = "10000000000000000"; // 0.01 MNT assuming 18 decimals
const POSITIVE_PATH_LOG_HEADERS: &[&str] = &[
    "block_number",
    "path_signature",
    "hops",
    "input_amount",
    "output_amount",
    "profit",
    "net_profit",
    "roi_percent",
    "path",
];
const BEST_PATH_LOG_HEADERS: &[&str] = &[
    "block_number",
    "path_signature",
    "hops",
    "input_amount",
    "output_amount",
    "profit",
    "net_profit",
    "roi_percent",
    "path",
];
const FAILED_OPPORTUNITIES_PATH: &str = "logs/failed_opportunities.json";
const MAX_APPEARANCES: u32 = 3;

fn resolve_ws_endpoint() -> String {
    let raw = std::env::var("RPC_WS_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_WS_URL").ok())
        .unwrap_or_else(|| "wss://mantle.publicnode.com".to_string());
    let normalized = normalize_ws_endpoint(raw.trim());
    if normalized != raw {
        info!(
            target: "v3.config",
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

#[derive(Debug, Deserialize)]
struct PoolRow {
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "Protocol")]
    protocol: String,
}

#[derive(Clone, Debug)]
struct PositiveCandidate {
    signature: String,
    hops: usize,
    input: U256,
    output: U256,
    profit: I256,
    net_profit: U256,
    pool_addresses: Vec<Address>,
    token_path: Vec<Address>,
    amounts_out: Vec<U256>,
    expected_states: Vec<U256>,
    log_hops: String,
    roi: String,
}

#[derive(Clone)]
struct ExecutionJob {
    candidate: PositiveCandidate,
    block_number: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct OpportunitySignature {
    pool_addresses: Vec<Address>,
    token_path: Vec<Address>,
    quantized_input: U256,
}

impl OpportunitySignature {
    fn from_candidate(candidate: &PositiveCandidate) -> Self {
        let quantization_factor = U256::from(1_000_000_000_000u64);
        let quantized_input = (candidate.input / quantization_factor) * quantization_factor;

        Self {
            pool_addresses: candidate.pool_addresses.clone(),
            token_path: candidate.token_path.clone(),
            quantized_input,
        }
    }
}

#[derive(Clone, Debug)]
struct LoggedPathRecord {
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

#[derive(Default)]
struct AppearanceTracker {
    appearances: HashMap<String, (u64, u32)>,
    max_appearances: u32,
}

impl AppearanceTracker {
    fn new(max_appearances: u32) -> Self {
        Self {
            appearances: HashMap::new(),
            max_appearances,
        }
    }

    fn filter_and_update(
        &mut self,
        block_number: u64,
        candidates: Vec<PositiveCandidate>,
    ) -> Vec<PositiveCandidate> {
        let mut filtered = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let entry = self
                .appearances
                .entry(candidate.signature.clone())
                .or_insert((0, 0));

            if entry.0 != block_number {
                entry.0 = block_number;
                entry.1 += 1;
            }

            if entry.1 <= self.max_appearances {
                filtered.push(candidate);
            } else {
                info!(
                    target: "v3.tracker",
                    signature = %candidate.signature,
                    count = entry.1,
                    "Filtered stale opportunity"
                );
            }
        }
        filtered
    }
}

struct FailedOpportunityStore {
    path: PathBuf,
    failed_signatures: HashSet<OpportunitySignature>,
}

impl FailedOpportunityStore {
    fn new(path: &str) -> Result<Self> {
        let path = PathBuf::from(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut store = Self {
            path,
            failed_signatures: HashSet::new(),
        };
        store.load()?;
        Ok(store)
    }

    fn load(&mut self) -> Result<()> {
        if self.path.exists() {
            let file = File::open(&self.path)?;
            let reader = BufReader::new(file);
            let signatures: Vec<OpportunitySignature> = serde_json::from_reader(reader)?;
            self.failed_signatures = signatures.into_iter().collect();
            info!(
                target: "v3.failure_store",
                loaded = self.failed_signatures.len(),
                "Loaded failed opportunities"
            );
        }
        Ok(())
    }

    fn is_failed(&self, signature: &OpportunitySignature) -> bool {
        self.failed_signatures.contains(signature)
    }

    fn mark_as_failed(&mut self, signature: OpportunitySignature) -> Result<()> {
        if self.failed_signatures.insert(signature.clone()) {
            let signatures: Vec<OpportunitySignature> =
                self.failed_signatures.iter().cloned().collect();
            let file = File::create(&self.path)?;
            serde_json::to_writer_pretty(file, &signatures)?;
            warn!(target: "v3.failure_store", "Marked opportunity as failed and persisted to disk");
        }
        Ok(())
    }
}

struct PathCache {
    paths: Vec<ArbitragePath>,
    state_pools: Vec<AMM>,
    pool_to_path_indices: HashMap<Address, Vec<usize>>,
}

fn build_path_cache(
    pools: &HashMap<Address, AgniPool>,
    max_hops: usize,
    wmnt_address: Address,
) -> Result<PathCache> {
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }

    let graph = build_graph(&state)?;
    let constraints = PathConstraints {
        max_length: max_hops,
        required_start_token: Some(wmnt_address),
        required_end_token: Some(wmnt_address),
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

    if unique_paths.is_empty() {
        return Ok(PathCache {
            paths: Vec::new(),
            state_pools: Vec::new(),
            pool_to_path_indices: HashMap::new(),
        });
    }

    let paths: Vec<ArbitragePath> = unique_paths.into_values().collect();
    let mut pool_to_path_indices: HashMap<Address, Vec<usize>> = HashMap::new();
    for (idx, path) in paths.iter().enumerate() {
        for hop in &path.hops {
            pool_to_path_indices
                .entry(hop.pool_address)
                .or_default()
                .push(idx);
        }
    }

    let state_pools: Vec<AMM> = state.state.values().cloned().collect();

    Ok(PathCache {
        paths,
        state_pools,
        pool_to_path_indices,
    })
}

fn select_non_conflicting_opportunities(
    mut candidates: Vec<PositiveCandidate>,
) -> Vec<PositiveCandidate> {
    candidates.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

    let mut selected = Vec::new();
    let mut used_pools = HashSet::new();

    for candidate in candidates {
        let has_conflict = candidate
            .pool_addresses
            .iter()
            .any(|addr| used_pools.contains(addr));

        if !has_conflict {
            for addr in &candidate.pool_addresses {
                used_pools.insert(*addr);
            }
            selected.push(candidate);
        }
    }

    selected
}

#[derive(Clone)]
struct ServiceConfig {
    ws_endpoint: String,
    http_endpoint: String,
    executor_address: Address,
    wmnt_address: Address,
    min_gross_profit: U256,
    min_net_profit: U256,
    execution_slippage_bps: u32,
    block_cooldown: u64,
}

impl ServiceConfig {
    fn from_env() -> Result<Self> {
        let ws_endpoint = resolve_ws_endpoint();
        info!(target: "v3.config", ws = %ws_endpoint, "Using WebSocket endpoint");

        let http_endpoint = resolve_http_endpoint();
        info!(target: "v3.config", http = %http_endpoint, "Using HTTP endpoint");
        let (executor_address, executor_source) = read_address_from_env(&[
            "ARBITRAGE_EXECUTOR_ADDRESS",
            "EXECUTOR_ADDRESS",
            "EXECUTION_EXECUTOR_ADDRESS",
            "PRIVATE_EXECUTOR_ADDRESS",
        ])?;
        info!(
            target: "v3.config",
            executor = %executor_address,
            source = executor_source,
            "Using executor address"
        );
        let wmnt_address = WMNT_ADDRESS;

        let min_profit_floor = U256::from_str(MIN_PROFIT_FLOOR_WEI)?;
        let min_gross_profit =
            read_min_profit_threshold("MIN_GROSS_PROFIT_WEI", &min_profit_floor)?;
        let min_net_profit_raw =
            read_min_profit_threshold("MIN_NET_PROFIT_WEI", &min_profit_floor)?;
        let min_net_profit = if min_net_profit_raw < min_gross_profit {
            warn!(
                target: "v3.config",
                provided = %min_net_profit_raw,
                adjusted = %min_gross_profit,
                "Net profit threshold below gross profit threshold; using gross threshold"
            );
            min_gross_profit.clone()
        } else {
            min_net_profit_raw
        };
        let execution_slippage_bps = std::env::var("EXECUTION_SLIPPAGE_BPS")
            .unwrap_or_else(|_| "30".to_string())
            .parse()?;
        let block_cooldown = std::env::var("EXECUTION_BLOCK_COOLDOWN")
            .unwrap_or_else(|_| "1".to_string())
            .parse()?;

        Ok(Self {
            ws_endpoint,
            http_endpoint,
            executor_address,
            wmnt_address,
            min_gross_profit,
            min_net_profit,
            execution_slippage_bps,
            block_cooldown,
        })
    }
}

fn read_min_profit_threshold(var: &str, floor: &U256) -> Result<U256> {
    let value = match std::env::var(var) {
        Ok(raw) => {
            let parsed = U256::from_str(raw.trim())?;
            if parsed < *floor {
                warn!(
                    target: "v3.config",
                    variable = var,
                    provided = %parsed,
                    floor = %floor,
                    "Configured profit threshold below floor; using floor"
                );
                floor.clone()
            } else {
                parsed
            }
        }
        Err(_) => floor.clone(),
    };
    Ok(value)
}

fn read_address_from_env<'a>(vars: &'a [&'a str]) -> Result<(Address, &'a str)> {
    for &var in vars {
        if let Ok(raw) = std::env::var(var) {
            let parsed = raw.trim().parse()?;
            return Ok((parsed, var));
        }
    }
    Err(eyre!("Missing executor address env variable"))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_tracing();

    let config = ServiceConfig::from_env()?;

    let private_key = std::env::var("EXECUTION_PRIVATE_KEY")
        .or_else(|_| std::env::var("PRIVATE_KEY"))
        .context("Missing EXECUTION_PRIVATE_KEY or PRIVATE_KEY")?;
    let signer = PrivateKeySigner::from_str(private_key.trim())?;
    let wallet = EthereumWallet::from(signer);

    let http_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(config.http_endpoint.parse().expect("invalid http endpoint"));

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;

    info!(
        target: "v3.service",
        executor = %config.executor_address,
        "Starting Agni (UniV3-style) monitoring + execution service on Mantle"
    );

    run_service(ws_provider, http_provider, config).await
}

async fn run_service<P, H>(ws_provider: P, http_provider: H, config: ServiceConfig) -> Result<()>
where
    P: Provider + Clone,
    H: Provider + Clone + Send + Sync + 'static,
{
    let config = Arc::new(config);

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

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/positive_path_simulations.csv"));
    let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/best_arbitrage_paths.csv"));
    ensure_log_headers(&positive_sim_log_path, POSITIVE_PATH_LOG_HEADERS)?;
    ensure_log_headers(&best_paths_log_path, BEST_PATH_LOG_HEADERS)?;

    let latest_block = ws_provider.get_block_number().await?;
    let latest_block_id = alloy::eips::BlockId::from(latest_block);

    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    initialize_agni_pools(&ws_provider, latest_block_id, &mut pools).await?;

    if pools.is_empty() {
        warn!(target: "v3.service", "No Agni pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "v3.service",
        pools = pools.len(),
        "Initialized Agni pools"
    );

    for pool in pools.values() {
        log_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }

    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    let path_cache = Arc::new(build_path_cache(&pools, MAX_HOPS, config.wmnt_address)?);

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "v3.service", "Subscribed to block stream");

    let gas_config = GasConfig::default();
    let http_provider = Arc::new(http_provider);
    let last_executions = Arc::new(AsyncMutex::new(HashMap::<String, u64>::new()));
    let failed_store = Arc::new(AsyncMutex::new(FailedOpportunityStore::new(
        FAILED_OPPORTUNITIES_PATH,
    )?));
    let appearance_tracker = Arc::new(AsyncMutex::new(AppearanceTracker::new(MAX_APPEARANCES)));
    let logged_paths = Arc::new(AsyncMutex::new(HashMap::<String, LoggedPathRecord>::new()));
    let last_selection = Arc::new(AsyncMutex::new(None::<SelectionSnapshot>));

    let (tx, mut rx) = mpsc::channel::<ExecutionJob>(64);

    let execution_config = Arc::clone(&config);
    let execution_provider = Arc::clone(&http_provider);
    let execution_last = Arc::clone(&last_executions);
    let execution_failed_store = Arc::clone(&failed_store);
    let execution_task = tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let should_skip = {
                let executions = execution_last.lock().await;
                executions
                    .get(&job.candidate.signature)
                    .map(|last_block| {
                        job.block_number.saturating_sub(*last_block)
                            < execution_config.block_cooldown
                    })
                    .unwrap_or(false)
            };

            if should_skip {
                info!(
                    target: "v3.exec",
                    block = job.block_number,
                    signature = %job.candidate.signature,
                    "Skipping execution due to cooldown"
                );
                continue;
            }

            match attempt_execution(
                &*execution_provider,
                &job.candidate,
                execution_config.as_ref(),
            )
            .await
            {
                Ok(tx_hash) => {
                    let mut executions = execution_last.lock().await;
                    executions.insert(job.candidate.signature.clone(), job.block_number);
                    info!(
                        target: "v3.exec",
                        block = job.block_number,
                        tx = %tx_hash,
                        signature = %job.candidate.signature,
                        "Execution submitted"
                    );
                }
                Err(err) => {
                    error!(
                        target: "v3.exec",
                        block = job.block_number,
                        signature = %job.candidate.signature,
                        error = ?err,
                        "Execution attempt failed"
                    );
                    let signature = OpportunitySignature::from_candidate(&job.candidate);
                    let mut store = execution_failed_store.lock().await;
                    if let Err(mark_err) = store.mark_as_failed(signature) {
                        error!(
                            target: "v3.failure_store",
                            error = ?mark_err,
                            "Failed to persist failed opportunity"
                        );
                    }
                }
            }
        }
    });

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number.saturating_sub(1);
        info!(target: "v3.block", block = target_number, "Processing block");

        let windowed = filter.clone().select(target_number);
        match ws_provider.get_logs(&windowed).await {
            Ok(logs) => {
                if logs.is_empty() {
                    continue;
                }

                let changed = apply_logs(&mut pools, &logs, target_number, &pool_log_path)?;
                if changed.is_empty() {
                    continue;
                }

                let mut tracker = appearance_tracker.lock().await;
                let mut selection_history = last_selection.lock().await;
                let mut logged = logged_paths.lock().await;

                let candidates = find_profitable_candidates(
                    &pools,
                    &gas_config,
                    config.as_ref(),
                    target_number,
                    &path_cache,
                )?;
                if candidates.is_empty() {
                    continue;
                }

                log_positive_candidates(
                    target_number,
                    &candidates,
                    &positive_sim_log_path,
                    &best_paths_log_path,
                    &mut logged,
                )?;

                let fresh_candidates = tracker.filter_and_update(target_number, candidates);
                if fresh_candidates.is_empty() {
                    continue;
                }

                let selected_candidates = select_non_conflicting_opportunities(fresh_candidates);
                if selected_candidates.is_empty() {
                    continue;
                }

                let selection_changed =
                    record_selection_snapshot(&selected_candidates, &mut selection_history);

                if selection_changed {
                    info!(
                        target: "v3.exec",
                        block = target_number,
                        total_candidates = selected_candidates.len(),
                        "Selected non-conflicting candidates"
                    );
                } else {
                    info!(
                        target: "v3.exec",
                        block = target_number,
                        total_candidates = selected_candidates.len(),
                        "Selection unchanged from previous block"
                    );
                }

                drop(logged);
                drop(selection_history);
                drop(tracker);

                for candidate in selected_candidates {
                    let signature = OpportunitySignature::from_candidate(&candidate);
                    let failed_guard = failed_store.lock().await;
                    if failed_guard.is_failed(&signature) {
                        info!(
                            target: "v3.exec",
                            block = target_number,
                            signature = %candidate.signature,
                            "Skipping execution due to previous failure"
                        );
                        continue;
                    }
                    drop(failed_guard);
                    if tx
                        .send(ExecutionJob {
                            candidate,
                            block_number: target_number,
                        })
                        .await
                        .is_err()
                    {
                        warn!(
                            target: "v3.exec",
                            block = target_number,
                            "Execution queue closed; stopping dispatch"
                        );
                        break;
                    }
                }
            }
            Err(e) => {
                error!(target: "v3.block", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    drop(tx);
    if let Err(join_err) = execution_task.await {
        error!(target: "v3.exec", error = ?join_err, "Execution worker failed");
    }

    Ok(())
}

async fn initialize_agni_pools<P: Provider + Clone>(
    provider: &P,
    block_id: alloy::eips::BlockId,
    pools: &mut HashMap<Address, AgniPool>,
) -> Result<()> {
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    let mut init_jobs = Vec::new();

    for row in rdr.deserialize::<PoolRow>() {
        let row = row?;
        if !row.protocol.to_lowercase().contains("agni") {
            continue;
        }
        let addr = Address::from_str(row.pair_address.trim()).context("Invalid pool address")?;
        init_jobs.push(addr);
    }

    if init_jobs.is_empty() {
        warn!(target: "v3.service", "No Agni pools found in CSV");
        return Ok(());
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let result = AgniPool::new(addr).init_basic(block_id, provider).await;
            (addr, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, result)) = init_stream.next().await {
        match result {
            Ok(pool) => {
                info!(target: "v3.init", address = %addr, fee = pool.fee, "Initialized Agni pool");
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "v3.init",
                    address = %addr,
                    error = %err,
                    "Failed to initialize Agni pool"
                );
            }
        }
    }

    Ok(())
}

fn apply_logs(
    pools: &mut HashMap<Address, AgniPool>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
) -> Result<HashSet<Address>> {
    let mut changed = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let before_tick = pool.tick;
            let before_liq = pool.liquidity;
            let before_sqrt = pool.sqrt_price;

            match log.topics()[0] {
                sig if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "v3.pool", address = %addr, error = ?e, "sync error (Swap)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "swap", pool)?;
                    info!(
                        target: "v3.pool",
                        address = %addr,
                        event = "Swap",
                        tick_from = before_tick,
                        tick_to = pool.tick,
                        sqrt_from = %before_sqrt,
                        sqrt_to = %pool.sqrt_price,
                        liq_from = before_liq,
                        liq_to = pool.liquidity,
                        "Applied swap event"
                    );
                    changed.insert(addr);
                }
                sig if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "v3.pool", address = %addr, error = ?e, "sync error (Mint)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "mint", pool)?;
                    info!(
                        target: "v3.pool",
                        address = %addr,
                        event = "Mint",
                        tick_from = before_tick,
                        tick_to = pool.tick,
                        liq_from = before_liq,
                        liq_to = pool.liquidity,
                        "Applied mint event"
                    );
                    changed.insert(addr);
                }
                sig if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "v3.pool", address = %addr, error = ?e, "sync error (Burn)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "burn", pool)?;
                    info!(
                        target: "v3.pool",
                        address = %addr,
                        event = "Burn",
                        tick_from = before_tick,
                        tick_to = pool.tick,
                        liq_from = before_liq,
                        liq_to = pool.liquidity,
                        "Applied burn event"
                    );
                    changed.insert(addr);
                }
                _ => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "v3.pool", address = %addr, error = ?e, "sync error (Unknown)");
                    }
                    log_pool_state(log_path, block_number, "unknown", pool)?;
                }
            }
        }
    }

    if !changed.is_empty() {
        info!(target: "v3.pool", block = block_number, changed = changed.len(), "Updated Agni pools");
    }

    Ok(changed)
}

fn find_profitable_candidates(
    pools: &HashMap<Address, AgniPool>,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    block_number: u64,
    path_cache: &PathCache,
) -> Result<Vec<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates: Vec<PositiveCandidate> = path_cache
        .paths
        .par_iter()
        .filter_map(|path| {
            let pools_for_path = match pools_for_path(path, &path_cache.state_pools) {
                Ok(p) => p,
                Err(err) => {
                    error!(target: "v3.sim", error = ?err, "Failed to gather pools for path");
                    return None;
                }
            };

            let simulation = best_path_simulation_with_steps(path, &pools_for_path)?;

            let path_sig = path_signature(path);
            
            info!(
                target: "v3.sim.detail",
                block = block_number,
                path = %path_sig,
                profit = %simulation.profit,
                input = %simulation.input,
                output = %simulation.output,
                "Simulated path"
            );

            if simulation.profit <= I256::ZERO {
                info!(
                    target: "v3.sim.filter",
                    block = block_number,
                    path = %path_sig,
                    profit = %simulation.profit,
                    "Filtered: non-positive profit"
                );
                return None;
            }

            let profit_u256 = U256::from_limbs(*simulation.profit.as_limbs());
            if profit_u256 < config.min_gross_profit {
                info!(
                    target: "v3.sim.filter",
                    block = block_number,
                    path = %path_sig,
                    profit = %profit_u256,
                    threshold = %config.min_gross_profit,
                    "Filtered: below min_gross_profit"
                );
                return None;
            }

            let num_hops = path.hops.len();
            let net_profit = match gas_config.net_profit(profit_u256, num_hops) {
                Some(net) => net,
                None => {
                    info!(
                        target: "v3.sim.filter",
                        block = block_number,
                        path = %path_sig,
                        gross_profit = %profit_u256,
                        hops = num_hops,
                        "Filtered: net_profit calculation returned None (likely negative after gas)"
                    );
                    return None;
                }
            };

            info!(
                target: "v3.sim.detail",
                block = block_number,
                path = %path_sig,
                gross_profit = %profit_u256,
                net_profit = %net_profit,
                hops = num_hops,
                "Profit after gas calculation"
            );

            if net_profit < config.min_net_profit {
                info!(
                    target: "v3.sim.filter",
                    block = block_number,
                    path = %path_sig,
                    net_profit = %net_profit,
                    threshold = %config.min_net_profit,
                    "Filtered: below min_net_profit"
                );
                return None;
            }

            if !gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.2) {
                info!(
                    target: "v3.sim.filter",
                    block = block_number,
                    path = %path_sig,
                    gross_profit = %profit_u256,
                    hops = num_hops,
                    "Filtered: not profitable after gas with 1.2x safety factor"
                );
                return None;
            }

            let mut token_path = build_token_path(path);
            if token_path.first().copied() != Some(config.wmnt_address) {
                return None;
            }
            if token_path.last().copied() != Some(config.wmnt_address) {
                return None;
            }

            let pool_addresses: Vec<Address> =
                path.hops.iter().map(|hop| hop.pool_address).collect();

            let expected_states = match collect_agni_expected_states(&pools_for_path) {
                Ok(states) => states,
                Err(err) => {
                    error!(target: "v3.state", error = ?err, "Failed to collect expected states");
                    return None;
                }
            };

            let roi = format_roi_percent(simulation.profit, simulation.input)
                .unwrap_or_else(|| "-".to_string());

            Some(PositiveCandidate {
                signature: path_signature(path),
                hops: num_hops,
                input: simulation.input,
                output: simulation.output,
                profit: simulation.profit,
                net_profit,
                pool_addresses,
                token_path: token_path.drain(..).collect(),
                amounts_out: simulation.step_outputs.clone(),
                expected_states,
                log_hops: hops_description(path),
                roi,
            })
        })
        .collect();

    candidates.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));

    if !candidates.is_empty() {
        info!(
            target: "v3.candidate",
            block = block_number,
            count = candidates.len(),
            "Found profitable candidates"
        );
    }

    Ok(candidates)
}

async fn attempt_execution<H: Provider + Clone>(
    provider: &H,
    candidate: &PositiveCandidate,
    config: &ServiceConfig,
) -> Result<alloy::primitives::TxHash> {
    let executor = IArbitrageExecutor::new(config.executor_address, provider.clone());
    let wmnt_contract = IERC20::new(config.wmnt_address, provider.clone());

    let executor_balance = wmnt_contract
        .balanceOf(config.executor_address)
        .call()
        .await?;

    if executor_balance < candidate.input {
        warn!(
            target: "v3.exec",
            required = %candidate.input,
            available = %executor_balance,
            "Executor contract balance insufficient"
        );
        return Err(eyre!("Executor contract lacks WMNT balance"));
    }

    let mut amounts_out_with_slippage: Vec<U256> = candidate
        .amounts_out
        .iter()
        .map(|amount| apply_slippage(*amount, config.execution_slippage_bps))
        .collect();

    if let Some(last) = amounts_out_with_slippage.last_mut() {
        *last = (*last).max(candidate.input);
    }

    let pool_types = vec![1u8; candidate.pool_addresses.len()];

    info!(
        target: "v3.exec",
        signature = %candidate.signature,
        hops = candidate.hops,
        input = %candidate.input,
        expected_output = %candidate.output,
        "Sending executeArbitrage"
    );

    let pending_tx = executor
        .executeArbitrage(
            candidate.input,
            candidate.token_path.clone(),
            candidate.pool_addresses.clone(),
            pool_types,
            amounts_out_with_slippage,
            candidate.output.saturating_sub(candidate.input),
            alloy::primitives::U256::from(u64::MAX),
        )
        .gas(gas_limit_for_hops(candidate.hops))
        .send()
        .await?;

    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;

    info!(target: "v3.exec", tx = %tx_hash, "Execution confirmed on-chain");

    Ok(tx_hash)
}

struct PathSimulation {
    input: U256,
    output: U256,
    profit: I256,
    step_outputs: Vec<U256>,
}

fn best_path_simulation_with_steps(path: &ArbitragePath, pools: &[AMM]) -> Option<PathSimulation> {
    const MIN_INPUT: u128 = 1_000_000_000_000;
    const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000;

    let best = best_path_simulation(path, pools, U256::from(MIN_INPUT), U256::from(MAX_INPUT))?;
    let (step_outputs, profit) = simulate_path_steps(path, pools, best.0).ok()?;

    Some(PathSimulation {
        input: best.0,
        output: best.1,
        profit,
        step_outputs,
    })
}

fn simulate_path_steps(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> Result<(Vec<U256>, I256)> {
    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        current = amm.simulate_swap(hop.token_in, hop.token_out, current)?;
        outputs.push(current);
    }
    Ok((outputs, I256::from_raw(current) - I256::from_raw(amount_in)))
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
        let (output, profit) = simulate_path_raw(path, pools, amount).ok()?;
        Some((amount, output, profit))
    };
    let mut candidates = vec![min_input];
    if max_input > min_input {
        candidates.push(max_input);
        candidates.push(min_input + (max_input - min_input) / U256::from(2));
    }
    let mut best = None;
    for amount in candidates.into_iter().filter(|a| *a >= min_input) {
        if let Some(candidate) = evaluate(amount) {
            if best.as_ref().map_or(true, |(_, _, p)| candidate.2 > *p) {
                best = Some(candidate);
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
    for _ in 0..64 {
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
            step = step.checked_div(U256::from(2)).unwrap_or_default();
        }
    }
    Some(best)
}

fn simulate_path_raw(path: &ArbitragePath, pools: &[AMM], amount_in: U256) -> Result<(U256, I256)> {
    let mut current = amount_in;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        current = amm.simulate_swap(hop.token_in, hop.token_out, current)?;
    }
    Ok((current, I256::from_raw(current) - I256::from_raw(amount_in)))
}

fn collect_agni_expected_states(pools: &[AMM]) -> Result<Vec<U256>> {
    pools
        .iter()
        .map(|amm| match amm {
            AMM::AgniPool(p) => Ok(vec![U256::from(p.sqrt_price), U256::from(p.liquidity)]),
            _ => Err(eyre!("Non-Agni pool encountered")),
        })
        .collect::<Result<Vec<_>>>()
        .map(|v| v.into_iter().flatten().collect())
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

fn log_pool_state(path: &Path, block_number: u64, event: &str, pool: &AgniPool) -> Result<()> {
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

fn log_positive_candidates(
    block_number: u64,
    candidates: &[PositiveCandidate],
    positive_log_path: &Path,
    best_path_log_path: &Path,
    logged_paths: &mut HashMap<String, LoggedPathRecord>,
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }

    let mut best_by_signature: HashMap<&str, &PositiveCandidate> = HashMap::new();
    for candidate in candidates {
        best_by_signature
            .entry(candidate.signature.as_str())
            .and_modify(|existing| {
                if candidate.profit > existing.profit {
                    *existing = candidate;
                }
            })
            .or_insert(candidate);
    }

    let mut unique_candidates: Vec<&PositiveCandidate> = best_by_signature.into_values().collect();
    unique_candidates.sort_by(|a, b| b.profit.cmp(&a.profit));

    if let Some(best) = unique_candidates.first() {
        let mut best_writer = WriterBuilder::new().has_headers(false).from_writer(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(best_path_log_path)?,
        );

        let mut record = StringRecord::new();
        record.push_field(&block_number.to_string());
        record.push_field(&best.signature);
        record.push_field(&best.hops.to_string());
        record.push_field(&best.input.to_string());
        record.push_field(&best.output.to_string());
        record.push_field(&best.profit.to_string());
        record.push_field(&best.net_profit.to_string());
        record.push_field(&best.roi);
        record.push_field(&best.log_hops);

        best_writer.write_record(&record)?;
        best_writer.flush()?;
    }

    const PROFIT_CHANGE_THRESHOLD: f64 = 5.0;
    let candidates_to_log: Vec<&PositiveCandidate> = unique_candidates
        .iter()
        .copied()
        .filter(|candidate| should_log_path(candidate, PROFIT_CHANGE_THRESHOLD, logged_paths))
        .collect();

    if candidates_to_log.is_empty() {
        return Ok(());
    }

    let mut writer = WriterBuilder::new().has_headers(false).from_writer(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(positive_log_path)?,
    );

    for candidate in &candidates_to_log {
        let mut record = StringRecord::new();
        record.push_field(&block_number.to_string());
        record.push_field(&candidate.signature);
        record.push_field(&candidate.hops.to_string());
        record.push_field(&candidate.input.to_string());
        record.push_field(&candidate.output.to_string());
        record.push_field(&candidate.profit.to_string());
        record.push_field(&candidate.net_profit.to_string());
        record.push_field(&candidate.roi);
        record.push_field(&candidate.log_hops);
        writer.write_record(&record)?;
    }
    writer.flush()?;

    update_logged_paths(&candidates_to_log, logged_paths);

    Ok(())
}

fn should_log_path(
    candidate: &PositiveCandidate,
    threshold_percent: f64,
    logged_paths: &HashMap<String, LoggedPathRecord>,
) -> bool {
    if let Some(last) = logged_paths.get(&candidate.signature) {
        let profit_diff = (candidate.profit - last.profit).abs();
        if last.profit.is_zero() {
            return !candidate.profit.is_zero();
        }

        let profit_diff_f64 = profit_diff.to_string().parse::<f64>().unwrap_or_default();
        let last_profit_f64 = last.profit.to_string().parse::<f64>().unwrap_or(1.0);

        let change_percent = if last_profit_f64.abs() < f64::EPSILON {
            0.0
        } else {
            (profit_diff_f64 / last_profit_f64.abs()) * 100.0
        };

        change_percent >= threshold_percent
    } else {
        true
    }
}

fn update_logged_paths(
    candidates: &[&PositiveCandidate],
    logged_paths: &mut HashMap<String, LoggedPathRecord>,
) {
    for candidate in candidates {
        logged_paths.insert(
            candidate.signature.clone(),
            LoggedPathRecord {
                profit: candidate.profit,
                input: candidate.input,
                output: candidate.output,
                roi: candidate.roi.clone(),
            },
        );
    }
}

fn record_selection_snapshot(
    selected: &[PositiveCandidate],
    history: &mut Option<SelectionSnapshot>,
) -> bool {
    let snapshot = SelectionSnapshot {
        signatures: selected.iter().map(|c| c.signature.clone()).collect(),
        profits: selected.iter().map(|c| c.profit).collect(),
    };

    let changed = history.as_ref() != Some(&snapshot);
    if changed {
        *history = Some(snapshot);
    }
    changed
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

fn apply_slippage(amount: U256, bps: u32) -> U256 {
    if bps == 0 {
        return amount;
    }
    amount * U256::from(10_000u32.saturating_sub(bps)) / U256::from(10_000u32)
}

fn init_tracing() {
    if tracing_subscriber::fmt::try_init().is_err() {
        let level = std::env::var("RUST_LOG")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(tracing::Level::INFO);
        let _ = tracing_subscriber::fmt()
            .with_target(true)
            .with_level(true)
            .with_file(true)
            .compact()
            .with_max_level(level)
            .try_init();
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
