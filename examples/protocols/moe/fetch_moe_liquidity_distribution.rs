/// Fetch full-range liquidity distribution for specific Moe LB pools
///
/// This script:
/// - Fetches TVL Top5 and Volume Top5 Merchant Moe pools
/// - Fetches complete bin data across the full liquidity range
/// - Displays human-readable amounts with token symbols and decimals
/// - Shows the price at each bin
/// - Exports all results to a CSV file
///
/// Usage:
/// ```bash
/// cargo run --example fetch_moe_liquidity_distribution
/// ```

use alloy::eips::BlockId;
use alloy::primitives::{address, Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol_types::SolValue;
use amms::amms::{
    amm::AMM,
    moe::{sync_slot0_batch, sync_token_decimals, MoeLbPair},
    GetMoeLBPairBinDataBatchRequest,
};
use csv::Writer;
use eyre::{Context, Result};
use std::collections::HashMap;
use std::fs::File;
use tracing::{info, warn, Level};
use tracing_subscriber::FmtSubscriber;

/// Number of bins to query in each direction from active bin
/// We'll query a wide range to capture full liquidity distribution
const BINS_RADIUS: u32 = 200;

/// Number of bins to fetch in each batch to avoid RPC limits
const BATCH_SIZE: u32 = 40;

/// Common tokens on Mantle for symbol lookup
const WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
const USDT: Address = address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");
const METH: Address = address!("cDA86A272531e8640cD7F1a92c01839911B90bb0");
const WETH: Address = address!("dEAddEaDdeadDEadDEADDEAddEADDEAddead1111");
const USDC: Address = address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9");
const USDE: Address = address!("5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34");
const AUSD: Address = address!("00000000eFE302BEAA2b3e6e1b18d08D69a9012a");
const CMETH: Address = address!("E6829d9a7eE3040e1276Fa75293Bde931859e8fA");
const FBTC: Address = address!("C96dE26018A54D51c097160568752c4E3BD6C364");
const WBTC: Address = address!("CAbAE6f6Ea1ecaB08Ad02fE02ce9A44F09aebfA2");
const MOE: Address = address!("4515A45337F461A11Ff0FE8aBF3c606AE5dC00c9");
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
            name: "cmETH-USDE".to_string(),
            address: address!("38E2a053E67697e411344B184B3aBAe4fab42cC2"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "FBTC-cmETH".to_string(),
            address: address!("2612E3280ca8836F58173bF7EcC35e258Dc1b54B"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "AUSD-USDE".to_string(),
            address: address!("5A59359a1ad9b0A59aa70145dFeCeb6d9Ee07253"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "sUSDE-USDE".to_string(),
            address: address!("E50019C79Cbd7C49cfFA7C3f8080EA238DE75962"),
            category: "TVL Top5".to_string(),
        },
        PoolConfig {
            name: "USDE-WMNT".to_string(),
            address: address!("5d54d430D1FD9425976147318E6080479bffC16D"),
            category: "TVL Top5".to_string(),
        },
        // Volume Top5
        PoolConfig {
            name: "cmETH-WETH".to_string(),
            address: address!("F0601AA87a7341a38034B49f9517dd3adC2DdeC4"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "cmETH-mETH".to_string(),
            address: address!("3d887CE4988fb46AEC6E0027171f65DB3526E5f1"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "USDC-USDT".to_string(),
            address: address!("48C1A89af1102Cad358549e9Bb16aE5f96CddFEc"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "mETH-WETH".to_string(),
            address: address!("3b6c029E6409f2868769871F9Ed6825b15BDca15"),
            category: "Volume Top5".to_string(),
        },
        PoolConfig {
            name: "USDE-WMNT (Volume)".to_string(),
            address: address!("5d54d430D1FD9425976147318E6080479bffC16D"),
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
        AUSD => "AUSD",
        CMETH => "CMETH",
        FBTC => "FBTC",
        WBTC => "WBTC",
        MOE => "MOE",
        SUSDE => "SUSDE",
        _ => "UNKNOWN",
    }
}

/// Format amount with proper decimals (returns human-readable string)
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

/// Convert amount to human-readable f64 (for calculations)
fn amount_to_f64(amount: u128, decimals: u8) -> f64 {
    let divisor = 10u128.pow(decimals as u32);
    amount as f64 / divisor as f64
}

/// Calculate total pool reserves from all bins
fn calculate_pool_reserves(
    bins: &HashMap<u32, (u128, u128)>,
    token_x_decimals: u8,
    token_y_decimals: u8,
) -> (f64, f64) {
    let total_x: u128 = bins.values().map(|(x, _)| x).sum();
    let total_y: u128 = bins.values().map(|(_, y)| y).sum();
    
    (
        amount_to_f64(total_x, token_x_decimals),
        amount_to_f64(total_y, token_y_decimals)
    )
}

/// Batch fetch bin data for a pool across a wide range
/// Fetches bins in smaller batches to avoid RPC limits
async fn fetch_pool_bins<N, P>(
    pool: &MoeLbPair,
    block: BlockId,
    provider: P,
) -> Result<HashMap<u32, (u128, u128)>>
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    let active_id = pool.active_id;
    let start_id = active_id.saturating_sub(BINS_RADIUS);
    let end_id = active_id.saturating_add(BINS_RADIUS);
    
    let total_bins = (end_id - start_id + 1) as usize;
    
    info!(
        "Fetching {} bins for pool {} (active: {}) in batches of {}",
        total_bins,
        pool.address,
        active_id,
        BATCH_SIZE
    );
    
    let mut all_bins = HashMap::new();
    let mut current_start = start_id;
    let mut batch_num = 0;
    
    // Fetch bins in batches
    while current_start <= end_id {
        batch_num += 1;
        let current_end = (current_start + BATCH_SIZE - 1).min(end_id);
        let batch_ids: Vec<u32> = (current_start..=current_end).collect();
        
        info!(
            "   Batch {}: Fetching bins {} to {} ({} bins)",
            batch_num,
            current_start,
            current_end,
            batch_ids.len()
        );
        
        let request = GetMoeLBPairBinDataBatchRequest::BinDataRequest {
            pair: pool.address,
            ids: batch_ids.iter().map(|&id| U256::from(id).to()).collect(),
        };
        
        match GetMoeLBPairBinDataBatchRequest::deploy_builder(provider.clone(), vec![request])
            .call_raw()
            .block(block)
            .await
        {
            Ok(ret) => {
                // Decode: Vec<Vec<(u128, u128)>> where each inner vec contains (reserveX, reserveY) tuples
                match <Vec<Vec<(u128, u128)>>>::abi_decode(&ret) {
                    Ok(results) => {
                        if let Some(pool_results) = results.first() {
                            let mut bins_with_liquidity = 0;
                            for (idx, bin_id) in batch_ids.iter().enumerate() {
                                if idx < pool_results.len() {
                                    let (reserve_x, reserve_y) = pool_results[idx];
                                    
                                    // Only store bins with non-zero liquidity
                                    if reserve_x > 0 || reserve_y > 0 {
                                        all_bins.insert(*bin_id, (reserve_x, reserve_y));
                                        bins_with_liquidity += 1;
                                    }
                                }
                            }
                            info!("      ✓ Found {} bins with liquidity in this batch", bins_with_liquidity);
                        }
                    }
                    Err(e) => {
                        warn!("      ✗ Failed to decode batch {}: {}", batch_num, e);
                    }
                }
            }
            Err(e) => {
                warn!("      ✗ RPC call failed for batch {}: {}", batch_num, e);
            }
        }
        
        current_start = current_end + 1;
        
        // Small delay between batches to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    info!(
        "   ✅ Total: Found {} bins with liquidity across {} batches",
        all_bins.len(),
        batch_num
    );
    
    Ok(all_bins)
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("🚀 Starting Moe LB Liquidity Distribution Fetcher");

    // Get target pools (TVL Top5 + Volume Top5)
    let pool_configs = get_target_pools();
    info!("📊 Analyzing {} Merchant Moe pools:", pool_configs.len());
    info!("   - TVL Top5: 5 pools");
    info!("   - Volume Top5: 5 pools");

    // Connect to Mantle RPC
    let rpc_url = get_mantle_rpc();
    info!("🔌 Connecting to Mantle RPC: {}", rpc_url);

    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    let block_number = provider.get_block_number().await?;
    info!("📦 Current block number: {}", block_number);

    // Create AMM instances for all pools
    let mut amms: Vec<AMM> = pool_configs
        .iter()
        .map(|config| AMM::MoeLbPair(MoeLbPair::new(config.address)))
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

    // Step 3: Fetch full-range bin data for each pool
    info!("🔄 Fetching full-range liquidity distribution...");
    
    // We'll write directly to CSV to maintain proper structure
    let output_path = "output/moe_liquidity_distribution.csv";
    let output_file = File::create(output_path).context("Failed to create output CSV")?;
    let mut writer = Writer::from_writer(output_file);
    
    // Write header manually to match our structure
    writer.write_record(&[
        "record_type",
        "pool_address", 
        "pool_name",
        "category",
        "bin_id_or_active_bin",
        "token_x_symbol",
        "token_x_amount_readable",
        "token_y_symbol", 
        "token_y_amount_readable",
        "token_x_reserve_raw",
        "token_y_reserve_raw",
        "bin_price",
        "distance_from_active",
        "is_active_bin"
    ])?;
    
    let mut total_bins_count = 0;
    
    for (idx, amm) in amms.iter().enumerate() {
        if let AMM::MoeLbPair(pool) = amm {
            let config = &pool_configs[idx];
            
            info!(
                "📊 Processing pool {}/{}: {} - {} ({})",
                idx + 1,
                amms.len(),
                config.category,
                config.name,
                pool.address
            );
            
            match fetch_pool_bins(pool, block_id, provider.clone()).await {
                Ok(bins) => {
                    info!("   ✅ Found {} bins with liquidity", bins.len());
                    
                    let token_x_symbol = get_token_symbol(pool.token_x.address);
                    let token_y_symbol = get_token_symbol(pool.token_y.address);
                    
                    // Sort bins by ID for better readability
                    let mut sorted_bins: Vec<_> = bins.iter().collect();
                    sorted_bins.sort_by_key(|(id, _)| *id);
                    
                    // Calculate totals (both raw and human-readable)
                    let total_x: u128 = sorted_bins.iter().map(|(_, (x, _))| x).sum();
                    let total_y: u128 = sorted_bins.iter().map(|(_, (_, y))| y).sum();
                    let (total_x_readable, total_y_readable) = calculate_pool_reserves(&bins, pool.token_x.decimals, pool.token_y.decimals);
                    
                    // Calculate current price
                    let current_price = pool.get_price_from_id(pool.active_id);
                    
                    // Display summary
                    println!("\n{}", "=".repeat(80));
                    println!("🔷 Pool: {} - {} ({})", config.category, config.name, pool.address);
                    println!("{}", "=".repeat(80));
                    println!("Active Bin: {}", pool.active_id);
                    println!("Bin Step: {} ({}%)", pool.bin_step, pool.bin_step as f64 / 100.0);
                    println!("Total Bins: {}", bins.len());
                    println!("Current Price: {:.8} {}/{}", current_price, token_y_symbol, token_x_symbol);
                    println!("{}", "-".repeat(80));
                    println!("💰 Token Reserves:");
                    println!("   {} Reserve: {:.6} {}", token_x_symbol, total_x_readable, token_x_symbol);
                    println!("   {} Reserve: {:.6} {}", token_y_symbol, total_y_readable, token_y_symbol);
                    println!("   Total Value (in {}): {:.2} {}", token_y_symbol, 
                        total_x_readable * current_price + total_y_readable, token_y_symbol);
                    println!("{}", "=".repeat(80));
                    
                    // Show first few bins as sample
                    println!("\n   Sample bins:");
                    for (bin_id, (reserve_x, reserve_y)) in sorted_bins.iter().take(5) {
                        let _distance = **bin_id as i64 - pool.active_id as i64;
                        let price = pool.get_price_from_id(**bin_id);
                        let is_active = **bin_id == pool.active_id;
                        
                        let x_formatted = format_amount(*reserve_x, pool.token_x.decimals);
                        let y_formatted = format_amount(*reserve_y, pool.token_y.decimals);
                        
                        let active_marker = if is_active { " ⭐ ACTIVE" } else { "" };
                        
                        println!(
                            "      Bin {}: {}={}, {}={}, price={:.8}{}",
                            bin_id,
                            token_x_symbol,
                            x_formatted,
                            token_y_symbol,
                            y_formatted,
                            price,
                            active_marker
                        );
                    }
                    
                    // Write pool summary record with detailed labels
                    writer.write_record(&[
                        "POOL_SUMMARY",
                        &format!("{:?}", pool.address),
                        &config.name,
                        &config.category,
                        &format!("Active_Bin={}", pool.active_id),
                        &format!("Bin_Step={}_bps", pool.bin_step),
                        &format!("{:.6}", total_x_readable),
                        &format!("{:.6}", total_y_readable),
                        &format!("{:.2}", total_x_readable * current_price + total_y_readable),
                        &format!("Total_Bins={}", bins.len()),
                        "",
                        &format!("Current_Price={:.8}", current_price),
                        "",
                        "",
                    ])?;
                    
                    // Write bin data records
                    for (bin_id, (reserve_x, reserve_y)) in sorted_bins {
                        let distance = *bin_id as i64 - pool.active_id as i64;
                        let price = pool.get_price_from_id(*bin_id);
                        let is_active = *bin_id == pool.active_id;
                        
                        let x_readable = amount_to_f64(*reserve_x, pool.token_x.decimals);
                        let y_readable = amount_to_f64(*reserve_y, pool.token_y.decimals);
                        
                        writer.write_record(&[
                            "BIN_DATA",
                            &format!("{:?}", pool.address),
                            &config.name,
                            &config.category,
                            &bin_id.to_string(),
                            token_x_symbol,
                            &format!("{:.8}", x_readable),
                            token_y_symbol,
                            &format!("{:.8}", y_readable),
                            &reserve_x.to_string(),
                            &reserve_y.to_string(),
                            &format!("{:.8}", price),
                            &distance.to_string(),
                            &is_active.to_string(),
                        ])?;
                    }
                    
                    // Write separator (empty line)
                    writer.write_record(&["", "", "", "", "", "", "", "", "", "", "", "", "", ""])?;
                    
                    total_bins_count += bins.len();
                }
                Err(e) => {
                    warn!("   ❌ Failed to fetch bins: {}", e);
                }
            }
        }
    }
    
    writer.flush()?;
    
    info!("\n✅ Successfully exported liquidity distribution to {}", output_path);
    
    // Summary statistics
    println!("\n{}", "=".repeat(80));
    println!("📊 SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Total pools processed: {}", amms.len());
    println!("Total bins with liquidity: {}", total_bins_count);
    println!("Output file: {}", output_path);
    println!("{}", "=".repeat(80));

    Ok(())
}

