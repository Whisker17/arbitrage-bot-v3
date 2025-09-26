use alloy::{
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{amms::uniswap_v3::UniswapV3Factory, state_space::StateSpaceBuilder};
use dotenvy::dotenv;
use futures::StreamExt;
use std::sync::Arc;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();
    dotenv().ok();
    let rpc_endpoint = std::env::var("RPC_URL").or_else(|_| std::env::var("ETHEREUM_PROVIDER"))?;
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(500))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    // 替换为 Mantle 上的 Uniswap V3 Factory 地址与创建区块
    let factories = vec![UniswapV3Factory::new(
        address!("0d922fb1bc191f64970ac40376643808b4b74df9"), // Mantle V3 Factory
        63795918,                                             // 该 Factory 的创建区块
    )
    .into()];

    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_factories(factories)
        .sync()
        .await?;

    // 订阅新块并跟踪池变更（会输出更新的池地址）
    let mut stream = state_space_manager.subscribe().await?;
    while let Some(updated) = stream.next().await {
        match updated {
            Ok(amms) => println!("Updated AMMs: {:?}", amms),
            Err(e) => eprintln!("Subscribe error: {e:?}"),
        }
    }

    Ok(())
}
