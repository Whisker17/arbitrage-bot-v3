/// Moe LBT 套利监控与执行服务
///
/// 功能：
/// 1. 监控 Moe Liquidity Book 池子的状态变化
/// 2. 实时发现套利机会
/// 3. 自动执行盈利的套利路径
/// 4. 完整的日志系统和失败追踪
///
/// 使用方法：
/// 1. 配置环境变量：
///    - RPC_WS_URL / MANTLE_WS_URL: WebSocket RPC 端点
///    - RPC_HTTP_URL / MANTLE_HTTP_URL: HTTP RPC 端点
///    - EXECUTION_PRIVATE_KEY / PRIVATE_KEY: 执行账户私钥
///    - ARBITRAGE_EXECUTOR_ADDRESS: 执行器合约地址
///    - MIN_GROSS_PROFIT_WEI: 最小毛利润（默认 0.01 MNT）
///    - MIN_NET_PROFIT_WEI: 最小净利润（默认 0.01 MNT）
///    - EXECUTION_SLIPPAGE_BPS: 执行滑点（默认 30 bps）
///    - EXECUTION_BLOCK_COOLDOWN: 执行冷却期（默认 1 区块）
///
/// 2. 运行服务：
///    cargo run --example moe_monitor_executor_service
use alloy::consensus::BlockHeader;
use alloy::eips::BlockId;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, B256, I256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::client::ClientBuilder;
use alloy::rpc::types::{Filter, FilterSet, Log};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::Signer;
use alloy::sol_types::SolEvent;
use alloy::transports::layers::{RetryBackoffLayer, ThrottleLayer};
use alloy::transports::ws::WsConnect;
#[path = "../intent_service_support.rs"]
mod intent_service_support;
#[path = "../legacy_service_support.rs"]
mod legacy_service_support;
use amms::amms::{
    amm::{AutomatedMarketMaker, Variant, AMM},
    moe::{
        default_moe_pool_list_path, sync_moe_snapshots_batch, IMoeLBPairEvents, MoeLbPair,
        MoePoolList, MoeSnapshotContext, MoeSnapshotSyncConfig, CANONICAL_MOE_FACTORY,
    },
};
use amms::arbitrage::{
    graph::build_graph,
    optimizer::pools_for_path,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::execution::{
    BinCrossingBucket, Executor, ExecutorConfig, IArbitrageExecutor, ProtocolKind, RouteKey,
    ShadowExecutionContext, IERC20,
};
use amms::state_space::{
    hash_pinned_logs_filter, hash_pinned_state_block_id, max_input_bound_for_snapshot,
    BlockHeaderContext, MarketSnapshot, PoolProtocol, ProtocolCoverage, SnapshotBoundBalance,
    SnapshotId, SnapshotStatus, StateSpace,
};
use csv::{StringRecord, WriterBuilder};
use eyre::{eyre, Context, Result};
use futures::{stream, StreamExt};
use legacy_service_support::{
    default_gas_safety_margin, gas_limit_for_hops, is_on_cooldown,
    plan_resized_execution_default_margin, route_is_structurally_valid, wait_for_block_logs,
    FailureStore, GasConfig,
};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, warn};

// ============================================
// 常量配置
// ============================================

const MAX_HOPS: usize = 3;
const WMNT_ADDRESS: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
// ⚠️ BINS_RADIUS 是关键参数！
// - 太小（如 50）：模拟精度差，大额交易会高估利润
// - 太大（如 500）：同步慢，占用内存多
// - 推荐 200：与 verify_swap_path.rs 保持一致，误差约 0.3%
const BINS_RADIUS: u32 = 200;
const BINS_BATCH_SIZE: u32 = 15;
// ⚠️ 考虑到 ~0.3% 的模拟误差，需要足够的安全边际
// 对于 ROI 1-2% 的套利机会，至少需要 0.3-0.5 MNT 的利润缓冲
const MIN_PROFIT_FLOOR_WEI: &str = "250000000000000000"; // 0.3 MNT (考虑模拟误差和 gas 波动)
const FAILED_OPPORTUNITIES_PATH: &str = "logs/moe_failed_opportunities.json";
const MIN_QUOTE_INPUT: u128 = 1_000_000_000_000;
const MAX_QUOTE_INPUT: u128 = 1_000_000_000_000_000_000_000_000;

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

// ============================================
// 辅助函数
// ============================================

fn resolve_ws_endpoint() -> String {
    let raw = std::env::var("RPC_WS_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_WS_URL").ok())
        .unwrap_or_else(|| "wss://mantle.publicnode.com".to_string());
    let normalized = normalize_ws_endpoint(raw.trim());
    if normalized != raw {
        info!(
            target: "moe.config",
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

// ============================================
// 数据结构
// ============================================

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
    /// Snapshot of path pools at discovery time — used to re-simulate after input resize.
    path: ArbitragePath,
    pools: Vec<AMM>,
    log_hops: String,
    roi: String,
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

#[derive(Clone)]
struct PathCache {
    paths: Vec<ArbitragePath>,
    state_pools: Vec<AMM>,
    pool_to_path_indices: HashMap<Address, Vec<usize>>,
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
        info!(target: "moe.config", ws = %ws_endpoint, "Using WebSocket endpoint");

        let http_endpoint = resolve_http_endpoint();
        info!(target: "moe.config", http = %http_endpoint, "Using HTTP endpoint");

        let (executor_address, executor_source) = read_address_from_env(&[
            "ARBITRAGE_EXECUTOR_ADDRESS",
            "EXECUTOR_ADDRESS",
            "EXECUTION_EXECUTOR_ADDRESS",
        ])?;
        info!(
            target: "moe.config",
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
                target: "moe.config",
                provided = %min_net_profit_raw,
                adjusted = %min_gross_profit,
                "Net profit threshold below gross profit threshold; using gross threshold"
            );
            min_gross_profit
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
        executor_config.min_net_profit_mnt_wei = min_net_profit;
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
                    target: "moe.config",
                    variable = var,
                    provided = %parsed,
                    floor = %floor,
                    "Configured profit threshold below floor; using floor"
                );
                *floor
            } else {
                parsed
            }
        }
        Err(_) => *floor,
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

// ============================================
// 主函数
// ============================================

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    init_tracing();

    let config = ServiceConfig::from_env()?;

    let ws_provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(config.ws_endpoint.clone()))
        .await
        .context("Failed to connect WS provider")?;

    if intent_service_support::shadow_mode_enabled() {
        // WHI-549: signerless shadow mode. No wallet is ever constructed; the
        // placeholder `signer_address` only stands in for `caller` in the shadow
        // `eth_call`'s `admin` bypass override, which accepts any address since
        // `eth_call`'s `from` is unauthenticated.
        let signer_address = Address::ZERO;

        // HTTP is used for fail-closed pool-list on-chain validation + init (~768 calls).
        // Retry/throttle matching generate_moe_pool_list so startup survives RPC flakes.
        let http_client = ClientBuilder::default()
            .layer(ThrottleLayer::new(40))
            .layer(RetryBackoffLayer::new(8, 250, 500))
            .http(
                config
                    .http_endpoint
                    .parse()
                    .context("invalid http endpoint")?,
            );
        let http_provider: DynProvider = ProviderBuilder::new()
            .connect_client(http_client)
            .erased();

        let shadow_ctx = intent_service_support::build_shadow_execution_context_or_monitor_only(
            http_provider.clone(),
            config.executor_address,
            config.wmnt_address,
            config.executor_config.clone(),
            Path::new("logs/shadow_ledger_moe.jsonl"),
            "moe.service",
        )
        .map(Arc::new);

        info!(
            target: "moe.service",
            executor = %config.executor_address,
            shadow_enabled = shadow_ctx.is_some(),
            max_hops = MAX_HOPS,
            "Starting Moe LBT monitoring + SHADOW execution service on Mantle"
        );

        run_service(ws_provider, http_provider, config, None, shadow_ctx, signer_address).await
    } else {
        let private_key = std::env::var("EXECUTION_PRIVATE_KEY")
            .or_else(|_| std::env::var("PRIVATE_KEY"))
            .context("Missing EXECUTION_PRIVATE_KEY or PRIVATE_KEY")?;
        let signer = PrivateKeySigner::from_str(private_key.trim())?;
        let signer_address = signer.address();
        let wallet = EthereumWallet::from(signer);

        // HTTP is used for fail-closed pool-list on-chain validation + init (~768 calls).
        // Retry/throttle matching generate_moe_pool_list so startup survives RPC flakes.
        let http_client = ClientBuilder::default()
            .layer(ThrottleLayer::new(40))
            .layer(RetryBackoffLayer::new(8, 250, 500))
            .http(
                config
                    .http_endpoint
                    .parse()
                    .context("invalid http endpoint")?,
            );
        let http_provider: DynProvider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_client(http_client)
            .erased();

        // Degrade closed, never die: an executor-identity mismatch (e.g. a Sepolia
        // deployment vs the pinned mainnet gas-profile artifact) leaves `executor == None`
        // and the service runs monitor-only instead of killing monitoring at startup.
        let executor = intent_service_support::build_execution_runtime_or_monitor_only(
            http_provider.clone(),
            config.executor_address,
            config.wmnt_address,
            config.executor_config.clone(),
            "moe.service",
        )
        .await
        .map(Arc::new);

        info!(
            target: "moe.service",
            executor = %config.executor_address,
            execution_enabled = executor.is_some(),
            max_hops = MAX_HOPS,
            "Starting Moe LBT monitoring + execution service on Mantle"
        );

        run_service(ws_provider, http_provider, config, executor, None, signer_address).await
    }
}

// ============================================
// 服务主循环
// ============================================

#[allow(clippy::too_many_arguments)]
async fn run_service<P, H>(
    ws_provider: P,
    http_provider: H,
    config: ServiceConfig,
    executor: Option<Arc<Executor>>,
    shadow_ctx: Option<Arc<ShadowExecutionContext>>,
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

    // 初始化日志文件
    let pool_log_path = std::env::var("POOL_UPDATE_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_pool_updates.csv"));
    ensure_log_headers(
        &pool_log_path,
        &[
            "block_number",
            "pool_address",
            "event",
            "active_id",
            "reserve_x",
            "reserve_y",
        ],
    )?;

    let positive_sim_log_path = std::env::var("POSITIVE_PATH_SIM_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_positive_path_simulations.csv"));
    let best_paths_log_path = std::env::var("BEST_ARBITRAGE_PATHS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("logs/moe_best_arbitrage_paths.csv"));
    ensure_log_headers(&positive_sim_log_path, POSITIVE_PATH_LOG_HEADERS)?;
    ensure_log_headers(&best_paths_log_path, BEST_PATH_LOG_HEADERS)?;

    // 初始化池子 (HTTP + retry/throttle)
    let latest_block = http_provider.get_block_number().await?;
    let pin_hash = legacy_service_support::canonical_block_hash_at_number(
        &http_provider,
        latest_block,
    )
    .await?;
    // Shadow mode's `signer_address` is a placeholder (`Address::ZERO`), never a real
    // hot-executor signer, so this on-chain role check is production-only.
    if shadow_ctx.is_none() {
        amms::execution::verify_execution_signer_roles(
            &http_provider,
            config.executor_address,
            signer_address,
            pin_hash,
        )
        .await?;
    }
    let mut pools: HashMap<Address, MoeLbPair> = HashMap::new();
    initialize_moe_pools(&http_provider, pin_hash, &mut pools).await?;

    info!(
        target: "moe.service",
        pools = pools.len(),
        "Initialized Moe LBPairs"
    );

    // 验证 bins 覆盖范围
    let mut critical_issues = 0;
    let mut asymmetric_pools = 0;
    let mut good_pools = 0;
    let min_expected_bins = (BINS_RADIUS * 2) / 4; // 至少期望 1/4 的范围有 bins

    for pool in pools.values() {
        let (ok, message) = verify_bins_coverage(pool, min_expected_bins);
        if !ok {
            warn!(target: "moe.init", "{}", message);
            critical_issues += 1;
        } else if message.contains("asymmetric") {
            debug!(target: "moe.init", "{}", message);
            asymmetric_pools += 1;
        } else {
            debug!(target: "moe.init", "{}", message);
            good_pools += 1;
        }
    }

    if critical_issues > 0 {
        warn!(
            target: "moe.init",
            critical = critical_issues,
            asymmetric = asymmetric_pools,
            good = good_pools,
            total = pools.len(),
            "⚠️  {} pools have critical coverage issues (extremely low liquidity). These pools will be filtered out during path finding.",
            critical_issues
        );
    } else if asymmetric_pools > 0 {
        info!(
            target: "moe.init",
            asymmetric = asymmetric_pools,
            good = good_pools,
            total = pools.len(),
            "✅ Bins coverage check complete: {} pools have asymmetric liquidity (normal), {} pools have good coverage",
            asymmetric_pools,
            good_pools
        );
    } else {
        info!(
            target: "moe.init",
            total = pools.len(),
            "✅ All {} pools have good bins coverage (BINS_RADIUS={})",
            pools.len(),
            BINS_RADIUS
        );
    }

    for pool in pools.values() {
        log_pool_state(&pool_log_path, latest_block, "init", pool)?;
    }

    // 构建路径缓存
    let path_cache = Arc::new(build_path_cache(&pools, MAX_HOPS, config.wmnt_address)?);
    let admitted_pool_addresses: HashSet<Address> = path_cache
        .state_pools
        .iter()
        .map(AutomatedMarketMaker::address)
        .collect();
    legacy_service_support::verify_executable_pool_provenance(
        &http_provider,
        config.executor_address,
        CANONICAL_MOE_FACTORY,
        PoolProtocol::MoeLb,
        path_cache.state_pools.iter(),
        pin_hash,
    )
    .await?;
    let pool_universe_fingerprint = legacy_service_support::executable_pool_universe_fingerprint(
        chain_id,
        config.wmnt_address,
        CANONICAL_MOE_FACTORY,
        PoolProtocol::MoeLb,
        path_cache.state_pools.iter(),
    )?;
    info!(
        target: "moe.service",
        candidate_paths = path_cache.paths.len(),
        "Pre-computed arbitrage candidate paths"
    );

    // 设置事件过滤器
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IMoeLBPairEvents::Swap::SIGNATURE_HASH,
        IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH,
        IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH,
    ]));
    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // 订阅区块流
    let mut block_stream = ws_provider.subscribe_blocks().await?.into_stream();
    info!(target: "moe.service", "Subscribed to block stream");

    // 初始化共享状态
    let gas_config = GasConfig::default();
    let http_provider = Arc::new(http_provider);
    let last_executions = Arc::new(AsyncMutex::new(HashMap::<String, u64>::new()));
    let failed_store = Arc::new(AsyncMutex::new(FailureStore::new(
        FAILED_OPPORTUNITIES_PATH,
    )?));
    let logged_paths = Arc::new(AsyncMutex::new(HashMap::<String, LoggedPathRecord>::new()));
    let last_selection = Arc::new(AsyncMutex::new(None::<SelectionSnapshot>));

    // 创建执行队列
    let job_slot = intent_service_support::new_job_slot::<ExecutionJob>();
    let worker_slot = Arc::clone(&job_slot);
    let latest_tip = Arc::new(AsyncMutex::new(None::<SnapshotStatus>));
    let worker_latest_tip = Arc::clone(&latest_tip);

    // 启动执行任务
    let execution_config = Arc::clone(&config);
    let execution_provider = Arc::clone(&http_provider);
    let execution_last = Arc::clone(&last_executions);
    let execution_failed_store = Arc::clone(&failed_store);
    let execution_executor = executor.clone();
    let execution_shadow_ctx = shadow_ctx.clone();
    let execution_task = tokio::spawn(async move {
        loop {
            let Some(job) = worker_slot.take() else {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                continue;
            };
            let signature = OpportunitySignature::from_candidate(&job.candidate);
            if !route_is_structurally_valid(
                execution_config.wmnt_address,
                &job.candidate.token_path,
                &job.candidate.pool_addresses,
                Variant::MoeLbPair,
                &job.candidate.pools,
            ) {
                let mut store = execution_failed_store.lock().await;
                if let Err(mark_err) = store.mark_permanent(signature.clone()) {
                    error!(
                        target: "moe.failure_store",
                        error = ?mark_err,
                        "Failed to persist structural failure"
                    );
                }
                continue;
            }
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
                    target: "moe.exec",
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
                    error!(target: "moe.exec", error = %err, "Rejecting opportunity without a matching live Ready tip");
                    continue;
                }
            };

            // Monitor-only when the execution runtime is absent (executor identity did
            // not match the pinned gas-profile artifact at startup): never send.
            let Some(execution) = execution_executor
                .as_deref()
                .map(intent_service_support::ServiceExecutionContext::Production)
                .or_else(|| {
                    execution_shadow_ctx
                        .as_deref()
                        .map(intent_service_support::ServiceExecutionContext::Shadow)
                })
            else {
                info!(
                    target: "moe.exec",
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
                &execution,
                execution_shadow_ctx.as_deref(),
                signer_address,
                &live_status,
                job.header,
                job.pool_universe_fingerprint,
                job.base_fee_per_gas,
                job.block_gas_limit,
            )
            .await
            {
                Ok(ExecutionAttempt::Submitted(tx_hash)) => {
                    let mut executions = execution_last.lock().await;
                    executions.insert(job.candidate.signature.clone(), job.block_number);
                    info!(
                        target: "moe.exec",
                        block = job.block_number,
                        tx = %tx_hash,
                        signature = %job.candidate.signature,
                        net_profit = %job.candidate.net_profit,
                        "✅ Execution submitted successfully"
                    );
                }
                Ok(ExecutionAttempt::ProductionGateBlocked {
                    amount_in,
                    min_profit,
                }) => {
                    // Typed M2-8 gate: principal plan was valid; do not permanently blacklist.
                    warn!(
                        target: "moe.exec",
                        block = job.block_number,
                        signature = %job.candidate.signature,
                        amount_in = %amount_in,
                        min_profit = %min_profit,
                        "Skipped send (production gate); not marking opportunity failed"
                    );
                }
                Err(err) => {
                    error!(
                        target: "moe.exec",
                        block = job.block_number,
                        signature = %job.candidate.signature,
                        error = ?err,
                        "❌ Execution attempt failed"
                    );
                    let mut store = execution_failed_store.lock().await;
                    if let Err(mark_err) = store.mark_transient(signature) {
                        error!(
                            target: "moe.failure_store",
                            error = ?mark_err,
                            "Failed to persist failed opportunity"
                        );
                    }
                }
            }
        }
    });

    // 主监控循环
    while let Some(block) = block_stream.next().await {
        let number = block.number();
        if number == 0 {
            continue;
        }
        let target_number = number;
        info!(target: "moe.block", block = target_number, "Processing block");

        let target_header = legacy_service_support::canonical_block_header(
            &http_provider,
            target_number,
            block.hash(),
        )
        .await?;
        let context = MoeSnapshotContext::new(
            target_header.header().hash(),
            target_header.header().timestamp,
        );
        let snapshot_id = SnapshotId::new(chain_id, target_number, context.block_hash);
        let header = BlockHeaderContext::new(
            target_header.header().parent_hash(),
            target_header.header().timestamp(),
        );
        // A fabricated zero base fee would be bound into the permit's BlockFeeContext
        // and silently mis-price every candidate in this block. Skip the block instead.
        let Some(base_fee_per_gas) = target_header.header().base_fee_per_gas() else {
            warn!(
                target: "moe.block",
                block = target_number,
                "Block header carries no base fee; skipping block (fee context must be exact)"
            );
            continue;
        };
        let base_fee_per_gas = base_fee_per_gas as u128;
        let block_gas_limit = target_header.header().gas_limit();
        let windowed = hash_pinned_logs_filter(filter.clone(), context.block_hash);
        match wait_for_block_logs(&http_provider, &windowed, target_number, context.block_hash)
            .await
        {
            Ok(logs) => {
                if logs.is_empty() {
                    continue;
                }

                let mut working_pools = pools.clone();
                let changed = apply_logs(&mut working_pools, &logs, target_number, &pool_log_path)?;
                if changed.is_empty() {
                    continue;
                }

                // 重新同步变化的池子 (HTTP + retry/throttle)
                let mut snapshot_amms: Vec<AMM> = working_pools
                    .values()
                    .cloned()
                    .map(AMM::MoeLbPair)
                    .collect();
                sync_moe_snapshots_at_context(
                    &mut snapshot_amms,
                    http_provider.clone(),
                    context,
                    BINS_RADIUS,
                    BINS_BATCH_SIZE,
                )
                .await?;
                working_pools = snapshot_amms
                    .into_iter()
                    .filter_map(|amm| match amm {
                        AMM::MoeLbPair(pair) => Some((pair.address, pair)),
                        _ => None,
                    })
                    .collect();
                pools = working_pools;
                let mut coverage = ProtocolCoverage::default();
                coverage.pool_universe_fingerprint = Some(pool_universe_fingerprint);
                let snapshot_pools = pools
                    .iter()
                    .filter(|(address, _)| admitted_pool_addresses.contains(*address))
                    .map(|(address, pool)| (*address, AMM::MoeLbPair(pool.clone())))
                    .collect();
                let snapshot_status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
                    snapshot_id,
                    header,
                    snapshot_pools,
                    coverage,
                )));
                *latest_tip.lock().await = Some(snapshot_status.clone());

                let executor_balance = executor_balance_at_snapshot(
                    http_provider.as_ref(),
                    config.as_ref(),
                    snapshot_id,
                )
                .await
                .context("Failed to read snapshot-bound executor WMNT balance")?;

                // 查找盈利机会
                let mut selection_history = last_selection.lock().await;
                let mut logged = logged_paths.lock().await;

                let candidates = find_profitable_candidates(
                    &pools,
                    &gas_config,
                    config.as_ref(),
                    target_number,
                    &path_cache,
                    &changed,
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
                    info!(
                        target: "moe.exec",
                        block = target_number,
                        "No candidates selected after conflict resolution"
                    );
                    continue;
                }

                let selection_changed =
                    record_selection_snapshot(&selected_candidates, &mut selection_history);

                if selection_changed && !selected_candidates.is_empty() {
                    info!(
                        target: "moe.exec",
                        block = target_number,
                        candidates = selected_candidates.len(),
                        "✅ Selected non-conflicting opportunities for execution"
                    );
                }

                drop(logged);
                drop(selection_history);

                // 提交执行任务
                for candidate in selected_candidates {
                    let signature = OpportunitySignature::from_candidate(&candidate);
                    let failed_guard = failed_store.lock().await;
                    if failed_guard.is_failed(&signature) {
                        info!(
                            target: "moe.exec.skip",
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
                error!(target: "moe.block", block = target_number, error = ?e, "get_logs failed");
                return Err(e.into());
            }
        }
    }

    // worker uses latest-wins slot; no channel to drop
    if let Err(join_err) = execution_task.await {
        error!(target: "moe.exec", error = ?join_err, "Execution worker failed");
    }

    Ok(())
}

// ============================================
// 池子初始化与同步
// ============================================

async fn initialize_moe_pools<P: Provider + Clone>(
    provider: &P,
    block_hash: B256,
    pools: &mut HashMap<Address, MoeLbPair>,
) -> Result<()> {
    let block_id = hash_pinned_state_block_id(block_hash);
    let csv_path = default_moe_pool_list_path();
    let list = MoePoolList::load_and_validate_on_chain(
        &csv_path,
        provider.clone(),
        block_id,
        CANONICAL_MOE_FACTORY,
    )
    .await
    .with_context(|| {
        format!(
            "Failed to load/validate dedicated Moe pool list at {} (no Agni fallback)",
            csv_path.display()
        )
    })?;

    info!(
        target: "moe.service",
        path = %csv_path.display(),
        pools = list.len(),
        snapshot_block = list.snapshot_block(),
        "Loaded and on-chain-validated dedicated Moe pool list"
    );

    let init_jobs: Vec<Address> = list.entries.iter().map(|e| e.pool).collect();
    let expected = init_jobs.len();

    const MAX_INIT_CONCURRENCY: usize = 8;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|addr| {
        let provider = provider.clone();
        async move {
            let result = MoeLbPair::new(addr).init_basic(block_id, provider).await;
            (addr, result)
        }
    }))
    .buffer_unordered(MAX_INIT_CONCURRENCY);

    let mut init_errors = Vec::new();
    while let Some((addr, result)) = init_stream.next().await {
        match result {
            Ok(pool) => {
                info!(target: "moe.init", address = %addr, bin_step = pool.bin_step, "Initialized Moe pool");
                pools.insert(addr, pool);
            }
            Err(err) => {
                error!(
                    target: "moe.init",
                    address = %addr,
                    error = %err,
                    "Failed to initialize Moe pool"
                );
                init_errors.push(format!("{addr}: {err}"));
            }
        }
    }

    if !init_errors.is_empty() {
        return Err(eyre!(
            "fail-closed: {}/{} Moe pools failed to initialize: {}",
            init_errors.len(),
            expected,
            init_errors.join("; ")
        ));
    }
    if pools.len() != expected || pools.is_empty() {
        return Err(eyre!(
            "fail-closed: expected {expected} initialized Moe pools, got {}",
            pools.len()
        ));
    }

    // 同步 bins 数据
    info!(
        target: "moe.init",
        radius = BINS_RADIUS,
        batch_size = BINS_BATCH_SIZE,
        "Syncing bin data for active bins"
    );
    let mut pool_vec: Vec<AMM> = pools.values().cloned().map(AMM::MoeLbPair).collect();
    let header = provider
        .get_block_by_hash(block_hash)
        .await?
        .ok_or_else(|| eyre!("missing block {block_hash}"))?;
    let context = MoeSnapshotContext::new(header.header().hash(), header.header().timestamp);
    sync_moe_snapshots_at_context(
        &mut pool_vec,
        provider.clone(),
        context,
        BINS_RADIUS,
        BINS_BATCH_SIZE,
    )
    .await
    .context("Failed to sync Moe snapshot")?;
    for amm in pool_vec {
        if let AMM::MoeLbPair(p) = amm {
            pools.insert(p.address, p);
        }
    }
    info!(target: "moe.init", "Bin data synced successfully");

    Ok(())
}

async fn sync_moe_snapshots_at_context<P: Provider + Clone>(
    amms: &mut Vec<AMM>,
    provider: P,
    context: MoeSnapshotContext,
    radius: u32,
    batch_size: u32,
) -> Result<()> {
    let block_id = BlockId::hash_canonical(context.block_hash);
    sync_moe_snapshots_batch(
        amms,
        block_id,
        provider,
        context,
        MoeSnapshotSyncConfig {
            bins_radius: radius,
            bins_per_request: batch_size,
        },
    )
    .await?;
    Ok(())
}

fn apply_logs(
    pools: &mut HashMap<Address, MoeLbPair>,
    logs: &[Log],
    block_number: u64,
    log_path: &Path,
) -> Result<HashSet<Address>> {
    let mut changed = HashSet::new();
    let mut swap_count = 0;
    let mut deposit_count = 0;
    let mut withdraw_count = 0;

    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            match log.topics()[0] {
                sig if sig == IMoeLBPairEvents::Swap::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Swap)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "swap", pool)?;
                    swap_count += 1;
                    changed.insert(addr);
                }
                sig if sig == IMoeLBPairEvents::DepositedToBins::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Deposit)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "deposit", pool)?;
                    deposit_count += 1;
                    changed.insert(addr);
                }
                sig if sig == IMoeLBPairEvents::WithdrawnFromBins::SIGNATURE_HASH => {
                    if let Err(e) = pool.sync(log) {
                        error!(target: "moe.pool", address = %addr, error = ?e, "sync error (Withdraw)");
                        continue;
                    }
                    log_pool_state(log_path, block_number, "withdraw", pool)?;
                    withdraw_count += 1;
                    changed.insert(addr);
                }
                _ => {}
            }
        }
    }

    if !changed.is_empty() {
        info!(
            target: "moe.pool",
            block = block_number,
            changed_pools = changed.len(),
            swaps = swap_count,
            deposits = deposit_count,
            withdraws = withdraw_count,
            "Pool update summary"
        );
    }

    Ok(changed)
}

// ============================================
// 路径发现与选择
// ============================================

fn is_pool_reasonable(pool: &MoeLbPair) -> bool {
    if pool.reserve_x == 0 || pool.reserve_y == 0 {
        warn!(
            target: "moe.filter",
            address = %pool.address,
            reserve_x = pool.reserve_x,
            reserve_y = pool.reserve_y,
            "Pool has zero reserves, excluding"
        );
        return false;
    }
    true
}

fn build_path_cache(
    pools: &HashMap<Address, MoeLbPair>,
    max_hops: usize,
    wmnt_address: Address,
) -> Result<PathCache> {
    let mut state = StateSpace::default();
    let mut filtered_count = 0;

    for pool in pools.values() {
        if is_pool_reasonable(pool) {
            state
                .state
                .insert(pool.address(), AMM::MoeLbPair(pool.clone()));
        } else {
            filtered_count += 1;
        }
    }

    if filtered_count > 0 {
        info!(
            target: "moe.monitor",
            filtered = filtered_count,
            total = pools.len(),
            "Filtered {} pools with zero reserves",
            filtered_count
        );
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

fn find_profitable_candidates(
    pools: &HashMap<Address, MoeLbPair>,
    gas_config: &GasConfig,
    config: &ServiceConfig,
    block_number: u64,
    path_cache: &PathCache,
    changed_pools: &HashSet<Address>,
    snapshot_id: SnapshotId,
    executor_balance: SnapshotBoundBalance,
) -> Result<Vec<PositiveCandidate>> {
    if pools.is_empty() {
        return Ok(Vec::new());
    }

    let mut state = StateSpace::default();
    for pool in pools.values() {
        state
            .state
            .insert(pool.address(), AMM::MoeLbPair(pool.clone()));
    }
    let state_pools: Vec<AMM> = state.state.values().cloned().collect();
    let max_input_bound =
        max_input_bound_for_snapshot(snapshot_id, executor_balance, U256::from(MAX_QUOTE_INPUT))?;

    // 收集受影响的路径索引
    let mut affected_path_indices = HashSet::new();
    for pool_addr in changed_pools {
        if let Some(indices) = path_cache.pool_to_path_indices.get(pool_addr) {
            affected_path_indices.extend(indices);
        }
    }

    // 如果没有受影响的路径，直接返回
    if affected_path_indices.is_empty() {
        return Ok(Vec::new());
    }

    // 过滤统计
    use std::sync::atomic::{AtomicUsize, Ordering};
    let negative_profit = AtomicUsize::new(0);
    let below_gross_threshold = AtomicUsize::new(0);
    let gas_makes_negative = AtomicUsize::new(0);
    let below_net_threshold = AtomicUsize::new(0);
    let safety_factor_fail = AtomicUsize::new(0);

    let candidates: Vec<PositiveCandidate> = affected_path_indices
        .par_iter()
        .filter_map(|&&path_idx| {
            let path = &path_cache.paths[path_idx];
            let pools_for_path = match pools_for_path(path, &state_pools) {
                Ok(p) => p,
                Err(_) => return None,
            };

            let simulation =
                best_path_simulation_with_steps(path, &pools_for_path, max_input_bound)?;

            if simulation.profit <= I256::ZERO {
                negative_profit.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            let profit_u256 = U256::from_limbs(*simulation.profit.as_limbs());
            if profit_u256 < config.min_gross_profit {
                below_gross_threshold.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            let num_hops = path.hops.len();
            let net_profit = match gas_config.net_profit(profit_u256, num_hops) {
                Some(net) => net,
                None => {
                    gas_makes_negative.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
            };

            if net_profit < config.min_net_profit {
                below_net_threshold.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            if !gas_config.is_profitable_after_gas(
                profit_u256,
                num_hops,
                default_gas_safety_margin(),
            ) {
                safety_factor_fail.fetch_add(1, Ordering::Relaxed);
                return None;
            }

            let token_path = build_token_path(path);
            if token_path.first().copied() != Some(config.wmnt_address) {
                return None;
            }
            if token_path.last().copied() != Some(config.wmnt_address) {
                return None;
            }

            let pool_addresses: Vec<Address> =
                path.hops.iter().map(|hop| hop.pool_address).collect();

            let roi = format_roi_percent(simulation.profit, simulation.input)
                .unwrap_or_else(|| "-".to_string());

            Some(PositiveCandidate {
                snapshot_id,
                signature: path_signature(path),
                hops: num_hops,
                input: simulation.input,
                output: simulation.output,
                profit: simulation.profit,
                net_profit,
                pool_addresses,
                token_path,
                path: path.clone(),
                pools: pools_for_path,
                log_hops: hops_description(path),
                roi,
            })
        })
        .collect();

    // 打印过滤汇总
    let affected_paths = affected_path_indices.len();
    let passed = candidates.len();
    info!(
        target: "moe.candidate",
        block = block_number,
        affected_paths = affected_paths,
        negative_profit = negative_profit.load(Ordering::Relaxed),
        below_gross_threshold = below_gross_threshold.load(Ordering::Relaxed),
        gas_makes_negative = gas_makes_negative.load(Ordering::Relaxed),
        below_net_threshold = below_net_threshold.load(Ordering::Relaxed),
        safety_factor_fail = safety_factor_fail.load(Ordering::Relaxed),
        passed = passed,
        "Path filtering summary"
    );

    Ok(candidates)
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

// ============================================
// 执行逻辑
// ============================================

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

/// Outcome of a Moe execution attempt.
///
/// Production-gate blocks are a distinct success-path variant so the worker never has to
/// pattern-match human-readable error strings (review finding on WHI-503).
#[derive(Debug)]
enum ExecutionAttempt {
    Submitted(alloy::primitives::TxHash),
    /// Principal plan was valid; M2-8 human gate blocked the send.
    ProductionGateBlocked {
        amount_in: U256,
        min_profit: U256,
    },
}

#[allow(clippy::too_many_arguments)]
async fn attempt_execution<H: Provider + Clone + 'static>(
    provider: &H,
    candidate: &PositiveCandidate,
    config: &ServiceConfig,
    execution: &intent_service_support::ServiceExecutionContext<'_>,
    shadow_ctx: Option<&ShadowExecutionContext>,
    signer_address: Address,
    snapshot_status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: alloy::primitives::B256,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> Result<ExecutionAttempt> {
    let wmnt_contract = IERC20::new(config.wmnt_address, provider.clone());
    let executor_balance = wmnt_contract
        .balanceOf(config.executor_address)
        .call()
        .await?;
    let gas_config = GasConfig::default();
    let gas_cost = gas_config.calculate_gas_cost(candidate.hops);
    let mut step_outputs = Vec::new();
    let mut measured_route_key = None;
    let plan = plan_resized_execution_default_margin(
        candidate.input,
        executor_balance,
        gas_cost,
        config.min_net_profit,
        config.execution_slippage_bps,
        |amount_in| {
            let (outputs, _profit, route_key) = simulate_path_steps_with_route_key(
                &candidate.path,
                &candidate.pools,
                amount_in,
                header.block_timestamp,
            )?;
            let output = outputs.last().copied().unwrap_or(amount_in);
            step_outputs = outputs;
            measured_route_key = Some(route_key);
            Ok::<U256, eyre::Report>(output)
        },
    )
    .map_err(|e| eyre!("Principal protection aborted send: {e}"))?;

    info!(
        target: "moe.exec",
        signature = %candidate.signature,
        amount_in = %plan.amount_in,
        min_profit = %plan.min_profit,
        "Routing candidate through the wallet-free pipeline head (WHI-553)"
    );

    let route_key =
        measured_route_key.ok_or_else(|| eyre!("final simulation did not measure route key"))?;
    let min_amount_out = intent_service_support::min_amount_out_from_plan(
        plan.amount_in,
        plan.simulated_output,
        execution.config(),
    );
    let inputs = intent_service_support::execution_params_inputs_from_pools(
        &candidate.pools,
        candidate.token_path.clone(),
        step_outputs,
        min_amount_out,
        plan.net_profit,
    )?;

    let preflight = match shadow_ctx {
        Some(ctx) => {
            let shadow_inputs = intent_service_support::shadow_override_inputs_from_pools(
                &candidate.pools,
                config.executor_address,
                signer_address,
            );
            let shadow_preflight = ctx
                .build_preflight(&shadow_inputs)
                .map_err(|e| eyre!("failed to build shadow preflight: {e}"))?;
            intent_service_support::ServicePreflight::Shadow(shadow_preflight)
        }
        None => intent_service_support::ServicePreflight::Production(
            intent_service_support::production_preflight(provider.clone()),
        ),
    };
    intent_service_support::run_candidate_through_pipeline_head(
        signer_address,
        execution,
        snapshot_status,
        header,
        pool_universe_fingerprint,
        route_key,
        plan.amount_in,
        inputs,
        execution.config().execution_deadline_secs,
        base_fee_per_gas,
        block_gas_limit,
        &preflight,
    )
    .await
    .map_err(|err| eyre!("Pipeline head exercise failed: {err}"))?;

    if !intent_service_support::production_send_allowed() {
        warn!(
            target: "moe.exec",
            signature = %candidate.signature,
            amount_in = %plan.amount_in,
            min_profit = %plan.min_profit,
            "production send disabled until the execution gate is approved (WHI-526); pipeline head exercised"
        );
        return Ok(ExecutionAttempt::ProductionGateBlocked {
            amount_in: plan.amount_in,
            min_profit: plan.min_profit,
        });
    }

    Err(eyre!("production send path not enabled"))
}

fn moe_production_send_allowed() -> bool {
    intent_service_support::production_send_allowed()
}

async fn refresh_moe_states<P: Provider + Clone>(
    provider: &P,
    pool_addresses: &[Address],
) -> Result<Vec<U256>> {
    use amms::execution::IMoeLBPair;

    let mut states = Vec::with_capacity(pool_addresses.len() * 2);

    for pool_addr in pool_addresses {
        let pool = IMoeLBPair::new(*pool_addr, provider);
        let active_id = pool.getActiveId().call().await?;
        let bin_step = pool.getBinStep().call().await?;

        states.push(U256::from(active_id));
        states.push(U256::from(bin_step));
    }

    Ok(states)
}

// ============================================
// 模拟与优化
// ============================================

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
    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        current = amm.simulate_swap(hop.token_in, hop.token_out, current)?;
        outputs.push(current);
    }
    Ok((outputs, I256::from_raw(current) - I256::from_raw(amount_in)))
}

/// Measured-crossing-evidence simulation that also captures each hop's output (needed for
/// `ExecutionParamsInputs::step_amounts_out`, WHI-553), mirroring
/// `legacy_service_support::agni_path_steps_with_route_key`'s shape for the Agni/V3
/// services.
fn simulate_path_steps_with_route_key(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
    timestamp: u64,
) -> Result<(Vec<U256>, I256, RouteKey)> {
    let mut current = amount_in;
    let mut outputs = Vec::with_capacity(path.hops.len());
    let mut crossings = 0u32;
    for (hop, amm) in path.hops.iter().zip(pools.iter()) {
        let AMM::MoeLbPair(pool) = amm else {
            return Err(eyre!("Moe route contains a non-Moe pool"));
        };
        let swap_for_y = hop.token_in == pool.token_x.address;
        let evidence = pool.simulate_swap_with_crossing_evidence(swap_for_y, current, timestamp)?;
        crossings = crossings.saturating_add(evidence.crossing_count);
        current = evidence.amount_out;
        outputs.push(current);
    }
    let route_key = RouteKey::new(vec![ProtocolKind::Moe; path.hops.len()])?
        .with_moe_bins(BinCrossingBucket::from_crossings(crossings));
    Ok((
        outputs,
        I256::from_raw(current) - I256::from_raw(amount_in),
        route_key,
    ))
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

// ============================================
// 日志系统
// ============================================

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

fn log_pool_state(path: &Path, block_number: u64, event: &str, pool: &MoeLbPair) -> Result<()> {
    let mut writer = WriterBuilder::new()
        .has_headers(false)
        .from_writer(OpenOptions::new().create(true).append(true).open(path)?);

    writer.write_record([
        block_number.to_string(),
        format!("{:#x}", pool.address()),
        event.to_owned(),
        pool.active_id.to_string(),
        pool.reserve_x.to_string(),
        pool.reserve_y.to_string(),
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

    // 记录最佳路径 - 只记录净利润 >= 0.1 WMNT 的路径
    const MIN_BEST_PATH_NET_PROFIT: u128 = 100_000_000_000_000_000; // 0.1 WMNT
    if let Some(best) = unique_candidates.first() {
        if best.net_profit >= U256::from(MIN_BEST_PATH_NET_PROFIT) {
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
            record.push_field(&format_mnt(best.input));
            record.push_field(&format_mnt(best.output));
            record.push_field(&format_mnt_i256(best.profit));
            record.push_field(&format_mnt(best.net_profit));
            record.push_field(&best.roi);
            record.push_field(&best.log_hops);

            best_writer.write_record(&record)?;
            best_writer.flush()?;
        }
    }

    // 过滤需要记录的路径
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
        record.push_field(&format_mnt(candidate.input));
        record.push_field(&format_mnt(candidate.output));
        record.push_field(&format_mnt_i256(candidate.profit));
        record.push_field(&format_mnt(candidate.net_profit));
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

// ============================================
// 辅助工具函数
// ============================================

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
    Some(format!("{ratio:.2}"))
}

/// 将 wei 转换为 MNT (除以 1e18)
fn format_mnt(wei: U256) -> String {
    let wei_str = wei.to_string();
    let wei_f64 = wei_str.parse::<f64>().unwrap_or(0.0);
    let mnt = wei_f64 / 1e18;
    format!("{:.6}", mnt)
}

/// 将 I256 wei 转换为 MNT (除以 1e18)
fn format_mnt_i256(wei: I256) -> String {
    let wei_str = wei.to_string();
    let wei_f64 = wei_str.parse::<f64>().unwrap_or(0.0);
    let mnt = wei_f64 / 1e18;
    format!("{:.6}", mnt)
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

fn init_tracing() {
    if tracing_subscriber::fmt::try_init().is_err() {
        let level = std::env::var("RUST_LOG")
            .ok()
            .and_then(|s| s.parse().ok())
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
}

/// 验证池子的 bins 覆盖范围是否充足
///
/// 注意：Moe LB 的流动性分布通常是不对称的，这是正常现象
/// 我们主要关注：
/// 1. 总 bins 数量（过滤极低流动性池子）
/// 2. 至少一侧有足够覆盖（用于 swap 模拟）
fn verify_bins_coverage(pool: &MoeLbPair, min_bins: u32) -> (bool, String) {
    let active_id = pool.active_id;
    let bin_count = pool.bins.len() as u32;

    // 严重不足：bins 数量太少，这种池子应该被过滤
    if bin_count < min_bins / 2 {
        return (
            false,
            format!(
                "Pool {} has only {} bins (expected >= {})",
                pool.address,
                bin_count,
                min_bins / 2
            ),
        );
    }

    // 检查 bins 的分布
    let bin_ids: Vec<u32> = pool.bins.keys().copied().collect();
    if bin_ids.is_empty() {
        return (false, format!("Pool {} has no bins data", pool.address));
    }

    let min_bin = bin_ids.iter().min().copied().unwrap_or(active_id);
    let max_bin = bin_ids.iter().max().copied().unwrap_or(active_id);

    let lower_range = active_id.saturating_sub(min_bin);
    let upper_range = max_bin.saturating_sub(active_id);

    // 宽松的检查：只要至少一侧有足够覆盖就可以
    // 因为流动性分布通常是不对称的
    let min_acceptable_range = BINS_RADIUS / 3; // 至少 1/3 的预期范围

    if lower_range < min_acceptable_range && upper_range < min_acceptable_range {
        return (
            false,
            format!(
            "Pool {} bins coverage too narrow: active_id={}, range=[{}, {}] (lower={}, upper={})",
            pool.address, active_id, min_bin, max_bin, lower_range, upper_range
        ),
        );
    }

    // 信息性警告：分布不均匀但不影响使用
    if lower_range < BINS_RADIUS / 2 || upper_range < BINS_RADIUS / 2 {
        return (true, format!(
            "Pool {} bins coverage asymmetric (OK): {} bins, range=[{}, {}] (lower={}, upper={}) around active_id={}",
            pool.address, bin_count, min_bin, max_bin, lower_range, upper_range, active_id
        ));
    }

    (
        true,
        format!(
            "Pool {} bins coverage good: {} bins, range=[{}, {}] around active_id={}",
            pool.address, bin_count, min_bin, max_bin, active_id
        ),
    )
}
