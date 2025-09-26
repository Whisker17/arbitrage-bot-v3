use std::{path::PathBuf, sync::Arc};

use alloy::{
    eips::BlockId,
    primitives::address,
    providers::ProviderBuilder,
    rpc::client::ClientBuilder,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::{
    amms::{
        amm::AMM,
        factory::Factory,
        uniswap_v3::{UniswapV3Factory, UniswapV3Pool},
    },
    arbitrage::{
        ArbitrageMonitor, MonitorConfig, OpportunisticScanResult, OptimizationConfig,
        PathConstraints,
    },
    state_space::StateSpaceBuilder,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    tracing_subscriber::fmt::init();

    let rpc_endpoint = std::env::var("RPC_WS_URL")
        .or_else(|_| std::env::var("MANTLE_WS_URL"))
        .unwrap_or_else(|_| "wss://mantle.publicnode.com".to_string());

    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(250))
        .layer(RetryBackoffLayer::new(5, 200, 330))
        .ws(rpc_endpoint.parse()?);

    let provider = Arc::new(ProviderBuilder::new().connect_client(client));

    let fallback_pools: Vec<AMM> = vec![
        UniswapV3Pool::new(address!("086F766b336DFB0f705Dc030dB01993b22D81266")).into(),
        UniswapV3Pool::new(address!("082a6df295d9efeedd2838d154a2bbc255fa0745")).into(),
    ];

    let factories: Vec<Factory> = vec![UniswapV3Factory::new(
        address!("0d922fb1bc191f64970ac40376643808b4b74df9"),
        63_795_918,
    )
    .into()];

    let config = MonitorConfig {
        factories: factories.clone(),
        manual_pools: fallback_pools.clone(),
        constraints: PathConstraints {
            max_length: 4,
            allow_self_cycle: false,
        },
        optimization: OptimizationConfig::default(),
        opportunity_log_path: Some(
            std::env::var("OPPORTUNITY_LOG_CSV")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("logs/arbitrage_opportunities.csv")),
        ),
    };

    let monitor: ArbitrageMonitor<_, _> = ArbitrageMonitor::new(provider.clone(), config).await?;
    let scan_result = monitor.opportunistic_scan().await?;

    display_scan(&scan_result);

    let state_space_manager = StateSpaceBuilder::new(provider.clone())
        .with_factories(factories)
        .with_amms(fallback_pools)
        .sync()
        .await?;

    let mut stream = state_space_manager.subscribe().await?;

    while let Some(update) = stream.next().await {
        match update {
            Ok(updated_pools) => {
                monitor.handle_updates(updated_pools.clone()).await?;
                let scan = monitor.opportunistic_scan().await?;
                display_scan(&scan);
            }
            Err(e) => tracing::error!(target: "arb-monitor", error = ?e, "Subscribe error"),
        }
    }

    Ok(())
}

fn display_scan(result: &OpportunisticScanResult) {
    tracing::info!(
        target: "arb-monitor",
        block = result.block_number,
        opportunities = result.opportunities.len(),
        "Opportunistic scan"
    );

    for opportunity in &result.opportunities {
        let hop_desc: Vec<_> = opportunity
            .path
            .hops
            .iter()
            .map(|hop| {
                format!(
                    "{} -> {} via {:?}",
                    hop.token_in, hop.token_out, hop.pool_address
                )
            })
            .collect();

        tracing::info!(
            target: "arb-monitor",
            profit = ?opportunity.expected_profit,
            input = ?opportunity.optimal_input,
            hops = ?hop_desc,
            "Profitable path"
        );
    }
}
