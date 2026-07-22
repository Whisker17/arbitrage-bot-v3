use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, I256, U256};
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
    amm::{AutomatedMarketMaker, Variant, AMM},
    uniswap_v2::{IUniswapV2Pair, UniswapV2Pool},
};
use amms::arbitrage::{
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::execution::{IArbitrageExecutor, ProtocolKind, RouteKey, IERC20};
use amms::state_space::{
    hash_pinned_logs_filter, hash_pinned_state_block_id, max_input_bound_for_snapshot,
    BlockHeaderContext, MarketSnapshot as GateMarketSnapshot, PoolProtocol, ProtocolCoverage,
    SnapshotBoundBalance, SnapshotId, SnapshotStatus, StateSpace,
};
use csv::{ReaderBuilder, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use legacy_service_support::{
    gas_limit_for_hops, is_on_cooldown, plan_resized_execution_default_margin,
    route_is_structurally_valid, wait_for_block_logs, FailureStore, GasConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{create_dir_all, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

const MAX_HOPS: usize = 3;
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
    let signer_address = signer.address();
    let wallet = EthereumWallet::from(signer.clone());

    let http_provider = ProviderBuilder::new()
        .wallet(wallet.clone())
        .connect_http(config.http_endpoint.parse().expect("invalid http endpoint"));

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;
    let failed_store = Arc::new(Mutex::new(FailureStore::new(
        "logs/failed_opportunities.json",
    )?));
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
        signer_address,
        failed_store,
        &mut csv_logger,
    )
    .await
}

async fn run_service<P, H>(
    ws_provider: P,
    http_provider: H,
    config: ServiceConfig,
    signer_address: Address,
    failed_store: Arc<Mutex<FailureStore<OpportunitySignature>>>,
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

    let mut pools: HashMap<Address, AMM> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();

    initialize_v2_pools(&ws_provider, latest_block_id, &mut pools, &mut fee_tiers).await?;

    if pools.is_empty() {
        warn!(target: "v2.service", "No V2 pools loaded. Exiting.");
        return Ok(());
    }
    let factory_address = std::env::var("AGNI_V2_FACTORY_ADDRESS")
        .or_else(|_| std::env::var("V2_FACTORY_ADDRESS"))
        .context("Missing AGNI_V2_FACTORY_ADDRESS or V2_FACTORY_ADDRESS")?
        .parse()
        .context("Invalid V2 factory address")?;
    legacy_service_support::verify_executable_pool_provenance(
        &http_provider,
        config.executor_address,
        factory_address,
        PoolProtocol::UniswapV2,
        pools.values(),
        pin_hash,
    )
    .await?;
    let pool_universe_fingerprint = legacy_service_support::executable_pool_universe_fingerprint(
        chain_id,
        config.wmnt_address,
        factory_address,
        PoolProtocol::UniswapV2,
        pools.values(),
    )?;

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
        let target_number = number;
        info!(target: "v2.block", block = target_number, "Processing block");

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
                if logs.is_empty() {
                    continue;
                }

                let market_snapshot =
                    apply_logs(&mut pools, &logs, snapshot_id).context("apply_logs")?;
                let mut coverage = ProtocolCoverage::default();
                coverage.pool_universe_fingerprint = Some(pool_universe_fingerprint);
                let gate_status = SnapshotStatus::Ready(Arc::new(GateMarketSnapshot::new(
                    snapshot_id,
                    header,
                    market_snapshot.pools.clone(),
                    coverage,
                )));
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

                if all_candidates.is_empty() {
                    continue;
                }

                for candidate in &all_candidates {
                    if let Err(e) = csv_logger.log_opportunity(candidate, target_number) {
                        error!(target: "v2.csv", error = ?e, "Failed to log opportunity");
                    }
                }

                // 选择无冲突的机会组合
                let selected_opportunities = select_non_conflicting_opportunities(all_candidates);

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
                    if !route_is_structurally_valid(
                        config.wmnt_address,
                        &candidate.token_path,
                        &candidate.pool_addresses,
                        Variant::UniswapV2Pool,
                        &candidate.pools,
                    ) {
                        let mut store = failed_store.lock().await;
                        if let Err(e) = store.mark_permanent(signature.clone()) {
                            error!(target: "v2.failure_store", error = ?e, "Failed to save structural failure");
                        }
                        continue;
                    }
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

                    let should_skip = is_on_cooldown(
                        last_executions.get(&candidate.signature).copied(),
                        target_number,
                        config.block_cooldown,
                    );

                    if should_skip {
                        info!(
                            target: "v2.exec",
                            block = target_number,
                            signature = %candidate.signature,
                            "Skipping execution due to cooldown"
                        );
                        continue;
                    }

                    match attempt_execution(
                        &http_provider,
                        &candidate,
                        &config,
                        signer_address,
                        &gate_status,
                        header,
                        pool_universe_fingerprint,
                    )
                    .await
                    {
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
                            if let Err(e) = store.mark_transient(signature) {
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
                return Err(err.into());
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
    intent_service_support::executor_balance_at_snapshot(
        provider,
        config.wmnt_address,
        config.executor_address,
        snapshot_id,
    )
    .await
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
    signer_address: Address,
    snapshot_status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: alloy::primitives::B256,
) -> Result<alloy::primitives::TxHash> {
    let _ = provider;
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

    info!(
        target: "v2.exec",
        signature = %candidate.signature,
        hops = candidate.hops,
        input = %plan.amount_in,
        expected_output = %plan.simulated_output,
        "Routing candidate through nonce-intent state machine (WHI-519)"
    );

    let route_key = RouteKey::new(vec![ProtocolKind::V2; candidate.hops])?;
    intent_service_support::route_candidate_through_sm(
        signer_address,
        snapshot_status,
        header,
        pool_universe_fingerprint,
        route_key,
        plan.amount_in,
    )?;

    if !intent_service_support::production_send_allowed() {
        return Err(eyre!(
            "production send is disabled until the execution gate is approved (WHI-526); SM prebroadcast path exercised"
        ));
    }

    // Live broadcast path remains intentionally unreachable until WHI-526.
    Err(eyre!("production send path not enabled"))
}

fn m1_production_send_allowed() -> bool {
    intent_service_support::production_send_allowed()
}

// region: --- 未修改的辅助函数 ---
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
