use std::sync::Arc;

use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::uniswap_v3::{UniswapV3Factory, UniswapV3Pool},
    state_space::StateSpaceBuilder,
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

    /*
       The `StateSpaceBuilder` is used to sync a state space of AMMs.

       When specifying a set of factories to sync from, the `sync()` method fetches all pool creation logs
       from the factory contracts specified and syncs all pools to the latest block. This method returns a
       `StateSpaceManager` which can be used to subscribe to state changes and interact with AMMs
       the state space.
    */
    let factories = vec![
        // Mantle UniswapV3 Factory
        UniswapV3Factory::new(
            address!("0d922fb1bc191f64970ac40376643808b4b74df9"),
            63795918,
        )
        .into(),
    ];

    let _state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_factories(factories.clone())
        .sync()
        .await?;

    // ======================================================================================== //

    /*
    You can also sync pools directly without specifying factories. This is great for when you only
    need to track a handful of specific pools.
    */
    let amms = vec![
        // Mantle USDC-WMNT pool from poolLists.csv
        UniswapV3Pool::new(address!("086F766b336DFB0f705Dc030dB01993b22D81266")).into(),
        // Mantle WMNT-WETH pool from poolLists.csv
        UniswapV3Pool::new(address!("082a6df295d9efeedd2838d154a2bbc255fa0745")).into(),
    ];

    let _state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_amms(amms)
        .sync()
        .await?;

    // ======================================================================================== //

    /*
    Additionally, you can specify specific factories to discover and sync pools from, as well as
    specify specific AMMs to sync. This can be helpful when there isnt a factory for a given AMM
    as is the case with ERC4626 vaults.
    */
    let amms: Vec<amms::amms::amm::AMM> = vec![];

    let _state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_factories(factories)
        .with_amms(amms)
        .sync()
        .await?;

    Ok(())
}
