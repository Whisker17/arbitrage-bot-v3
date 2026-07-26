use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol_types::SolEvent;
use alloy::transports::ws::WsConnect;
#[path = "../intent_service_support.rs"]
mod intent_service_support;
#[path = "../legacy_service_support.rs"]
mod legacy_service_support;
use amms::amms::{
    agni::{AgniPool, IAgniPoolEvents},
    amm::{AutomatedMarketMaker, Variant, AMM},
};
use amms::arbitrage::{
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::execution::{Executor, ExecutorConfig, IERC20};
use amms::state_space::{
    hash_pinned_logs_filter, hash_pinned_state_block_id, max_input_bound_for_snapshot,
    BlockHeaderContext, MarketSnapshot, PoolProtocol, ProtocolCoverage, SnapshotBoundBalance,
    SnapshotId, SnapshotStatus, StateSpace,
};
use csv::{ReaderBuilder, StringRecord, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use legacy_service_support::{
    gas_limit_for_hops, is_on_cooldown, plan_resized_execution_default_margin,
    route_is_structurally_valid, wait_for_block_logs, FailureStore, GasConfig,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
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
const MIN_QUOTE_INPUT: u128 = 1_000_000_000_000;
const MAX_QUOTE_INPUT: u128 = 1_000_000_000_000_000_000_000_000;

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
    snapshot_id: SnapshotId,
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
    path: ArbitragePath,
    pools: Vec<AMM>,
    log_hops: String,
    roi: String,
}

#[derive(Clone)]
struct GrossCandidate {
    snapshot_id: SnapshotId,
    signature: String,
    hops: usize,
    input: U256,
    output: U256,
    profit: I256,
    pool_addresses: Vec<Address>,
    token_path: Vec<Address>,
    amounts_out: Vec<U256>,
    expected_states: Vec<U256>,
    path: ArbitragePath,
    pools: Vec<AMM>,
    log_hops: String,
    roi: String,
}

struct CandidateCache {
    quotes: Vec<Option<GrossCandidate>>,
    initialized: bool,
    max_input_bound: Option<U256>,
    snapshot_id: Option<SnapshotId>,
}

impl CandidateCache {
    fn new(path_count: usize) -> Self {
        Self {
            quotes: vec![None; path_count],
            initialized: false,
            max_input_bound: None,
            snapshot_id: None,
        }
    }
}

#[derive(Clone)]
struct ExecutionJob {
    candidate: PositiveCandidate,
    block_number: u64,
    header: BlockHeaderContext,
    pool_universe_fingerprint: alloy::primitives::B256,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
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

struct PathCache {
    paths: Vec<ArbitragePath>,
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

    Ok(PathCache {
        paths,
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
    executor_config: ExecutorConfig,
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

        // Thread the env-derived economics into the Executor itself, so the pipeline
        // head enforces the same MIN_NET_PROFIT_WEI / priority fee the service screens
        // candidates with (mirrors v3_monitor_executor_service_1559).
        let mut executor_config = ExecutorConfig::default();
        executor_config.min_net_profit_mnt_wei = min_net_profit.clone();
        if let Ok(raw) = std::env::var("EXECUTOR_PRIORITY_FEE_WEI") {
            if let Ok(value) = raw.trim().parse::<u128>() {
                executor_config.default_priority_fee_wei = value;
            }
        }

        Ok(Self {
            ws_endpoint,
            http_endpoint,
            executor_address,
            wmnt_address,
            min_gross_profit,
            min_net_profit,
            execution_slippage_bps,
            block_cooldown,
            executor_config,
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
    let signer_address = signer.address();
    let wallet = EthereumWallet::from(signer);

    let http_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(config.http_endpoint.parse().expect("invalid http endpoint"));

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;

    // Degrade closed, never die: an executor-identity mismatch (e.g. a Sepolia
    // deployment vs the pinned mainnet gas-profile artifact) leaves `executor == None`
    // and the service runs monitor-only instead of killing monitoring at startup.
    let executor = intent_service_support::build_execution_runtime_or_monitor_only(
        http_provider.clone(),
        config.executor_address,
        config.wmnt_address,
        config.executor_config.clone(),
        "v3.service",
    )
    .await
    .map(Arc::new);

    info!(
        target: "v3.service",
        executor = %config.executor_address,
        execution_enabled = executor.is_some(),
        "Starting Agni (UniV3-style) monitoring + execution service on Mantle"
    );

    run_service(ws_provider, http_provider, config, executor, signer_address).await
}

async fn run_service<P, H>(
    ws_provider: P,
    http_provider: H,
    config: ServiceConfig,
    executor: Option<Arc<Executor>>,
    signer_address: Address,
) -> Result<()>
where
    P: Provider + Clone,
    H: Provider + Clone + Send + Sync + 'static,
{
    let chain_id = http_provider.get_chain_id().await?;
    if ws_provider.get_chain_id().await? != chain_id {
        return Err(eyre!(
            "HTTP and WS providers are connected to different chains"
        ));
    }
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
    let pin_hash = legacy_service_support::canonical_block_hash_at_number(
        &http_provider,
        latest_block,
    )
    .await?;
    let latest_block_id = amms::state_space::hash_pinned_state_block_id(pin_hash);
    amms::execution::verify_execution_signer_roles(
        &http_provider,
        config.executor_address,
        signer_address,
        pin_hash,
    )
    .await?;

    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    initialize_agni_pools(&ws_provider, latest_block_id, &mut pools).await?;

    if pools.is_empty() {
        warn!(target: "v3.service", "No Agni pools loaded. Exiting.");
        return Ok(());
    }
    let factory_address = std::env::var("AGNI_FACTORY_ADDRESS")
        .context("Missing AGNI_FACTORY_ADDRESS")?
        .parse()
        .context("Invalid AGNI_FACTORY_ADDRESS")?;
    let universe_amms: Vec<AMM> = pools.values().cloned().map(AMM::AgniPool).collect();
    legacy_service_support::verify_executable_pool_provenance(
        &http_provider,
        config.executor_address,
        factory_address,
        PoolProtocol::Agni,
        universe_amms.iter(),
        pin_hash,
    )
    .await?;
    let pool_universe_fingerprint = legacy_service_support::executable_pool_universe_fingerprint(
        chain_id,
        config.wmnt_address,
        factory_address,
        PoolProtocol::Agni,
        universe_amms.iter(),
    )?;

    info!(
        target: "v3.service",
        pools = pools.len(),
        "Initialized Agni pools"
    );

    for pool in pools.values() {
        log_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }
    let mut last_applied_block = latest_block;

    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    let path_cache = Arc::new(build_path_cache(&pools, MAX_HOPS, config.wmnt_address)?);
    let mut candidate_cache = CandidateCache::new(path_cache.paths.len());

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "v3.service", "Subscribed to block stream");

    let http_provider = Arc::new(http_provider);
    let last_executions = Arc::new(AsyncMutex::new(HashMap::<String, u64>::new()));
    let failed_store = Arc::new(AsyncMutex::new(FailureStore::new(
        FAILED_OPPORTUNITIES_PATH,
    )?));
    let logged_paths = Arc::new(AsyncMutex::new(HashMap::<String, LoggedPathRecord>::new()));
    let last_selection = Arc::new(AsyncMutex::new(None::<SelectionSnapshot>));

    let job_slot = intent_service_support::new_job_slot::<ExecutionJob>();
    let worker_slot = Arc::clone(&job_slot);
    let latest_tip = Arc::new(AsyncMutex::new(None::<SnapshotStatus>));
    let worker_latest_tip = Arc::clone(&latest_tip);
    let execution_halted = Arc::new(AtomicBool::new(false));
    let worker_execution_halted = Arc::clone(&execution_halted);

    let execution_config = Arc::clone(&config);
    let execution_provider = Arc::clone(&http_provider);
    let execution_last = Arc::clone(&last_executions);
    let execution_failed_store = Arc::clone(&failed_store);
    let execution_executor = executor.clone();
    let execution_task = tokio::spawn(async move {
        loop {
            let Some(job) = worker_slot.take() else {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                continue;
            };
            if !should_process_execution_job(&worker_execution_halted) {
                break;
            }
            let signature = OpportunitySignature::from_candidate(&job.candidate);
            if !route_is_structurally_valid(
                execution_config.wmnt_address,
                &job.candidate.token_path,
                &job.candidate.pool_addresses,
                Variant::AgniPool,
                &job.candidate.pools,
            ) {
                let mut store = execution_failed_store.lock().await;
                if let Err(mark_err) = store.mark_permanent(signature.clone()) {
                    error!(
                        target: "v3.failure_store",
                        error = ?mark_err,
                        "Failed to persist structural failure"
                    );
                }
                continue;
            }
            let failed_guard = execution_failed_store.lock().await;
            if failed_guard.is_failed(&signature) {
                info!(
                    target: "v3.exec",
                    block = job.block_number,
                    signature = %job.candidate.signature,
                    "Skipping queued opportunity after previous failure"
                );
                continue;
            }
            drop(failed_guard);
            let should_skip = {
                let executions = execution_last.lock().await;
                is_on_cooldown(
                    executions.get(&job.candidate.signature).copied(),
                    job.block_number,
                    execution_config.block_cooldown,
                )
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

            let live_status = match intent_service_support::require_matching_ready_tip(
                worker_latest_tip.lock().await.clone(),
                job.candidate.snapshot_id,
            ) {
                Ok(status) => status,
                Err(err) => {
                    error!(target: "v3.exec", error = %err, "Rejecting opportunity without a matching live Ready tip");
                    continue;
                }
            };

            // Monitor-only when the execution runtime is absent (executor identity did
            // not match the pinned gas-profile artifact at startup): never send.
            let Some(executor_for_job) = execution_executor.as_deref() else {
                info!(
                    target: "v3.exec",
                    block = job.block_number,
                    signature = %job.candidate.signature,
                    "Monitor-only: no execution runtime; not routing candidate through \
                     the pipeline head"
                );
                continue;
            };

            match attempt_execution(
                &*execution_provider,
                &job.candidate,
                execution_config.as_ref(),
                executor_for_job,
                signer_address,
                &live_status,
                job.header,
                job.pool_universe_fingerprint,
                job.base_fee_per_gas,
                job.block_gas_limit,
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
                    if let Err(mark_err) = store.mark_transient(signature) {
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
        let target_number = number;
        info!(target: "v3.block", block = target_number, "Processing block");

        let target_header = legacy_service_support::canonical_block_header(
            &http_provider,
            target_number,
            block.hash(),
        )
        .await?;
        let snapshot_id = SnapshotId::new(chain_id, target_number, target_header.header().hash);
        let header = BlockHeaderContext::new(
            target_header.header().parent_hash(),
            target_header.header().timestamp(),
        );
        // A fabricated zero base fee would be bound into the permit's BlockFeeContext
        // and silently mis-price every candidate in this block. Skip the block instead.
        let Some(base_fee_per_gas) = target_header.header().base_fee_per_gas() else {
            warn!(
                target: "v3.block",
                block = target_number,
                "Block header carries no base fee; skipping block (fee context must be exact)"
            );
            continue;
        };
        let base_fee_per_gas = base_fee_per_gas as u128;
        let block_gas_limit = target_header.header().gas_limit();
        let windowed = hash_pinned_logs_filter(filter.clone(), snapshot_id.block_hash);
        match wait_for_block_logs(
            &ws_provider,
            &windowed,
            target_number,
            snapshot_id.block_hash,
        )
        .await
        {
            Ok(logs) => {
                let changed = if !should_apply_block(target_number, last_applied_block) {
                    HashSet::new()
                } else if logs.is_empty() {
                    last_applied_block = target_number;
                    HashSet::new()
                } else {
                    match apply_logs(&mut pools, &logs, target_number, &pool_log_path) {
                        Ok(changed) => {
                            last_applied_block = target_number;
                            changed
                        }
                        Err(err) => {
                            execution_halted.store(true, Ordering::Release);
                            return Err(err);
                        }
                    }
                };
                let mut coverage = ProtocolCoverage::default();
                coverage.pool_universe_fingerprint = Some(pool_universe_fingerprint);
                let snapshot_pools = pools
                    .iter()
                    .map(|(address, pool)| (*address, AMM::AgniPool(pool.clone())))
                    .collect();
                let snapshot_status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
                    snapshot_id,
                    header,
                    snapshot_pools,
                    coverage,
                )));
                *latest_tip.lock().await = Some(snapshot_status.clone());
                let executor_balance = match executor_balance_at_snapshot(
                    http_provider.as_ref(),
                    config.as_ref(),
                    snapshot_id,
                )
                .await
                {
                    Ok(balance) => balance,
                    Err(err) => {
                        execution_halted.store(true, Ordering::Release);
                        return Err(err).context("Failed to read executor WMNT balance");
                    }
                };
                let Some(gas_config) = gas_config_for_base_fee(block.base_fee_per_gas()) else {
                    refresh_gross_quotes(
                        &pools,
                        config.as_ref(),
                        &path_cache,
                        &changed,
                        &mut candidate_cache,
                        snapshot_id,
                        executor_balance,
                    )?;
                    warn!(target: "v3.block", block = target_number, "Missing block base fee; refreshed gross quotes without candidate selection");
                    continue;
                };

                let mut selection_history = last_selection.lock().await;
                let mut logged = logged_paths.lock().await;

                let candidates = find_profitable_candidates(
                    &pools,
                    &gas_config,
                    config.as_ref(),
                    target_number,
                    &path_cache,
                    &changed,
                    &mut candidate_cache,
                    snapshot_id,
                    executor_balance,
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

                if candidates.is_empty() {
                    continue;
                }

                let selected_candidates = select_non_conflicting_opportunities(candidates);
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
                    job_slot.publish(ExecutionJob {
                        candidate,
                        block_number: target_number,
                        header,
                        pool_universe_fingerprint,
                        base_fee_per_gas,
                        block_gas_limit,
                    });
                }
            }
            Err(e) => {
                error!(target: "v3.block", block = target_number, error = ?e, "get_logs failed");
                return Err(e.into());
            }
        }
    }

    // worker uses latest-wins slot; no channel to drop
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
                        return Err(eyre!("failed to synchronize swap for pool {addr}: {e}"));
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
                        return Err(eyre!("failed to synchronize mint for pool {addr}: {e}"));
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
                        return Err(eyre!("failed to synchronize burn for pool {addr}: {e}"));
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
                        return Err(eyre!("failed to synchronize event for pool {addr}: {e}"));
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

fn should_apply_block(block_number: u64, last_applied_block: u64) -> bool {
    block_number > last_applied_block
}

async fn executor_balance_at_snapshot<H: Provider + Clone>(
    provider: &H,
    config: &ServiceConfig,
    snapshot_id: SnapshotId,
) -> Result<SnapshotBoundBalance> {
    intent_service_support::executor_balance_at_snapshot(
        provider,
        config.wmnt_address,
        config.executor_address,
        snapshot_id,
    )
    .await
}

fn should_process_execution_job(halted: &AtomicBool) -> bool {
    !halted.load(Ordering::Acquire)
}

fn live_quote_pools(pools: &HashMap<Address, AgniPool>) -> Vec<AMM> {
    pools.values().cloned().map(AMM::AgniPool).collect()
}

fn paths_to_requote(
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
    cache_initialized: bool,
    balance_bound_changed: bool,
) -> Vec<usize> {
    if !cache_initialized || balance_bound_changed {
        return (0..path_cache.paths.len()).collect();
    }

    let mut requote = vec![false; path_cache.paths.len()];
    for pool_address in changed_pools {
        if let Some(indices) = path_cache.pool_to_path_indices.get(pool_address) {
            for &index in indices {
                requote[index] = true;
            }
        }
    }

    requote
        .into_iter()
        .enumerate()
        .filter_map(|(index, affected)| affected.then_some(index))
        .collect()
}

fn refresh_cached_quotes<F>(
    candidate_cache: &mut CandidateCache,
    path_cache: &PathCache,
    path_indices: &[usize],
    quote_path: F,
) -> usize
where
    F: Fn(&ArbitragePath) -> Option<GrossCandidate> + Send + Sync,
{
    let updates: Vec<(usize, Option<GrossCandidate>)> = path_indices
        .par_iter()
        .map(|&path_index| (path_index, quote_path(&path_cache.paths[path_index])))
        .collect();

    for (path_index, quote) in updates {
        candidate_cache.quotes[path_index] = quote;
    }
    candidate_cache.initialized = true;

    path_indices.len()
}

fn gas_config_for_base_fee(base_fee_per_gas: Option<u64>) -> Option<GasConfig> {
    let mut gas_config = GasConfig::default();
    gas_config.gas_price_wei = u128::from(base_fee_per_gas?);
    Some(gas_config)
}

fn quote_gross_candidate(
    path: &ArbitragePath,
    quote_pools: &[AMM],
    config: &ServiceConfig,
    snapshot_id: SnapshotId,
    max_input_bound: U256,
) -> Option<GrossCandidate> {
    let pools_for_path = match pools_for_path(path, quote_pools) {
        Ok(pools_for_path) => pools_for_path,
        Err(err) => {
            error!(target: "v3.sim", error = ?err, "Failed to gather pools for path");
            return None;
        }
    };
    let simulation = best_path_simulation_with_steps(path, &pools_for_path, max_input_bound)?;

    if simulation.profit <= I256::ZERO {
        return None;
    }

    let profit_u256 = U256::from_limbs(*simulation.profit.as_limbs());
    if profit_u256 < config.min_gross_profit {
        return None;
    }

    let token_path = build_token_path(path);
    if token_path.first().copied() != Some(config.wmnt_address)
        || token_path.last().copied() != Some(config.wmnt_address)
    {
        return None;
    }

    let expected_states = match collect_agni_expected_states(&pools_for_path) {
        Ok(states) => states,
        Err(err) => {
            error!(target: "v3.state", error = ?err, "Failed to collect expected states");
            return None;
        }
    };

    Some(GrossCandidate {
        snapshot_id,
        signature: path_signature(path),
        hops: path.hops.len(),
        input: simulation.input,
        output: simulation.output,
        profit: simulation.profit,
        pool_addresses: path.hops.iter().map(|hop| hop.pool_address).collect(),
        token_path,
        amounts_out: simulation.step_outputs,
        expected_states,
        path: path.clone(),
        pools: pools_for_path,
        log_hops: hops_description(path),
        roi: format_roi_percent(simulation.profit, simulation.input)
            .unwrap_or_else(|| "-".to_string()),
    })
}

fn candidate_from_gross_quote(
    quote: &GrossCandidate,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    snapshot_id: SnapshotId,
) -> Option<PositiveCandidate> {
    if quote.snapshot_id != snapshot_id {
        return None;
    }

    let profit_u256 = U256::from_limbs(*quote.profit.as_limbs());
    let net_profit = gas_config.net_profit(profit_u256, quote.hops)?;
    if net_profit < config.min_net_profit
        || !gas_config.is_profitable_after_gas(profit_u256, quote.hops, 1.2)
    {
        return None;
    }

    Some(PositiveCandidate {
        snapshot_id,
        signature: quote.signature.clone(),
        hops: quote.hops,
        input: quote.input,
        output: quote.output,
        profit: quote.profit,
        net_profit,
        pool_addresses: quote.pool_addresses.clone(),
        token_path: quote.token_path.clone(),
        amounts_out: quote.amounts_out.clone(),
        expected_states: quote.expected_states.clone(),
        path: quote.path.clone(),
        pools: quote.pools.clone(),
        log_hops: quote.log_hops.clone(),
        roi: quote.roi.clone(),
    })
}

fn cached_candidates(
    candidate_cache: &CandidateCache,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    snapshot_id: SnapshotId,
) -> Vec<PositiveCandidate> {
    let mut candidates: Vec<PositiveCandidate> = candidate_cache
        .quotes
        .iter()
        .filter_map(|quote| {
            quote.as_ref().and_then(|quote| {
                candidate_from_gross_quote(quote, gas_config, config, snapshot_id)
            })
        })
        .collect();
    candidates.sort_by(|a, b| b.net_profit.cmp(&a.net_profit));
    candidates
}

fn refresh_gross_quotes(
    pools: &HashMap<Address, AgniPool>,
    config: &ServiceConfig,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
    candidate_cache: &mut CandidateCache,
    snapshot_id: SnapshotId,
    executor_balance: SnapshotBoundBalance,
) -> Result<usize> {
    if pools.is_empty() {
        return Ok(0);
    }

    let max_input_bound = effective_max_input(snapshot_id, executor_balance)?;
    let snapshot_changed = candidate_cache.snapshot_id != Some(snapshot_id);
    let balance_bound_changed = candidate_cache.max_input_bound != Some(max_input_bound);
    let path_indices = if snapshot_changed {
        (0..path_cache.paths.len()).collect()
    } else {
        paths_to_requote(
            path_cache,
            changed_pools,
            candidate_cache.initialized,
            balance_bound_changed,
        )
    };
    candidate_cache.max_input_bound = Some(max_input_bound);
    candidate_cache.snapshot_id = Some(snapshot_id);
    if !path_indices.is_empty() {
        let quote_pools = live_quote_pools(pools);
        Ok(refresh_cached_quotes(
            candidate_cache,
            path_cache,
            &path_indices,
            |path| quote_gross_candidate(path, &quote_pools, config, snapshot_id, max_input_bound),
        ))
    } else {
        candidate_cache.initialized = true;
        Ok(0)
    }
}

fn find_profitable_candidates(
    pools: &HashMap<Address, AgniPool>,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    block_number: u64,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
    candidate_cache: &mut CandidateCache,
    snapshot_id: SnapshotId,
    executor_balance: SnapshotBoundBalance,
) -> Result<Vec<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(Vec::new());
    }

    refresh_gross_quotes(
        pools,
        config,
        path_cache,
        changed_pools,
        candidate_cache,
        snapshot_id,
        executor_balance,
    )?;

    let candidates = cached_candidates(candidate_cache, gas_config, config, snapshot_id);

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

#[allow(clippy::too_many_arguments)]
async fn attempt_execution<H: Provider + Clone>(
    provider: &H,
    candidate: &PositiveCandidate,
    config: &ServiceConfig,
    executor: &Executor,
    signer_address: Address,
    snapshot_status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: alloy::primitives::B256,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> Result<alloy::primitives::TxHash> {
    let wmnt_contract = IERC20::new(config.wmnt_address, provider.clone());
    let executor_balance = wmnt_contract
        .balanceOf(config.executor_address)
        .call()
        .await?;

    let mut step_outputs = Vec::new();
    let mut measured_route_key = None;
    let plan = plan_resized_execution_default_margin(
        candidate.input,
        executor_balance,
        GasConfig::default().calculate_gas_cost(candidate.hops),
        config.min_net_profit,
        config.execution_slippage_bps,
        |amount_in| {
            let (outputs, _profit, route_key) =
                legacy_service_support::agni_path_steps_with_route_key(
                    &candidate.path,
                    &candidate.pools,
                    amount_in,
                )?;
            let output = outputs.last().copied().unwrap_or(amount_in);
            step_outputs = outputs;
            measured_route_key = Some(route_key);
            Ok::<U256, eyre::Report>(output)
        },
    )
    .map_err(|err| eyre!("Execution plan rejected after fresh simulation: {err}"))?;

    let mut amounts_out_with_slippage: Vec<U256> = step_outputs
        .iter()
        .map(|amount| apply_slippage(*amount, config.execution_slippage_bps))
        .collect();
    if let Some(last) = amounts_out_with_slippage.last_mut() {
        *last = (*last).max(plan.amount_in);
    }

    info!(
        target: "v3.exec",
        signature = %candidate.signature,
        hops = candidate.hops,
        input = %plan.amount_in,
        expected_output = %plan.simulated_output,
        "Routing candidate through the wallet-free pipeline head (WHI-553) pool_type=1"
    );

    let route_key =
        measured_route_key.ok_or_else(|| eyre!("final simulation did not measure route key"))?;
    let min_amount_out = intent_service_support::min_amount_out_from_plan(
        plan.amount_in,
        plan.simulated_output,
        &executor.config,
    );
    let inputs = intent_service_support::execution_params_inputs_from_pools(
        &candidate.pools,
        candidate.token_path.clone(),
        step_outputs,
        min_amount_out,
        plan.net_profit,
    )?;

    intent_service_support::run_candidate_through_pipeline_head(
        signer_address,
        executor,
        snapshot_status,
        header,
        pool_universe_fingerprint,
        route_key,
        plan.amount_in,
        inputs,
        executor.config.execution_deadline_secs,
        base_fee_per_gas,
        block_gas_limit,
    )
    .await
    .map_err(|err| eyre!("Pipeline head exercise failed: {err}"))?;

    if !intent_service_support::production_send_allowed() {
        return Err(eyre!(
            "production send is disabled until the execution gate is approved (WHI-526); pipeline head exercised"
        ));
    }
    Err(eyre!("production send path not enabled"))
}

fn m1_production_send_allowed() -> bool {
    intent_service_support::production_send_allowed()
}

struct PathSimulation {
    input: U256,
    output: U256,
    profit: I256,
    step_outputs: Vec<U256>,
}

fn effective_max_input(
    snapshot_id: SnapshotId,
    executor_balance: SnapshotBoundBalance,
) -> Result<U256> {
    Ok(max_input_bound_for_snapshot(
        snapshot_id,
        executor_balance,
        U256::from(MAX_QUOTE_INPUT),
    )?)
}

fn best_path_simulation_with_steps(
    path: &ArbitragePath,
    pools: &[AMM],
    max_input_bound: U256,
) -> Option<PathSimulation> {
    let min_input = U256::from(MIN_QUOTE_INPUT);
    if max_input_bound < min_input {
        return None;
    }

    let best = best_path_simulation(path, pools, min_input, max_input_bound)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Bytes, B256, U160};
    use amms::{amms::Token, arbitrage::pathfinder::PathHop};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn address(last_byte: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[19] = last_byte;
        Address::from(bytes)
    }

    fn snapshot_id(block_number: u64) -> SnapshotId {
        SnapshotId::new(5000, block_number, B256::repeat_byte(block_number as u8))
    }

    fn path(pool_addresses: &[Address]) -> ArbitragePath {
        ArbitragePath {
            hops: pool_addresses
                .iter()
                .enumerate()
                .map(|(index, pool_address)| PathHop {
                    pool_address: *pool_address,
                    token_in: address(10 + index as u8),
                    token_out: address(11 + index as u8),
                    fee_bps: 3_000,
                })
                .collect(),
        }
    }

    fn cycle_path(first_pool: Address, second_pool: Address) -> ArbitragePath {
        ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: first_pool,
                    token_in: address(10),
                    token_out: address(11),
                    fee_bps: 3_000,
                },
                PathHop {
                    pool_address: second_pool,
                    token_in: address(11),
                    token_out: address(10),
                    fee_bps: 3_000,
                },
            ],
        }
    }

    fn pool(pool_address: Address, sqrt_price: U256) -> AgniPool {
        let mut pool = AgniPool::new(pool_address);
        pool.token_a = Token::new_with_decimals(address(10), 18);
        pool.token_b = Token::new_with_decimals(address(11), 18);
        pool.liquidity = 1_000_000_000_000_000_000_000_000;
        pool.sqrt_price = sqrt_price;
        pool.fee = 3_000;
        pool.tick_spacing = 60;
        pool
    }

    fn swap_log(pool_address: Address, sqrt_price: U256) -> Log {
        let event = IAgniPoolEvents::Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: I256::ZERO,
            amount1: I256::ZERO,
            sqrtPriceX96: U160::from(sqrt_price),
            liquidity: 1_000_000_000_000_000_000_000_000u128,
            tick: alloy::primitives::Signed::<24, 1>::ZERO,
            protocolFeesToken0: 0,
            protocolFeesToken1: 0,
        };
        let encoded = event.encode_log_data();
        Log {
            inner: alloy::primitives::Log::new_unchecked(
                pool_address,
                encoded.topics().to_vec(),
                encoded.data.clone(),
            ),
            block_hash: None,
            block_number: Some(42),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    fn config() -> ServiceConfig {
        ServiceConfig {
            ws_endpoint: String::new(),
            http_endpoint: String::new(),
            executor_address: Address::ZERO,
            wmnt_address: address(10),
            min_gross_profit: U256::ZERO,
            min_net_profit: U256::ZERO,
            execution_slippage_bps: 0,
            block_cooldown: 0,
            executor_config: ExecutorConfig::default(),
        }
    }

    #[test]
    fn quotes_path_from_live_pool_state() {
        let pool_address = address(1);
        let startup_pool = pool(pool_address, U256::from(1) << 96);
        let live_pool = pool(pool_address, U256::from(2) << 96);
        let mut pools = HashMap::new();
        pools.insert(pool_address, live_pool);

        let quote_pools = pools_for_path(&path(&[pool_address]), &live_quote_pools(&pools))
            .expect("live pool must be available to the quote");
        let startup_quote = simulate_path_raw(
            &path(&[pool_address]),
            &[AMM::AgniPool(startup_pool.clone())],
            U256::from(1_000_000_000_000u64),
        )
        .expect("startup pool quote must succeed");
        let live_quote = simulate_path_raw(
            &path(&[pool_address]),
            &quote_pools,
            U256::from(1_000_000_000_000u64),
        )
        .expect("live pool quote must succeed");

        match &quote_pools[0] {
            AMM::AgniPool(quoted_pool) => {
                assert_eq!(quoted_pool.sqrt_price, U256::from(2) << 96);
                assert_ne!(quoted_pool.sqrt_price, startup_pool.sqrt_price);
                assert_ne!(live_quote.0, startup_quote.0);
            }
            _ => panic!("expected an Agni pool"),
        }
    }

    #[test]
    fn block_n_pipeline_quotes_live_pools_with_balance_bound() {
        let first_pool = address(1);
        let second_pool = address(2);
        let route = cycle_path(first_pool, second_pool);
        let path_cache = PathCache {
            paths: vec![route],
            pool_to_path_indices: HashMap::from([(first_pool, vec![0]), (second_pool, vec![0])]),
        };
        let pools = HashMap::from([
            (first_pool, pool(first_pool, U256::from(1) << 96)),
            (second_pool, pool(second_pool, U256::from(1) << 95)),
        ]);
        let mut candidate_cache = CandidateCache::new(path_cache.paths.len());
        let candidates = find_profitable_candidates(
            &pools,
            &GasConfig::default(),
            &config(),
            42,
            &path_cache,
            &HashSet::from([first_pool, second_pool]),
            &mut candidate_cache,
            snapshot_id(42),
            SnapshotBoundBalance::new(snapshot_id(42), U256::from(MAX_QUOTE_INPUT)),
        )
        .expect("live block candidate selection must succeed");

        assert_eq!(
            candidate_cache.max_input_bound,
            Some(U256::from(MAX_QUOTE_INPUT))
        );
        assert!(
            !candidates.is_empty(),
            "live pool state should produce a candidate"
        );
        let lower_balance = U256::from(MIN_QUOTE_INPUT * 2);
        assert_eq!(
            refresh_gross_quotes(
                &pools,
                &config(),
                &path_cache,
                &HashSet::new(),
                &mut candidate_cache,
                snapshot_id(42),
                SnapshotBoundBalance::new(snapshot_id(42), lower_balance),
            )
            .unwrap(),
            1
        );
        assert_eq!(
            refresh_gross_quotes(
                &pools,
                &config(),
                &path_cache,
                &HashSet::new(),
                &mut candidate_cache,
                snapshot_id(42),
                SnapshotBoundBalance::new(snapshot_id(42), lower_balance),
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn block_n_price_log_changes_cached_candidate_quote() {
        let first_pool = address(1);
        let second_pool = address(2);
        let path_cache = PathCache {
            paths: vec![cycle_path(first_pool, second_pool)],
            pool_to_path_indices: HashMap::from([(first_pool, vec![0]), (second_pool, vec![0])]),
        };
        let mut pools = HashMap::from([
            (first_pool, pool(first_pool, U256::from(1) << 96)),
            (second_pool, pool(second_pool, U256::from(1) << 94)),
        ]);
        let mut candidate_cache = CandidateCache::new(path_cache.paths.len());
        let initial_candidates = find_profitable_candidates(
            &pools,
            &GasConfig::default(),
            &config(),
            41,
            &path_cache,
            &HashSet::from([first_pool, second_pool]),
            &mut candidate_cache,
            snapshot_id(41),
            SnapshotBoundBalance::new(snapshot_id(41), U256::from(MAX_QUOTE_INPUT)),
        )
        .expect("block N-1 candidate selection must succeed");
        assert_eq!(initial_candidates.len(), 1);
        let initial_quote = (initial_candidates[0].input, initial_candidates[0].output);

        let log_path =
            std::env::temp_dir().join(format!("whi-511-block-n-quote-{}.log", std::process::id()));
        let changed = apply_logs(
            &mut pools,
            &[swap_log(first_pool, U256::from(2) << 96)],
            42,
            &log_path,
        )
        .expect("block N price log must synchronize");
        let _ = std::fs::remove_file(&log_path);
        assert_eq!(changed, HashSet::from([first_pool]));

        let updated_candidates = find_profitable_candidates(
            &pools,
            &GasConfig::default(),
            &config(),
            42,
            &path_cache,
            &changed,
            &mut candidate_cache,
            snapshot_id(42),
            SnapshotBoundBalance::new(snapshot_id(42), U256::from(MAX_QUOTE_INPUT)),
        )
        .expect("block N candidate selection must succeed");
        assert_eq!(updated_candidates.len(), 1);
        assert_ne!(
            initial_quote,
            (updated_candidates[0].input, updated_candidates[0].output)
        );
    }

    #[test]
    fn balance_bound_is_capped_and_rejects_below_minimum() {
        let id = snapshot_id(42);
        assert_eq!(
            effective_max_input(id, SnapshotBoundBalance::new(id, U256::from(123u64))).unwrap(),
            U256::from(123u64)
        );
        assert_eq!(
            effective_max_input(id, SnapshotBoundBalance::new(id, U256::MAX)).unwrap(),
            U256::from(MAX_QUOTE_INPUT)
        );
        assert!(best_path_simulation_with_steps(
            &path(&[address(1)]),
            &[AMM::AgniPool(pool(address(1), U256::from(1) << 96))],
            U256::from(MIN_QUOTE_INPUT - 1),
        )
        .is_none());
    }

    #[test]
    fn does_not_reapply_the_initialization_block() {
        assert!(!should_apply_block(42, 42));
        assert!(!should_apply_block(41, 42));
        assert!(should_apply_block(43, 42));
    }

    #[test]
    fn halted_execution_worker_drops_queued_jobs() {
        let halted = AtomicBool::new(false);
        assert!(should_process_execution_job(&halted));
        halted.store(true, Ordering::Release);
        assert!(!should_process_execution_job(&halted));
    }

    #[test]
    fn persistent_candidate_remains_eligible_across_blocks() {
        let pool_addresses = vec![address(1), address(2)];
        let candidate = PositiveCandidate {
            snapshot_id: snapshot_id(42),
            signature: "route".to_string(),
            hops: 2,
            input: U256::from(1),
            output: U256::from(2),
            profit: I256::from_raw(U256::from(1)),
            net_profit: U256::from(1),
            pool_addresses,
            token_path: vec![address(10), address(11), address(10)],
            amounts_out: Vec::new(),
            expected_states: Vec::new(),
            path: cycle_path(address(1), address(2)),
            pools: vec![
                AMM::AgniPool(pool(address(1), U256::from(1) << 96)),
                AMM::AgniPool(pool(address(2), U256::from(1) << 96)),
            ],
            log_hops: String::new(),
            roi: String::new(),
        };
        let signature = OpportunitySignature::from_candidate(&candidate);
        let failure_store_path = std::env::temp_dir().join(format!(
            "whi-515-v3-persistent-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let failure_store_path = failure_store_path.to_string_lossy().into_owned();
        let failure_store = FailureStore::with_ttl(&failure_store_path, 60).unwrap();
        let mut last_execution_block = None;

        assert!(is_on_cooldown(Some(1), 1, 1));
        assert!(!is_on_cooldown(Some(1), 2, 1));

        for block in 1..=8 {
            assert!(route_is_structurally_valid(
                address(10),
                &candidate.token_path,
                &candidate.pool_addresses,
                Variant::AgniPool,
                &candidate.pools,
            ));
            assert!(!failure_store.is_failed(&signature));
            assert!(!is_on_cooldown(last_execution_block, block, 1));

            let selected = select_non_conflicting_opportunities(vec![candidate.clone()]);
            assert_eq!(selected.len(), 1);
            last_execution_block = Some(block);
        }

        let _ = std::fs::remove_file(failure_store_path);
    }

    #[test]
    fn rejects_failed_pool_sync() {
        let pool_address = address(1);
        let sqrt_price = U256::from(1) << 96;
        let mut pools = HashMap::from([(pool_address, pool(pool_address, sqrt_price))]);
        let invalid_log = Log {
            inner: alloy::primitives::Log::new_unchecked(
                pool_address,
                vec![B256::ZERO],
                Bytes::new(),
            ),
            block_hash: None,
            block_number: Some(42),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };

        assert!(apply_logs(&mut pools, &[invalid_log], 42, Path::new("unused.log")).is_err());
        assert_eq!(pools[&pool_address].sqrt_price, sqrt_price);
    }

    #[test]
    fn requotes_only_paths_touching_changed_pools_after_cache_warmup() {
        let first = address(1);
        let second = address(2);
        let third = address(3);
        let cache = PathCache {
            paths: vec![
                path(&[first, second]),
                path(&[second, third]),
                path(&[third]),
            ],
            pool_to_path_indices: HashMap::from([
                (first, vec![0]),
                (second, vec![0, 1]),
                (third, vec![1, 2]),
            ]),
        };

        assert_eq!(
            paths_to_requote(&cache, &HashSet::new(), false, false),
            vec![0, 1, 2]
        );
        let affected = paths_to_requote(&cache, &HashSet::from([second]), true, false);
        assert_eq!(affected, vec![0, 1]);
        assert_eq!(
            paths_to_requote(&cache, &HashSet::new(), true, true),
            vec![0, 1, 2]
        );

        let quote_calls = AtomicUsize::new(0);
        let mut candidate_cache = CandidateCache::new(cache.paths.len());
        candidate_cache.initialized = true;
        let re_quoted = refresh_cached_quotes(&mut candidate_cache, &cache, &affected, |_| {
            quote_calls.fetch_add(1, Ordering::Relaxed);
            None
        });
        assert_eq!(re_quoted, 2);
        assert_eq!(quote_calls.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn missing_fee_context_refreshes_changed_gross_quotes() {
        let pool_address = address(1);
        let mut pools = HashMap::new();
        pools.insert(pool_address, pool(pool_address, U256::from(1) << 96));
        let path_cache = PathCache {
            paths: vec![path(&[pool_address])],
            pool_to_path_indices: HashMap::from([(pool_address, vec![0])]),
        };
        let mut candidate_cache = CandidateCache::new(path_cache.paths.len());
        candidate_cache.initialized = true;

        assert_eq!(
            refresh_gross_quotes(
                &pools,
                &config(),
                &path_cache,
                &HashSet::from([pool_address]),
                &mut candidate_cache,
                snapshot_id(42),
                SnapshotBoundBalance::new(snapshot_id(42), U256::from(MAX_QUOTE_INPUT)),
            )
            .unwrap(),
            1
        );
    }

    #[test]
    fn cached_quote_can_become_ineligible_when_block_fee_changes() {
        let quote = GrossCandidate {
            snapshot_id: snapshot_id(42),
            signature: "route".to_string(),
            hops: 2,
            input: U256::from(1u64),
            output: U256::from(2_000_000_001u64),
            profit: I256::from_raw(U256::from(2_000_000_000u64)),
            pool_addresses: vec![address(1), address(2)],
            token_path: vec![address(10), address(11), address(10)],
            amounts_out: vec![U256::from(2_000_000_001u64)],
            expected_states: Vec::new(),
            path: ArbitragePath { hops: Vec::new() },
            pools: Vec::new(),
            log_hops: "route".to_string(),
            roi: "-".to_string(),
        };

        let low_fee = gas_config_for_base_fee(Some(1)).expect("base fee is present");
        let high_fee = gas_config_for_base_fee(Some(3)).expect("base fee is present");

        let candidate_cache = CandidateCache {
            quotes: vec![Some(quote)],
            initialized: true,
            max_input_bound: None,
            snapshot_id: Some(snapshot_id(42)),
        };
        assert_eq!(
            cached_candidates(&candidate_cache, &low_fee, &config(), snapshot_id(42)).len(),
            1
        );
        assert!(
            cached_candidates(&candidate_cache, &high_fee, &config(), snapshot_id(42)).is_empty()
        );
    }
}
