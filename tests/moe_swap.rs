use alloy::{
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AMM,
    moe::{sync_active_bins_batch, sync_slot0_batch, sync_token_decimals, BinReserve, MoeLbPair},
    Token,
};
use eyre::{bail, Result};
use serde::Deserialize;
use std::{fs::File, str::FromStr};
use tracing::info;

/// CSV row as stored in `data/poolLists_moe.csv`
#[derive(Debug, Deserialize)]
struct PoolRow {
    #[serde(rename = "Pair Address")]
    pair_address: String,
}

/// Pools used for cross-checking against live Mantle RPC.
const TEST_POOLS: &[&str] = &[
    // FBTC ↔ CMETH
    "0x2612E3280ca8836F58173bF7EcC35e258Dc1b54B",
    // WMNT ↔ USDT
    "0x1606c79Be3eBD70d8D40bAc6287E23005CFbEfA2",
    // CMETH ↔ USDE
    "0x3d887cE4988fB46AEc6E0027171F65db3526e5f1",
];

/// Number of bins to pull on each side of active id for swap simulation.
const BINS_RADIUS: u32 = 10;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .try_init();
}

fn mantle_rpc_url() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

async fn init_provider() -> Result<impl Provider + Clone> {
    let url = mantle_rpc_url();
    let provider = ProviderBuilder::new().connect_http(url.parse()?);
    Ok(provider)
}

async fn build_pair(provider: impl Provider + Clone, address: &str) -> Result<MoeLbPair> {
    let addr = Address::from_str(address)?;
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
    pair: Address,
    amount_in: U256,
    swap_for_y: bool,
) -> Result<(U256, U256, U256)> {
    use amms::execution::contract::IMoeLBPair;

    let contract = IMoeLBPair::new(pair, provider);
    let amount_in_u128: u128 = amount_in
        .try_into()
        .map_err(|_| eyre::eyre!("amount too large for uint128"))?;
    let ret = contract
        .getSwapOut(amount_in_u128, swap_for_y)
        .call()
        .await?;

    Ok((
        U256::from(ret.amountInLeft),
        U256::from(ret.amountOut),
        U256::from(ret.fee),
    ))
}

fn load_pools() -> Result<Vec<PoolRow>> {
    let file = File::open("data/poolLists_moe.csv")?;
    let mut reader = csv::Reader::from_reader(file);
    let mut rows = Vec::new();
    for result in reader.deserialize() {
        let row: PoolRow = result?;
        rows.push(row);
    }
    Ok(rows)
}

fn offline_fixture_pair() -> MoeLbPair {
    let mut pair = MoeLbPair::new(address!("0x1234567890123456789012345678901234567890"));
    pair.token_x = Token {
        address: address!("0xdEAddEaDdeAddEAddeadDEadDEADDEAddead0000"),
        decimals: 18,
    };
    pair.token_y = Token {
        address: address!("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270"),
        decimals: 6,
    };
    pair.bin_step = 20;
    pair.active_id = 8_388_608;
    pair.reserve_x = 10_000_000_000;
    pair.reserve_y = 10_000_000;
    pair.protocol_share_bps = 100;
    pair.max_volatility_acc = 250_000;
    pair.time_of_last_update = 1_700_000_000;
    pair.bins.insert(
        pair.active_id,
        BinReserve {
            reserve_x: 10_000_000_000,
            reserve_y: 10_000_000,
        },
    );
    pair.bins.insert(
        pair.active_id + 1,
        BinReserve {
            reserve_x: 5_000_000_000,
            reserve_y: 5_000_000,
        },
    );
    pair
}

async fn compare_pool(provider: impl Provider + Clone, row_address: &str) -> Result<()> {
    info!(target: "moe.monitor", "Testing pool {}", row_address);
    let mut pair = build_pair(provider.clone(), row_address).await?;
    let block_timestamp = u64::from(pair.time_of_last_update);

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

/// Offline deterministic coverage for Moe swap simulation core behavior.
#[test]
fn test_moe_swap_simulation_offline_fixture() {
    let amount_in = U256::from(1_000_000_000u128);

    let mut pair_y = offline_fixture_pair();
    let timestamp = u64::from(pair_y.time_of_last_update);
    let out_y = pair_y
        .simulate_swap_precise(true, amount_in, timestamp)
        .expect("swap_for_y should succeed");
    assert!(out_y > U256::ZERO, "swap_for_y must produce output");

    let mut pair_x = offline_fixture_pair();
    let out_x = pair_x
        .simulate_swap_precise(false, amount_in, timestamp)
        .expect("swap_for_x should succeed");
    assert!(out_x > U256::ZERO, "swap_for_x must produce output");
}

/// Live Mantle RPC differential vs on-chain `getSwapOut`.
/// Requires network access; run explicitly with `--ignored`.
#[tokio::test]
#[ignore = "live Mantle RPC; run with --ignored when credentials/network available"]
async fn test_moe_swap_simulation_matches_onchain() -> Result<()> {
    init_tracing();
    let pools_csv = load_pools()?;
    assert!(
        !pools_csv.is_empty(),
        "pool list required; run build script to refresh CSV"
    );

    let provider = init_provider().await?;

    for address in TEST_POOLS {
        compare_pool(provider.clone(), address).await?;
    }

    Ok(())
}
