use alloy::sol_types::SolEvent;
use alloy::{
    eips::BlockId,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    rpc::types::{Filter, FilterSet},
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::agni::IAgniFactory;
use amms::execution::{IAgniPool, IMoePair, IERC20};
use eyre::WrapErr;
use std::{fs, path::Path};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    // Network selection: Mantle (default) or Mantle Sepolia
    let arg_net = std::env::args().nth(1).unwrap_or_default();
    let env_net = std::env::var("NETWORK")
        .or_else(|_| std::env::var("CHAIN"))
        .unwrap_or_default();
    let is_sepolia = matches!(
        arg_net.to_lowercase().as_str(),
        "sepolia" | "mantle-sepolia" | "testnet"
    ) || matches!(
        env_net.to_lowercase().as_str(),
        "sepolia" | "mantle-sepolia" | "testnet"
    );

    // Resolve RPC URL
    let rpc = if is_sepolia {
        std::env::var("RPC_URL")
            .or_else(|_| std::env::var("MANTLE_SEPOLIA_RPC_URL"))
            .unwrap_or_else(|_| "https://rpc.sepolia.mantle.xyz".to_string())
    } else {
        std::env::var("RPC_URL")
            .or_else(|_| std::env::var("MANTLE_RPC_URL"))
            .or_else(|_| std::env::var("MANTLE_PROVIDER_URL"))
            .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
    };

    // Agni Factory address and creation block (overridable via env)
    let (default_factory, default_creation_block) = if is_sepolia {
        ("0xA9AcD50B042A72c33d05fDcC8ad209d3aD361762", 0u64)
    } else {
        ("0x25780dc8Fc3cfBD75F33bFDAB65e969b603b2035", 110692u64)
    };

    let factory_addr: Address = std::env::var("AGNI_FACTORY_ADDRESS")
        .unwrap_or_else(|_| default_factory.to_string())
        .parse()?;

    let creation_block_env_key = if is_sepolia {
        "MANTLE_SEPOLIA_AGNI_FACTORY_DEPLOY_BLOCK"
    } else {
        "MANTLE_AGNI_FACTORY_DEPLOY_BLOCK"
    };
    let creation_block = std::env::var("AGNI_FACTORY_CREATION_BLOCK")
        .or_else(|_| std::env::var(creation_block_env_key))
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(default_creation_block);

    println!(
        "[agni:list] network={} rpc={}",
        if is_sepolia {
            "mantle-sepolia"
        } else {
            "mantle"
        },
        rpc
    );
    println!(
        "[agni:list] factory={:?} creation_block={}",
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

    // Discover pools via PoolCreated logs with safe block window (<=10,000)
    let head_num: u64 = head;
    let mut pool_addrs: Vec<Address> = Vec::new();
    let base_filter = Filter::new()
        .event_signature(FilterSet::from(vec![
            IAgniFactory::PoolCreated::SIGNATURE_HASH,
        ]))
        .address(vec![factory_addr]);
    let step: u64 = std::env::var("LOG_STEP")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0 && v <= 10_000)
        .unwrap_or(9_000);
    let mut from = creation_block.max(0);
    while from <= head_num {
        let to = from.saturating_add(step - 1).min(head_num);
        let mut f = base_filter.clone();
        f = f.from_block(from);
        f = f.to_block(to);
        let logs = provider
            .get_logs(&f)
            .await
            .with_context(|| format!("get_logs failed for range [{} - {}]", from, to))?;
        for log in logs {
            let ev = IAgniFactory::PoolCreated::decode_log(&log.inner)?;
            pool_addrs.push(ev.pool);
        }
        from = to.saturating_add(1);
    }

    println!("[agni:list] discovered {} pools", pool_addrs.len());

    // Prepare CSV writer to logs/
    let default_filename = if is_sepolia {
        "agni_pools_sepolia.csv"
    } else {
        "agni_pools.csv"
    };
    let output_path =
        std::env::var("OUTPUT_CSV").unwrap_or_else(|_| format!("logs/{}", default_filename));
    if let Some(parent) = Path::new(&output_path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let mut wtr = csv::Writer::from_path(&output_path)?;
    wtr.write_record([
        "protocol",
        "pool",
        "token0_addr",
        "token0_symbol",
        "reserve0",
        "token1_addr",
        "token1_symbol",
        "reserve1",
        "fee_bps",
    ])?;
    let mut row_count: usize = 0;

    for pool_addr in pool_addrs.iter().copied() {
        // Try Agni (v3-style)
        let agni_fee = IAgniPool::new(pool_addr, provider.clone())
            .fee()
            .call()
            .block(latest_block)
            .await;

        if let Ok(fee_u24) = agni_fee {
            // Fetch token addresses
            let t0 = IAgniPool::new(pool_addr, provider.clone())
                .token0()
                .call()
                .block(latest_block)
                .await?;
            let t1 = IAgniPool::new(pool_addr, provider.clone())
                .token1()
                .call()
                .block(latest_block)
                .await?;

            // Symbols (best-effort)
            let s0 = IERC20::new(t0, provider.clone())
                .symbol()
                .call()
                .await
                .unwrap_or_else(|_| "UNKNOWN".to_string());
            let s1 = IERC20::new(t1, provider.clone())
                .symbol()
                .call()
                .await
                .unwrap_or_else(|_| "UNKNOWN".to_string());

            // Reserves via ERC20 balanceOf(pool)
            let r0 = IERC20::new(t0, provider.clone())
                .balanceOf(pool_addr)
                .call()
                .block(latest_block)
                .await
                .unwrap_or_default();
            let r1 = IERC20::new(t1, provider.clone())
                .balanceOf(pool_addr)
                .call()
                .block(latest_block)
                .await
                .unwrap_or_default();

            wtr.write_record([
                "Agni".to_string(),
                format!("{:?}", pool_addr),
                format!("{:?}", t0),
                s0,
                r0.to_string(),
                format!("{:?}", t1),
                s1,
                r1.to_string(),
                fee_u24.to::<u32>().to_string(),
            ])?;
            row_count += 1;
            continue;
        }

        // Try Uniswap V2-style (e.g., Moe Pair)
        let pair = IMoePair::new(pool_addr, provider.clone());
        let tok0 = match pair.token0().call().block(latest_block).await {
            Ok(t) => t,
            Err(_) => {
                println!("Unknown,{:?},,,,,,,,", pool_addr);
                continue;
            }
        };
        let tok1 = pair.token1().call().block(latest_block).await?;
        let reserves = pair.getReserves().call().block(latest_block).await?;

        let s0 = IERC20::new(tok0, provider.clone())
            .symbol()
            .call()
            .await
            .unwrap_or_else(|_| "UNKNOWN".to_string());
        let s1 = IERC20::new(tok1, provider.clone())
            .symbol()
            .call()
            .await
            .unwrap_or_else(|_| "UNKNOWN".to_string());

        wtr.write_record([
            "UniswapV2".to_string(),
            format!("{:?}", pool_addr),
            format!("{:?}", tok0),
            s0,
            reserves._0.to_string(),
            format!("{:?}", tok1),
            s1,
            reserves._1.to_string(),
            String::new(),
        ])?;
        row_count += 1;
    }

    wtr.flush()?;
    println!("[agni:list] wrote {} rows to {}", row_count, output_path);

    Ok(())
}
