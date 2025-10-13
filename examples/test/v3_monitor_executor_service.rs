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
use csv::ReaderBuilder;
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{error, info, warn};

const MAX_HOPS: usize = 4;
const WMNT_ADDRESS: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const MIN_PROFIT_FLOOR_WEI: &str = "10000000000000000"; // 0.01 MNT assuming 18 decimals

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
            if candidate
                .pool_addresses
                .iter()
                .any(|pool| used_pools.contains(pool))
            {
                continue;
            }

            candidate.pool_addresses.iter().for_each(|pool| {
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
            candidate.pool_addresses.iter().for_each(|pool| {
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
        let executor_address = std::env::var("ARBITRAGE_EXECUTOR_ADDRESS")
            .context("Missing ARBITRAGE_EXECUTOR_ADDRESS")?
            .parse()?;
        let wmnt_address = WMNT_ADDRESS;

        let min_profit_floor = U256::from_str(MIN_PROFIT_FLOOR_WEI)?;
        let min_gross_profit = read_min_profit_threshold("MIN_GROSS_PROFIT_WEI", &min_profit_floor)?;
        let min_net_profit_raw = read_min_profit_threshold("MIN_NET_PROFIT_WEI", &min_profit_floor)?;
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
    H: Provider + Clone,
{
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

    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "v3.service", "Subscribed to block stream");

    let mut last_executions: HashMap<String, u64> = HashMap::new();
    let gas_config = GasConfig::default();

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

                let changed = apply_logs(&mut pools, &logs, target_number)?;
                if changed.is_empty() {
                    continue;
                }

                let candidates =
                    find_profitable_candidates(&pools, &gas_config, &config, target_number)?;
                if candidates.is_empty() {
                    continue;
                }

                let selected_indices = select_best_non_conflicting_paths(&candidates);
                if selected_indices.is_empty() {
                    continue;
                }

                info!(
                    target: "v3.exec",
                    block = target_number,
                    total_candidates = candidates.len(),
                    selected = selected_indices.len(),
                    "Selected non-conflicting candidates"
                );

                for idx in selected_indices {
                    let candidate = &candidates[idx];
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

                    match attempt_execution(&http_provider, candidate, &config).await {
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
            Err(e) => {
                error!(target: "v3.block", block = target_number, error = ?e, "get_logs failed");
            }
        }
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
) -> Result<Vec<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(Vec::new());
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
            error!(target: "v3.graph", error = ?err, "Failed to build graph");
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
    let mut candidates = Vec::new();

    for (signature, path) in unique_paths.into_iter() {
        let pools_for_path = match pools_for_path(&path, &state_pools) {
            Ok(p) => p,
            Err(err) => {
                error!(target: "v3.sim", error = ?err, "Failed to gather pools for path");
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

        let expected_states = match collect_agni_expected_states(&pools_for_path) {
            Ok(states) => states,
            Err(err) => {
                error!(target: "v3.state", error = ?err, "Failed to collect expected states");
                continue;
            }
        };

        candidates.push(PositiveCandidate {
            signature: signature.clone(),
            hops: num_hops,
            input: simulation.input,
            output: simulation.output,
            profit: simulation.profit,
            net_profit,
            pool_addresses,
            token_path: token_path.drain(..).collect(),
            amounts_out: simulation.step_outputs.clone(),
            expected_states,
            log_hops: hops_description(&path),
        });
    }

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
            candidate.expected_states.clone(),
            amounts_out_with_slippage,
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
