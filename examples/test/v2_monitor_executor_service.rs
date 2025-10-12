use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, I256, U256};
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
use amms::execution::{gas_schedule::gas_limit_for_hops, IArbitrageExecutor, IERC20};
use amms::state_space::StateSpace;
use csv::ReaderBuilder;
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{error, info, warn};

const MAX_HOPS: usize = 4;
const V2_FEE_BPS: usize = 300; // 0.3%

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
            .unwrap_or_else(U256::ZERO);

        let min_net_profit = std::env::var("MIN_NET_PROFIT_WEI")
            .ok()
            .and_then(|s| U256::from_str(&s).ok())
            .unwrap_or_else(U256::ZERO);

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
    let from_address = signer.address();
    let wallet = EthereumWallet::from(signer.clone());

    let http_provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(config.http_endpoint.parse()?)
        .context("Failed to connect HTTP provider")?;

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;

    info!(
        target: "v2.service",
        from = %from_address,
        executor = %config.executor_address,
        "Starting Uniswap V2 monitoring + execution service"
    );

    run_service(ws_provider, http_provider, from_address, config).await
}

async fn run_service<P, H>(
    ws_provider: P,
    http_provider: H,
    from_address: Address,
    config: ServiceConfig,
) -> Result<()>
where
    P: Provider + Clone,
    H: Provider + Clone,
{
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

        let windowed = filter.clone().select(target_number);
        match ws_provider.get_logs(&windowed).await {
            Ok(logs) => {
                if logs.is_empty() {
                    continue;
                }

                let changed = apply_logs(&mut pools, &logs, target_number).context("apply_logs")?;
                if changed.is_empty() {
                    continue;
                }

                if let Some(candidate) =
                    find_best_candidate(&pools, &gas_config, &config, target_number)?
                {
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

                    match attempt_execution(&http_provider, &candidate, from_address, &config).await
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
    block_number: u64,
) -> Result<HashSet<Address>> {
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
            info!(
                target: "v2.pool",
                address = %addr,
                block = block_number,
                "Applied Sync event"
            );
        }
    }
    Ok(changed)
}

fn find_best_candidate(
    pools: &HashMap<Address, AMM>,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    block_number: u64,
) -> Result<Option<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(None);
    }

    let mut state = StateSpace::default();
    for amm in pools.values() {
        state.state.insert(amm.address(), amm.clone());
    }

    let graph = match build_graph(&state) {
        Ok(graph) => graph,
        Err(err) => {
            error!(target: "v2.graph", error = ?err, "Failed to build graph");
            return Ok(None);
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
        return Ok(None);
    }

    let state_pools: Vec<AMM> = state.state.values().cloned().collect();
    let mut best: Option<PositiveCandidate> = None;

    for (signature, path) in unique_paths.into_iter() {
        let pools_for_path = match pools_for_path(&path, &state_pools) {
            Ok(p) => p,
            Err(err) => {
                error!(target: "v2.sim", error = ?err, "Failed to gather pools for path");
                continue;
            }
        };

        let simulation = match best_path_simulation_with_steps(&path, &pools_for_path) {
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

        if !gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.2) {
            continue;
        }

        let mut token_path = build_token_path(&path);
        if token_path.first().copied() != Some(config.wmnt_address) {
            continue;
        }
        if token_path.last().copied() != Some(config.wmnt_address) {
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

        let amounts_out = simulation.step_outputs.clone();

        let candidate = PositiveCandidate {
            signature,
            hops: num_hops,
            input: simulation.input,
            output: simulation.output,
            profit: simulation.profit,
            net_profit,
            pool_addresses,
            token_path: token_path.drain(..).collect(),
            amounts_out,
            expected_states,
            path: path.clone(),
            log_hops: hops_description(&path),
        };

        let is_better = match &best {
            Some(current) => simulation.profit > current.profit,
            None => true,
        };

        if is_better {
            best = Some(candidate);
        }
    }

    if let Some(ref candidate) = best {
        info!(
            target: "v2.candidate",
            block = block_number,
            signature = %candidate.signature,
            hops = candidate.hops,
            input = %candidate.input,
            output = %candidate.output,
            profit = %candidate.profit,
            net_profit = %candidate.net_profit,
            path = %candidate.log_hops,
            "Best V2 candidate"
        );
    }

    Ok(best)
}

async fn attempt_execution<H: Provider + Clone>(
    provider: &H,
    candidate: &PositiveCandidate,
    from_address: Address,
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
            target: "v2.exec",
            required = %candidate.input,
            available = %executor_balance,
            "Executor contract balance insufficient"
        );
        return Err(eyre!("Executor contract lacks WMNT balance"));
    }

    let mut amounts_out = candidate.amounts_out.clone();
    if let Some(last) = amounts_out.last_mut() {
        *last = apply_slippage(*last, config.execution_slippage_bps);
    }

    let pool_types = vec![0u8; candidate.pool_addresses.len()];

    info!(
        target: "v2.exec",
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
            candidate.expected_states.clone(),
            amounts_out,
        )
        .gas(gas_limit_for_hops(candidate.hops))
        .send()
        .await?;

    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;

    info!(target: "v2.exec", tx = %tx_hash, "Execution confirmed on-chain");

    Ok(tx_hash)
}

struct PathSimulation {
    input: U256,
    output: U256,
    profit: I256,
    step_outputs: Vec<U256>,
}

fn best_path_simulation_with_steps(path: &ArbitragePath, pools: &[AMM]) -> Option<PathSimulation> {
    const MIN_INPUT: u128 = 1_000_000_000_000; // 10^12
    const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000; // 10^24

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
