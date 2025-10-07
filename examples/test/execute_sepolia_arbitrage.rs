/// Mantle Sepolia 套利监控和执行服务
///
/// 基于 monitor_pools.rs，增加了自动执行套利的功能
///
/// 功能：
/// - 监控 Mantle Sepolia 测试网上的 Agni 池子
/// - 实时检测套利机会
/// - 自动调用 ArbitrageExecutor 合约执行套利
/// - 记录执行结果和日志
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了以下变量：
///    - MANTLE_SEPOLIA_RPC_URL
///    - MANTLE_SEPOLIA_RPC_WS_URL  
///    - MANTLE_SEPOLIA_PRIVATE_KEY
///    - ARBITRAGE_EXECUTOR_ADDRESS
///
/// 2. 运行服务：
///    cargo run --example execute_sepolia_arbitrage
use alloy::consensus::BlockHeader;
use alloy::network::{EthereumWallet, TransactionBuilder};
use alloy::primitives::{address, Address, Bytes, I256, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::{
    eips::BlockId,
    providers::{Provider, ProviderBuilder, WalletProvider},
    rpc::types::{Filter, FilterSet, Log},
    signers::local::PrivateKeySigner,
    sol,
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
use eyre::{Report, Result, WrapErr};
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{error, info, warn};

use amms::execution::gas_schedule::gas_limit_for_hops;

// ArbitrageExecutor 合约接口
sol! {
    #[sol(rpc)]
    interface IArbitrageExecutor {
        function executeArbitrage(
            uint256 amountIn,
            address[] calldata path,
            address[] calldata pools,
            uint8[] calldata poolTypes,
            uint256[] calldata expectedStates,
            uint256[] calldata amountsOut
        ) external;

        function owner() external view returns (address);
        function WMNT() external view returns (address);
    }
}

sol! {
    #[sol(rpc)]
    interface IAgniPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
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
    path: ArbitragePath,
}

#[derive(Clone, Debug)]
struct LoggedPathRecord {
    signature: String,
    profit: I256,
    input: U256,
    output: U256,
    roi: String,
}

static LAST_EXECUTED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
static LOGGED_PATHS: OnceLock<Mutex<HashMap<String, LoggedPathRecord>>> = OnceLock::new();

/// 执行套利交易
async fn execute_arbitrage<P: Provider + Clone>(
    path: &ArbitragePath,
    optimal_input: U256,
    expected_output: U256,
    pools: &HashMap<Address, AgniPool>,
    executor_address: Address,
    provider: Arc<P>,
) -> Result<bool> {
    // 构建交易参数
    let num_hops = path.hops.len();

    // 构建 path (tokens)
    let mut token_path = Vec::with_capacity(num_hops + 1);
    token_path.push(path.hops[0].token_in);
    for hop in &path.hops {
        token_path.push(hop.token_out);
    }

    // 构建 pools
    let pool_addresses: Vec<Address> = path.hops.iter().map(|h| h.pool_address).collect();

    // 构建 poolTypes (所有都是 Agni = 1)
    let pool_types: Vec<u8> = vec![1u8; num_hops];

    // 构建 expectedStates
    let mut expected_states = Vec::new();
    for hop in &path.hops {
        if let Some(pool) = pools.get(&hop.pool_address) {
            expected_states.push(pool.sqrt_price);
            expected_states.push(U256::from(pool.liquidity));
        } else {
            return Err(eyre::eyre!("Pool not found for hop"));
        }
    }

    // 模拟计算每一步的输出
    let mut amounts_out = Vec::new();
    let mut current = optimal_input;

    for hop in &path.hops {
        if let Some(pool) = pools.get(&hop.pool_address) {
            let amm = AMM::AgniPool(pool.clone());
            match amm.simulate_swap(hop.token_in, hop.token_out, current) {
                Ok(output) => {
                    amounts_out.push(output);
                    current = output;
                }
                Err(e) => {
                    error!(
                        target: "executor",
                        error = ?e,
                        "Failed to simulate swap for output calculation"
                    );
                    return Err(eyre::eyre!("Simulation failed: {:?}", e));
                }
            }
        }
    }

    // 构建合约调用
    let executor = IArbitrageExecutor::new(executor_address, provider.clone());

    info!(
        target: "executor",
        optimal_input = %optimal_input,
        expected_output = %expected_output,
        num_hops = num_hops,
        "Preparing arbitrage transaction"
    );

    let call_builder = executor.executeArbitrage(
        optimal_input,
        token_path.clone(),
        pool_addresses.clone(),
        pool_types.clone(),
        expected_states.clone(),
        amounts_out.clone(),
    );

    let gas_limit = gas_limit_for_hops(num_hops);
    info!(
        target: "executor",
        gas_limit = gas_limit,
        hops = num_hops,
        "Using hop-based gas limit for arbitrage execution"
    );

    let pending_tx = call_builder.gas(gas_limit).send().await?;
    info!(target: "executor", "Transaction sent, waiting for confirmation...");

    // 等待上链并获取回执，确认执行状态
    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;

    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("Arbitrage tx missing receipt after inclusion"))?;

    if !receipt.status() {
        error!(
            target: "executor",
            tx_hash = %receipt.transaction_hash,
            "❌ Arbitrage transaction reverted"
        );
        return Ok(false);
    }

    info!(
        target: "executor",
        tx_hash = %receipt.transaction_hash,
        "✅ Arbitrage executed successfully!"
    );
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 加载环境变量
    dotenv::dotenv().ok();

    // 读取配置
    let rpc_url =
        std::env::var("MANTLE_SEPOLIA_RPC_URL").expect("MANTLE_SEPOLIA_RPC_URL must be set");
    let ws_url =
        std::env::var("MANTLE_SEPOLIA_RPC_WS_URL").expect("MANTLE_SEPOLIA_RPC_WS_URL must be set");
    let private_key = std::env::var("MANTLE_SEPOLIA_PRIVATE_KEY")
        .expect("MANTLE_SEPOLIA_PRIVATE_KEY must be set");
    let executor_address = Address::from_str(
        &std::env::var("ARBITRAGE_EXECUTOR_ADDRESS")
            .expect("ARBITRAGE_EXECUTOR_ADDRESS must be set"),
    )?;

    info!(
        target: "monitor",
        rpc = %rpc_url,
        ws = %ws_url,
        executor = %executor_address,
        "Starting Mantle Sepolia arbitrage monitor"
    );

    // 创建带签名的 provider
    let signer = PrivateKeySigner::from_str(&private_key)?;
    let wallet = EthereumWallet::from(signer);

    let ws_provider = Arc::new(
        ProviderBuilder::new()
            .wallet(wallet.clone())
            .connect_ws(WsConnect::new(ws_url))
            .await?,
    );

    let http_provider = Arc::new(
        ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(rpc_url.parse()?),
    );

    // 加载池子
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists_testnet.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    // 初始化池子
    let latest_block = BlockId::from(ws_provider.get_block_number().await?);
    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();

    let pool_log_path = PathBuf::from("logs/sepolia_pool_updates.csv");
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

    let mut init_jobs = Vec::new();
    let mut total_rows = 0usize;
    let mut agni_rows = 0usize;

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
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|(addr, fee_tier)| {
        let provider = ws_provider.clone();
        let block = latest_block;
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

    // 检查执行器合约余额
    let wmnt_address = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let wmnt = IERC20::new(wmnt_address, http_provider.clone());
    let executor_balance = wmnt.balanceOf(executor_address).call().await?;
    info!(
        target: "monitor",
        balance = %executor_balance,
        "Executor WMNT balance"
    );

    if executor_balance == U256::ZERO {
        warn!(
            target: "monitor",
            "⚠️  Executor has zero WMNT balance! Please fund the contract first."
        );
    }

    // 构建事件过滤器
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));
    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // 订阅新区块
    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "monitor", "✅ Subscribed to blocks, monitoring for arbitrage opportunities...");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "monitor.block", block = target_number, "Processing block");

        let windowed = filter.clone().select(target_number);
        match ws_provider.get_logs(&windowed).await {
            Ok(logs) => {
                info!(target: "monitor.block", block = target_number, logs = logs.len(), "Fetched logs");
                apply_logs(
                    &mut pools,
                    &logs,
                    target_number,
                    &pool_log_path,
                    &fee_tiers,
                    executor_address,
                    http_provider.clone(),
                )?;
            }
            Err(e) => {
                error!(target: "monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

fn parse_address(s: &str) -> Result<Address> {
    let s = s.trim();
    let addr = s.parse::<Address>()?;
    Ok(addr)
}

fn apply_logs<P: Provider + Clone>(
    pools: &mut HashMap<Address, AgniPool>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
    fee_tiers: &HashMap<Address, Option<u32>>,
    executor_address: Address,
    provider: Arc<P>,
) -> Result<()> {
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let sig = log.topics()[0];
            if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                match IAgniPoolEvents::Swap::decode_log(log.as_ref()) {
                    Ok(_) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Swap)");
                            continue;
                        }
                        log_pool_state(log_path, block_number, "swap", pool)?;
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Swap failed");
                    }
                }
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Mint)");
                    continue;
                }
                log_pool_state(log_path, block_number, "mint", pool)?;
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                if let Err(e) = pool.sync(log) {
                    error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Burn)");
                    continue;
                }
                log_pool_state(log_path, block_number, "burn", pool)?;
            }
        }
    }

    // 检查套利机会并执行
    let pools_clone = pools.clone();
    let fee_tiers_clone = fee_tiers.clone();
    tokio::spawn(async move {
        check_and_execute_arbitrage(
            pools_clone,
            block_number,
            fee_tiers_clone,
            executor_address,
            provider,
        )
        .await;
    });

    Ok(())
}

async fn check_and_execute_arbitrage<P: Provider + Clone + Send + Sync + 'static>(
    pools: HashMap<Address, AgniPool>,
    block_number: u64,
    fee_tiers: HashMap<Address, Option<u32>>,
    executor_address: Address,
    provider: Arc<P>,
) {
    if pools.is_empty() {
        return;
    }

    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(
                target: "monitor",
                error = ?err,
                "Failed to build graph for arbitrage"
            );
            return;
        }
    };

    let constraints = PathConstraints {
        max_length: 3,
        required_start_token: Some(address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF")), // WMNT
        required_end_token: Some(address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF")),
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
        return;
    }

    let mut positive_candidates = Vec::new();
    let state_pools: Vec<AMM> = state.state.values().cloned().collect();
    const MIN_INPUT: u128 = 1_000_000_000_000; // 10^12
    const MAX_INPUT: u128 = 100_000_000_000_000_000; // 0.1 WMNT

    for (idx, (signature, path)) in path_entries.iter().enumerate() {
        if !path
            .hops
            .iter()
            .all(|hop| match fee_tiers.get(&hop.pool_address) {
                Some(Some(expected_fee)) => pools
                    .get(&hop.pool_address)
                    .map(|pool| pool.fee == *expected_fee)
                    .unwrap_or(false),
                Some(None) | None => true,
            })
        {
            continue;
        }

        let pools_for_path = match pools_for_path(path, &state_pools) {
            Ok(p) => p,
            Err(_) => continue,
        };

        let best_simulation = best_path_simulation(
            path,
            &pools_for_path,
            U256::from(MIN_INPUT),
            U256::from(MAX_INPUT),
        );

        if let Some((input, output, profit)) = best_simulation {
            if profit > I256::ZERO {
                let gas_config = GasConfig::default();
                let num_hops = path.hops.len();
                let profit_u256 = U256::from_limbs(*profit.as_limbs());

                // 只执行净利润超过 gas 成本 50% 的交易
                if gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.5) {
                    let roi_str =
                        format_roi_percent(profit, input).unwrap_or_else(|| "-".to_string());
                    positive_candidates.push(PositiveCandidate {
                        index: idx,
                        profit,
                        input,
                        output,
                        roi: roi_str,
                        signature: signature.clone(),
                        hops: hops_description(path),
                        pools: path.hops.iter().map(|hop| hop.pool_address).collect(),
                        path: path.clone(),
                    });
                }
            }
        }
    }

    if !positive_candidates.is_empty() {
        // 按利润排序
        positive_candidates.sort_by(|a, b| b.profit.cmp(&a.profit));

        // 选择最佳候选
        let best = &positive_candidates[0];

        info!(
            target: "monitor.arb",
            block = block_number,
            profit = %best.profit,
            roi = %best.roi,
            input = %best.input,
            path = %best.signature,
            "🎯 Profitable arbitrage opportunity detected!"
        );

        // 检查是否最近刚执行过
        let last_executed_mutex = LAST_EXECUTED.get_or_init(|| Mutex::new(None));
        let should_execute = {
            let last_executed = last_executed_mutex.lock().unwrap();
            match last_executed.as_ref() {
                Some(last_sig) => last_sig != &best.signature,
                None => true,
            }
        };

        if should_execute {
            info!(
                target: "executor",
                "🚀 Attempting to execute arbitrage..."
            );

            match execute_arbitrage(
                &best.path,
                best.input,
                best.output,
                &pools,
                executor_address,
                provider,
            )
            .await
            {
                Ok(true) => {
                    info!(
                        target: "executor",
                        "✅ Arbitrage executed successfully!"
                    );
                    let mut last_executed = last_executed_mutex.lock().unwrap();
                    *last_executed = Some(best.signature.clone());
                }
                Ok(false) => {
                    warn!(
                        target: "executor",
                        "⚠️  Arbitrage execution failed (transaction reverted or gas estimation failed)"
                    );
                }
                Err(e) => {
                    error!(
                        target: "executor",
                        error = ?e,
                        "❌ Error during arbitrage execution"
                    );
                }
            }
        } else {
            info!(
                target: "monitor.arb",
                "⏭️  Skipping execution (same path recently executed)"
            );
        }
    }
}

fn ensure_log_headers(path: &Path, header: &[&str]) -> Result<()> {
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

fn simulate_path_raw(path: &ArbitragePath, pools: &[AMM], amount_in: U256) -> Result<(U256, I256)> {
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
                "{:#x}->{:#x}@{:#x}",
                hop.token_in, hop.token_out, hop.pool_address
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
                "{:#x}->{:#x}@{:#x}",
                hop.token_in, hop.token_out, hop.pool_address
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}
