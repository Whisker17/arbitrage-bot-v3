/// Fetch full-range liquidity distribution for Agni and FusionX (Uniswap V3) pools
///
/// This script:
/// - Reads Agni and FusionX pools from CSV
/// - Fetches complete tick data across the full range
/// - Displays human-readable amounts with token symbols and decimals
/// - Shows the price at each tick
/// - Exports all results to a CSV file
///
/// Usage:
/// ```bash
/// cargo run --example fetch_v3_liquidity_distribution
/// ```

use alloy::eips::BlockId;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use amms::amms::{
    amm::AMM,
    agni::{AgniFactory, AgniPool},
};
use csv::{ReaderBuilder, Writer};
use eyre::{Context, Result};
use serde::Deserialize;
use std::fs::File;
use std::str::FromStr;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use uniswap_v3_math::tick_math;

/// Common tokens on Mantle for symbol lookup
const WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const USDT: Address = address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");
const METH: Address = address!("cDA86A272531e8640cD7F1a92c01839911B90bb0");
const WETH: Address = address!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111");
const USDC: Address = address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9");
const USDE: Address = address!("5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34");
const USDY: Address = address!("5bE26527e817998A7206475496fDE1E68957c5A6");

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
    fee_tier: String,
}

fn get_mantle_rpc() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

fn get_token_symbol(addr: Address) -> &'static str {
    match addr {
        WMNT => "WMNT",
        USDT => "USDT",
        METH => "METH",
        WETH => "WETH",
        USDC => "USDC",
        USDE => "USDE",
        USDY => "USDY",
        _ => "UNKNOWN",
    }
}

/// Format amount with proper decimals
fn format_amount(amount: u128, decimals: u8) -> String {
    if amount == 0 {
        return "0".to_string();
    }
    
    let divisor = 10u128.pow(decimals as u32);
    let whole = amount / divisor;
    let frac = amount % divisor;
    
    // Format with appropriate precision
    if decimals <= 6 {
        format!("{}.{:0width$}", whole, frac, width = decimals as usize)
    } else {
        // For high decimal tokens, show up to 8 decimal places
        let display_decimals = 8.min(decimals as usize);
        let frac_scaled = frac / 10u128.pow((decimals as usize - display_decimals) as u32);
        format!("{}.{:0width$}", whole, frac_scaled, width = display_decimals)
    }
}

/// Calculate price from sqrt_price_x96
fn calculate_price_from_sqrt(sqrt_price_x96: U256, decimals_0: u8, decimals_1: u8) -> f64 {
    // Convert U256 to f64 by converting to string first (safe but slower)
    // This avoids overflow issues with large U256 values
    let sqrt_price_str = sqrt_price_x96.to_string();
    let sqrt_price_val: f64 = sqrt_price_str.parse().unwrap_or(0.0);
    let q96: f64 = (1u128 << 96) as f64;
    let sqrt_price = sqrt_price_val / q96;
    let price = sqrt_price * sqrt_price;
    
    // Adjust for token decimals
    let decimal_adjustment = 10f64.powi(decimals_0 as i32 - decimals_1 as i32);
    price * decimal_adjustment
}

/// Calculate liquidity at a specific tick
fn calculate_tick_liquidity(
    pool: &AgniPool,
    tick: i32,
) -> (u128, u128) {
    // Get liquidity info for this tick
    let tick_info = pool.ticks.get(&tick);
    
    if let Some(info) = tick_info {
        // For V3, liquidity is concentrated in ranges
        // We'll return the gross liquidity as an approximation
        let liquidity = info.liquidity_gross;
        
        // Calculate token amounts based on current price and tick
        if tick <= pool.tick {
            // Below current price: only token0
            (liquidity, 0)
        } else {
            // Above current price: only token1
            (0, liquidity)
        }
    } else {
        (0, 0)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Starting Agni/FusionX V3 Liquidity Distribution Fetcher");

    // Read pool addresses from CSV
    let csv_path = "data/poolLists.csv";
    info!("📖 Reading pools from: {}", csv_path);

    let file = File::open(csv_path).context("Failed to open CSV file")?;
    let mut reader = ReaderBuilder::new().from_reader(file);

    let mut pool_rows = Vec::new();
    for result in reader.deserialize() {
        let record: PoolRow = result?;
        if record.protocol == "Agni" || record.protocol == "FusionX" {
            pool_rows.push(record);
        }
    }

    info!("✅ Found {} Agni/FusionX pools in CSV", pool_rows.len());

    // Connect to Mantle RPC
    let rpc_url = get_mantle_rpc();
    info!("🔌 Connecting to Mantle RPC: {}", rpc_url);

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);

    info!("🔄 Initializing {} pools (rate limited)...", pool_rows.len());

    // Initialize each pool individually using init_basic
    let block_id = BlockId::Number(block_number.into());
    let mut pools = Vec::new();
    
    for (idx, row) in pool_rows.iter().enumerate() {
        let addr = Address::from_str(&row.pair_address).expect("Invalid address in CSV");
        
        info!("   Initializing pool {}/{}: {} ({})", idx + 1, pool_rows.len(), row.pair_name, addr);
        
        // Retry logic for initialization
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        loop {
            match AgniPool::new(addr).init_basic(block_id, provider.clone()).await {
                Ok(pool) => {
                    info!("      ✓ Liquidity: {}, Ticks: {}", pool.liquidity, pool.ticks.len());
                    pools.push((pool, row));
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        warn!("      ✗ Failed to initialize after {} retries: {}", MAX_RETRIES, e);
                        break;
                    } else {
                        warn!("      ⚠ Retry {}/{}: {}", retry_count, MAX_RETRIES, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                    }
                }
            }
        }
        
        // Add delay between initializations (500ms = 2 req/s to be safe)
        if idx < pool_rows.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    info!("✅ Successfully initialized {} pools", pools.len());
    
    // Now sync tick data for all pools in batches to avoid RPC rate limits
    // Using 500ms delay (2 req/s) to avoid hitting rate limits
    info!("🔄 Syncing tick data for all pools (rate limited to 2 req/s)...");
    let total_pools = pools.len();
    let mut synced_pools = Vec::new();
    
    for (idx, (pool, row)) in pools.into_iter().enumerate() {
        info!("   Syncing pool {}/{}: {}", idx + 1, total_pools, row.pair_name);
        
        // Retry logic for tick sync with exponential backoff
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 5;
        let mut synced_pool_opt = None;
        
        loop {
            let amms = vec![AMM::AgniPool(pool.clone())];
            
            match AgniFactory::sync_all_pools(amms, block_id, provider.clone()).await {
                Ok(mut synced_amms) => {
                    if let Some(AMM::AgniPool(synced_pool)) = synced_amms.pop() {
                        info!("      ✓ Synced {} ticks", synced_pool.ticks.len());
                        synced_pool_opt = Some(synced_pool);
                    }
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        warn!("      ✗ Failed to sync tick data after {} retries: {}", MAX_RETRIES, e);
                        break;
                    } else {
                        // Exponential backoff: 3s, 6s, 12s, 24s
                        let delay_secs = 3 * (1 << (retry_count - 1));
                        warn!("      ⚠ Retry {}/{}, waiting {}s: {}", retry_count, MAX_RETRIES, delay_secs, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(delay_secs)).await;
                    }
                }
            }
        }
        
        // Use synced pool if available, otherwise keep original
        synced_pools.push((synced_pool_opt.unwrap_or(pool), row));
        
        // Rate limiting: 500ms delay = 2 requests per second (conservative to avoid 429)
        if idx < total_pools - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }
    
    pools = synced_pools;
    info!("✅ Tick data synced for {} pools", pools.len());

    // Prepare CSV output
    let output_path = "output/v3_liquidity_distribution.csv";
    let output_file = File::create(output_path).context("Failed to create output CSV")?;
    let mut writer = Writer::from_writer(output_file);
    
    // Write header
    writer.write_record(&[
        "record_type",
        "pool_address",
        "pool_name",
        "tick_or_current_tick",
        "token0_symbol_or_fee",
        "token0_amount_or_total_ticks",
        "token1_symbol_or_spacing",
        "token1_amount_or_liquidity",
        "price_or_sqrt_price",
        "distance_from_current_or_empty",
        "is_current_tick_or_empty"
    ])?;
    
    let mut total_ticks_count = 0;

    for (idx, (pool, row)) in pools.iter().enumerate() {
        info!(
            "📊 Processing pool {}/{}: {} ({})",
            idx + 1,
            pools.len(),
            row.pair_name,
            pool.address
        );
            
            let token_a_symbol = get_token_symbol(pool.token_a.address);
            let token_b_symbol = get_token_symbol(pool.token_b.address);
            
            // Get all initialized ticks
            let mut ticks: Vec<i32> = pool.ticks.keys().copied().collect();
            ticks.sort();
            
            info!("   ✅ Found {} initialized ticks", ticks.len());
            
            // Calculate total liquidity
            let total_liquidity = pool.liquidity;
            
            // Calculate current price
            let current_price = calculate_price_from_sqrt(
                pool.sqrt_price,
                pool.token_a.decimals,
                pool.token_b.decimals
            );
            
            // Display summary
            println!("\n🔷 Pool: {} ({})", row.pair_name, pool.address);
            println!("   Protocol: {}", row.protocol);
            println!("   Current Tick: {}", pool.tick);
            println!("   Tick Spacing: {}", pool.tick_spacing);
            println!("   Fee: {} bps", pool.fee);
            println!("   Total Liquidity: {}", total_liquidity);
            println!("   Current Price: {:.8} {}/{}", current_price, token_b_symbol, token_a_symbol);
            println!("   Total Initialized Ticks: {}", ticks.len());
            
            // Show first few ticks as sample
            println!("\n   Sample ticks:");
            for tick in ticks.iter().take(5) {
                let tick_info = pool.ticks.get(tick).unwrap();
                let _distance = tick - pool.tick;
                let is_current = *tick == pool.tick;
                
                let sqrt_price = tick_math::get_sqrt_ratio_at_tick(*tick).unwrap();
                let tick_price = calculate_price_from_sqrt(
                    sqrt_price,
                    pool.token_a.decimals,
                    pool.token_b.decimals
                );
                
                let active_marker = if is_current { " ⭐ CURRENT" } else { "" };
                
                println!(
                    "      Tick {}: liquidity={}, price={:.8}{}",
                    tick,
                    tick_info.liquidity_gross,
                    tick_price,
                    active_marker
                );
            }
            
            // Write pool summary record
            writer.write_record(&[
                "POOL_SUMMARY",
                &format!("{:?}", pool.address),
                &row.pair_name,
                &format!("Current_Tick={}", pool.tick),
                &format!("Fee={}_bps", pool.fee),
                &format!("Total_Ticks={}", ticks.len()),
                &format!("Tick_Spacing={}", pool.tick_spacing),
                &format!("Total_Liquidity={}", total_liquidity),
                &format!("Current_Price={:.8}", current_price),
                "",
                "",
            ])?;
            
            // Write tick data records
            // Find the closest tick to current tick for marking
            let closest_tick = ticks.iter()
                .min_by_key(|t| ((**t) - pool.tick).abs())
                .copied();
            
            for tick in &ticks {
                let tick_info = pool.ticks.get(tick).unwrap();
                let distance = tick - pool.tick;
                // Mark as current if it's exactly the pool's current tick, 
                // or if it's the closest tick to the current tick
                let is_current = *tick == pool.tick || Some(*tick) == closest_tick;
                
                let sqrt_price = tick_math::get_sqrt_ratio_at_tick(*tick).unwrap();
                let tick_price = calculate_price_from_sqrt(
                    sqrt_price,
                    pool.token_a.decimals,
                    pool.token_b.decimals
                );
                
                writer.write_record(&[
                    "TICK_DATA",
                    &format!("{:?}", pool.address),
                    &row.pair_name,
                    &tick.to_string(),
                    token_a_symbol,
                    &tick_info.liquidity_gross.to_string(),
                    token_b_symbol,
                    &tick_info.liquidity_net.to_string(),
                    &format!("{:.8}", tick_price),
                    &distance.to_string(),
                    &is_current.to_string(),
                ])?;
            }
            
            // Write separator (empty line)
            writer.write_record(&["", "", "", "", "", "", "", "", "", "", ""])?;
        
        total_ticks_count += ticks.len();
    }
    
    writer.flush()?;
    
    info!("\n✅ Successfully exported liquidity distribution to {}", output_path);
    
    // Summary statistics
    println!("\n{}", "=".repeat(80));
    println!("📊 SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Total pools processed: {}", pools.len());
    println!("Total initialized ticks: {}", total_ticks_count);
    println!("Output file: {}", output_path);
    println!("{}", "=".repeat(80));

    Ok(())
}

