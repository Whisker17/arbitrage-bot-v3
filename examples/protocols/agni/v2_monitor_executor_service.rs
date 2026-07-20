use alloy::consensus::BlockHeader;
use alloy::network::primitives::BlockResponse;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolEvent;
use alloy::transports::ws::WsConnect;
use amms::amms::{
    amm::{AutomatedMarketMaker, AMM},
    uniswap_v2::{IUniswapV2Pair, UniswapV2Pool},
};
use amms::arbitrage::{
    gas::GasConfig,
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::execution::{
    gas_schedule::gas_limit_for_hops, plan_resized_execution_default_margin, IArbitrageExecutor,
    IERC20,
};
use amms::state_space::{
    hash_pinned_logs_filter, hash_pinned_state_block_id, max_input_bound_for_snapshot,
    SnapshotBoundBalance, SnapshotId, StateSpace,
};
use csv::{ReaderBuilder, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

const MAX_HOPS: usize = 4;
const V2_FEE_BPS: usize = 300; // 0.3%
const MIN_QUOTE_INPUT: u128 = 1_000_000_000_000;
const MAX_QUOTE_INPUT: u128 = 1_000_000_000_000_000_000_000_000;

// region: --- 新增和修改的结构体

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct OpportunitySignature {
    pool_addresses: Vec<Address>,
    token_path: Vec<Address>,
    quantized_input: U256,
}

impl OpportunitySignature {
    fn from_candidate(candidate: &PositiveCandidate) -> Self {
        // 量化输入金额以聚合相似的机会，例如，按 10^12 取整
        let quantization_factor = U256::from(1_000_000_000_000u64);
        let quantized_input = (candidate.input / quantization_factor) * quantization_factor;

        Self {
            pool_addresses: candidate.pool_addresses.clone(),
            token_path: candidate.token_path.clone(),
            quantized_input,
        }
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
            create_dir_all(parent)?;
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
            info!(target: "v2.failure_store", loaded = self.failed_signatures.len(), "Loaded failed opportunities");
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
            serde_json::to_writer(file, &signatures)?;
            warn!(target: "v2.failure_store", "Marked opportunity as failed and saved to store");
        }
        Ok(())
    }
}

struct AppearanceTracker {
    // Key: Path signature (String), Value: (last_seen_block, distinct_block_count)
    appearances: HashMap<String, (u64, u32)>,
    // 如果一个机会连续出现超过这个区块数，就过滤掉它
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
        let mut filtered = Vec::new();
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
                    target: "v2.tracker",
                    signature = %candidate.signature,
                    count = entry.1,
                    "Filtered stale opportunity"
                );
            }
        }
        filtered
    }
}

struct OpportunityCsvLogger {
    writer: csv::Writer<File>,
}

impl OpportunityCsvLogger {
    fn new(path: &str) -> Result<Self> {
        let path = Path::new(path);
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        let file_exists = path.exists();
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .append(true)
            .open(path)?;
        let mut writer = WriterBuilder::new().from_writer(file);

        if !file_exists {
            writer.write_record([
                "timestamp",
                "block_number",
                "signature",
                "hops",
                "input_amount",
                "gross_profit",
                "net_profit",
                "path_description",
            ])?;
            writer.flush()?;
        }
        Ok(Self { writer })
    }

    fn log_opportunity(&mut self, candidate: &PositiveCandidate, block_number: u64) -> Result<()> {
        self.writer.write_record([
            unix_timestamp().to_string(),
            block_number.to_string(),
            candidate.signature.clone(),
            candidate.hops.to_string(),
            candidate.input.to_string(),
            candidate.profit.to_string(),
            candidate.net_profit.to_string(),
            candidate.log_hops.clone(),
        ])?;
        self.writer.flush()?;
        Ok(())
    }
}

struct MarketSnapshot {
    snapshot_id: SnapshotId,
    pools: HashMap<Address, AMM>,
}

// endregion

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
        let ws_endpoint = std::env::var("RPC_WS_URL")
            .or_else(|_| std::env::var("MANTLE_WS_URL"))
            .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());

        let http_endpoint = std::env::var("RPC_HTTP_URL")
            .or_else(|_| std::env::var("MANTLE_HTTP_URL"))
            .or_else(|_| std::env::var("MANTLE_SEPOLIA_RPC_URL"))
            .unwrap_or_else(|_| "https://rpc.sepolia.mantle.xyz".to_string());

        let executor_address = std::env::var("ARBITRAGE_EXECUTOR_ADDRESS")
            .map(|s| Address::from_str(s.trim()))
            .unwrap_or_else(|_| Address::from_str("0x59E5019B0d0e40762Df46fE472c0ae5a5c80b80f"))
            .context("Invalid ARBITRAGE_EXECUTOR_ADDRESS")?;

        let wmnt_address = std::env::var("SERVICE_WMNT_ADDRESS")
            .map(|s| Address::from_str(s.trim()))
            .unwrap_or_else(|_| Address::from_str("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"))
            .context("Invalid SERVICE_WMNT_ADDRESS")?;

        let min_gross_profit = std::env::var("MIN_GROSS_PROFIT_WEI")
            .ok()
            .and_then(|s| U256::from_str(&s).ok())
            .unwrap_or_else(|| U256::ZERO);

        let min_net_profit = std::env::var("MIN_NET_PROFIT_WEI")
            .ok()
            .and_then(|s| U256::from_str(&s).ok())
            .unwrap_or_else(|| U256::ZERO);

        let execution_slippage_bps = std::env::var("EXECUTION_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(30); // 0.3%

        let block_cooldown = std::env::var("EXECUTION_BLOCK_COOLDOWN")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);

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

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_tracing();

    let config = ServiceConfig::from_env()?;

    let private_key = std::env::var("EXECUTION_PRIVATE_KEY")
        .or_else(|_| std::env::var("MANTLE_SEPOLIA_PRIVATE_KEY"))
        .context("Missing EXECUTION_PRIVATE_KEY or MANTLE_SEPOLIA_PRIVATE_KEY")?;

    let signer = PrivateKeySigner::from_str(private_key.trim())?;
    let wallet = EthereumWallet::from(signer.clone());

    let http_provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect_http(config.http_endpoint.parse().expect("invalid http endpoint"));

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;

    let failed_store = Arc::new(Mutex::new(FailedOpportunityStore::new(
        "logs/failed_opportunities.json",
    )?));
    let mut appearance_tracker = AppearanceTracker::new(3);
    let mut csv_logger = OpportunityCsvLogger::new("logs/opportunities.csv")?;

    info!(
        target: "v2.service",
        executor = %config.executor_address,
        "Starting Uniswap V2 monitoring + execution service"
    );

    run_service(
        ws_provider,
        http_provider,
        config,
        failed_store,
        &mut appearance_tracker,
        &mut csv_logger,
    )
    .await
}

async fn run_service<P, H>(
    ws_provider: P,
    http_provider: H,
    config: ServiceConfig,
    failed_store: Arc<Mutex<FailedOpportunityStore>>,
    appearance_tracker: &mut AppearanceTracker,
    csv_logger: &mut OpportunityCsvLogger,
) -> Result<()>
where
    P: Provider + Clone,
    H: Provider + Clone,
{
    let chain_id = http_provider.get_chain_id().await?;
    if ws_provider.get_chain_id().await? != chain_id {
        return Err(eyre!(
            "HTTP and WS providers are connected to different chains"
        ));
    }
    let latest_block = ws_provider.get_block_number().await?;
    let latest_block_id = alloy::eips::BlockId::from(latest_block);

    let mut pools: HashMap<Address, AMM> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();

    initialize_v2_pools(&ws_provider, latest_block_id, &mut pools, &mut fee_tiers).await?;

    if pools.is_empty() {
        warn!(target: "v2.service", "No V2 pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "v2.service",
        pools = pools.len(),
        "Initialized Uniswap V2 pools"
    );

    let mut filter = Filter::new()
        .event_signature(FilterSet::from(vec![IUniswapV2Pair::Sync::SIGNATURE_HASH]))
        .address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "v2.service", "Subscribed to block stream");

    let mut last_executions: HashMap<String, u64> = HashMap::new();
    let gas_config = GasConfig::default();

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "v2.block", block = target_number, "Processing block");

        let target_header = http_provider
            .get_block_by_number(target_number.into())
            .await?
            .ok_or_else(|| eyre!("missing block {target_number}"))?;
        let snapshot_id = SnapshotId::new(chain_id, target_number, target_header.header().hash);
        let windowed = hash_pinned_logs_filter(filter.clone(), snapshot_id.block_hash);
        match ws_provider.get_logs(&windowed).await {
            Ok(logs) => {
                if logs.is_empty() {
                    continue;
                }

                let market_snapshot =
                    apply_logs(&mut pools, &logs, snapshot_id).context("apply_logs")?;
                let executor_balance =
                    executor_balance_at_snapshot(&http_provider, &config, snapshot_id)
                        .await
                        .context("Failed to read snapshot-bound executor WMNT balance")?;

                let all_candidates = find_all_profitable_candidates(
                    &market_snapshot,
                    &gas_config,
                    &config,
                    executor_balance,
                )?;

                // 过滤掉陈旧的机会
                let fresh_candidates =
                    appearance_tracker.filter_and_update(target_number, all_candidates);
                if fresh_candidates.is_empty() {
                    continue;
                }

                // 记录所有新鲜机会到CSV
                for candidate in &fresh_candidates {
                    if let Err(e) = csv_logger.log_opportunity(candidate, target_number) {
                        error!(target: "v2.csv", error = ?e, "Failed to log opportunity");
                    }
                }

                // 选择无冲突的机会组合
                let selected_opportunities = select_non_conflicting_opportunities(fresh_candidates);

                if selected_opportunities.is_empty() {
                    continue;
                }

                info!(
                    target: "v2.selection",
                    block = target_number,
                    count = selected_opportunities.len(),
                    "Selected non-conflicting opportunities for execution"
                );

                for candidate in selected_opportunities {
                    let signature = OpportunitySignature::from_candidate(&candidate);
                    let store = failed_store.lock().await;
                    if store.is_failed(&signature) {
                        info!(
                            target: "v2.exec",
                            block = target_number,
                            signature = %candidate.signature,
                            "Skipping execution due to previous failure"
                        );
                        continue;
                    }
                    drop(store); // 释放锁

                    let should_skip = last_executions
                        .get(&candidate.signature)
                        .map(|last_block| {
                            target_number.saturating_sub(*last_block) < config.block_cooldown
                        })
                        .unwrap_or(false);

                    if should_skip {
                        info!(
                            target: "v2.exec",
                            block = target_number,
                            signature = %candidate.signature,
                            "Skipping execution due to cooldown"
                        );
                        continue;
                    }

                    match attempt_execution(&http_provider, &candidate, &config).await {
                        Ok(tx_hash) => {
                            info!(
                                target: "v2.exec",
                                block = target_number,
                                tx = %tx_hash,
                                signature = %candidate.signature,
                                profit = %candidate.profit,
                                net_profit = %candidate.net_profit,
                                "Submitted arbitrage execution"
                            );
                            last_executions.insert(candidate.signature.clone(), target_number);
                        }
                        Err(err) => {
                            error!(
                                target: "v2.exec",
                                block = target_number,
                                signature = %candidate.signature,
                                error = ?err,
                                "Execution attempt failed"
                            );
                            // 将失败的签名记录下来
                            let mut store = failed_store.lock().await;
                            if let Err(e) = store.mark_as_failed(signature) {
                                error!(target: "v2.failure_store", error = ?e, "Failed to save failed opportunity");
                            }
                        }
                    }
                }
            }
            Err(err) => {
                error!(
                    target: "v2.block",
                    block = target_number,
                    error = ?err,
                    "Failed to fetch logs"
                );
                sleep(Duration::from_millis(250)).await;
            }
        }
    }

    Ok(())
}

fn select_non_conflicting_opportunities(
    mut candidates: Vec<PositiveCandidate>,
) -> Vec<PositiveCandidate> {
    // 按净利润降序排序
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

async fn initialize_v2_pools<P: Provider + Clone>(
    provider: &P,
    block_id: alloy::eips::BlockId,
    pools: &mut HashMap<Address, AMM>,
    fee_tiers: &mut HashMap<Address, Option<u32>>,
) -> Result<()> {
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists_v2.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(file);

    let mut init_jobs = Vec::new();

    for row in rdr.deserialize::<V2PoolRow>() {
        let row = row?;
        let addr = Address::from_str(row.Pair_Address.trim()).context("Invalid pool address")?;
        init_jobs.push(addr);
    }

    if init_jobs.is_empty() {
        warn!(target: "v2.service", "No pools found in CSV");
        return Ok(());
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let pool = UniswapV2Pool::new(addr, V2_FEE_BPS)
                .init::<_, _>(block_id, provider)
                .await;
            (addr, pool)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, pool)) = init_stream.next().await {
        match pool {
            Ok(amm) => {
                info!(target: "v2.init", address = %addr, "Initialized V2 pool");
                fee_tiers.insert(addr, Some(V2_FEE_BPS as u32));
                pools.insert(addr, AMM::from(amm));
            }
            Err(err) => {
                error!(
                    target: "v2.init",
                    address = %addr,
                    error = ?err,
                    "Failed to initialize pool"
                );
            }
        }
    }

    Ok(())
}

fn apply_logs(
    pools: &mut HashMap<Address, AMM>,
    logs: &[Log],
    snapshot_id: SnapshotId,
) -> Result<MarketSnapshot> {
    let mut changed = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(amm) = pools.get_mut(&addr) {
            if let Err(err) = amm.sync(log) {
                error!(
                    target: "v2.pool",
                    address = %addr,
                    error = ?err,
                    "Failed to sync pool"
                );
                continue;
            }
            changed.insert(addr);
        }
    }
    if !changed.is_empty() {
        info!(
            target: "v2.pool",
            block = snapshot_id.block_number,
            count = changed.len(),
            "Applied Sync events"
        );
    }

    Ok(MarketSnapshot {
        snapshot_id,
        pools: pools.clone(),
    })
}

async fn executor_balance_at_snapshot<H: Provider + Clone>(
    provider: &H,
    config: &ServiceConfig,
    snapshot_id: SnapshotId,
) -> Result<SnapshotBoundBalance> {
    let wmnt_contract = IERC20::new(config.wmnt_address, provider.clone());
    let amount = wmnt_contract
        .balanceOf(config.executor_address)
        .call()
        .block(hash_pinned_state_block_id(snapshot_id.block_hash))
        .await?;
    Ok(SnapshotBoundBalance::new(snapshot_id, amount))
}

fn find_all_profitable_candidates(
    snapshot: &MarketSnapshot,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    executor_balance: SnapshotBoundBalance,
) -> Result<Vec<PositiveCandidate>> {
    if snapshot.pools.is_empty() {
        return Ok(Vec::new());
    }

    let mut state = StateSpace::default();
    for amm in snapshot.pools.values() {
        state.state.insert(amm.address(), amm.clone());
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(target: "v2.graph", error = ?err, "Failed to build graph");
            return Ok(Vec::new());
        }
    };

    let constraints = PathConstraints {
        max_length: MAX_HOPS,
        required_start_token: Some(config.wmnt_address),
        required_end_token: Some(config.wmnt_address),
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
        return Ok(Vec::new());
    }

    let state_pools: Vec<AMM> = state.state.values().cloned().collect();
    let max_input_bound = max_input_bound_for_snapshot(
        snapshot.snapshot_id,
        executor_balance,
        U256::from(MAX_QUOTE_INPUT),
    )?;
    let mut candidates = Vec::new();

    for (signature, path) in unique_paths.into_iter() {
        let pools_for_path = match pools_for_path(&path, &state_pools) {
            Ok(p) => p,
            Err(err) => {
                error!(target: "v2.sim", error = ?err, "Failed to gather pools for path");
                continue;
            }
        };

        let simulation =
            match best_path_simulation_with_steps(&path, &pools_for_path, max_input_bound) {
                Some(sim) => sim,
                None => continue,
            };

        if simulation.profit <= I256::ZERO {
            continue;
        }

        let profit_u256 = U256::from_limbs(*simulation.profit.as_limbs());
        if profit_u256 < config.min_gross_profit {
            continue;
        }

        let num_hops = path.hops.len();
        let net_profit = match gas_config.net_profit(profit_u256, num_hops) {
            Some(net) => net,
            None => continue,
        };

        if net_profit < config.min_net_profit {
            continue;
        }

        let mut token_path = build_token_path(&path);
        if token_path.first().copied() != Some(config.wmnt_address)
            || token_path.last().copied() != Some(config.wmnt_address)
        {
            continue;
        }

        let pool_addresses: Vec<Address> = path.hops.iter().map(|hop| hop.pool_address).collect();

        let expected_states = match collect_v2_expected_states(&pools_for_path) {
            Ok(states) => states,
            Err(err) => {
                error!(target: "v2.state", error = ?err, "Failed to collect expected states");
                continue;
            }
        };

        let candidate = PositiveCandidate {
            snapshot_id: snapshot.snapshot_id,
            signature,
            hops: num_hops,
            input: simulation.input,
            output: simulation.output,
            profit: simulation.profit,
            net_profit,
            pool_addresses,
            token_path: token_path.drain(..).collect(),
            amounts_out: simulation.step_outputs.clone(),
            expected_states,
            path: path.clone(),
            pools: pools_for_path.clone(),
            log_hops: hops_description(&path),
        };

        candidates.push(candidate);
    }

    if !candidates.is_empty() {
        info!(
            target: "v2.candidate",
            block = snapshot.snapshot_id.block_number,
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

    let mut step_outputs = Vec::new();
    let plan = plan_resized_execution_default_margin(
        candidate.input,
        executor_balance,
        GasConfig::default().calculate_gas_cost(candidate.hops),
        config.min_net_profit,
        config.execution_slippage_bps,
        |amount_in| {
            let (outputs, _profit) =
                simulate_path_steps(&candidate.path, &candidate.pools, amount_in)?;
            let output = outputs.last().copied().unwrap_or(amount_in);
            step_outputs = outputs;
            Ok::<U256, eyre::Report>(output)
        },
    )
    .map_err(|err| eyre!("Execution plan rejected after fresh simulation: {err}"))?;

    if plan.was_resized {
        info!(
            target: "v2.exec",
            original_input = %candidate.input,
            adjusted_input = %plan.amount_in,
            available = %executor_balance,
            "Executor balance insufficient; re-simulated path at adjusted input"
        );
    }

    // 正确地为每一步都应用滑点
    let amounts_out_with_slippage: Vec<U256> = step_outputs
        .iter()
        .map(|amount| apply_slippage(*amount, config.execution_slippage_bps))
        .collect();

    let pool_types = vec![0u8; candidate.pool_addresses.len()];

    info!(
        target: "v2.exec",
        signature = %candidate.signature,
        hops = candidate.hops,
        input = %plan.amount_in,
        expected_output = %plan.simulated_output,
        "Sending executeArbitrage"
    );

    if !m1_production_send_allowed() {
        return Err(eyre!(
            "M1 production send is disabled until the execution gate is approved"
        ));
    }

    let pending_tx = executor
        .executeArbitrage(
            plan.amount_in,
            candidate.token_path.clone(),
            candidate.pool_addresses.clone(),
            pool_types,
            amounts_out_with_slippage,
            plan.min_profit,
            alloy::primitives::U256::from(u64::MAX),
        )
        .gas(gas_limit_for_hops(candidate.hops))
        .send()
        .await?;

    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;

    info!(target: "v2.exec", tx = %tx_hash, "Execution confirmed on-chain");

    Ok(tx_hash)
}

fn m1_production_send_allowed() -> bool {
    false
}

// region: --- 未修改的辅助函数 ---

struct PathSimulation {
    input: U256,
    output: U256,
    profit: I256,
    step_outputs: Vec<U256>,
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
    if path.hops.is_empty() {
        return Ok((Vec::new(), I256::ZERO));
    }

    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());

    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let output = amm
            .simulate_swap(hop.token_in, hop.token_out, current)
            .context("simulate_swap failed")?;
        outputs.push(output);
        current = output;
    }

    let profit = I256::from_raw(current) - I256::from_raw(amount_in);
    Ok((outputs, profit))
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
        let output = amm
            .simulate_swap(hop.token_in, hop.token_out, current)
            .context("simulate_swap failed")?;
        current = output;
    }

    let profit = I256::from_raw(current) - I256::from_raw(amount_in);
    Ok((current, profit))
}

fn collect_v2_expected_states(pools: &[AMM]) -> Result<Vec<U256>> {
    let mut states = Vec::with_capacity(pools.len() * 2);
    for amm in pools {
        match amm {
            AMM::UniswapV2Pool(pool) => {
                states.push(U256::from(pool.reserve_0));
                states.push(U256::from(pool.reserve_1));
            }
            _ => return Err(eyre!("Non-V2 pool encountered in V2 expected states")),
        }
    }
    Ok(states)
}

fn build_token_path(path: &ArbitragePath) -> Vec<Address> {
    let mut tokens = Vec::with_capacity(path.hops.len() + 1);
    if let Some(first) = path.hops.first() {
        tokens.push(first.token_in);
    }
    for hop in &path.hops {
        tokens.push(hop.token_out);
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
    let numerator = U256::from(10_000u32.saturating_sub(bps));
    amount * numerator / U256::from(10_000u32)
}

fn init_tracing() {
    if tracing_subscriber::fmt::try_init().is_ok() {
        return;
    }
    let level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse::<tracing::Level>().ok())
        .unwrap_or(tracing::Level::INFO);
    let _ = tracing_subscriber::fmt()
        .with_target(true)
        .with_level(true)
        .with_file(true)
        .compact()
        .with_max_level(level)
        .try_init();
}

#[allow(dead_code)]
fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}
