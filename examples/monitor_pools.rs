use alloy::{
    eips::BlockId,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::types::{Filter, FilterSet, Log},
    sol_types::SolEvent,
    transports::ws::WsConnect,
};
use futures::StreamExt;
use amms::amms::{
    amm::AutomatedMarketMaker,
    uniswap_v3::{IUniswapV3PoolEvents, UniswapV3Pool},
};
use csv::ReaderBuilder;
use eyre::WrapErr;
use serde::Deserialize;
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;
use tracing::{error, info};
use alloy::consensus::BlockHeader;

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
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    // WebSocket endpoint
    let ws_endpoint = std::env::var("RPC_WS_URL")?;
    let provider = ProviderBuilder::new().connect_ws(WsConnect::new(ws_endpoint)).await?;

    // Load pools from CSV
    let mut csv_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    csv_path.push("data/poolLists.csv");
    let file = File::open(&csv_path)
        .with_context(|| format!("Failed to open {}", csv_path.display()))?;
    let mut rdr = ReaderBuilder::new().has_headers(true).from_reader(file);

    // Initialize pools
    let latest_block = BlockId::from(provider.get_block_number().await?);
    let mut pools: HashMap<Address, UniswapV3Pool> = HashMap::new();

    for result in rdr.deserialize::<PoolRow>() {
        let row = result?;
        if !row.Protocol.to_lowercase().contains("uniswap v3") {
            continue;
        }
        let addr = parse_address(&row.Pair_Address)?;
        let pool = UniswapV3Pool::new(addr)
            .init(latest_block, provider.clone())
            .await
            .with_context(|| format!("init pool {} failed", addr))?;
        info!(target: "monitor", address = ?addr, "Initialized pool");
        pools.insert(addr, pool);
    }

    if pools.is_empty() {
        info!(target: "monitor", "No pools loaded. Exiting.");
        return Ok(());
    }

    // Build event filter for only these pools and only relevant events
    let mut filter = Filter::new()
        .event_signature(FilterSet::from(vec![
            IUniswapV3PoolEvents::Mint::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Burn::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Swap::SIGNATURE_HASH,
        ]));

    filter = filter.address(pools.keys().copied().collect::<Vec<_>>());

    // Subscribe to new blocks over WS and fetch logs per block
    let mut block_stream = provider.subscribe_blocks().await?.into_stream();
    info!(target: "monitor", "Subscribed to blocks over WS");

    while let Some(block) = block_stream.next().await {
        let number = block.number();
        let windowed = filter.clone().select(number);
        match provider.get_logs(&windowed).await {
            Ok(logs) => {
                apply_logs(&mut pools, &logs);
            }
            Err(e) => {
                error!(target: "monitor", error = ?e, "get_logs failed");
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

fn apply_logs(pools: &mut HashMap<Address, UniswapV3Pool>, logs: &[Log]) {
    for log in logs {
        let addr = log.address();
        if let Some(pool) = pools.get_mut(&addr) {
            if let Err(e) = pool.sync(log) {
                error!(target: "monitor", address = ?addr, error = ?e, "sync error");
            } else {
                info!(target: "monitor", address = ?addr, "applied log");
            }
        }
    }
}


