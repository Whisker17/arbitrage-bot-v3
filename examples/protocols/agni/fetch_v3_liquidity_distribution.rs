/// Fetch full-range liquidity distribution for specific Agni (Uniswap V3) pools
///
/// This script:
/// - Fetches TVL Top5 and Volume Top5 Agni pools
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
use csv::Writer;
use eyre::{Context, Result};
use std::fs::File;
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
const CMETH: Address = address!("E6829d9a7eE3040e1276Fa75293Bde931859e8fA");
const FBTC: Address = address!("C96dE26018A54D51c097160568752c4E3BD6C364");
const SUSDE: Address = address!("211Cc4DD073734dA055fbF44a2b4667d5E5fE5d2");

/// Pool configuration
#[derive(Debug, Clone)]
struct PoolConfig {
    name: String,
    address: Address,
    category: String, // "TVL Top5" or "Volume Top5"
}

/// Get the list of pools to analyze
fn get_target_pools() -> Vec<PoolConfig> {
    vec![
        // TVL Top5
        PoolConfig {
            name: "WETH-cmETH".to_string(),
            address: address!("0d9e39d357337edde4a9bc12178da40256e2f533"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "FBTC-cmETH".to_string(),
            address: address!("ea2a184da675f9eff9e5af3cf269fb4946082241"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "USDE-cmETH".to_string(),
            address: address!("95d39c45668d59141dc5bcc940e6c191f1ebb98c"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "USDE-WMNT".to_string(),
            address: address!("eafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "sUSDE-USDE".to_string(),
            address: address!("07277f7c1567b5324aa50a3d2f1f003e2287fbfc"),
            category: "TVL Top5".to_string(),
        },
        // Volume Top5
        PoolConfig {
            name: "USDE-WMNT (Volume)".to_string(),
            address: address!("eafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "USDE-cmETH (Volume)".to_string(),
            address: address!("95d39c45668d59141dc5bcc940e6c191f1ebb98c"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "USDC-USDE".to_string(),
            address: address!("bcf99c834e65e8a58090e20edc058279317865bd"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "USDT-USDE".to_string(),
            address: address!("36a7aff497eef6a9cd7d0e7bc243793fcb3e57e2"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "WETH-cmETH (Volume)".to_string(),
            address: address!("0d9e39d357337edde4a9bc12178da40256e2f533"),
            category: "Volume Top5".to_string(),
        },
    ]
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
        CMETH => "CMETH",
        FBTC => "FBTC",
        SUSDE => "SUSDE",
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

/// Calculate token amounts for a specific tick range
/// Returns (token0_amount, token1_amount) in human-readable format
fn calculate_tick_range_amounts(
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
    current_tick: i32,
    current_sqrt_price: U256,
    decimals_0: u8,
    decimals_1: u8,
) -> (f64, f64) {
    if liquidity == 0 {
        return (0.0, 0.0);
    }
    
    // Check if ticks are within valid range
    const MIN_TICK: i32 = -887272;
    const MAX_TICK: i32 = 887272;
    
    if tick_lower < MIN_TICK || tick_lower > MAX_TICK || tick_upper < MIN_TICK || tick_upper > MAX_TICK {
        return (0.0, 0.0);
    }
    
    let liquidity_f64 = liquidity as f64;
    
    // Get sqrt prices for the range
    let sqrt_price_lower = match tick_math::get_sqrt_ratio_at_tick(tick_lower) {
        Ok(price) => price,
        Err(_) => return (0.0, 0.0),
    };
    let sqrt_price_upper = match tick_math::get_sqrt_ratio_at_tick(tick_upper) {
        Ok(price) => price,
        Err(_) => return (0.0, 0.0),
    };
    
    let q96 = (1u128 << 96) as f64;
    let sa = sqrt_price_lower.to_string().parse::<f64>().unwrap_or(0.0) / q96;
    let sb = sqrt_price_upper.to_string().parse::<f64>().unwrap_or(0.0) / q96;
    let sp = current_sqrt_price.to_string().parse::<f64>().unwrap_or(0.0) / q96;
    
    let (amount0, amount1) = if current_tick < tick_lower {
        // Current price is below the range - only token0
        let amount0 = liquidity_f64 * (1.0 / sa - 1.0 / sb);
        (amount0, 0.0)
    } else if current_tick >= tick_upper {
        // Current price is above the range - only token1
        let amount1 = liquidity_f64 * (sb - sa);
        (0.0, amount1)
    } else {
        // Current price is within the range - both tokens
        let amount0 = liquidity_f64 * (1.0 / sp - 1.0 / sb);
        let amount1 = liquidity_f64 * (sp - sa);
        (amount0, amount1)
    };
    
    // Convert to human-readable amounts
    let token0_readable = amount0 / 10f64.powi(decimals_0 as i32);
    let token1_readable = amount1 / 10f64.powi(decimals_1 as i32);
    
    (token0_readable, token1_readable)
}

/// Calculate total pool reserves
fn calculate_pool_reserves(
    pool: &AgniPool,
) -> (f64, f64) {
    let current_tick = pool.tick;
    let current_sqrt_price = pool.sqrt_price;
    
    let mut token0_total = 0.0;
    let mut token1_total = 0.0;
    
    // Sort ticks
    let mut sorted_ticks: Vec<(i32, &amms::amms::agni::Info)> = pool.ticks.iter()
        .map(|(tick, info)| (*tick, info))
        .collect();
    sorted_ticks.sort_by_key(|(tick, _)| *tick);
    
    // Track active liquidity
    let mut active_liquidity: i128 = 0;
    
    for (tick, tick_info) in sorted_ticks.iter() {
        // Update active liquidity when crossing this tick
        if *tick <= current_tick {
            active_liquidity += tick_info.liquidity_net;
        }
        
        // For positions that include the current tick, calculate token amounts
        if active_liquidity > 0 {
            let liquidity = active_liquidity as u128;
            let tick_spacing = pool.tick_spacing;
            let next_tick = tick + tick_spacing;
            
            // Check if next_tick is within valid range
            const MAX_TICK: i32 = 887272;
            if next_tick > MAX_TICK {
                continue;
            }
            
            let (amount0, amount1) = calculate_tick_range_amounts(
                *tick,
                next_tick,
                liquidity,
                current_tick,
                current_sqrt_price,
                pool.token_a.decimals,
                pool.token_b.decimals,
            );
            
            token0_total += amount0;
            token1_total += amount1;
        }
    }
    
    (token0_total, token1_total)
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

    info!("🚀 Starting Agni V3 Liquidity Distribution Fetcher");

    // Get target pools (TVL Top5 + Volume Top5)
    let pool_configs = get_target_pools();
    info!("📊 Analyzing {} Agni pools:", pool_configs.len());
    info!("   - TVL Top5: 5 pools");
    info!("   - Volume Top5: 5 pools");

    // Connect to Mantle RPC
    let rpc_url = get_mantle_rpc();
    info!("🔌 Connecting to Mantle RPC: {}", rpc_url);

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);

    info!("🔄 Initializing {} pools (rate limited)...", pool_configs.len());

    // Initialize each pool individually using init_basic
    let block_id = BlockId::Number(block_number.into());
    let mut pools = Vec::new();
    
    for (idx, config) in pool_configs.iter().enumerate() {
        info!("   Initializing pool {}/{}: {} - {} ({})", 
            idx + 1, pool_configs.len(), config.category, config.name, config.address);
        
        // Retry logic for initialization
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        loop {
            match AgniPool::new(config.address).init_basic(block_id, provider.clone()).await {
                Ok(pool) => {
                    info!("      ✓ Liquidity: {}, Ticks: {}", pool.liquidity, pool.ticks.len());
                    pools.push((pool, config));
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
        if idx < pool_configs.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
        }
    }

    info!("✅ Successfully initialized {} pools", pools.len());
    
    // Now sync tick data for all pools in batches to avoid RPC rate limits
    // Using 500ms delay (2 req/s) to avoid hitting rate limits
    info!("🔄 Syncing tick data for all pools (rate limited to 2 req/s)...");
    let total_pools = pools.len();
    let mut synced_pools = Vec::new();
    
    for (idx, (pool, config)) in pools.into_iter().enumerate() {
        info!("   Syncing pool {}/{}: {} - {}", idx + 1, total_pools, config.category, config.name);
        
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
        synced_pools.push((synced_pool_opt.unwrap_or(pool), config));
        
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
        "category",
        "tick_lower",
        "tick_upper",
        "token0_symbol",
        "token0_amount",
        "token1_symbol",
        "token1_amount",
        "liquidity_gross",
        "liquidity_net",
        "price_lower",
        "price_upper",
        "distance_from_current",
        "is_current_range"
    ])?;
    
    let mut total_ticks_count = 0;

    for (idx, (pool, config)) in pools.iter().enumerate() {
        info!(
            "📊 Processing pool {}/{}: {} - {} ({})",
            idx + 1,
            pools.len(),
            config.category,
            config.name,
            pool.address
        );
            
            let token_a_symbol = get_token_symbol(pool.token_a.address);
            let token_b_symbol = get_token_symbol(pool.token_b.address);
            
            // Get all initialized ticks
            let mut ticks: Vec<i32> = pool.ticks.keys().copied().collect();
            ticks.sort();
            
            info!("   ✅ Found {} initialized ticks", ticks.len());
            
            // Calculate total liquidity and reserves
            let total_liquidity = pool.liquidity;
            let (token0_reserve, token1_reserve) = calculate_pool_reserves(pool);
            
            // Calculate current price
            let current_price = calculate_price_from_sqrt(
                pool.sqrt_price,
                pool.token_a.decimals,
                pool.token_b.decimals
            );
            
            // Display summary
            println!("\n{}", "=".repeat(80));
            println!("🔷 Pool: {} - {} ({})", config.category, config.name, pool.address);
            println!("{}", "=".repeat(80));
            println!("Current Tick: {}", pool.tick);
            println!("Tick Spacing: {}", pool.tick_spacing);
            println!("Fee: {} bps ({}%)", pool.fee, pool.fee as f64 / 10000.0);
            println!("Total Liquidity (L): {}", total_liquidity);
            println!("Current Price: {:.8} {}/{}", current_price, token_b_symbol, token_a_symbol);
            println!("Total Initialized Ticks: {}", ticks.len());
            println!("{}", "-".repeat(80));
            println!("💰 Actual Token Reserves:");
            println!("   {} Reserve: {:.6} {}", token_a_symbol, token0_reserve, token_a_symbol);
            println!("   {} Reserve: {:.6} {}", token_b_symbol, token1_reserve, token_b_symbol);
            println!("   Total Value (in {}): {:.2} {}", token_b_symbol, 
                token0_reserve * current_price + token1_reserve, token_b_symbol);
            println!("{}", "=".repeat(80));
            
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
                &config.name,
                &config.category,
                &format!("Current_Tick={}", pool.tick),
                &format!("Tick_Spacing={}", pool.tick_spacing),
                &format!("Fee={}_bps", pool.fee),
                &format!("{:.6}", token0_reserve),
                &format!("{:.6}", token1_reserve),
                &format!("{:.2}", token0_reserve * current_price + token1_reserve),
                &format!("Total_Liquidity={}", total_liquidity),
                &format!("Total_Ticks={}", ticks.len()),
                &format!("Current_Price={:.8}", current_price),
                "",
                "",
                "",
            ])?;
            
            // Write tick data records
            // Each tick represents a range from tick_lower to tick_upper (tick_lower + tick_spacing)
            for tick_lower in &ticks {
                let tick_info = pool.ticks.get(tick_lower).unwrap();
                let tick_upper = tick_lower + pool.tick_spacing;
                
                // Skip if tick_upper is out of range
                const MAX_TICK: i32 = 887272;
                if tick_upper > MAX_TICK {
                    continue;
                }
                
                let distance = tick_lower - pool.tick;
                
                // Check if current tick is within this range
                let is_current_range = pool.tick >= *tick_lower && pool.tick < tick_upper;
                
                // Calculate token amounts for this tick range
                let (token0_amount, token1_amount) = calculate_tick_range_amounts(
                    *tick_lower,
                    tick_upper,
                    tick_info.liquidity_gross,
                    pool.tick,
                    pool.sqrt_price,
                    pool.token_a.decimals,
                    pool.token_b.decimals,
                );
                
                // Get prices for the range
                let sqrt_price_lower = match tick_math::get_sqrt_ratio_at_tick(*tick_lower) {
                    Ok(price) => price,
                    Err(_) => continue,
                };
                let sqrt_price_upper = match tick_math::get_sqrt_ratio_at_tick(tick_upper) {
                    Ok(price) => price,
                    Err(_) => continue,
                };
                
                let price_lower = calculate_price_from_sqrt(
                    sqrt_price_lower,
                    pool.token_a.decimals,
                    pool.token_b.decimals
                );
                let price_upper = calculate_price_from_sqrt(
                    sqrt_price_upper,
                    pool.token_a.decimals,
                    pool.token_b.decimals
                );
                
                writer.write_record(&[
                    "TICK_RANGE",
                    &format!("{:?}", pool.address),
                    &config.name,
                    &config.category,
                    &tick_lower.to_string(),
                    &tick_upper.to_string(),
                    token_a_symbol,
                    &format!("{:.8}", token0_amount),
                    token_b_symbol,
                    &format!("{:.8}", token1_amount),
                    &tick_info.liquidity_gross.to_string(),
                    &tick_info.liquidity_net.to_string(),
                    &format!("{:.8}", price_lower),
                    &format!("{:.8}", price_upper),
                    &distance.to_string(),
                    &is_current_range.to_string(),
                ])?;
            }
            
            // Write separator (empty line)
            writer.write_record(&["", "", "", "", "", "", "", "", "", "", "", "", "", "", "", ""])?;
        
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

