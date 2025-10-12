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
use csv::ReaderBuilder;
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use rayon::prelude::*;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;
use tracing::{error, info, warn};

const MAX_HOPS: usize = 4;

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
            .unwrap_or(30);

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
        target: "v3.service",
        from = %from_address,
        executor = %config.executor_address,
        "Starting Agni monitoring + execution service"
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

    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();

    initialize_agni_pools(&ws_provider, latest_block_id, &mut pools, &mut fee_tiers).await?;

    if pools.is_empty() {
        warn!(target: "v3.service", "No Agni pools loaded. Exiting.");
        return Ok(());
    }

    info!(
        target: "v3.service",
        pools = pools.len(),
        "Initialized Agni pools"
    );

    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "v3.service", "Subscribed to block stream");

    let cache = build_path_cache(&pools, &fee_tiers, MAX_HOPS);
    let gas_config = GasConfig::default();
    let mut last_executions: HashMap<String, u64> = HashMap::new();

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "v3.block", block = target_number, "Processing block");

        let windowed = filter.clone().select(target_number);
        match ws_provider.get_logs(&windowed).await {
            Ok(logs) => {
                if logs.is_empty() {
                    continue;
                }

                let changed = apply_logs(&mut pools, &logs, target_number)?;
                if changed.is_empty() {
                    continue;
                }

                if let Some(candidate) = find_best_candidate(
                    &pools,
                    &gas_config,
                    &config,
                    target_number,
                    &cache,
                    &changed,
                )? {
                    let should_skip = last_executions
                        .get(&candidate.signature)
                        .map(|last_block| {
                            target_number.saturating_sub(*last_block) < config.block_cooldown
                        })
                        .unwrap_or(false);

                    if should_skip {
                        info!(
                            target: "v3.exec",
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
                                target: "v3.exec",
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
                                target: "v3.exec",
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
                    target: "v3.block",
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

async fn initialize_agni_pools<P: Provider + Clone>(
    provider: &P,
    block_id: alloy::eips::BlockId,
    pools: &mut HashMap<Address, AgniPool>,
    fee_tiers: &mut HashMap<Address, Option<u32>>,
) -> Result<()> {
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    let mut init_jobs = Vec::new();

    for row in rdr.deserialize::<PoolRow>() {
        let row = row?;
        if !row.Protocol.to_lowercase().contains("agni") {
            continue;
        }
        let addr = Address::from_str(row.Pair_Address.trim()).context("Invalid pool address")?;
        init_jobs.push((addr, row.Fee_Tier));
    }

    if init_jobs.is_empty() {
        warn!(target: "v3.service", "No Agni pools found in CSV");
        return Ok(());
    }

    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|(addr, fee)| {
        let provider = provider.clone();
        async move {
            let pool = AgniPool::new(addr).init_basic(block_id, provider).await;
            (addr, fee, pool)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    while let Some((addr, fee_tier, pool)) = init_stream.next().await {
        match pool {
            Ok(pool) => {
                if let Some(csv_fee) = fee_tier {
                    if pool.fee != csv_fee {
                        warn!(
                            target: "v3.init",
                            address = %addr,
                            csv_fee,
                            pool_fee = pool.fee,
                            "Fee tier mismatch"
                        );
                    }
                }
                info!(target: "v3.init", address = %addr, fee = pool.fee, "Initialized pool");
                fee_tiers.insert(addr, fee_tier);
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "v3.init",
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
    pools: &mut HashMap<Address, AgniPool>,
    logs: &[Log],
    block_number: u64,
) -> Result<HashSet<Address>> {
    let mut changed = HashSet::new();
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let sig = log.topics()[0];

            let before_tick = pool.tick;
            let before_liq = pool.liquidity;
            let before_sqrt = pool.sqrt_price;

            if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                match IAgniPoolEvents::Swap::decode_log(log.as_ref()) {
                    Ok(_event) => {
                        if let Err(err) = pool.sync(log) {
                            error!(
                                target: "v3.pool",
                                address = %addr,
                                error = ?err,
                                "sync error (Swap)"
                            );
                            continue;
                        }
                        changed.insert(addr);
                        info!(
                            target: "v3.pool",
                            address = %addr,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            sqrt_from = ?before_sqrt,
                            sqrt_to = ?pool.sqrt_price,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied Swap"
                        );
                    }
                    Err(err) => {
                        error!(
                            target: "v3.pool",
                            address = %addr,
                            error = ?err,
                            "decode Swap failed"
                        );
                    }
                }
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                match IAgniPoolEvents::Mint::decode_log(log.as_ref()) {
                    Ok(_event) => {
                        if let Err(err) = pool.sync(log) {
                            error!(
                                target: "v3.pool",
                                address = %addr,
                                error = ?err,
                                "sync error (Mint)"
                            );
                            continue;
                        }
                        changed.insert(addr);
                        info!(
                            target: "v3.pool",
                            address = %addr,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied Mint"
                        );
                    }
                    Err(err) => {
                        error!(
                            target: "v3.pool",
                            address = %addr,
                            error = ?err,
                            "decode Mint failed"
                        );
                    }
                }
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                match IAgniPoolEvents::Burn::decode_log(log.as_ref()) {
                    Ok(_event) => {
                        if let Err(err) = pool.sync(log) {
                            error!(
                                target: "v3.pool",
                                address = %addr,
                                error = ?err,
                                "sync error (Burn)"
                            );
                            continue;
                        }
                        changed.insert(addr);
                        info!(
                            target: "v3.pool",
                            address = %addr,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied Burn"
                        );
                    }
                    Err(err) => {
                        error!(
                            target: "v3.pool",
                            address = %addr,
                            error = ?err,
                            "decode Burn failed"
                        );
                    }
                }
            }
        }
    }
    Ok(changed)
}

#[derive(Clone)]
struct PathCache {
    paths: Vec<ArbitragePath>,
    signatures: Vec<String>,
    pool_to_path_indices: HashMap<Address, Vec<usize>>,
}

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
            error!(target: "v3.graph", error = ?err, "Failed to build graph");
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

    let mut unique_paths: HashMap<String, ArbitragePath> = HashMap::new();
    for path in finder
        .find_cycles()
        .into_iter()
        .chain(finder.find_two_pool_misprices())
    {
        if path
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
            let signature = path_signature(&path);
            unique_paths.entry(signature).or_insert(path);
        }
    }

    let mut entries: Vec<(String, ArbitragePath)> = unique_paths.into_iter().collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let signatures: Vec<String> = entries.iter().map(|(s, _)| s.clone()).collect();
    let paths: Vec<ArbitragePath> = entries.into_iter().map(|(_, p)| p).collect();

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

fn find_best_candidate(
    pools: &HashMap<Address, AgniPool>,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    block_number: u64,
    cache: &PathCache,
    changed_pools: &HashSet<Address>,
) -> Result<Option<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(None);
    }

    // rebuild state for simulations
    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::AgniPool(pool.clone()));
    }

    let state_pools: Vec<AMM> = state.state.values().cloned().collect();

    let mut candidate_indices: HashSet<usize> = HashSet::new();
    for changed in changed_pools {
        if let Some(indices) = cache.pool_to_path_indices.get(changed) {
            for &idx in indices {
                candidate_indices.insert(idx);
            }
        }
    }

    if candidate_indices.is_empty() {
        return Ok(None);
    }

    let mut candidates: Vec<Option<PositiveCandidate>> = candidate_indices
        .iter()
        .map(|idx| {
            let signature = cache.signatures.get(*idx)?.clone();
            let path = cache.paths.get(*idx)?.clone();
            Some((signature, path))
        })
        .collect::<Option<Vec<_>>>()
        .unwrap_or_default()
        .into_par_iter()
        .map(|(signature, path)| {
            let pools_for_path = match pools_for_path(&path, &state_pools) {
                Ok(p) => p,
                Err(err) => {
                    error!(target: "v3.sim", error = ?err, "Failed to gather pools for path");
                    return None;
                }
            };

            let simulation = match best_path_simulation_with_steps(&path, &pools_for_path) {
                Some(sim) => sim,
                None => return None,
            };

            if simulation.profit <= I256::ZERO {
                return None;
            }

            let profit_u256 = U256::from_limbs(*simulation.profit.as_limbs());
            if profit_u256 < config.min_gross_profit {
                return None;
            }

            let num_hops = path.hops.len();
            let net_profit = match gas_config.net_profit(profit_u256, num_hops) {
                Some(net) => net,
                None => return None,
            };

            if net_profit < config.min_net_profit {
                return None;
            }

            if !gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.2) {
                return None;
            }

            let mut token_path = build_token_path(&path);
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

            let candidate = PositiveCandidate {
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
                log_hops: hops_description(&path),
            };

            Some(candidate)
        })
        .collect();

    let mut best: Option<PositiveCandidate> = None;

    for candidate in candidates.drain(..).flatten() {
        let is_better = match &best {
            Some(current) => candidate.profit > current.profit,
            None => true,
        };
        if is_better {
            best = Some(candidate);
        }
    }

    if let Some(ref candidate) = best {
        info!(
            target: "v3.candidate",
            block = block_number,
            signature = %candidate.signature,
            hops = candidate.hops,
            input = %candidate.input,
            output = %candidate.output,
            profit = %candidate.profit,
            net_profit = %candidate.net_profit,
            path = %candidate.log_hops,
            "Best Agni candidate"
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
            target: "v3.exec",
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
            candidate.expected_states.clone(),
            amounts_out,
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

fn collect_agni_expected_states(pools: &[AMM]) -> Result<Vec<U256>> {
    let mut states = Vec::with_capacity(pools.len() * 2);
    for amm in pools {
        match amm {
            AMM::AgniPool(pool) => {
                states.push(U256::from(pool.sqrt_price));
                states.push(U256::from(pool.liquidity));
            }
            _ => return Err(eyre!("Non-Agni pool encountered in expected states")),
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
