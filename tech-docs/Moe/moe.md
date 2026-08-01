# Merchant Moe

```rs
use swap_path::data_sync::{DataSyncConfig, DataSyncServiceBuilder};
use swap_path::logic::{ArbitrageEngine, ArbitrageOpportunity};
use swap_path::logic::types::ArbitrageConfig;
use swap_path::data_sync::markets::{Market, MarketConfigSection};
use swap_path::{PoolWrapper, Token};
use swap_path::{Executor, ExecutorConfig, ExecutionContext};
use swap_path::execution::contract::IERC20;
use alloy_primitives::{Address, U256};
use alloy_provider::ProviderBuilder;
use eyre::Result;
use std::sync::Arc;
use tracing::{info, warn, error};
use std::env;
use std::fs;
use std::path::Path;
use serde::Deserialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Write};
use std::fs::OpenOptions;
use std::fs::create_dir_all;

// Mantle mainnet defaults
const MANTLE_MAINNET_RPC_WSS: &str = "wss://mantle.publicnode.com";
const MANTLE_MAINNET_RPC_HTTPS: &str = "https://rpc.mantle.xyz";
const MANTLE_MULTICALL3: &str = "0xcA11bde05977b3631167028862bE2a173976CA11";

// Common tokens (addresses follow live monitor set)
const WMNT: &str = "0x78c1b0C915C4FAA5FFFa6CAbf0219DA63d7f4cb8";
const METH: &str = "0xcDA86A272531e8640cD7F1a92c01839911B90bb0";
const MOE: &str = "0x4515A45337F461A11Ff0FE8aBF3c606AE5dC00c9";
const PUFF: &str = "0x26a6b0dcdCfb981362aFA56D581e4A7dBA3Be140";
const MINU: &str = "0x51CfE5b1E764dC253F4c8C1f19a081fF4C3517eD";
const LEND: &str = "0x25356aeca4210eF7553140edb9b8026089E49396";
const JOE: &str = "0x371c7ec6D8039ff7933a2AA28EB827Ffe1F52f07";
const USDC: &str = "0x09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9";
const ENA: &str = "0x58538e6A46E07434d7E7375Bc268D3cb839C0133";
const USDT: &str = "0x201EBa5CC46D216Ce6DC03F6a759e8E766e956aE";
const WETH: &str = "0xdEAddEaDdeadDEadDEADDEAddEADDEAddead1111";

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(false)
        .with_line_number(true)
        .init();

    info!("Starting live arbitrage executor...");

    // 1) Build data-sync config
    let config = DataSyncConfig {
        rpc_wss_url: env::var("RPC_WSS_URL").or_else(|_| env::var("MANTLE_RPC_WSS")).unwrap_or_else(|_| MANTLE_MAINNET_RPC_WSS.to_string()),
        rpc_http_url: env::var("RPC_HTTP_URL").or_else(|_| env::var("RPC_URL")).or_else(|_| env::var("MANTLE_RPC_HTTPS")).unwrap_or_else(|_| MANTLE_MAINNET_RPC_HTTPS.to_string()),
        multicall_address: MANTLE_MULTICALL3.to_string(),
        max_pools_per_batch: 50,
        ws_connection_timeout_secs: 30,
        max_reconnect_attempts: 10,
        reconnect_delay_secs: 5,
        http_timeout_secs: 20,
        channel_buffer_size: 1000,
    };

    // 2) Build market (tokens and pools loaded from CSV or env)
    let market_config = MarketConfigSection::default().with_max_hops(3); // DEFAULT_MAX_HOPS (WHI-529)
    let mut market = Market::new(market_config);

    // Load tokens from CSV and build symbol map
    let symbol_map = load_tokens_from_csv(&mut market)?;
    let wmnt = *symbol_map.get("WMNT").ok_or_else(|| eyre::eyre!("WMNT not found in tokenLists.csv"))?;

    // Prepare tokens vector to seed DataSyncService market with correct decimals
    let tokens_for_service: Vec<Arc<Token>> = market
        .token_graph
        .tokens
        .values()
        .cloned()
        .collect();

    // Load pools from env or CSV
    let mut pools: Vec<PoolWrapper> = Vec::new();
    if let Ok(pool_addresses) = env::var("POOL_ADDRESSES") {
        let meth = *symbol_map.get("mETH").or_else(|| symbol_map.get("METH")).ok_or_else(|| eyre::eyre!("mETH not found in tokenLists.csv"))?;
        for addr_str in pool_addresses.split(',') {
            let addr = addr_str.trim().parse::<Address>()?;
            let mock = swap_path::MockPool { address: addr, token0: wmnt, token1: meth };
            pools.push(PoolWrapper::new(Arc::new(mock)));
        }
    } else {
        match load_pools_from_csv() {
            Ok(csv_pools) if !csv_pools.is_empty() => {
                pools = csv_pools;
            }
            Ok(_) => {
                warn!("CSV pool list is empty: data/selected/poolLists.csv");
            }
            Err(e) => {
                warn!("Failed to load CSV pools: {}", e);
            }
        }
    }

    for p in &pools { market.add_pool(p.clone()); }

    // 3) Start data service
    let mut data_service = DataSyncServiceBuilder::new()
        .with_config(config)
        .with_pools(pools)
        .with_tokens(tokens_for_service)
        .build()
        .await?;

    // 4) Build arbitrage engine
    let arb_cfg = ArbitrageConfig { 
        // Require net profit >= 0.01 MNT at discovery stage
        min_profit_threshold_mnt_wei: U256::from(10_000_000_000_000_000u64),
        max_hops: 3, // DEFAULT_MAX_HOPS (WHI-529)
        gas_price_gwei: 0.02,
        gas_per_transaction: 700_000_000,
        max_precomputed_paths: 1000,
        enable_parallel_calculation: true,
    };
    let mut engine = ArbitrageEngine::new(arb_cfg);
    engine.initialize(&market.token_graph)?;

    // 5) Set up execution layer
    let executor_addr = env::var("EXECUTOR_CONTRACT").unwrap_or_else(|_| "0x4CF71B232CC67D6eDbE1fBF3153Faa2605E163c2".to_string());
    let exec_context = ExecutionContext {
        executor_contract: executor_addr.parse::<Address>()?,
        wmnt_address: wmnt,
    };
    let mut exec_config = ExecutorConfig::default();
    // Allow switching fee mode via env: FEE_MODE=legacy|eip1559 (default legacy)
    if let Ok(mode) = std::env::var("FEE_MODE") {
        exec_config.fee_mode = match mode.to_lowercase().as_str() {
            "eip1559" => swap_path::execution::types::FeeMode::Eip1559,
            _ => swap_path::execution::types::FeeMode::Legacy,
        };
    }
    // Require min net profit >= 0.01 MNT (10e15 wei)
    exec_config.min_net_profit_mnt_wei = U256::from(10_000_000_000_000_000u64);
    // Keep strict non-loss and include gas in min_out; fix gas price to 0.021 gwei
    exec_config.include_gas_cost_in_min_out = true;
    exec_config.enforce_non_loss = true;
    exec_config.fixed_gas_price_wei = Some(21_000_000u128);
    let executor = Executor::new(exec_context, exec_config);

    // 6) Providers: unsigned for reads; required signed for execution (by default)
    let http_url = env::var("RPC_HTTP_URL")
        .or_else(|_| env::var("RPC_URL"))
        .or_else(|_| env::var("MANTLE_RPC_HTTPS"))
        .unwrap_or_else(|_| MANTLE_MAINNET_RPC_HTTPS.to_string());

    let read_provider = ProviderBuilder::new().connect_http(http_url.parse()?) ;

    // Default: execute enabled unless EXECUTE_TRADES=0
    let execute_enabled = env::var("EXECUTE_TRADES").map(|v| v != "0").unwrap_or(true);

    let signed_provider = match env::var("PRIVATE_KEY") {
        Ok(pk) => {
            let signer = alloy_signer_local::PrivateKeySigner::from_slice(&hex::decode(pk.trim_start_matches("0x"))?)?;
            Some(ProviderBuilder::new().wallet(signer).connect_http(http_url.parse()?))
        }
        Err(_) => None,
    };

    if execute_enabled && signed_provider.is_none() {
        error!("Execution enabled by default, but PRIVATE_KEY is missing in env/.env");
        eyre::bail!("PRIVATE_KEY required for execution; set EXECUTE_TRADES=0 to disable");
    }

    let mut rx = data_service.start().await?;

    // Persistent store for previously failed opportunities (survives restarts)
    let mut failed_store = FailedOpportunityStore::load_or_default("logs/failed_opps.txt");

    // Attempt tracker: allow at most 1 execution per unique opportunity (no retries)
    let mut attempt_tracker = AttemptTracker::new(2048);

    // Rolling statistics summarizer (every 100 blocks)
    let mut stats = ChunkStats::new(100);
    let mut csv_logger = OpportunityCsvLogger::new("logs/opportunities.csv", "logs/opportunities_logged.txt");
    let mut reserves_logger = ReserveChangeCsvLogger::new("logs/pool_reserve_changes.csv");
    // Track repeated appearances of identical path opportunities across blocks
    let mut appearance_tracker = AppearanceTracker::new();
    // Track recently submitted opportunities (by path) to avoid duplicate submissions across consecutive blocks
    // De-dupe window reduced to 1 block to avoid suppressing too many legit opportunities
    let mut recent_submissions = RecentSubmissionTracker::new(1);

    loop {
        tokio::select! {
            maybe_snapshot = rx.recv() => {
                if let Some(snapshot) = maybe_snapshot {
                    // Log pool reserve changes before processing opportunities
                    reserves_logger.append_changes(snapshot.block_number, &snapshot);
                    stats.on_block(snapshot.block_number);
                    match engine.process_market_snapshot(&snapshot).await {
                        Ok(opps) if !opps.is_empty() => {
                            // Filter out opportunities previously marked as failed
                            let filtered0: Vec<ArbitrageOpportunity> = opps
                                .iter()
                                .cloned()
                                .filter(|o| {
                                    let sig = OpportunitySignature::from_opportunity(o, wmnt);
                                    !attempt_tracker.is_failed_signature(&sig) && !failed_store.is_failed(&sig)
                                })
                                .collect();
                            // Filter out opportunities that have already appeared in >=3 distinct blocks
                            let filtered: Vec<ArbitrageOpportunity> = appearance_tracker.filter_by_block_appearance(snapshot.block_number, &filtered0, wmnt);

                            if !filtered.is_empty() {
                                stats.record_opportunities(&filtered);
                                csv_logger.append_many(snapshot.block_number, &filtered, wmnt);
                                // Block-level opportunity summary
                                let mut sorted = filtered.clone();
                                sorted.sort_by(|a, b| b.net_profit_mnt_wei.cmp(&a.net_profit_mnt_wei));
                                info!("Block {} → {} opportunities found", snapshot.block_number, sorted.len());
                                for (idx, o) in sorted.iter().enumerate() {
                                    let hops = o.path.pools.len();
                                    let net = to_mnt(o.net_profit_mnt_wei);
                                    info!("  [{}] hops={} net≈{:.6} MNT margin={:.2}%", idx, hops, net, o.profit_margin_percent);
                                }
                                // Select multiple non-conflicting opportunities within WMNT balance
                                use std::collections::HashSet;
                                let mut used_pools: HashSet<Address> = HashSet::new();
                                let erc20 = IERC20::new(wmnt, &read_provider);
                                let bal = erc20.balanceOf(executor.context.executor_contract).call().await;
                                let mut remaining_wmnt = match bal {
                                    Ok(v) => v,
                                    Err(_) => U256::ZERO,
                                };
                                let mut selected: Vec<ArbitrageOpportunity> = Vec::new();
                                for o in sorted.iter() {
                                    // Skip non-profitable after gas
                                    if o.net_profit_mnt_wei.is_zero() { continue; }
                                    // Check pool conflict
                                    let pools: Vec<Address> = o.path.pools.iter().map(|p| p.get_address()).collect();
                                    if pools.iter().any(|a| used_pools.contains(a)) { continue; }
                                    // Check remaining funds
                                    if o.optimal_input_amount > remaining_wmnt { continue; }
                                    // Select it
                                    for a in pools { used_pools.insert(a); }
                                    remaining_wmnt = remaining_wmnt.saturating_sub(o.optimal_input_amount);
                                    selected.push(o.clone());
                                }

                                if !selected.is_empty() {
                                    info!("Selected {} non-conflicting opportunities this block", selected.len());
                                }

                                // Submit selected opportunities sequentially
                                for opp in selected.iter() {
                                    let path_key = PathKey::from_opportunity(opp, wmnt);
                                    if recent_submissions.is_recent(&path_key, snapshot.block_number) { continue; }
                                    recent_submissions.mark_submitted(path_key, snapshot.block_number);
                                    if let Err(e) = handle_opportunity(&executor, &read_provider, signed_provider.as_ref(), opp, snapshot.block_number, execute_enabled, &mut attempt_tracker, &mut failed_store, &mut stats).await {
                                        warn!("Handle opportunity failed: {}", e);
                                        stats.record_submission_failed();
                                    }
                                }
                            }
                            stats.maybe_log_summary();
                        }
                        Ok(_) => {}
                        Err(e) => warn!("Arb analysis error: {}", e),
                    }
                } else {
                    warn!("Market stream ended");
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                break;
            }
        }
    }

    Ok(())
}

async fn handle_opportunity<PRead, PSign>(
    executor: &Executor,
    read_provider: &PRead,
    signed_provider: Option<&PSign>,
    opportunity: &ArbitrageOpportunity,
    block_number: u64,
    execute_enabled: bool,
    attempts: &mut AttemptTracker,
    failed_store: &mut FailedOpportunityStore,
    stats: &mut ChunkStats,
) -> Result<()>
where
    PRead: alloy_provider::Provider,
    PSign: alloy_provider::Provider,
{
    // Profitability gate before building params (net profit threshold)
    if opportunity.net_profit_mnt_wei.is_zero() {
        warn!("Skip opportunity: non-profitable after gas");
        return Ok(());
    }

    let params = executor.build_params(read_provider, opportunity).await?;

    // Build stable key and check attempts
    let mut key = OpportunityKey::from_params(&params);
    key.block_hint = block_number;
    let count = attempts.get(&key);
    if count >= 1 {
        // Already attempted once; do not retry to avoid gas waste
        return Ok(());
    }
    {
        let hops = opportunity.path.pools.len();
        let net = to_mnt(opportunity.net_profit_mnt_wei);
        let in_mnt = to_mnt(opportunity.optimal_input_amount);
        info!("====================== 🚀 ARBITRAGE OPPORTUNITY (block {}) ======================", key.block_hint);
        info!("🔹 hops={} | amount_in={:.6} MNT | net≈{:.6} MNT | margin={:.2}%", hops, in_mnt, net, opportunity.profit_margin_percent);
        info!("===============================================================================");
    }

    // Count this attempt BEFORE sending, so we cap at 1 total attempt regardless of result
    attempts.increment(key);

    info!("Built params: amount_in={} hops={} min_out={}", params.amount_in, params.token_path.len().saturating_sub(1), params.min_amount_out);
    // Log expected execution gas pricing from executor configuration
    let base_fee_wei: u128 = 20_000_000; // 0.02 gwei
    let tip_wei: u128 = executor.config.default_priority_fee_wei; // e.g., 0.0001 gwei
    // Estimate cap per gas using 70% of net and gas limit heuristic (same as executor)
    let hops = params.token_path.len().saturating_sub(1);
    let gas_limit_to_use = if hops == 4 {
        750_000_000u128
    } else if hops == 2 {
        450_000_000u128
    } else {
        executor.config.gas_limit as u128
    };
    let net_expected_u128: u128 = params.expected_net_profit_mnt_wei.to_string().parse::<u128>().unwrap_or(0);
    let cap_price_from_profit = if gas_limit_to_use > 0 { (net_expected_u128.saturating_mul(70) / 100).saturating_div(gas_limit_to_use) } else { 0 };
    let floor = base_fee_wei.saturating_add(tip_wei);
    let global_cap = executor.config.global_fee_hard_cap_wei;
    let max_fee_estimate = floor.max(cap_price_from_profit).min(global_cap);
    info!("Gas config: baseFee={} tip={} maxFee_estimate={} (cap_from_70%={}, global_cap={})", base_fee_wei, tip_wei, max_fee_estimate, cap_price_from_profit, global_cap);

    if execute_enabled {
        if let Some(p) = signed_provider {
            match executor.execute(p, &params).await {
                Ok(tx_hash) => {
                    info!("Submitted tx: {:#}", tx_hash);
                    stats.record_submission();
                }
                Err(e) => {
                    // Mark this signature as failed so it won't be counted again
                    let sig = OpportunitySignature::from_params(&params);
                    attempts.mark_failed_signature(&sig);
                    failed_store.mark_failed(&sig);
                    return Err(e);
                }
            }
        } else {
            // Should not happen due to earlier check
            warn!("Execution enabled but missing signer; skipping execution");
        }
    }

    Ok(())
}

// ===== CSV and Token helpers (mirroring live monitor) =====

#[derive(Debug, Deserialize)]
struct PoolData {
    #[serde(rename = "Protocol")]
    _protocol: String,
    #[serde(rename = "Pair Name")] 
    pair_name: String,
    #[serde(rename = "Pair Address")] 
    pair_address: String,
    #[serde(rename = "TokenA Address")]
    token_a_address: String,
    #[serde(rename = "TokenB Address")]
    token_b_address: String,
}

#[derive(Debug, Deserialize)]
struct TokenCsvRow {
    #[serde(rename = "Token Symbol")] 
    symbol: String,
    #[serde(rename = "Token Address")] 
    address: String,
    #[serde(rename = "Decimals")] 
    decimals: Option<u8>,
}

fn load_tokens_from_csv(market: &mut Market) -> Result<HashMap<String, Address>> {
    let path = "data/selected/tokenLists.csv";
    if !Path::new(path).exists() {
        return Err(eyre::eyre!("Token CSV not found: {}", path));
    }
    let content = fs::read_to_string(path)?;
    let mut reader = csv::Reader::from_reader(content.as_bytes());
    let mut map = HashMap::new();
    for rec in reader.deserialize() {
        let row: TokenCsvRow = rec?;
        let addr = row.address.parse::<Address>()?;
        market.add_token(Token::new_with_data(addr, Some(row.symbol.clone()), None, row.decimals));
        map.insert(row.symbol.clone(), addr);
    }
    Ok(map)
}

fn load_pools_from_csv() -> Result<Vec<PoolWrapper>> {
    let path = "data/selected/poolLists.csv";
    if !Path::new(path).exists() {
        return Err(eyre::eyre!("CSV not found: {}", path));
    }
    let content = fs::read_to_string(path)?;
    let mut reader = csv::Reader::from_reader(content.as_bytes());
    let mut pools = Vec::new();
    for rec in reader.deserialize() {
        let row: PoolData = rec?;
        let pool_addr = row.pair_address.parse::<Address>()?;
        let token0 = row.token_a_address.parse::<Address>()?;
        let token1 = row.token_b_address.parse::<Address>()?;
        let mock = swap_path::MockPool { address: pool_addr, token0, token1 };
        pools.push(PoolWrapper::new(Arc::new(mock)));
    }
    Ok(pools)
}

// ===== Attempt tracking (limit to 3 executions per unique opportunity) =====

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OpportunityKey {
    amount_in: U256,
    tokens: Vec<Address>,
    pools: Vec<Address>,
    min_out: U256,
    block_hint: u64,
}

// ===== Rolling 100-block statistics =====

struct ChunkStats {
    window: u64,
    start_block: Option<u64>,
    last_block: Option<u64>,
    blocks_seen: u64,
    opportunities_found: u64,
    best_net_profit_mnt: f64,
    total_net_profit_mnt: f64,
    submissions: u64,
    submissions_failed: u64,
}

impl ChunkStats {
    fn new(window: u64) -> Self {
        Self {
            window,
            start_block: None,
            last_block: None,
            blocks_seen: 0,
            opportunities_found: 0,
            best_net_profit_mnt: 0.0,
            total_net_profit_mnt: 0.0,
            submissions: 0,
            submissions_failed: 0,
        }
    }

    fn on_block(&mut self, block: u64) {
        if self.start_block.is_none() {
            self.start_block = Some(block);
        }
        self.last_block = Some(block);
        self.blocks_seen += 1;
    }

    fn record_opportunities(&mut self, opps: &[ArbitrageOpportunity]) {
        self.opportunities_found += opps.len() as u64;
        if let Some(best) = opps.iter().max_by(|a, b| a.net_profit_mnt_wei.cmp(&b.net_profit_mnt_wei)) {
            let net = to_mnt(best.net_profit_mnt_wei);
            if net > self.best_net_profit_mnt {
                self.best_net_profit_mnt = net;
            }
        }
        let sum_net: f64 = opps.iter().map(|o| to_mnt(o.net_profit_mnt_wei)).sum();
        self.total_net_profit_mnt += sum_net;
    }

    fn record_submission(&mut self) {
        self.submissions += 1;
    }

    fn record_submission_failed(&mut self) {
        self.submissions_failed += 1;
    }

    fn maybe_log_summary(&mut self) {
        if self.blocks_seen >= self.window {
            let s = self.start_block.unwrap_or(0);
            let e = self.last_block.unwrap_or(s);
            info!("──────────────────────── 📊 100-BLOCK SUMMARY ────────────────────────");
            info!("Blocks: {} → {} ({} blocks)", s, e, self.blocks_seen);
            info!("Opportunities: {} | Submissions: {} (failed: {})", self.opportunities_found, self.submissions, self.submissions_failed);
            info!("Best net profit: {:.6} MNT | Total net (sum of opps): {:.6} MNT", self.best_net_profit_mnt, self.total_net_profit_mnt);
            info!("────────────────────────────────────────────────────────────────────");
            self.reset();
        }
    }

    fn reset(&mut self) {
        self.start_block = None;
        self.last_block = None;
        self.blocks_seen = 0;
        self.opportunities_found = 0;
        self.best_net_profit_mnt = 0.0;
        self.total_net_profit_mnt = 0.0;
        self.submissions = 0;
        self.submissions_failed = 0;
    }
}

fn to_mnt(v: U256) -> f64 { v.to_string().parse::<f64>().unwrap_or(0.0) / 1e18 }

impl OpportunityKey {
    fn from_params(params: &swap_path::ExecutionParams) -> Self {
        OpportunityKey {
            amount_in: params.amount_in,
            tokens: params.token_path.clone(),
            pools: params.pool_addresses.clone(),
            min_out: params.min_amount_out,
            block_hint: 0,
        }
    }
}

struct AttemptTracker {
    counts: HashMap<(U256, Vec<Address>, Vec<Address>, U256), u32>,
    order: VecDeque<(U256, Vec<Address>, Vec<Address>, U256)>,
    capacity: usize,
    failed_sigs: HashSet<OpportunitySignature>,
}

impl AttemptTracker {
    fn new(capacity: usize) -> Self {
        Self { counts: HashMap::new(), order: VecDeque::new(), capacity, failed_sigs: HashSet::new() }
    }

    fn key_tuple(key: &OpportunityKey) -> (U256, Vec<Address>, Vec<Address>, U256) {
        (key.amount_in, key.tokens.clone(), key.pools.clone(), key.min_out)
    }

    fn get(&self, key: &OpportunityKey) -> u32 {
        let t = Self::key_tuple(key);
        *self.counts.get(&t).unwrap_or(&0)
    }

    fn increment(&mut self, key: OpportunityKey) {
        let t = Self::key_tuple(&key);
        let entry = self.counts.entry(t.clone()).or_insert(0);
        *entry = entry.saturating_add(1);
        self.order.push_back(t.clone());
        if self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                if let Some(c) = self.counts.get_mut(&old) {
                    if *c <= 1 { self.counts.remove(&old); } else { *c -= 1; }
                }
            }
        }
    }
}

// ===== Signature to uniquely identify an opportunity across runs =====
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OpportunitySignature {
    amount_in: U256,
    tokens: Vec<Address>,
    pools: Vec<Address>,
    min_out: U256,
}

impl OpportunitySignature {
    fn from_params(params: &swap_path::ExecutionParams) -> Self {
        Self {
            amount_in: params.amount_in,
            tokens: params.token_path.clone(),
            pools: params.pool_addresses.clone(),
            min_out: params.min_amount_out,
        }
    }

    fn from_opportunity(o: &ArbitrageOpportunity, wmnt: Address) -> Self {
        let mut tokens: Vec<Address> = o.path.tokens.iter().map(|t| t.get_address()).collect();
        if !tokens.is_empty() {
            if tokens.first().copied() != Some(wmnt) { tokens[0] = wmnt; }
            if tokens.last().copied() != Some(wmnt) { if let Some(last) = tokens.last_mut() { *last = wmnt; } }
        }
        let pools: Vec<Address> = o.path.pools.iter().map(|p| p.get_address()).collect();
        Self { amount_in: o.optimal_input_amount, tokens, pools, min_out: o.expected_output_amount }
    }

    fn to_line(&self) -> String {
        let toks: Vec<String> = self.tokens.iter().map(|a| format!("0x{:x}", a)).collect();
        let pls: Vec<String> = self.pools.iter().map(|a| format!("0x{:x}", a)).collect();
        format!(
            "amount_in={};min_out={};tokens={};pools={}",
            self.amount_in,
            self.min_out,
            toks.join(","),
            pls.join(",")
        )
    }

    fn from_line(line: &str) -> Option<Self> {
        let parts: Vec<&str> = line.trim().split(';').collect();
        if parts.len() != 4 { return None; }
        let mut amount_in = U256::ZERO;
        let mut min_out = U256::ZERO;
        let mut tokens: Vec<Address> = Vec::new();
        let mut pools: Vec<Address> = Vec::new();
        for p in parts {
            if let Some(v) = p.strip_prefix("amount_in=") { amount_in = U256::from_str_radix(v, 10).ok()?; }
            else if let Some(v) = p.strip_prefix("min_out=") { min_out = U256::from_str_radix(v, 10).ok()?; }
            else if let Some(v) = p.strip_prefix("tokens=") {
                if !v.is_empty() { for s in v.split(',') { if !s.is_empty() { tokens.push(s.parse().ok()?); } } }
            }
            else if let Some(v) = p.strip_prefix("pools=") {
                if !v.is_empty() { for s in v.split(',') { if !s.is_empty() { pools.push(s.parse().ok()?); } } }
            }
        }
        Some(Self { amount_in, tokens, pools, min_out })
    }
}

// Track recently submitted opportunities to avoid duplicate submissions across consecutive blocks
struct RecentSubmissionTracker {
    // path key -> last submitted block
    last_submitted_block: HashMap<PathKey, u64>,
    // how many blocks to consider as "recent"
    window_blocks: u64,
}

impl RecentSubmissionTracker {
    fn new(window_blocks: u64) -> Self {
        Self { last_submitted_block: HashMap::new(), window_blocks }
    }

    fn is_recent(&self, key: &PathKey, current_block: u64) -> bool {
        if let Some(&b) = self.last_submitted_block.get(key) { return current_block == b; }
        false
    }

    fn mark_submitted(&mut self, key: PathKey, block: u64) {
        self.last_submitted_block.insert(key, block);
    }
}

// Track how many distinct blocks a given opportunity (by path+amount) has appeared in
struct AppearanceTracker {
    // key -> (first_seen_block, last_seen_block, distinct_blocks_count)
    seen: HashMap<PathKey, (u64, u64, u32)>,
}

impl AppearanceTracker {
    fn new() -> Self { Self { seen: HashMap::new() } }

    fn filter_by_block_appearance(&mut self, current_block: u64, opps: &[ArbitrageOpportunity], wmnt: Address) -> Vec<ArbitrageOpportunity> {
        let mut result = Vec::with_capacity(opps.len());
        for o in opps.iter().cloned() {
            let key = PathKey::from_opportunity(&o, wmnt);
            let entry = self.seen.entry(key).or_insert((current_block, current_block, 0));
            if entry.1 != current_block {
                // new block occurrence for this key
                entry.1 = current_block;
                entry.2 = entry.2.saturating_add(1);
            }
            if entry.2 < 3 {
                result.push(o);
            } else {
                // silently drop opportunities persisting >=3 blocks
            }
        }
        result
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PathKey {
    amount_in: U256,
    tokens: Vec<Address>,
    pools: Vec<Address>,
}

impl PathKey {
    fn from_opportunity(o: &ArbitrageOpportunity, wmnt: Address) -> Self {
        let mut tokens: Vec<Address> = o.path.tokens.iter().map(|t| t.get_address()).collect();
        if !tokens.is_empty() {
            if tokens.first().copied() != Some(wmnt) { tokens[0] = wmnt; }
            if tokens.last().copied() != Some(wmnt) { if let Some(last) = tokens.last_mut() { *last = wmnt; } }
        }
        let pools: Vec<Address> = o.path.pools.iter().map(|p| p.get_address()).collect();
        Self { amount_in: o.optimal_input_amount, tokens, pools }
    }
}

struct FailedOpportunityStore {
    failed: HashSet<OpportunitySignature>,
    path: String,
}

impl FailedOpportunityStore {
    fn load_or_default(path: &str) -> Self {
        let mut set = HashSet::new();
        if let Some(dir) = std::path::Path::new(path).parent() { let _ = create_dir_all(dir); }
        if let Ok(file) = std::fs::File::open(path) {
            let reader = std::io::BufReader::new(file);
            for line in reader.lines().flatten() {
                if let Some(sig) = OpportunitySignature::from_line(&line) { set.insert(sig); }
            }
        }
        Self { failed: set, path: path.to_string() }
    }

    fn is_failed(&self, sig: &OpportunitySignature) -> bool { self.failed.contains(sig) }

    fn mark_failed(&mut self, sig: &OpportunitySignature) {
        if self.failed.insert(sig.clone()) {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
                let _ = writeln!(f, "{}", sig.to_line());
            }
        }
    }
}

impl AttemptTracker {
    fn is_failed_signature(&self, sig: &OpportunitySignature) -> bool { self.failed_sigs.contains(sig) }
    fn mark_failed_signature(&mut self, sig: &OpportunitySignature) { self.failed_sigs.insert(sig.clone()); }
}

// ===== CSV logger for arbitrage opportunities =====
struct OpportunityCsvLogger {
    path: String,
    header_written: bool,
    sig_store_path: String,
    logged_sigs: HashSet<OpportunitySignature>,
}

impl OpportunityCsvLogger {
    fn new(path: &str, sig_store_path: &str) -> Self {
        if let Some(dir) = std::path::Path::new(path).parent() { let _ = create_dir_all(dir); }
        // Detect if file exists and non-empty for header logic
        let header_written = std::path::Path::new(path).exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        // Load existing logged signatures
        let mut logged_sigs = HashSet::new();
        if let Some(dir) = std::path::Path::new(sig_store_path).parent() { let _ = create_dir_all(dir); }
        if let Ok(f) = std::fs::File::open(sig_store_path) {
            let reader = std::io::BufReader::new(f);
            for line in reader.lines().flatten() {
                if let Some(sig) = OpportunitySignature::from_line(&line) { logged_sigs.insert(sig); }
            }
        }
        Self { path: path.to_string(), header_written, sig_store_path: sig_store_path.to_string(), logged_sigs }
    }

    fn append_many(&mut self, block: u64, opps: &[ArbitrageOpportunity], wmnt: Address) {
        if opps.is_empty() { return; }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            if !self.header_written {
                let _ = writeln!(f, "timestamp,block,hops,amount_in_wei,expected_out_wei,net_profit_wei,profit_margin_percent,tokens,pools");
                self.header_written = true;
            }
            let ts = chrono::Utc::now().to_rfc3339();
            let mut sig_file = OpenOptions::new().create(true).append(true).open(&self.sig_store_path).ok();
            for o in opps {
                // Deduplicate by signature (same path + amounts)
                let sig = OpportunitySignature::from_opportunity(o, wmnt);
                if self.logged_sigs.contains(&sig) { continue; }
                let hops = o.path.pools.len();
                let amount_in = o.optimal_input_amount;
                let expected_out = o.expected_output_amount;
                let net = o.net_profit_mnt_wei;
                let margin = o.profit_margin_percent;
                let tokens: Vec<String> = o.path.tokens.iter().map(|t| format!("0x{:x}", t.get_address())).collect();
                let pools: Vec<String> = o.path.pools.iter().map(|p| format!("0x{:x}", p.get_address())).collect();
                let _ = writeln!(
                    f,
                    "{},{},{},{},{},{},{:.4},\"{}\",\"{}\"",
                    ts,
                    block,
                    hops,
                    amount_in,
                    expected_out,
                    net,
                    margin,
                    tokens.join("|"),
                    pools.join("|"),
                );
                // Mark as logged
                self.logged_sigs.insert(sig.clone());
                if let Some(sf) = sig_file.as_mut() {
                    let _ = writeln!(sf, "{}", sig.to_line());
                }
            }
        }
    }
}

// ===== CSV logger for pool reserve changes =====
struct ReserveChangeCsvLogger {
    path: String,
    header_written: bool,
    last_reserves: std::collections::HashMap<swap_path::logic::pools::PoolId, (U256, U256)>,
}

impl ReserveChangeCsvLogger {
    fn new(path: &str) -> Self {
        if let Some(dir) = std::path::Path::new(path).parent() { let _ = create_dir_all(dir); }
        let header_written = std::path::Path::new(path).exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        Self { path: path.to_string(), header_written, last_reserves: std::collections::HashMap::new() }
    }

    fn append_changes(&mut self, block: u64, snapshot: &swap_path::logic::types::MarketSnapshot) {
        // Compare current reserves with last snapshot and log differences
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            if !self.header_written {
                let _ = writeln!(f, "timestamp,block,pool_id,reserve0_prev,reserve1_prev,reserve0_cur,reserve1_cur,delta0,delta1");
                self.header_written = true;
            }
            let ts = chrono::Utc::now().to_rfc3339();
            for (pool_id, (r0, r1)) in snapshot.pool_reserves.iter() {
                let prev = self.last_reserves.get(pool_id).copied().unwrap_or((U256::ZERO, U256::ZERO));
                if prev != (*r0, *r1) {
                    let (p0, p1) = prev;
                    let d0 = if *r0 >= p0 { *r0 - p0 } else { p0 - *r0 };
                    let d1 = if *r1 >= p1 { *r1 - p1 } else { p1 - *r1 };
                    let _ = writeln!(
                        f,
                        "{},{},\"{}\",{},{},{},{},{},{}",
                        ts,
                        block,
                        format!("{}", pool_id),
                        p0,
                        p1,
                        r0,
                        r1,
                        d0,
                        d1,
                    );
                }
                // Update cache for this pool only. Do not reset others if this snapshot is partial.
                self.last_reserves.insert(*pool_id, (*r0, *r1));
            }
        }
        // Do not overwrite cache wholesale; we updated per-pool above to preserve previous data on partial failures.
    }
}
```