use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::uniswap_v3::UniswapV3Factory,
    state_space::{
        filters::whitelist::{PoolWhitelistFilter, TokenWhitelistFilter},
        StateSpaceBuilder,
    },
};
use std::sync::Arc;

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
        amms::amms::uniswap_v3::UniswapV3Factory::new(
            address!("0d922fb1bc191f64970ac40376643808b4b74df9"),
            63795918,
        )
        .into(),
    ];

    /*  PoolFilters are applied all AMMs when syncing the state space.
       Filters have two "stages", `FilterStage::Discovery` or `FilterStage::Sync`.
       Discovery filters are applied to AMMs after the `StateSpaceManager` has processed all pool created events.
       Sync filters are applied to AMMs after the `StateSpaceManager` has processed all pool sync events.
       This allows for efficient syncing of the state space by minimizing the amount of pools that need to sync state.
       In the following example, the `PoolWhitelistFilter` is applied to the `Discovery` stage
       and the `TokenWhitelistFilter` is applied to the `Sync` stage. Rather than syncing all pools from the factory,
       only the whitelisted pools are synced. The `TokenWhitelistFilter` is applied after syncing since pool creation logs
       do not always emit the tokens included in the pool, but this data will always be populated after syncing.
    */
    let filters = vec![
        // Mantle USDC-WMNT pool from poolLists.csv
        PoolWhitelistFilter::new(vec![address!("086F766b336DFB0f705Dc030dB01993b22D81266")]).into(),
        // Mantle USDC token from poolLists.csv
        TokenWhitelistFilter::new(vec![address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9")])
            .into(),
    ];

    let _state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_factories(factories)
        .with_filters(filters)
        .sync()
        .await;

    Ok(())
}
