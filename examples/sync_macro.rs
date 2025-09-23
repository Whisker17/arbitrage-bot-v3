use std::sync::Arc;

use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::uniswap_v3::UniswapV3Factory,
    state_space::{
        filters::{
            whitelist::{PoolWhitelistFilter, TokenWhitelistFilter},
            PoolFilter,
        },
        StateSpaceBuilder,
    },
    sync,
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    let rpc_endpoint = std::env::var("ETHEREUM_PROVIDER")?;

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let factories = vec![
        // Mantle UniswapV3 Factory
        UniswapV3Factory::new(
            address!("0d922fb1bc191f64970ac40376643808b4b74df9"),
            63795918,
        )
        .into(),
    ];

    let filters: Vec<PoolFilter> = vec![
        // Mantle USDC-WMNT pool from poolLists.csv
        PoolWhitelistFilter::new(vec![address!("086F766b336DFB0f705Dc030dB01993b22D81266")]).into(),
        // Mantle USDC token from poolLists.csv
        TokenWhitelistFilter::new(vec![address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9")])
            .into(),
    ];

    let _state_space_manager = sync!(factories, filters, provider);

    Ok(())
}
