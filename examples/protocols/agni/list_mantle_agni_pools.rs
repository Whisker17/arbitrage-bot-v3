use alloy::{
    eips::BlockId,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    agni::AgniFactory,
    amm::{AutomatedMarketMaker, AMM},
    factory::DiscoverySync,
};
use eyre::WrapErr;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    // Mantle RPC URL
    let rpc = std::env::var("MANTLE_PROVIDER_URL")
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string());

    // Agni Factory on Mantle (provided by user)
    let factory_addr: Address = "0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035".parse()?;

    // Optional: factory creation block to speed up discovery; if unknown, start from 0
    // If you know the deployment block, set MANTLE_AGNI_FACTORY_DEPLOY_BLOCK env var
    let creation_block_env = std::env::var("MANTLE_AGNI_FACTORY_DEPLOY_BLOCK").ok();
    let creation_block = creation_block_env
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(110692);

    println!("[agni:list] rpc={}", rpc);
    println!(
        "[agni:list] factory={:?} creation_block={} ",
        factory_addr, creation_block
    );

    // Build provider with some retry/backoff
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    // Use a numeric block height to avoid BlockId::latest() issues
    let head = provider.get_block_number().await?;
    let latest_block = BlockId::from(head);

    let factory = AgniFactory::new(factory_addr, creation_block);

    // Discover pools via PoolCreated logs
    let pools_unsynced = factory
        .discover::<_, _>(latest_block, provider.clone())
        .await
        .with_context(|| "discover pools via PoolCreated logs")?;

    println!(
        "[agni:list] discovered {} pools (unsynced)",
        pools_unsynced.len()
    );

    // Optionally sync basic state (slot0, decimals, tick bitmaps & data) to filter out dust pools
    // This can be heavy; enable via env flag if desired
    let do_sync = std::env::var("SYNC_POOL_STATE")
        .map(|v| v == "1" || v.to_lowercase() == "true")
        .unwrap_or(false);

    if do_sync {
        let pools_synced = factory
            .sync::<_, _>(pools_unsynced, latest_block, provider.clone())
            .await
            .with_context(|| "sync pools state")?;
        println!("[agni:list] synced and kept {} pools", pools_synced.len());
        for amm in pools_synced.iter() {
            if let AMM::AgniPool(p) = amm {
                println!(
                    "pool={:?} token0={:?}({}) token1={:?}({}) fee={} tickSpacing={} liq={} tick={} sqrt={}",
                    p.address,
                    p.token_a.address,
                    p.token_a.decimals,
                    p.token_b.address,
                    p.token_b.decimals,
                    p.fee,
                    p.tick_spacing,
                    p.liquidity,
                    p.tick,
                    p.sqrt_price,
                );
            }
        }
    } else {
        // Print minimal info from logs (addresses, fee, tickSpacing known; tokens may be zero until synced)
        for amm in pools_unsynced.iter() {
            if let AMM::AgniPool(p) = amm {
                println!(
                    "pool={:?} token0={:?} token1={:?} fee={} tickSpacing={}",
                    p.address, p.token_a.address, p.token_b.address, p.fee, p.tick_spacing,
                );
            }
        }
    }

    Ok(())
}
