/// Fetch tick data for large pools by querying tick ranges in small segments
/// This avoids RPC rate limits by making many small requests instead of one large request

use alloy::{
    primitives::{Address, U256, U160, Signed, Uint},
    providers::{Provider, ProviderBuilder},
    sol,
};
use csv::Writer;
use eyre::{Context, Result};
use std::collections::BTreeMap;
use std::fs::File;
use std::str::FromStr;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;
use uniswap_v3_math::tick_math;

// Type aliases for Uniswap V3 types
type I24 = Signed<24, 1>;
type U24 = Uint<24, 1>;

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

/// Get Mantle RPC URL
fn get_mantle_rpc() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

/// Common tokens
const WMNT: Address = alloy::primitives::address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const USDT: Address = alloy::primitives::address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");

fn get_token_symbol(address: Address) -> &'static str {
    match address {
        WMNT => "WMNT",
        USDT => "USDT",
        _ => "UNKNOWN",
    }
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

#[derive(Debug, Clone)]
struct TickInfo {
    liquidity_gross: u128,
    liquidity_net: i128,
}

#[derive(Debug)]
struct PoolData {
    address: Address,
    name: String,
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

/// Fetch ticks in a specific range
async fn fetch_tick_range<P: Provider + Clone>(
    pool_address: Address,
    tick_spacing: i32,
    start_tick: i32,
    end_tick: i32,
    provider: P,
) -> Result<Vec<(i32, TickInfo)>> {
    let pool = IUniswapV3Pool::new(pool_address, provider);
    
    let mut ticks = Vec::new();
    let mut current = start_tick;
    
    while current <= end_tick {
        // Align to tick spacing
        let aligned_tick = (current / tick_spacing) * tick_spacing;
        let tick_i24 = I24::try_from(aligned_tick).unwrap_or(I24::ZERO);
        
        match pool.ticks(tick_i24).call().await {
            Ok(tick_data) => {
                if tick_data.initialized {
                    ticks.push((
                        aligned_tick,
                        TickInfo {
                            liquidity_gross: tick_data.liquidityGross,
                            liquidity_net: tick_data.liquidityNet,
                        },
                    ));
                }
            }
            Err(_) => {
                // Tick not initialized or error, skip
            }
        }
        
        current += tick_spacing;
    }
    
    Ok(ticks)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Starting Segmented Tick Fetcher for Large Pools");

    // The two pools that failed
    let failed_pools = vec![
        ("0xD08C50F7E69e9aeb2867DefF4A8053d9A855e26A", "USDT-WMNT"),
        ("0x262255F4770aEbE2D0C8b97a46287dCeCc2a0AfF", "USDT-WMNT"),
    ];

    // Connect to Mantle RPC
    let rpc_url = get_mantle_rpc();
    info!("🔌 Connecting to Mantle RPC: {}", rpc_url);

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);

    let mut results = Vec::new();

    for (idx, (addr_str, name)) in failed_pools.iter().enumerate() {
        info!("\n🔄 Processing pool {}/{}: {} ({})", idx + 1, failed_pools.len(), name, addr_str);
        
        let addr = Address::from_str(addr_str)?;
        let pool_contract = IUniswapV3Pool::new(addr, provider.clone());
        
        // Get basic pool info
        info!("   📊 Fetching pool metadata...");
        let slot0 = pool_contract.slot0().call().await?;
        let liquidity = pool_contract.liquidity().call().await?;
        let fee = pool_contract.fee().call().await?;
        let tick_spacing = pool_contract.tickSpacing().call().await?;
        let token0 = pool_contract.token0().call().await?;
        let token1 = pool_contract.token1().call().await?;
        
        let current_tick_i32: i32 = slot0.tick.try_into().unwrap_or(0);
        let tick_spacing_i32: i32 = tick_spacing.try_into().unwrap_or(1);
        let fee_u32: u32 = fee.try_into().unwrap_or(0);
        
        info!("      ✓ Current tick: {}, Liquidity: {}, Fee: {} bps", 
            current_tick_i32, liquidity, fee_u32);
        
        let mut pool_data = PoolData {
            address: addr,
            name: name.to_string(),
            sqrt_price: slot0.sqrtPriceX96,
            tick: slot0.tick,
            liquidity,
            fee,
            tick_spacing,
            token0,
            token1,
            token0_decimals: 6, // USDT decimals
            token1_decimals: 18, // WMNT decimals
            ticks: BTreeMap::new(),
        };
        
        // Define tick range to scan (around current tick)
        // For Uniswap V3, typical range is ±887272 ticks (full range)
        // But we'll scan a reasonable range around current price
        let scan_range = 100000; // Scan ±100k ticks around current
        let start_tick = (current_tick_i32 - scan_range).max(-887272);
        let end_tick = (current_tick_i32 + scan_range).min(887272);
        
        info!("   📍 Scanning tick range: {} to {} (spacing: {})", 
            start_tick, end_tick, tick_spacing_i32);
        
        // Fetch ticks in segments
        const SEGMENT_SIZE: i32 = 1000; // Process 1000 ticks at a time
        let total_ticks_to_scan = (end_tick - start_tick) / tick_spacing_i32;
        let total_segments = (total_ticks_to_scan + SEGMENT_SIZE - 1) / SEGMENT_SIZE;
        
        info!("   🔢 Total segments to process: {}", total_segments);
        
        let mut current_start = start_tick;
        let mut segment_idx = 0;
        
        while current_start <= end_tick {
            segment_idx += 1;
            let segment_end = (current_start + SEGMENT_SIZE * tick_spacing_i32).min(end_tick);
            
            info!("      📦 Segment {}/{}: ticks {} to {}", 
                segment_idx, total_segments, current_start, segment_end);
            
            // Retry logic for this segment
            let mut retry_count = 0;
            const MAX_RETRIES: u32 = 3;
            
            loop {
                match fetch_tick_range(
                    addr,
                    tick_spacing_i32,
                    current_start,
                    segment_end,
                    provider.clone()
                ).await {
                    Ok(segment_ticks) => {
                        let new_ticks = segment_ticks.len();
                        for (tick, info) in segment_ticks {
                            pool_data.ticks.insert(tick, info);
                        }
                        info!("         ✓ Found {} ticks (total: {})", new_ticks, pool_data.ticks.len());
                        break;
                    }
                    Err(e) => {
                        retry_count += 1;
                        if retry_count >= MAX_RETRIES {
                            warn!("         ✗ Segment failed after {} retries: {}", MAX_RETRIES, e);
                            break;
                        } else {
                            warn!("         ⚠ Retry {}/{}: {}", retry_count, MAX_RETRIES, e);
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            }
            
            current_start = segment_end + tick_spacing_i32;
            
            // Delay between segments (200ms = 5 req/sec)
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        }
        
        info!("   ✅ Total ticks found: {}", pool_data.ticks.len());
        results.push(pool_data);
        
        // Longer delay between pools
        if idx < failed_pools.len() - 1 {
            info!("   ⏳ Waiting 5 seconds before next pool...");
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        }
    }

    // Write results to CSV
    let output_path = "output/v3_failed_pools_ticks.csv";
    info!("\n📝 Writing results to {}...", output_path);
    
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
    
    let mut total_ticks = 0;
    let results_len = results.len();
    
    for pool_data in results {
        let token_a_symbol = get_token_symbol(pool_data.token0);
        let token_b_symbol = get_token_symbol(pool_data.token1);
        
        // Convert types for display
        let current_tick_i32: i32 = pool_data.tick.try_into().unwrap_or(0);
        let fee_u32: u32 = pool_data.fee.try_into().unwrap_or(0);
        let tick_spacing_i32: i32 = pool_data.tick_spacing.try_into().unwrap_or(1);
        let sqrt_price_u256 = U256::from(pool_data.sqrt_price);
        
        let current_price = calculate_price_from_sqrt(
            sqrt_price_u256,
            pool_data.token0_decimals,
            pool_data.token1_decimals
        );
        
        let ticks: Vec<i32> = pool_data.ticks.keys().copied().collect();
        
        info!("   Pool {}: {} ticks", pool_data.name, ticks.len());
        
        // Write pool summary
        writer.write_record(&[
            "POOL_SUMMARY",
            &format!("{:?}", pool_data.address),
            &pool_data.name,
            &format!("Current_Tick={}", current_tick_i32),
            &format!("Fee={}_bps", fee_u32),
            &format!("Total_Ticks={}", ticks.len()),
            &format!("Tick_Spacing={}", tick_spacing_i32),
            &format!("Total_Liquidity={}", pool_data.liquidity),
            &format!("Current_Price={:.8}", current_price),
            "",
            "",
        ])?;
        
        // Find closest tick to current
        let closest_tick = ticks.iter()
            .min_by_key(|t| ((**t) - current_tick_i32).abs())
            .copied();
        
        // Write tick data
        for tick in &ticks {
            let tick_info = pool_data.ticks.get(tick).unwrap();
            let distance = tick - current_tick_i32;
            let is_current = *tick == current_tick_i32 || Some(*tick) == closest_tick;
            
            let sqrt_price = tick_math::get_sqrt_ratio_at_tick(*tick).unwrap();
            let tick_price = calculate_price_from_sqrt(
                sqrt_price,
                pool_data.token0_decimals,
                pool_data.token1_decimals
            );
            
            writer.write_record(&[
                "TICK_DATA",
                &format!("{:?}", pool_data.address),
                &pool_data.name,
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
        
        // Write separator
        writer.write_record(&["", "", "", "", "", "", "", "", "", "", ""])?;
        
        total_ticks += ticks.len();
    }
    
    writer.flush()?;
    
    info!("\n✅ Successfully exported tick data");
    println!("\n{}", "=".repeat(80));
    println!("📊 SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Pools processed: {}", results_len);
    println!("Total ticks: {}", total_ticks);
    println!("Output file: {}", output_path);
    println!("{}", "=".repeat(80));

    Ok(())
}

