use alloy::{
    primitives::B256,
    providers::{Provider, ProviderBuilder},
    rpc::client::ClientBuilder,
    rpc::types::{Filter, FilterSet},
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let rpc_endpoint = std::env::var("MANTLE_RPC").unwrap_or_else(|_| {
        "https://rpc-moon.mantle.xyz/v1/NjdmYzA5Mjc3ZjQ1N2IwOTliZGJiMjU0".to_string()
    });

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(2, 200, 330))
        .http(rpc_endpoint.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    let latest = provider.get_block_number().await?;
    println!("latest block: {}", latest);

    // Query a very small range with a dummy topic to minimize load
    let from = latest.saturating_sub(1000);
    let filter = Filter::new()
        .from_block(from)
        .to_block(latest)
        .event_signature(FilterSet::from(vec![B256::ZERO]));

    match provider.get_logs(&filter).await {
        Ok(logs) => {
            println!(
                "eth_getLogs OK, logs_count={} (expected usually 0)",
                logs.len()
            );
        }
        Err(e) => {
            eprintln!("eth_getLogs error: {e:?}");
        }
    }

    Ok(())
}
