use alloy::consensus::BlockHeader;
use alloy::{
    eips::BlockId,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, FilterSet, Log},
    sol_types::SolEvent,
    transports::ws::WsConnect,
};
use amms::amms::{
    agni::{AgniPool, IAgniPoolEvents},
    amm::{AutomatedMarketMaker, AMM},
};
use amms::arbitrage::{
    graph::build_graph,
    pathfinder::{PathConstraints, PathFinder},
    ArbitragePath,
};
use amms::state_space::StateSpace;
use csv::ReaderBuilder;
use eyre::WrapErr;
use futures::{stream, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use tracing::{error, info, warn};

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

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    // WebSocket endpoint (fallback to a public Mantle WS if not provided)
    let ws_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());
    info!(target: "monitor", ws = %ws_endpoint, "Using WebSocket endpoint");

    let provider = ProviderBuilder::new()
        .connect_ws(WsConnect::new(ws_endpoint))
        .await?;

    // Load pools from CSV
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file =
        File::open(&csv_path).with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    // Initialize pools
    let latest_block = BlockId::from(provider.get_block_number().await?);
    let mut pools: HashMap<Address, AgniPool> = HashMap::new();
    let mut fee_tiers: HashMap<Address, Option<u32>> = HashMap::new();
    let mut total_rows = 0usize;
    let mut agni_rows = 0usize;
    let mut init_jobs = Vec::new();

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
    let provider_for_init = provider.clone();
    let block_for_init = latest_block;
    let mut init_stream = stream::iter(init_jobs.into_iter().map(|(addr, fee_tier)| {
        let provider = provider_for_init.clone();
        let block = block_for_init;
        async move {
            let result = AgniPool::new(addr)
                .init_basic::<_, _>(block, provider)
                .await;
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

    // Estimate arbitrage path candidates using the in-memory state
    let arbitrage_paths = {
        let mut state = StateSpace::default();
        for pool in pools.values() {
            state
                .state
                .insert(pool.address(), AMM::AgniPool(pool.clone()));
        }

        match build_graph(&state) {
            Ok(graph) => {
                let finder = PathFinder::new(&graph, PathConstraints::default());
                let cycles = finder.find_cycles();
                let two_pool = finder.find_two_pool_misprices();

                let matches_fee = |path: &ArbitragePath| {
                    path.hops
                        .iter()
                        .all(|hop| match fee_tiers.get(&hop.pool_address) {
                            Some(Some(expected_fee)) => pools
                                .get(&hop.pool_address)
                                .map(|pool| pool.fee == *expected_fee)
                                .unwrap_or(false),
                            Some(None) | None => true,
                        })
                };

                cycles.iter().filter(|path| matches_fee(path)).count()
                    + two_pool.iter().filter(|path| matches_fee(path)).count()
            }
            Err(err) => {
                error!(target: "monitor", error = ?err, "Failed to build arbitrage graph");
                0
            }
        }
    };

    // Build event filter for only these pools and only relevant events
    let mut filter = Filter::new().event_signature(FilterSet::from(vec![
        IAgniPoolEvents::Mint::SIGNATURE_HASH,
        IAgniPoolEvents::Burn::SIGNATURE_HASH,
        IAgniPoolEvents::Swap::SIGNATURE_HASH,
    ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // Subscribe to new blocks over WS and fetch logs per block
    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target: "monitor", "Subscribed to blocks over WS");
    info!(target: "monitor", candidate_paths = arbitrage_paths, "Pre-computed arbitrage candidate paths");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        // Some RPC providers (e.g., Mantle public) may not have logs indexed for the tip yet.
        // Query the previous block to avoid "invalid block range params" errors.
        if number == 0 {
            continue;
        }
        let target_number = number - 1;
        info!(target: "monitor.block", block = target_number, "Processing block");
        let windowed = filter.clone().select(target_number);
        match provider.get_logs(&windowed).await {
            Ok(logs) => {
                info!(target: "monitor.block", block = target_number, logs = logs.len(), "Fetched logs");
                apply_logs(&mut pools, &logs);
            }
            Err(e) => {
                error!(target: "monitor", block = target_number, error = ?e, "get_logs failed");
            }
        }
    }

    Ok(())
}

fn parse_address(s: &str) -> eyre::Result<Address> {
    let s = s.trim();
    let addr = s.parse::<Address>()?;
    Ok(addr)
}

fn apply_logs(pools: &mut HashMap<Address, AgniPool>, logs: &[Log]) {
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            let before_tick = pool.tick;
            let before_liq = pool.liquidity;
            let before_sqrt = pool.sqrt_price;

            let sig = log.topics()[0];
            if sig == IAgniPoolEvents::Swap::SIGNATURE_HASH {
                match IAgniPoolEvents::Swap::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Swap)");
                            continue;
                        }
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Swap",
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            sqrt_from = ?before_sqrt,
                            sqrt_to = ?pool.sqrt_price,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Swap failed");
                    }
                }
            } else if sig == IAgniPoolEvents::Mint::SIGNATURE_HASH {
                match IAgniPoolEvents::Mint::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Mint)");
                            continue;
                        }
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Mint",
                            owner = ?e.owner,
                            tick_lower = ?e.tickLower,
                            tick_upper = ?e.tickUpper,
                            amount = e.amount,
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Mint failed");
                    }
                }
            } else if sig == IAgniPoolEvents::Burn::SIGNATURE_HASH {
                match IAgniPoolEvents::Burn::decode_log(log.as_ref()) {
                    Ok(e) => {
                        if let Err(e) = pool.sync(log) {
                            error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Burn)");
                            continue;
                        }
                        info!(
                            target: "monitor.pool",
                            address = ?addr,
                            event = "Burn",
                            owner = ?e.owner,
                            tick_lower = ?e.tickLower,
                            tick_upper = ?e.tickUpper,
                            amount = e.amount,
                            amount0 = ?e.amount0,
                            amount1 = ?e.amount1,
                            tick_from = before_tick,
                            tick_to = pool.tick,
                            liq_from = before_liq,
                            liq_to = pool.liquidity,
                            "Applied"
                        );
                    }
                    Err(e) => {
                        error!(target: "monitor.pool", address = ?addr, error = ?e, "decode Burn failed");
                    }
                }
            } else {
                // Unknown event (should not happen due to filter)
                if let Err(e) = pool.sync(log) {
                    error!(target: "monitor.pool", address = ?addr, error = ?e, "sync error (Unknown)");
                } else {
                    info!(
                        target: "monitor.pool",
                        address = ?addr,
                        event = "Unknown",
                        tick_from = before_tick,
                        tick_to = pool.tick,
                        liq_from = before_liq,
                        liq_to = pool.liquidity,
                        "Applied"
                    );
                }
            }
        }
    }
}
