use alloy::{
    primitive_types::U256,
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AutomatedMarketMaker,
    moe::{
        default_moe_pool_list_path, sync_active_bins_batch, sync_slot0_batch, sync_token_decimals,
        MoeLbPair, MoePoolList,
    },
    AMM,
};
use eyre::{bail, ContextCompat, Result};
use std::{str::FromStr, time::Duration};
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

/// Pools used for cross-checking.
const TEST_POOLS: &[(usize, &str)] = &[
    // FBTC ↔ CMETH
    (0, "0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B"),
    // WMNT ↔ USDT
    (0, "0x1606c79Be3eBD70d8D40bAc6287E23005CFbEfA2"),
    // CMETH ↔ USDE
    (0, "0x3d887cE4988fB46AEc6E0027171F65db3526e5f1"),
];

/// Number of bins to pull on each side of active id for swap simulation.
const BINS_RADIUS: u32 = 10;

fn init_tracing() {
    let _ = fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            EnvFilter::from_default_env().add_directive("moe.monitor=info".parse().unwrap()),
        )
        .try_init();
}

fn mantle_rpc_url() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

async fn init_provider() -> Result<impl Provider + Clone> {
    let url = mantle_rpc_url();
    let provider = ProviderBuilder::new()
        .timeout(Duration::from_secs(20))
        .connect_http(url.parse()?);
    Ok(provider)
}

async fn build_pair(provider: impl Provider + Clone, address: &str) -> Result<MoeLbPair> {
    let addr = alloy::primitives::Address::from_str(address)?;
    let pool = MoeLbPair::new(addr);
    let mut amms = vec![AMM::MoeLbPair(pool)];
    let block_number = provider.get_block_number().await?;
    let block_id = alloy::eips::BlockId::Number(block_number.into());

    sync_slot0_batch(&mut amms, block_id, provider.clone()).await?;
    sync_token_decimals(&mut amms, provider.clone()).await?;
    sync_active_bins_batch(&mut amms, block_id, provider.clone(), BINS_RADIUS).await?;

    match amms.into_iter().next() {
        Some(AMM::MoeLbPair(pair)) => Ok(pair),
        _ => bail!("expected MoeLbPair"),
    }
}

async fn get_swap_out(
    provider: impl Provider + Clone,
    pair: alloy::primitives::Address,
    amount_in: U256,
    swap_for_y: bool,
) -> Result<(U256, U256, U256)> {
    use amms::execution::contract::IMoeLBPair;

    let contract = IMoeLBPair::new(pair, provider);
    let amount_in_u128 = amount_in
        .try_into()
        .context("amount too large for uint128")?;
    let (amount_in_left, amount_out, fee) = contract
        .getSwapOut(amount_in_u128, swap_for_y)
        .call()
        .await?;

    Ok((
        U256::from(amount_in_left),
        U256::from(amount_out),
        U256::from(fee),
    ))
}

fn load_pools() -> Result<MoePoolList> {
    Ok(MoePoolList::load_path(default_moe_pool_list_path())?)
}

async fn compare_pool(provider: impl Provider + Clone, row_address: &str) -> Result<()> {
    info!(target: "moe.monitor", "Testing pool {}", row_address);
    let mut pair = build_pair(provider.clone(), row_address).await?;

    let amount_in = U256::from(1_000_000_000_000_000u64);
    let (onchain_unused, onchain_out, _) =
        get_swap_out(provider.clone(), pair.address, amount_in, true).await?;

    assert!(
        onchain_unused <= amount_in,
        "unexpected leftover swap amount"
    );

    let sim_out = pair.simulate_swap_precise(true, amount_in, block_timestamp)?;
    let diff = if onchain_out > sim_out {
        onchain_out - sim_out
    } else {
        sim_out - onchain_out
    };

    let tolerance = amount_in / U256::from(10u64.pow(6));
    assert!(
        diff <= tolerance,
        "swap_for_y mismatch: onchain={onchain_out:?}, sim={sim_out:?}, diff={diff:?}"
    );

    let amount_in_y = U256::from(1_000_000_000u64);
    let (onchain_unused_y, onchain_out_y, _) =
        get_swap_out(provider.clone(), pair.address, amount_in_y, false).await?;

    assert!(
        onchain_unused_y <= amount_in_y,
        "unexpected leftover swap amount"
    );

    let sim_out_y = pair.simulate_swap_precise(false, amount_in_y, block_timestamp)?;
    let diff_y = if onchain_out_y > sim_out_y {
        onchain_out_y - sim_out_y
    } else {
        sim_out_y - onchain_out_y
    };
    let tolerance_y = amount_in_y / U256::from(10u64.pow(6));
    assert!(
        diff_y <= tolerance_y,
        "swap_for_x mismatch: onchain={onchain_out_y:?}, sim={sim_out_y:?}, diff={diff_y:?}"
    );

    Ok(())
}

#[tokio::test]
async fn test_moe_swap_simulation_matches_onchain() -> Result<()> {
    init_tracing();
    let pools_csv = load_pools()?;
    assert!(
        !pools_csv.is_empty(),
        "pool list required; run: cargo run --example generate_moe_pool_list"
    );

    let provider = init_provider().await?;

    for (_, address) in TEST_POOLS.iter().cloned() {
        compare_pool(provider.clone(), address).await?;
    }

    Ok(())
}
