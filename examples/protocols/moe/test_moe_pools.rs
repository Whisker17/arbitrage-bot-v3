/// Test script for Moe LB pools on Mantle mainnet
/// 
/// This script demonstrates:
/// - Reading pool addresses from CSV
/// - Batch syncing pool state (slot0, reserves, active bin)
/// - Syncing bin data for accurate swap simulation
/// - Calculating pool prices
/// - Simulating swaps
/// 
/// Usage:
/// ```bash
/// cargo run --example test_moe_pools
/// ```

use alloy::eips::BlockId;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use amms::amms::{
    moe::{sync_active_bins_batch, sync_slot0_batch, sync_token_decimals, MoeLbPair},
    amm::{AutomatedMarketMaker, AMM},
};
use csv::ReaderBuilder;
use eyre::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::str::FromStr;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Number of bins to sync on each side of the active bin
/// Keep this relatively small to avoid "max code size exceeded" errors
const BINS_RADIUS: u32 = 10;

/// Common tokens on Mantle for testing
const WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const USDT: Address = address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");
const METH: Address = address!("cDA86A272531e8640cD7F1a92c01839911B90bb0");
const WETH: Address = address!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111");

#[derive(Debug, Deserialize)]
struct PoolRow {
    #[serde(rename = "Protocol")]
    protocol: String,
    #[serde(rename = "Pair Name")]
    pair_name: String,
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "TokenA Address")]
    #[allow(dead_code)]
    token_a: String,
    #[serde(rename = "TokenB Address")]
    #[allow(dead_code)]
    token_b: String,
    #[serde(rename = "Fee Tier")]
    #[allow(dead_code)]
    fee_tier: String,
}

fn get_mantle_rpc() -> String {
    std::env::var("MANTLE_HTTP_URL")
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

fn get_token_symbol(addr: Address) -> &'static str {
    match addr {
        WMNT => "WMNT",
        USDT => "USDT",
        METH => "METH",
        WETH => "WETH",
        _ if addr == address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9") => "USDC",
        _ if addr == address!("5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34") => "USDE",
        _ if addr == address!("00000000eFE302BEAA2b3e6e1b18d08D69a9012a") => "AUSD",
        _ if addr == address!("E6829d9a7eE3040e1276Fa75293Bde931859e8fA") => "CMETH",
        _ if addr == address!("C96dE26018A54D51c097160568752c4E3BD6C364") => "FBTC",
        _ if addr == address!("CAbAE6f6Ea1ecaB08Ad02fE02ce9A44F09aebfA2") => "WBTC",
        _ if addr == address!("4515A45337F461A11Ff0FE8aBF3c606AE5dC00c9") => "MOE",
        _ => "UNKNOWN",
    }
}

fn format_amount(amount: u128, decimals: u8) -> String {
    let divisor = 10u128.pow(decimals as u32);
    let whole = amount / divisor;
    let frac = amount % divisor;
    format!(
        "{}.{:0width$}",
        whole,
        frac,
        width = decimals as usize
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(true)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Starting Moe LB Pools Test on Mantle Mainnet");

    // Read pool addresses from CSV
    let csv_path = "data/poolLists_moe.csv";
    info!("📖 Reading pools from: {}", csv_path);

    let file = File::open(csv_path).context("Failed to open CSV file")?;
    let mut reader = ReaderBuilder::new().from_reader(file);

    let mut pool_rows = Vec::new();
    for result in reader.deserialize() {
        let record: PoolRow = result?;
        if record.protocol == "Moe" {
            pool_rows.push(record);
        }
    }

    info!("✅ Found {} Moe pools in CSV", pool_rows.len());

    // Connect to Mantle RPC
    let rpc_url = get_mantle_rpc();
    info!("🔌 Connecting to Mantle RPC: {}", rpc_url);

    let provider = ProviderBuilder::new()
        .connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);

    // Create AMM instances for all pools
    let mut amms: Vec<AMM> = pool_rows
        .iter()
        .map(|row| {
            let addr = Address::from_str(&row.pair_address)
                .expect("Invalid address in CSV");
            AMM::MoeLbPair(MoeLbPair::new(addr))
        })
        .collect();

    info!("🔄 Batch syncing slot0 data for {} pools...", amms.len());

    // Step 1: Batch sync slot0 (active_id, bin_step, reserves, etc.)
    let block_id = BlockId::Number(block_number.into());
    sync_slot0_batch(&mut amms, block_id, provider.clone())
        .await
        .context("Failed to sync slot0")?;

    info!("✅ Slot0 data synced");

    // Step 2: Sync token decimals
    info!("🔄 Syncing token decimals...");
    sync_token_decimals(&mut amms, provider.clone())
        .await
        .context("Failed to sync token decimals")?;

    info!("✅ Token decimals synced");

    // Step 3: Sync active bins for accurate swap simulation
    info!("🔄 Syncing active bins (radius: {})...", BINS_RADIUS);
    sync_active_bins_batch(&mut amms, block_id, provider.clone(), BINS_RADIUS)
        .await
        .context("Failed to sync active bins")?;

    info!("✅ Active bins synced");

    println!("\n{}", "=".repeat(120));
    println!("📊 MOE LIQUIDITY BOOK POOLS STATUS");
    println!("{}", "=".repeat(120));

    // Display detailed information for each pool
    for (idx, amm) in amms.iter().enumerate() {
        if let AMM::MoeLbPair(pool) = amm {
            let row = &pool_rows[idx];

            println!("\n🔷 Pool #{}: {}", idx + 1, row.pair_name);
            println!("   Address:      {}", pool.address);
            println!("   Bin Step:     {} ({}%)", pool.bin_step, pool.bin_step as f64 / 100.0);
            println!("   Active Bin:   {}", pool.active_id);
            println!("   Protocol Fee: {}%", pool.protocol_share_bps as f64 / 100.0);

            // Token info
            let token_x_symbol = get_token_symbol(pool.token_x.address);
            let token_y_symbol = get_token_symbol(pool.token_y.address);

            println!("\n   💰 Reserves:");
            println!(
                "      Token X ({}): {} ({} decimals)",
                token_x_symbol,
                format_amount(pool.reserve_x, pool.token_x.decimals),
                pool.token_x.decimals
            );
            println!(
                "      Token Y ({}): {} ({} decimals)",
                token_y_symbol,
                format_amount(pool.reserve_y, pool.token_y.decimals),
                pool.token_y.decimals
            );

            // Calculate and display price
            match pool.calculate_price(pool.token_x.address, pool.token_y.address) {
                Ok(price) => {
                    println!(
                        "\n   📈 Price: 1 {} = {:.6} {}",
                        token_x_symbol, price, token_y_symbol
                    );
                    println!(
                        "      Price: 1 {} = {:.6} {}",
                        token_y_symbol,
                        1.0 / price,
                        token_x_symbol
                    );
                }
                Err(e) => {
                    println!("\n   ⚠️  Price calculation failed: {}", e);
                }
            }

            // Bin information
            println!("\n   📦 Bins: {} loaded", pool.bins.len());
            if !pool.bins.is_empty() {
                let mut sorted_bins: Vec<_> = pool.bins.iter().collect();
                sorted_bins.sort_by_key(|(id, _)| *id);

                // Show first 5 bins
                println!("      First 5 bins:");
                for (bin_id, bin_reserve) in sorted_bins.iter().take(5) {
                    let distance = **bin_id as i64 - pool.active_id as i64;
                    let price = pool.get_price_from_id(**bin_id);
                    println!(
                        "        Bin {}: X={}, Y={} (distance: {:+}, price: {:.6})",
                        bin_id,
                        format_amount(bin_reserve.reserve_x, pool.token_x.decimals),
                        format_amount(bin_reserve.reserve_y, pool.token_y.decimals),
                        distance,
                        price
                    );
                }
            }

            // Test swap simulation if pool has liquidity
            if pool.reserve_x > 0 && pool.reserve_y > 0 {
                println!("\n   🔄 Swap Simulation Test:");

                // Test swap: 0.1% of reserve_x
                let test_amount_in = U256::from(pool.reserve_x / 1000);
                if !test_amount_in.is_zero() {
                    match pool.simulate_swap(
                        pool.token_x.address,
                        pool.token_y.address,
                        test_amount_in,
                    ) {
                        Ok(amount_out) => {
                            let amount_in_formatted =
                                format_amount(test_amount_in.to::<u128>(), pool.token_x.decimals);
                            let amount_out_formatted =
                                format_amount(amount_out.to::<u128>(), pool.token_y.decimals);

                            println!(
                                "      Swap {} {} → {} {}",
                                amount_in_formatted,
                                token_x_symbol,
                                amount_out_formatted,
                                token_y_symbol
                            );

                            // Calculate effective price
                            if !amount_out.is_zero() {
                                let price_impact = (amount_out.to::<u128>() as f64
                                    / 10f64.powi(pool.token_y.decimals as i32))
                                    / (test_amount_in.to::<u128>() as f64
                                        / 10f64.powi(pool.token_x.decimals as i32));
                                println!("      Effective price: {:.6}", price_impact);
                            }
                        }
                        Err(e) => {
                            warn!("      Swap simulation failed: {}", e);
                        }
                    }
                }

                // Test reverse swap
                let test_amount_in_y = U256::from(pool.reserve_y / 1000);
                if !test_amount_in_y.is_zero() {
                    match pool.simulate_swap(
                        pool.token_y.address,
                        pool.token_x.address,
                        test_amount_in_y,
                    ) {
                        Ok(amount_out) => {
                            let amount_in_formatted = format_amount(
                                test_amount_in_y.to::<u128>(),
                                pool.token_y.decimals,
                            );
                            let amount_out_formatted =
                                format_amount(amount_out.to::<u128>(), pool.token_x.decimals);

                            println!(
                                "      Swap {} {} → {} {}",
                                amount_in_formatted,
                                token_y_symbol,
                                amount_out_formatted,
                                token_x_symbol
                            );
                        }
                        Err(e) => {
                            warn!("      Reverse swap simulation failed: {}", e);
                        }
                    }
                }
            }

            // Arbitrage opportunity indicators
            println!("\n   🎯 Arbitrage Metrics:");
            println!(
                "      Liquidity Score: {:.2}",
                calculate_liquidity_score(pool)
            );
            println!(
                "      Bin Concentration: {:.2}%",
                calculate_bin_concentration(pool) * 100.0
            );

            println!("\n   {}", "-".repeat(100));
        }
    }

    // Summary statistics
    println!("\n{}", "=".repeat(120));
    println!("📊 SUMMARY STATISTICS");
    println!("{}", "=".repeat(120));

    let total_pools = amms.len();
    let mut pools_with_liquidity = 0;
    let mut total_bins = 0;
    let mut pools_by_bin_step: std::collections::HashMap<u16, usize> =
        std::collections::HashMap::new();

    for amm in &amms {
        if let AMM::MoeLbPair(pool) = amm {
            if pool.reserve_x > 0 && pool.reserve_y > 0 {
                pools_with_liquidity += 1;
            }
            total_bins += pool.bins.len();
            *pools_by_bin_step.entry(pool.bin_step).or_insert(0) += 1;
        }
    }

    println!("Total Pools:          {}", total_pools);
    println!("Pools with Liquidity: {}", pools_with_liquidity);
    println!("Average Bins/Pool:    {:.2}", total_bins as f64 / total_pools as f64);
    println!("\nPools by Bin Step:");
    let mut sorted_bin_steps: Vec<_> = pools_by_bin_step.iter().collect();
    sorted_bin_steps.sort_by_key(|(step, _)| *step);
    for (step, count) in sorted_bin_steps {
        println!("  {} bps ({}%): {} pools", step, *step as f64 / 100.0, count);
    }

    println!("\n{}", "=".repeat(120));
    info!("✅ Test completed successfully!");

    Ok(())
}

/// Calculate a simple liquidity score for a pool
fn calculate_liquidity_score(pool: &MoeLbPair) -> f64 {
    let reserve_x_normalized = pool.reserve_x as f64 / 10f64.powi(pool.token_x.decimals as i32);
    let reserve_y_normalized = pool.reserve_y as f64 / 10f64.powi(pool.token_y.decimals as i32);

    // Geometric mean of reserves (rough TVL indicator)
    (reserve_x_normalized * reserve_y_normalized).sqrt()
}

/// Calculate what percentage of liquidity is in the active bin and nearby bins
fn calculate_bin_concentration(pool: &MoeLbPair) -> f64 {
    if pool.bins.is_empty() || pool.reserve_x == 0 || pool.reserve_y == 0 {
        return 0.0;
    }

    let active_id = pool.active_id;
    let nearby_range = 5; // Look at bins within 5 of active

    let mut nearby_reserve_x = 0u128;
    let mut nearby_reserve_y = 0u128;

    for (bin_id, bin_reserve) in &pool.bins {
        if bin_id.abs_diff(active_id) <= nearby_range {
            nearby_reserve_x += bin_reserve.reserve_x;
            nearby_reserve_y += bin_reserve.reserve_y;
        }
    }

    // Average concentration across both tokens
    let concentration_x = nearby_reserve_x as f64 / pool.reserve_x as f64;
    let concentration_y = nearby_reserve_y as f64 / pool.reserve_y as f64;

    (concentration_x + concentration_y) / 2.0
}

