use alloy::{
    hex,
    primitives::Address,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};

use std::str::FromStr;

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IAgniPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function liquidity() external view returns (uint128);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IERC20 {
        function decimals() external view returns (uint8);
    }
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc = std::env::var("MANTLE_PROVIDER_URL")
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string());
    let addr_str = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5".to_string());

    let pool_address = Address::from_str(&addr_str)?;

    println!("[probe] rpc={}", rpc);
    println!("[probe] pool={:?}", pool_address);

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc.parse()?);
    let provider = ProviderBuilder::new().connect_client(client);

    // 1) Check code size
    let code = provider.get_code_at(pool_address).await?;
    println!(
        "[probe] code bytes len={} (hex len={})",
        code.len(),
        hex::encode(&code).len()
    );
    if code.is_empty() {
        println!("[probe] ERROR: address has no code. Likely not a contract or wrong network.");
        return Ok(());
    }

    // 2) Try reading simple fields
    let pool = IAgniPool::new(pool_address, provider.clone());

    match pool.token0().call().await {
        Ok(t0) => println!("[probe] token0={:?}", t0),
        Err(e) => {
            println!("[probe] token0() reverted: {:?}", e);
            println!("[probe] This likely means the address is not an Agni/UniswapV3-style pool.");
            return Ok(());
        }
    }

    match pool.token1().call().await {
        Ok(t1) => println!("[probe] token1={:?}", t1),
        Err(e) => {
            println!("[probe] token1() reverted: {:?}", e);
            return Ok(());
        }
    }

    match pool.fee().call().await {
        Ok(fee) => println!("[probe] fee={} bps", fee),
        Err(e) => println!("[probe] fee() reverted: {:?}", e),
    }

    match pool.tickSpacing().call().await {
        Ok(spacing) => println!("[probe] tickSpacing={}", spacing),
        Err(e) => println!("[probe] tickSpacing() reverted: {:?}", e),
    }

    // 3) slot0 and liquidity
    match pool.slot0().call().await {
        Ok(slot0) => {
            println!(
                "[probe] slot0: sqrtPriceX96={}, tick={}, feeProtocol={}, unlocked={}",
                slot0.sqrtPriceX96, slot0.tick, slot0.feeProtocol, slot0.unlocked
            );

            // Read token decimals
            let t0 = pool.token0().call().await?;
            let t1 = pool.token1().call().await?;
            let d0 = IERC20::new(t0, provider.clone()).decimals().call().await?;
            let d1 = IERC20::new(t1, provider.clone()).decimals().call().await?;
            println!("[probe] decimals: token0={}, token1={}", d0, d1);

            // Compute tick from sqrtPrice (convert U160 -> U256)
            let sqrt_u256: alloy::primitives::U256 = slot0.sqrtPriceX96.to();
            match uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(sqrt_u256) {
                Ok(tick_i32) => {
                    let shift = (d0 as i32) - (d1 as i32);
                    let price0_in_1 = 1.0001_f64.powi(tick_i32) * 10_f64.powi(shift);
                    let price1_in_0 = 1.0 / price0_in_1;
                    println!("[probe] price token0 in token1: {}", price0_in_1);
                    println!("[probe] price token1 in token0: {}", price1_in_0);
                    println!(
                        "[probe] price product (should≈1): {}",
                        price0_in_1 * price1_in_0
                    );
                }
                Err(e) => println!("[probe] get_tick_at_sqrt_ratio error: {:?}", e),
            }
        }
        Err(e) => println!("[probe] slot0() reverted: {:?}", e),
    }

    match pool.liquidity().call().await {
        Ok(liq) => println!("[probe] liquidity={}", liq),
        Err(e) => println!("[probe] liquidity() reverted: {:?}", e),
    }

    Ok(())
}
