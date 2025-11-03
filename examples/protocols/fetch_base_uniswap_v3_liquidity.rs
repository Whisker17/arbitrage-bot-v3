/// Fetch liquidity distribution for a specific Uniswap V3 pool on Base network
///
/// This script:
/// - Fetches a single Uniswap V3 pool on Base using segmented tick fetching
/// - Avoids rate limits by querying ticks in small batches with delays
/// - Displays human-readable amounts with token symbols and decimals
/// - Shows the price at each tick
/// - Exports results to a CSV file
///
/// ⚠️ IMPORTANT: Public RPC has strict rate limits!
/// For best results, use your own RPC endpoint:
/// ```bash
/// export BASE_HTTP_URL="https://your-base-rpc-url"
/// cargo run --example fetch_base_uniswap_v3_liquidity
/// ```
///
/// The script uses very conservative rate limiting (3 req/sec) to work with public RPC,
/// but it will be SLOW. With a private RPC, you can increase the speed significantly.

use alloy::primitives::{address, Address, U256, Signed, Uint};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use csv::Writer;
use eyre::{Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use uniswap_v3_math::tick_math;

// Type aliases for Uniswap V3 types
type I24 = Signed<24, 1>;
type U24 = Uint<24, 1>;
type U160 = Uint<160, 3>;

// Define the Uniswap V3 Pool interface
sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function token0() external view returns (address);
        function token1() external view returns (address);
        
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
    }
}

/// Helper function to convert tick to word position
fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
    let mut compressed = tick / tick_spacing;
    if tick < 0 && tick % tick_spacing != 0 {
        compressed -= 1;
    }
    compressed >> 8
}

/// Target pool on Base network
const TARGET_POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

/// Common tokens on Base for symbol lookup
const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
const USDB_C: Address = address!("d9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA");
const DAI: Address = address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb");
const CBETH: Address = address!("2Ae3F1Ec7F1F5012CFEab0185bfc7aa3cf0DEc22");
const WSTETH: Address = address!("c1CBa3fCea344f92D9239c08C0568f6F2F0ee452");
const RETH: Address = address!("B6fe221Fe9EeF5aBa221c348bA20A1Bf5e73624c");

fn get_base_rpc() -> String {
    std::env::var("BASE_HTTP_URL").unwrap_or_else(|_| "https://mainnet.base.org".to_string())
}

fn get_token_symbol(addr: Address) -> &'static str {
    match addr {
        WETH => "WETH",
        USDC => "USDC",
        USDB_C => "USDbC",
        DAI => "DAI",
        CBETH => "cbETH",
        WSTETH => "wstETH",
        RETH => "rETH",
        _ => "UNKNOWN",
    }
}

#[derive(Debug, Clone)]
struct TickInfo {
    liquidity_gross: u128,
    liquidity_net: i128,
}

#[derive(Debug)]
struct PoolData {
    address: Address,
    sqrt_price: U160,
    tick: I24,
    liquidity: u128,
    fee: U24,
    tick_spacing: I24,
    token0: Address,
    token1: Address,
    token0_decimals: u8,
    token1_decimals: u8,
    ticks: BTreeMap<i32, TickInfo>,
}

/// Calculate price from sqrt_price_x96
fn calculate_price_from_sqrt(sqrt_price_x96: U256, decimals_0: u8, decimals_1: u8) -> f64 {
    let sqrt_price_str = sqrt_price_x96.to_string();
    let sqrt_price_val: f64 = sqrt_price_str.parse().unwrap_or(0.0);
    let q96: f64 = (1u128 << 96) as f64;
    let sqrt_price = sqrt_price_val / q96;
    let price = sqrt_price * sqrt_price;
    
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
    current_sqrt_price: U160,
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

/// Calculate actual token reserves in the pool
/// This sums up the liquidity across all active positions
fn calculate_pool_reserves(
    pool_data: &PoolData,
) -> (f64, f64) {
    let current_tick: i32 = pool_data.tick.try_into().unwrap_or(0);
    let current_sqrt_price = pool_data.sqrt_price;
    
    let mut token0_total = 0.0;
    let mut token1_total = 0.0;
    
    // Track active liquidity
    let mut active_liquidity: i128 = 0;
    
    // Sort ticks
    let mut sorted_ticks: Vec<(i32, &TickInfo)> = pool_data.ticks.iter()
        .map(|(tick, info)| (*tick, info))
        .collect();
    sorted_ticks.sort_by_key(|(tick, _)| *tick);
    
    for (tick, tick_info) in sorted_ticks.iter() {
        // Update active liquidity when crossing this tick
        if *tick <= current_tick {
            active_liquidity += tick_info.liquidity_net;
        }
        
        // For positions that include the current tick, calculate token amounts
        if active_liquidity > 0 {
            let liquidity = active_liquidity as f64;
            
            // Get sqrt prices for this tick and next tick
            let tick_sqrt_price = match tick_math::get_sqrt_ratio_at_tick(*tick) {
                Ok(price) => price,
                Err(_) => continue,
            };
            let tick_spacing: i32 = pool_data.tick_spacing.try_into().unwrap_or(10);
            let next_tick = tick + tick_spacing;
            
            // Check if next_tick is within valid range
            const MAX_TICK: i32 = 887272;
            if next_tick > MAX_TICK {
                continue;
            }
            
            let next_sqrt_price = match tick_math::get_sqrt_ratio_at_tick(next_tick) {
                Ok(price) => price,
                Err(_) => continue,
            };
            
            let sqrt_price_a = tick_sqrt_price.to_string().parse::<f64>().unwrap_or(0.0);
            let sqrt_price_b = next_sqrt_price.to_string().parse::<f64>().unwrap_or(0.0);
            let sqrt_price_current = current_sqrt_price.to_string().parse::<f64>().unwrap_or(0.0);
            
            let q96 = (1u128 << 96) as f64;
            let sa = sqrt_price_a / q96;
            let sb = sqrt_price_b / q96;
            let sp = sqrt_price_current / q96;
            
            // Calculate token amounts based on position relative to current price
            if *tick < current_tick && next_tick > current_tick {
                // Position spans current tick - has both tokens
                let amount0 = liquidity * (sb - sp) / (sp * sb);
                let amount1 = liquidity * (sp - sa);
                token0_total += amount0;
                token1_total += amount1;
            } else if next_tick <= current_tick {
                // Position is below current price - only token1
                let amount1 = liquidity * (sb - sa);
                token1_total += amount1;
            } else if *tick >= current_tick {
                // Position is above current price - only token0
                let amount0 = liquidity * (sb - sa) / (sa * sb);
                token0_total += amount0;
            }
        }
    }
    
    // Convert to human-readable amounts
    let decimals_0 = pool_data.token0_decimals;
    let decimals_1 = pool_data.token1_decimals;
    
    let token0_readable = token0_total / 10f64.powi(decimals_0 as i32);
    let token1_readable = token1_total / 10f64.powi(decimals_1 as i32);
    
    (token0_readable, token1_readable)
}

/// Fetch tick bitmap for a range of word positions
async fn fetch_tick_bitmaps<P: Provider + Clone>(
    pool_address: Address,
    tick_spacing: i32,
    provider: P,
) -> Result<Vec<i32>> {
    let pool = IUniswapV3Pool::new(pool_address, provider.clone());
    
    // Calculate word range for full tick range (-887272 to 887272)
    let min_word = tick_to_word(-887272, tick_spacing);
    let max_word = tick_to_word(887272, tick_spacing);
    
    info!("📊 Fetching tick bitmaps from word {} to {} ({} words)", 
        min_word, max_word, max_word - min_word + 1);
    
    let mut initialized_ticks = Vec::new();
    let mut words_with_liquidity = 0;
    
    // Fetch bitmaps in smaller batches with longer delays
    const BITMAP_BATCH_SIZE: i32 = 20; // Smaller batches
    let mut current_word = min_word;
    let mut batch_count = 0;
    
    while current_word <= max_word {
        batch_count += 1;
        let batch_end = (current_word + BITMAP_BATCH_SIZE).min(max_word);
        
        info!("   📦 Bitmap batch {}: words {} to {}", batch_count, current_word, batch_end);
        
        for word_pos in current_word..=batch_end {
            match pool.tickBitmap(word_pos as i16).call().await {
                Ok(bitmap) => {
                    if bitmap != U256::ZERO {
                        words_with_liquidity += 1;
                        // Extract tick indices from bitmap
                        for bit_pos in 0..256 {
                            if (bitmap & (U256::from(1) << bit_pos)) != U256::ZERO {
                                let tick_index = (word_pos * 256 + bit_pos) * tick_spacing;
                                if tick_index >= -887272 && tick_index <= 887272 {
                                    initialized_ticks.push(tick_index);
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch bitmap for word {}: {}", word_pos, e);
                    // Wait longer on error
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
            
            // Delay between each bitmap call (300ms = ~3 req/sec)
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        }
        
        current_word = batch_end + 1;
        
        // Longer delay between batches
        if current_word <= max_word {
            info!("      ✓ Found {} ticks so far, waiting 2s before next batch...", initialized_ticks.len());
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }
    
    info!("✅ Found {} words with liquidity, {} initialized ticks", 
        words_with_liquidity, initialized_ticks.len());
    
    Ok(initialized_ticks)
}

/// Fetch tick data for specific ticks
async fn fetch_ticks_data<P: Provider + Clone>(
    pool_address: Address,
    ticks: &[i32],
    provider: P,
) -> Result<Vec<(i32, TickInfo)>> {
    let pool = IUniswapV3Pool::new(pool_address, provider);
    
    let mut tick_data = Vec::new();
    
    for (idx, tick) in ticks.iter().enumerate() {
        let tick_i24 = I24::try_from(*tick).unwrap_or(I24::ZERO);
        
        match pool.ticks(tick_i24).call().await {
            Ok(data) => {
                if data.initialized {
                    tick_data.push((
                        *tick,
                        TickInfo {
                            liquidity_gross: data.liquidityGross,
                            liquidity_net: data.liquidityNet,
                        },
                    ));
                }
            }
            Err(e) => {
                warn!("Failed to fetch tick {}: {}", tick, e);
                // Wait longer on error
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        }
        
        // Delay between each tick call (300ms = ~3 req/sec)
        if idx < ticks.len() - 1 {
            tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
        }
    }
    
    Ok(tick_data)
}

/// Get token decimals (fallback to common values)
async fn get_token_decimals<P: Provider + Clone>(
    token_address: Address,
    _provider: P,
) -> u8 {
    // For Base network, use known decimals
    match token_address {
        WETH => 18,
        USDC => 6,
        USDB_C => 6,
        DAI => 18,
        CBETH => 18,
        WSTETH => 18,
        RETH => 18,
        _ => 18, // Default to 18 decimals
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

    info!("🚀 Starting Base Uniswap V3 Liquidity Distribution Fetcher (Segmented)");
    info!("📊 Target Pool: {:?}", TARGET_POOL);

    // Connect to Base RPC
    let rpc_url = get_base_rpc();
    info!("🔌 Connecting to Base RPC: {}", rpc_url);
    
    if rpc_url.contains("mainnet.base.org") {
        warn!("⚠️  Using public RPC - this will be SLOW due to rate limits!");
        warn!("⚠️  Consider using your own RPC: export BASE_HTTP_URL=\"https://your-rpc-url\"");
    }

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);
    
    // Initial delay to avoid hitting rate limit immediately
    info!("⏳ Waiting 2 seconds before starting...");
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    info!("🔄 Fetching pool metadata...");
    
    let pool_contract = IUniswapV3Pool::new(TARGET_POOL, provider.clone());
    
    // Get basic pool info with delays between calls to avoid rate limits
    let slot0 = pool_contract.slot0().call().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    let liquidity = pool_contract.liquidity().call().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    let fee = pool_contract.fee().call().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    let tick_spacing = pool_contract.tickSpacing().call().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    let token0 = pool_contract.token0().call().await?;
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    
    let token1 = pool_contract.token1().call().await?;
    
    let current_tick_i32: i32 = slot0.tick.try_into().unwrap_or(0);
    let tick_spacing_i32: i32 = tick_spacing.try_into().unwrap_or(1);
    let fee_u32: u32 = fee.try_into().unwrap_or(0);
    
    info!("   ✓ Pool metadata fetched");
    info!("      Token0: {:?} ({})", token0, get_token_symbol(token0));
    info!("      Token1: {:?} ({})", token1, get_token_symbol(token1));
    info!("      Current tick: {}", current_tick_i32);
    info!("      Liquidity: {}", liquidity);
    info!("      Fee: {} bps", fee_u32);
    info!("      Tick Spacing: {}", tick_spacing_i32);
    
    let token0_decimals = get_token_decimals(token0, provider.clone()).await;
    let token1_decimals = get_token_decimals(token1, provider.clone()).await;
    
    let mut pool_data = PoolData {
        address: TARGET_POOL,
        sqrt_price: slot0.sqrtPriceX96,
        tick: slot0.tick,
        liquidity,
        fee,
        tick_spacing,
        token0,
        token1,
        token0_decimals,
        token1_decimals,
        ticks: BTreeMap::new(),
    };
    
    // Step 1: Fetch tick bitmaps to find all initialized ticks
    info!("🔍 Step 1: Fetching tick bitmaps to identify initialized ticks...");
    let initialized_ticks = fetch_tick_bitmaps(TARGET_POOL, tick_spacing_i32, provider.clone()).await?;
    
    if initialized_ticks.is_empty() {
        warn!("⚠️  No initialized ticks found!");
        return Ok(());
    }
    
    info!("✅ Found {} initialized ticks across full range", initialized_ticks.len());
    
    // Step 2: Fetch tick data in batches
    info!("🔍 Step 2: Fetching tick data in batches...");
    const TICK_BATCH_SIZE: usize = 20; // Smaller batches: 20 ticks at a time
    let total_batches = (initialized_ticks.len() + TICK_BATCH_SIZE - 1) / TICK_BATCH_SIZE;
    
    for (batch_idx, tick_batch) in initialized_ticks.chunks(TICK_BATCH_SIZE).enumerate() {
        info!("   📦 Batch {}/{}: fetching {} ticks", 
            batch_idx + 1, total_batches, tick_batch.len());
        
        // Retry logic for this batch
        let mut retry_count = 0;
        const MAX_RETRIES: u32 = 3;
        
        loop {
            match fetch_ticks_data(TARGET_POOL, tick_batch, provider.clone()).await {
                Ok(tick_data) => {
                    for (tick, info) in tick_data {
                        pool_data.ticks.insert(tick, info);
                    }
                    info!("      ✓ Fetched {} ticks (total: {})", tick_batch.len(), pool_data.ticks.len());
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count >= MAX_RETRIES {
                        warn!("      ✗ Batch failed after {} retries: {}", MAX_RETRIES, e);
                        break;
                    } else {
                        warn!("      ⚠ Retry {}/{}: {}", retry_count, MAX_RETRIES, e);
                        tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                    }
                }
            }
        }
        
        // Longer delay between batches (3 seconds to avoid rate limits)
        if batch_idx < total_batches - 1 {
            info!("      Waiting 3s before next batch...");
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
        }
    }
    
    info!("✅ Total ticks with data: {}", pool_data.ticks.len());
    
    // Calculate actual token reserves
    info!("🔢 Calculating actual token reserves...");
    let (token0_reserve, token1_reserve) = calculate_pool_reserves(&pool_data);
    info!("   ✓ Token0 ({}) reserve: {:.6}", get_token_symbol(pool_data.token0), token0_reserve);
    info!("   ✓ Token1 ({}) reserve: {:.6}", get_token_symbol(pool_data.token1), token1_reserve);
    
    // Prepare CSV output
    let output_path = "output/base_uniswap_v3_liquidity.csv";
    info!("📝 Writing results to {}...", output_path);
    
    let output_file = File::create(output_path).context("Failed to create output CSV")?;
    let mut writer = Writer::from_writer(output_file);
    
    // Write header
    writer.write_record(&[
        "record_type",
        "pool_address",
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
    
    let token_a_symbol = get_token_symbol(pool_data.token0);
    let token_b_symbol = get_token_symbol(pool_data.token1);
    
    // Get all initialized ticks
    let ticks: Vec<i32> = pool_data.ticks.keys().copied().collect();
    
    let sqrt_price_u256 = U256::from(pool_data.sqrt_price);
    let current_price = calculate_price_from_sqrt(
        sqrt_price_u256,
        pool_data.token0_decimals,
        pool_data.token1_decimals
    );
    
    // Display summary
    println!("\n{}", "=".repeat(80));
    println!("🔷 Pool: {:?}", pool_data.address);
    println!("{}", "=".repeat(80));
    println!("Network: Base");
    println!("Token0: {} ({:?})", token_a_symbol, pool_data.token0);
    println!("Token1: {} ({:?})", token_b_symbol, pool_data.token1);
    println!("Current Tick: {}", current_tick_i32);
    println!("Tick Spacing: {}", tick_spacing_i32);
    println!("Fee: {} bps ({}%)", fee_u32, fee_u32 as f64 / 10000.0);
    println!("Total Liquidity (L): {}", pool_data.liquidity);
    println!("Current Price: {:.8} {}/{}", current_price, token_b_symbol, token_a_symbol);
    println!("Total Initialized Ticks: {}", ticks.len());
    println!("{}", "-".repeat(80));
    println!("💰 Actual Token Reserves:");
    println!("   {} Reserve: {:.6} {}", token_a_symbol, token0_reserve, token_a_symbol);
    println!("   {} Reserve: {:.6} {}", token_b_symbol, token1_reserve, token_b_symbol);
    println!("   Total Value (in {}): {:.2} {}", token_b_symbol, 
        token0_reserve * current_price + token1_reserve, token_b_symbol);
    println!("{}", "=".repeat(80));
    
    // Show sample tick ranges
    if !ticks.is_empty() {
        println!("\n📊 Sample tick ranges (first 10):");
        for tick_lower in ticks.iter().take(10) {
            let tick_info = pool_data.ticks.get(tick_lower).unwrap();
            let tick_upper = tick_lower + tick_spacing_i32;
            
            // Skip if tick_upper is out of range
            const MAX_TICK: i32 = 887272;
            if tick_upper > MAX_TICK {
                continue;
            }
            
            let is_current_range = current_tick_i32 >= *tick_lower && current_tick_i32 < tick_upper;
            
            let (token0_amount, token1_amount) = calculate_tick_range_amounts(
                *tick_lower,
                tick_upper,
                tick_info.liquidity_gross,
                current_tick_i32,
                pool_data.sqrt_price,
                pool_data.token0_decimals,
                pool_data.token1_decimals,
            );
            
            let sqrt_price_lower = match tick_math::get_sqrt_ratio_at_tick(*tick_lower) {
                Ok(price) => price,
                Err(_) => continue,
            };
            let price_lower = calculate_price_from_sqrt(
                sqrt_price_lower,
                pool_data.token0_decimals,
                pool_data.token1_decimals
            );
            
            let active_marker = if is_current_range { " ⭐ CURRENT" } else { "" };
            
            println!(
                "   [{:>7} → {:>7}]: {} {:.8} {}, {} {:.8} {}, price={:.8}{}",
                tick_lower,
                tick_upper,
                token_a_symbol,
                token0_amount,
                token_a_symbol,
                token_b_symbol,
                token1_amount,
                token_b_symbol,
                price_lower,
                active_marker
            );
        }
        
        if ticks.len() > 20 {
            println!("\n   ... ({} tick ranges omitted) ...", ticks.len() - 20);
            
            println!("\n📊 Sample tick ranges (last 10):");
            for tick_lower in ticks.iter().rev().take(10).rev() {
                let tick_info = pool_data.ticks.get(tick_lower).unwrap();
                let tick_upper = tick_lower + tick_spacing_i32;
                
                // Skip if tick_upper is out of range
                const MAX_TICK: i32 = 887272;
                if tick_upper > MAX_TICK {
                    continue;
                }
                
                let is_current_range = current_tick_i32 >= *tick_lower && current_tick_i32 < tick_upper;
                
                let (token0_amount, token1_amount) = calculate_tick_range_amounts(
                    *tick_lower,
                    tick_upper,
                    tick_info.liquidity_gross,
                    current_tick_i32,
                    pool_data.sqrt_price,
                    pool_data.token0_decimals,
                    pool_data.token1_decimals,
                );
                
                let sqrt_price_lower = match tick_math::get_sqrt_ratio_at_tick(*tick_lower) {
                    Ok(price) => price,
                    Err(_) => continue,
                };
                let price_lower = calculate_price_from_sqrt(
                    sqrt_price_lower,
                    pool_data.token0_decimals,
                    pool_data.token1_decimals
                );
                
                let active_marker = if is_current_range { " ⭐ CURRENT" } else { "" };
                
                println!(
                    "   [{:>7} → {:>7}]: {} {:.8} {}, {} {:.8} {}, price={:.8}{}",
                    tick_lower,
                    tick_upper,
                    token_a_symbol,
                    token0_amount,
                    token_a_symbol,
                    token_b_symbol,
                    token1_amount,
                    token_b_symbol,
                    price_lower,
                    active_marker
                );
            }
        }
    }
    
    // Write pool summary record
    writer.write_record(&[
        "POOL_SUMMARY",
        &format!("{:?}", pool_data.address),
        &format!("Current_Tick={}", current_tick_i32),
        &format!("Tick_Spacing={}", tick_spacing_i32),
        &format!("Fee={}_bps", fee_u32),
        &format!("{:.6}", token0_reserve),
        &format!("{:.6}", token1_reserve),
        &format!("{:.2}", token0_reserve * current_price + token1_reserve),
        &format!("Total_Liquidity={}", pool_data.liquidity),
        &format!("Total_Ticks={}", ticks.len()),
        &format!("Current_Price={:.8}", current_price),
        "",
        "",
        "",
    ])?;
    
    // Write tick data records
    // Each tick represents a range from tick_lower to tick_upper (tick_lower + tick_spacing)
    for tick_lower in &ticks {
        let tick_info = pool_data.ticks.get(tick_lower).unwrap();
        let tick_upper = tick_lower + tick_spacing_i32;
        
        // Skip if tick_upper is out of range
        const MAX_TICK: i32 = 887272;
        if tick_upper > MAX_TICK {
            continue;
        }
        
        let distance = tick_lower - current_tick_i32;
        
        // Check if current tick is within this range
        let is_current_range = current_tick_i32 >= *tick_lower && current_tick_i32 < tick_upper;
        
        // Calculate token amounts for this tick range
        let (token0_amount, token1_amount) = calculate_tick_range_amounts(
            *tick_lower,
            tick_upper,
            tick_info.liquidity_gross,
            current_tick_i32,
            pool_data.sqrt_price,
            pool_data.token0_decimals,
            pool_data.token1_decimals,
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
            pool_data.token0_decimals,
            pool_data.token1_decimals
        );
        let price_upper = calculate_price_from_sqrt(
            sqrt_price_upper,
            pool_data.token0_decimals,
            pool_data.token1_decimals
        );
        
        writer.write_record(&[
            "TICK_RANGE",
            &format!("{:?}", pool_data.address),
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
    
    writer.flush()?;
    
    info!("\n✅ Successfully exported liquidity distribution to {}", output_path);
    
    // Summary statistics
    println!("\n{}", "=".repeat(80));
    println!("📊 SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Network: Base");
    println!("Pool: {:?}", pool_data.address);
    println!("Pair: {}/{}", token_a_symbol, token_b_symbol);
    println!("Total initialized ticks: {}", ticks.len());
    println!("{}", "-".repeat(80));
    println!("💰 Token Reserves:");
    println!("   {}: {:.6}", token_a_symbol, token0_reserve);
    println!("   {}: {:.6}", token_b_symbol, token1_reserve);
    println!("   Total Value: {:.2} {}", token0_reserve * current_price + token1_reserve, token_b_symbol);
    println!("{}", "-".repeat(80));
    println!("Output file: {}", output_path);
    println!("{}", "=".repeat(80));

    Ok(())
}

