use alloy::eips::BlockId;
use alloy::primitives::U256;
use alloy::transports::layers::ThrottleLayer;
use alloy::{
    primitives::address, providers::ProviderBuilder, rpc::client::ClientBuilder,
    transports::layers::RetryBackoffLayer,
};
use amms::amms::{amm::AutomatedMarketMaker, uniswap_v3::UniswapV3Pool};
use std::sync::Arc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // Using Mantle USDC-WMNT pool from poolLists.csv
    let pool = UniswapV3Pool::new(address!("086F766b336DFB0f705Dc030dB01993b22D81266"))
        .init(BlockId::latest(), provider)
        .await?;

    let to_address = address!("DecafC0ffee15BadDecafC0ffee15BadDecafC0f");
    let swap_calldata = pool.swap_calldata(U256::from(10000), U256::ZERO, to_address, vec![]);

    println!("Swap calldata: {:?}", swap_calldata);

    Ok(())
}
