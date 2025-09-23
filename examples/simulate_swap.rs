use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::transports::layers::ThrottleLayer;
use alloy::{
    primitives::address, providers::ProviderBuilder, rpc::client::ClientBuilder,
    transports::layers::RetryBackoffLayer,
};
use amms::amms::amm::AutomatedMarketMaker;
use amms::amms::uniswap_v3::UniswapV3Pool;
use std::sync::Arc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(50))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // Using Mantle USDC-WMNT pool from poolLists.csv
    let pool = UniswapV3Pool::new(address!("086F766b336DFB0f705Dc030dB01993b22D81266"))
        .init(BlockId::latest(), provider)
        .await?;

    // Note that the token out does not need to be specified when
    // simulating a swap for pools with only two tokens.
    let amount_out = pool.simulate_swap(
        pool.token_a.address,
        Address::default(),
        U256::from(1000000),
    )?;
    println!("Amount out: {:?}", amount_out);

    Ok(())
}
