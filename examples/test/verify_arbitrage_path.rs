/// Arbitrage Path Verification Script
///
/// This script verifies the profitability of a specific arbitrage path
/// found during monitoring by:
/// 1. Parsing the path information from logs
/// 2. Fetching pool states at the specified block
/// 3. Re-simulating the swap to verify the profit
/// 4. Preparing transaction calldata if verification passes
///
/// Usage:
/// ```bash
/// cargo run --example verify_arbitrage_path
/// ```
use alloy::primitives::{utils::format_ether, Address, Bytes, U160, U256};
use alloy::{
    eips::BlockId,
    network::EthereumWallet,
    providers::{Provider, ProviderBuilder, WalletProvider},
    rpc::client::ClientBuilder,
    signers::local::PrivateKeySigner,
    sol,
    transports::layers::{RetryBackoffLayer, ThrottleLayer},
};
use amms::amms::{
    agni::AgniPool,
    amm::{AutomatedMarketMaker, AMM},
    uniswap_v2::UniswapV2Pool,
    uniswap_v3::UniswapV3Pool,
};
use amms::arbitrage::{
    optimizer::simulate_path,
    pathfinder::{ArbitragePath, PathHop},
};
use csv::ReaderBuilder;
use eyre::{Result, WrapErr};
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use tracing::{error, info, warn};

// Agni Pool interface for swaps
sol! {
    #[sol(rpc)]
    interface IAgniPool {
        function swap(
            address recipient,
            bool zeroForOne,
            int256 amountSpecified,
            uint160 sqrtPriceLimitX96,
            bytes calldata data
        ) external returns (int256 amount0, int256 amount1);
    }
}

// ERC20 interface for approvals
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

/// Arbitrage path from the logs
#[derive(Debug, Clone)]
struct ArbitrageOpportunity {
    block: u64,
    path_index: usize,
    optimal_input: U256,
    expected_output: U256,
    expected_profit: U256,
    roi_percentage: f64,
    #[allow(dead_code)]
    path_description: String,
    path: ArbitragePath,
}

impl ArbitrageOpportunity {
    /// Parse the log format:
    /// path=TOKEN_A->TOKEN_B@POOL_ADDRESS(fee_bps=FEE)|TOKEN_B->TOKEN_C@POOL_ADDRESS(fee_bps=FEE)|...
    fn parse_path_from_log(path_str: &str) -> Result<ArbitragePath> {
        let mut hops = Vec::new();

        for hop_str in path_str.split('|') {
            let parts: Vec<&str> = hop_str.split("->").collect();
            if parts.len() != 2 {
                return Err(eyre::eyre!("Invalid hop format: {}", hop_str));
            }

            let token_in = Address::from_str(parts[0].trim())?;

            // Parse: TOKEN_OUT@POOL_ADDRESS(fee_bps=FEE)
            let second_part = parts[1];
            let at_split: Vec<&str> = second_part.split('@').collect();
            if at_split.len() != 2 {
                return Err(eyre::eyre!("Invalid hop format (missing @): {}", hop_str));
            }

            let token_out = Address::from_str(at_split[0].trim())?;

            // Parse: POOL_ADDRESS(fee_bps=FEE)
            let pool_and_fee = at_split[1];
            let open_paren = pool_and_fee.find('(').ok_or_else(|| {
                eyre::eyre!("Invalid format (missing opening paren): {}", pool_and_fee)
            })?;

            let pool_address = Address::from_str(&pool_and_fee[..open_paren])?;

            // Parse fee_bps=FEE)
            let fee_str = &pool_and_fee[open_paren + 1..];
            let fee_bps = fee_str
                .trim_end_matches(')')
                .split('=')
                .nth(1)
                .ok_or_else(|| eyre::eyre!("Invalid fee format: {}", fee_str))?
                .trim()
                .parse::<u32>()?;

            hops.push(PathHop {
                pool_address,
                token_in,
                token_out,
                fee_bps,
            });
        }

        Ok(ArbitragePath { hops })
    }
}

#[derive(Debug, Clone)]
struct HopSimulationDetail {
    hop_index: usize,
    pool: Address,
    token_in: Address,
    token_out: Address,
    input_amount: U256,
    output_amount: U256,
}

#[derive(Debug, Clone)]
struct SimulationDiagnostics {
    final_output: U256,
    hop_details: Vec<HopSimulationDetail>,
    failure_hop: Option<usize>,
    failure_reason: Option<String>,
}

fn collect_simulation_diagnostics(
    opportunity: &ArbitrageOpportunity,
    pools: &[AMM],
    amount_in: U256,
) -> Result<SimulationDiagnostics> {
    let mut diagnostics = Vec::new();
    let mut current_amount = amount_in;

    for (index, (hop, pool)) in opportunity.path.hops.iter().zip(pools.iter()).enumerate() {
        let simulation_result = pool.simulate_swap(hop.token_in, hop.token_out, current_amount);

        match simulation_result {
            Ok(output_amount) => {
                diagnostics.push(HopSimulationDetail {
                    hop_index: index,
                    pool: hop.pool_address,
                    token_in: hop.token_in,
                    token_out: hop.token_out,
                    input_amount: current_amount,
                    output_amount,
                });

                if output_amount.is_zero() {
                    return Ok(SimulationDiagnostics {
                        final_output: output_amount,
                        hop_details: diagnostics,
                        failure_hop: Some(index),
                        failure_reason: Some("Swap produced zero output".to_string()),
                    });
                }

                current_amount = output_amount;
            }
            Err(err) => {
                diagnostics.push(HopSimulationDetail {
                    hop_index: index,
                    pool: hop.pool_address,
                    token_in: hop.token_in,
                    token_out: hop.token_out,
                    input_amount: current_amount,
                    output_amount: U256::ZERO,
                });

                return Ok(SimulationDiagnostics {
                    final_output: U256::ZERO,
                    hop_details: diagnostics,
                    failure_hop: Some(index),
                    failure_reason: Some(format!("Swap simulation error: {err}")),
                });
            }
        }
    }

    Ok(SimulationDiagnostics {
        final_output: current_amount,
        hop_details: diagnostics,
        failure_hop: None,
        failure_reason: None,
    })
}

/// Pool metadata from CSV
#[derive(Debug, Deserialize, Clone)]
struct PoolMetadata {
    #[serde(rename = "Protocol")]
    protocol: String,
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "Fee Tier")]
    fee_tier: Option<u32>,
}

/// Load pool metadata from CSV
fn load_pool_metadata() -> Result<HashMap<Address, PoolMetadata>> {
    let csv_path = "data/poolLists.csv";
    let mut reader = ReaderBuilder::new().has_headers(true).from_path(csv_path)?;

    let mut pools = HashMap::new();

    for result in reader.deserialize() {
        let record: PoolMetadata = result?;
        let address = Address::from_str(&record.pair_address)?;
        pools.insert(address, record);
    }

    info!(
        target: "verify.metadata",
        pool_count = %pools.len(),
        "Loaded pool metadata"
    );

    Ok(pools)
}

/// Fetch pool state at a specific block
async fn fetch_pool_state<N, P>(
    pool_address: Address,
    block: u64,
    provider: P,
    metadata: &HashMap<Address, PoolMetadata>,
) -> Result<AMM>
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    info!(
        target: "verify.fetch_pool",
        pool = %pool_address,
        block = %block,
        "Fetching pool state"
    );

    let block_id = BlockId::from(block);

    // Determine pool type from metadata
    let meta = metadata
        .get(&pool_address)
        .ok_or_else(|| eyre::eyre!("Pool {} not found in metadata", pool_address))?;

    info!(
        target: "verify.fetch_pool",
        pool = %pool_address,
        protocol = %meta.protocol,
        "Detected pool protocol"
    );

    match meta.protocol.as_str() {
        "Agni" => {
            let pool = AgniPool::new(pool_address).init_basic(block_id, provider).await?;
            Ok(AMM::AgniPool(pool))
        }
        "UniswapV2" | "UniswapV2-like" => {
            // UniswapV2 pools have a fixed 0.3% fee (30 bps = 3000 in basis points / 10)
            let fee = meta.fee_tier.unwrap_or(3000) / 10;
            let pool = UniswapV2Pool::new(pool_address, fee as usize)
                .init(block_id, provider)
                .await?;
            Ok(AMM::UniswapV2Pool(pool))
        }
        "UniswapV3" | "UniswapV3-like" => {
            let pool = UniswapV3Pool::new(pool_address)
                .init(block_id, provider)
                .await?;
            Ok(AMM::UniswapV3Pool(pool))
        }
        _ => {
            // Try Agni first (most common on Mantle), then fall back
            match AgniPool::new(pool_address)
                .init(block_id, provider.clone())
                .await
            {
                Ok(pool) => {
                    info!(
                        target: "verify.fetch_pool",
                        pool = %pool_address,
                        "Detected as Agni pool"
                    );
                    Ok(AMM::AgniPool(pool))
                }
                Err(_) => {
                    warn!(
                        target: "verify.fetch_pool",
                        pool = %pool_address,
                        protocol = %meta.protocol,
                        "Unknown pool protocol, trying fallbacks"
                    );

                    // Try UniswapV2 (assume 0.3% fee)
                    if let Ok(pool) = UniswapV2Pool::new(pool_address, 300)
                        .init(block_id, provider.clone())
                        .await
                    {
                        info!(target: "verify.fetch_pool", "Detected as UniswapV2 pool");
                        return Ok(AMM::UniswapV2Pool(pool));
                    }

                    Err(eyre::eyre!("Could not initialize pool as any known type"))
                }
            }
        }
    }
}

/// Verify the arbitrage path by re-simulating at the specified block
async fn verify_path<N, P>(
    opportunity: &ArbitrageOpportunity,
    provider: P,
    metadata: &HashMap<Address, PoolMetadata>,
) -> Result<bool>
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    info!(
        target: "verify.path",
        block = %opportunity.block,
        path_index = %opportunity.path_index,
        "Starting verification"
    );

    // 1. Fetch all pool states at the specified block
    let mut pools = Vec::new();
    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        info!(
            target: "verify.path",
            hop_index = i,
            pool = %hop.pool_address,
            "Fetching pool state for hop"
        );

        match fetch_pool_state::<N, P>(
            hop.pool_address,
            opportunity.block,
            provider.clone(),
            metadata,
        )
        .await
        {
            Ok(pool) => {
                info!(
                    target: "verify.path",
                    hop_index = i,
                    pool = %hop.pool_address,
                    "Successfully fetched pool state"
                );
                pools.push(pool);
            }
            Err(e) => {
                error!(
                    target: "verify.path",
                    hop_index = i,
                    pool = %hop.pool_address,
                    error = %e,
                    "Failed to fetch pool state"
                );
                return Err(e);
            }
        }
    }

    // 2. Simulate the swap with the optimal input
    info!(
        target: "verify.path",
        input = %opportunity.optimal_input,
        "Starting swap simulation"
    );

    let simulation_result = simulate_path(&opportunity.path, &pools, opportunity.optimal_input)?;

    match simulation_result {
        Some(result) => {
            let actual_profit = result.expected_profit;
            let actual_output = result.output_amount;

            info!(
                target: "verify.path",
                expected_profit = %opportunity.expected_profit,
                actual_profit = %actual_profit,
                expected_output = %opportunity.expected_output,
                actual_output = %actual_output,
                "Simulation results"
            );

            // Convert to human-readable format (assuming 18 decimals)
            let expected_profit_f64 = opportunity.expected_profit.to::<u128>() as f64 / 1e18;
            let actual_profit_f64 = actual_profit.to::<u128>() as f64 / 1e18;
            let profit_diff_f64 = (expected_profit_f64 - actual_profit_f64).abs();

            println!("\n📊 Simulation Comparison:");
            println!(
                "  Expected Profit (from logs): {:.6} tokens",
                expected_profit_f64
            );
            println!(
                "  Simulated Profit:           {:.6} tokens",
                actual_profit_f64
            );
            println!(
                "  Difference:                 {:.6} tokens ({:.4}%)",
                profit_diff_f64,
                (profit_diff_f64 / expected_profit_f64) * 100.0
            );

            println!("\n📈 Simulation Outputs:");
            println!("  • Input Used: {}", opportunity.optimal_input);
            println!(
                "  • Expected Output (from logs): {}",
                opportunity.expected_output
            );
            println!("  • Simulated Output: {}", actual_output);
            println!("  • Simulated Profit: {}", actual_profit);

            // Check if the profit is still valid (allowing for small differences due to precision)
            let profit_diff = if actual_profit > opportunity.expected_profit {
                actual_profit - opportunity.expected_profit
            } else {
                opportunity.expected_profit - actual_profit
            };

            // Allow 1% difference
            let tolerance = opportunity.expected_profit / U256::from(100);

            if profit_diff <= tolerance {
                info!(
                    target: "verify.path",
                    "✅ Path verification PASSED - profit within tolerance"
                );
                Ok(true)
            } else {
                warn!(
                    target: "verify.path",
                    profit_diff = %profit_diff,
                    tolerance = %tolerance,
                    "⚠️  Path verification FAILED - profit outside tolerance"
                );
                Ok(false)
            }
        }
        None => {
            error!(target: "verify.path", "Simulation returned no result");

            println!("\n📉 Simulation Diagnostics:");

            match collect_simulation_diagnostics(opportunity, &pools, opportunity.optimal_input) {
                Ok(diagnostics) => {
                    for detail in &diagnostics.hop_details {
                        println!("  Hop {} @ {}", detail.hop_index + 1, detail.pool);
                        println!("    • Token In:  {}", detail.token_in);
                        println!("    • Token Out: {}", detail.token_out);
                        println!("    • Input:     {}", detail.input_amount);
                        println!("    • Output:    {}", detail.output_amount);
                    }

                    println!("\n  Final Output Amount: {}", diagnostics.final_output);

                    if diagnostics.final_output >= opportunity.optimal_input {
                        let gain = diagnostics.final_output - opportunity.optimal_input;
                        println!("  Simulated Profit: {}", gain);
                    } else {
                        let loss = opportunity.optimal_input - diagnostics.final_output;
                        println!("  Simulated Profit: -{}", loss);
                    }

                    if let Some(hop_index) = diagnostics.failure_hop {
                        println!(
                            "\n  ⚠️  Simulation appears to have failed at hop {}",
                            hop_index + 1
                        );
                        if let Some(reason) = &diagnostics.failure_reason {
                            println!("  Reason: {}", reason);
                        }
                    }
                }
                Err(err) => {
                    println!("  Unable to compute detailed diagnostics: {}", err);
                }
            }

            Ok(false)
        }
    }
}

/// Print path details in a human-readable format
fn print_path_details(opportunity: &ArbitrageOpportunity) {
    println!("\n{}", "=".repeat(80));
    println!("🔍 Arbitrage Path Verification");
    println!("{}", "=".repeat(80));
    println!("📍 Block Number: {}", opportunity.block);
    println!("🔢 Path Index: {}", opportunity.path_index);
    println!("💰 Expected Profit: {} wei", opportunity.expected_profit);
    println!("📊 ROI: {:.4}%", opportunity.roi_percentage);
    println!("💵 Optimal Input: {} wei", opportunity.optimal_input);
    println!("💴 Expected Output: {} wei", opportunity.expected_output);
    println!("\n📍 Path Hops:");

    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        println!("  {}. {} → {}", i + 1, hop.token_in, hop.token_out);
        println!("     Pool: {}", hop.pool_address);
        println!(
            "     Fee: {} bps ({:.2}%)",
            hop.fee_bps,
            hop.fee_bps as f64 / 100.0
        );
    }
    println!("{}", "=".repeat(80));
}

/// Execute the arbitrage path on-chain
async fn execute_arbitrage<P>(opportunity: &ArbitrageOpportunity, provider: P) -> Result<()>
where
    P: Provider + WalletProvider + Clone,
{
    let from_address = provider.default_signer_address();

    info!(
        target: "execute.arb",
        from = %from_address,
        "Preparing to execute arbitrage"
    );

    // Get the start token (first hop's input token)
    let start_token = opportunity.path.hops[0].token_in;

    // Check balance
    let token_contract = IERC20::new(start_token, provider.clone());
    let balance = token_contract.balanceOf(from_address).call().await?;

    info!(
        target: "execute.arb",
        token = %start_token,
        balance = %balance,
        required = %opportunity.optimal_input,
        "Token balance check"
    );

    if balance < opportunity.optimal_input {
        return Err(eyre::eyre!(
            "Insufficient balance: have {}, need {}",
            balance,
            opportunity.optimal_input
        ));
    }

    // Approve each pool to spend tokens
    for hop in &opportunity.path.hops {
        let token = hop.token_in;
        let pool = hop.pool_address;

        let token_contract = IERC20::new(token, provider.clone());
        let allowance = token_contract.allowance(from_address, pool).call().await?;

        if allowance < opportunity.optimal_input {
            info!(
                target: "execute.arb",
                token = %token,
                pool = %pool,
                "Approving token spend"
            );

            let tx_hash = token_contract
                .approve(pool, U256::MAX)
                .send()
                .await?
                .watch()
                .await?;

            info!(
                target: "execute.arb",
                tx_hash = %tx_hash,
                "Approval confirmed"
            );
        }
    }

    // Execute swaps sequentially
    let mut current_amount = opportunity.optimal_input;

    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        info!(
            target: "execute.arb",
            hop = i,
            pool = %hop.pool_address,
            input = %current_amount,
            "Executing swap"
        );

        // Determine swap direction
        let zero_for_one = hop.token_in < hop.token_out;

        // sqrtPriceLimitX96: Use extreme values to accept any price
        let sqrt_price_limit: U160 = if zero_for_one {
            U160::from(4295128739u64) // Min sqrt price
        } else {
            // Max sqrt price for U160
            U160::from_str_radix("ffffffffffffffffffffffffffffffffffffffff", 16)?
        };

        let pool_contract = IAgniPool::new(hop.pool_address, provider.clone());

        // Estimate gas first
        let gas_estimate = pool_contract
            .swap(
                from_address,
                zero_for_one,
                current_amount.try_into()?,
                sqrt_price_limit,
                Bytes::new(),
            )
            .estimate_gas()
            .await?;

        info!(
            target: "execute.arb",
            hop = i,
            gas_estimate = %gas_estimate,
            "Gas estimate"
        );

        // Send transaction
        let tx_hash = pool_contract
            .swap(
                from_address,
                zero_for_one,
                current_amount.try_into()?,
                sqrt_price_limit,
                Bytes::new(),
            )
            .gas(gas_estimate + 50000) // Add buffer
            .send()
            .await?
            .watch()
            .await?;

        info!(
            target: "execute.arb",
            hop = i,
            tx_hash = %tx_hash,
            "Swap confirmed"
        );

        // Get output amount from the next token balance
        let next_token = hop.token_out;
        let token_contract = IERC20::new(next_token, provider.clone());
        current_amount = token_contract.balanceOf(from_address).call().await?;

        info!(
            target: "execute.arb",
            hop = i,
            output = %current_amount,
            "Swap output"
        );
    }

    // Calculate actual profit
    let final_balance = current_amount;
    let actual_profit = if final_balance > opportunity.optimal_input {
        final_balance - opportunity.optimal_input
    } else {
        U256::ZERO
    };

    println!("\n💰 Arbitrage Execution Results:");
    println!(
        "  Initial Amount: {} ({:.6} tokens)",
        opportunity.optimal_input,
        format_ether(opportunity.optimal_input)
    );
    println!(
        "  Final Amount:   {} ({:.6} tokens)",
        final_balance,
        format_ether(final_balance)
    );
    println!(
        "  Actual Profit:  {} ({:.6} tokens)",
        actual_profit,
        format_ether(actual_profit)
    );
    println!(
        "  Expected Profit: {} ({:.6} tokens)",
        opportunity.expected_profit,
        format_ether(opportunity.expected_profit)
    );

    if actual_profit >= opportunity.expected_profit {
        println!("\n✅ Arbitrage SUCCESSFUL! Profit achieved as expected or better.");
    } else if actual_profit > U256::ZERO {
        println!("\n⚠️  Arbitrage completed but profit is lower than expected.");
        println!("    This could be due to slippage or state changes.");
    } else {
        println!("\n❌ Arbitrage FAILED! No profit or loss occurred.");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .init();

    // Parse the arbitrage opportunity from the logs
    // Path from block 85705988 - path_index=153 (highest absolute profit)
    let opportunity = ArbitrageOpportunity {
        block: 85705988,
        path_index: 153,
        optimal_input: U256::from_str("1123525243417093362")?,
        expected_output: U256::from_str("1126098492810757050")?,
        expected_profit: U256::from_str("2573249393663688")?,
        roi_percentage: 0.2290,
        path_description: "WMNT->USDT->USDC->WMNT".to_string(),
        path: ArbitrageOpportunity::parse_path_from_log(
            "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x201eba5cc46d216ce6dc03f6a759e8e766e956ae@0xcb893a28933a89b5c4ee3d02ca37524d3d0bfc97(fee_bps=10000)|0x201eba5cc46d216ce6dc03f6a759e8e766e956ae->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0x36a7aff497eef6a9cd7d0e7bc243793fcb3e57e2(fee_bps=100)|0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500)"
        ).wrap_err("Failed to parse path from log")?,
    };

    // Print path details
    print_path_details(&opportunity);

    // Load pool metadata
    println!("\n📚 Loading pool metadata...");
    let metadata = match load_pool_metadata() {
        Ok(m) => m,
        Err(e) => {
            warn!(
                target: "verify.main",
                error = %e,
                "Failed to load pool metadata, continuing without it"
            );
            HashMap::new()
        }
    };

    // Connect to Mantle RPC
    info!(target: "verify.main", "Connecting to Mantle RPC...");
    let rpc_url = std::env::var("MANTLE_RPC_URL")
        .or_else(|_| std::env::var("MANTLE_PROVIDER_URL"))
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string());

    info!(target: "verify.main", rpc_url = %rpc_url, "Using RPC endpoint");

    // Create provider with retry and throttle layers for reliability
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(100))
        .layer(RetryBackoffLayer::new(5, 200, 1000))
        .http(rpc_url.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    // Verify the current chain state
    let current_block = provider.get_block_number().await?;
    info!(
        target: "verify.main",
        current_block = %current_block,
        target_block = %opportunity.block,
        "Connected to Mantle"
    );

    if current_block < opportunity.block {
        error!(
            target: "verify.main",
            "Current block {} is before target block {}",
            current_block,
            opportunity.block
        );
        return Err(eyre::eyre!("Cannot verify future block"));
    }

    let blocks_behind = current_block - opportunity.block;
    println!("\n⏰ Target block is {} blocks in the past", blocks_behind);

    // Check if we should execute or just simulate
    let should_execute = std::env::var("EXECUTE_ARBITRAGE")
        .map(|v| v.to_lowercase() == "true" || v == "1")
        .unwrap_or(false);

    if should_execute {
        println!("\n⚠️  EXECUTION MODE ENABLED - Will send real transactions!");
        println!("⚠️  This will use real funds from your wallet.");
    } else {
        println!("\n🔄 SIMULATION MODE - No real transactions will be sent");
        println!("💡 Set EXECUTE_ARBITRAGE=true to enable real execution\n");
    }

    println!("\n🔬 Running offline simulation...");

    let simulation_passed =
        match verify_path::<alloy::network::Ethereum, _>(&opportunity, provider, &metadata).await {
            Ok(true) => {
                println!("\n✅ VERIFICATION SUCCESSFUL!");
                println!("The arbitrage path is valid and profitable in simulation.");
                true
            }
            Ok(false) => {
                println!("\n❌ VERIFICATION FAILED");
                println!("The arbitrage path is no longer profitable or has changed.");
                println!("\n💡 Possible reasons:");
                println!(
                    "  - Pool states have changed since block {}",
                    opportunity.block
                );
                println!("  - Someone else already took this arbitrage");
                println!("  - Price impact calculation differs from monitoring");
                false
            }
            Err(e) => {
                error!(target: "verify.main", error = %e, "Verification error");
                return Err(e);
            }
        };

    if !should_execute {
        if simulation_passed {
            println!("\n📝 Next Steps:");
            println!("1. Set EXECUTE_ARBITRAGE=true environment variable");
            println!("2. Set PRIVATE_KEY environment variable with your private key");
            println!("3. Ensure you have sufficient token balance");
            println!("4. Run the script again to execute the real transaction");
            println!("\n⚠️  Note: This simulation was done at block {} which is {} blocks behind current.", 
                opportunity.block, blocks_behind);
            println!("    The current state may have changed. Consider monitoring for fresh opportunities.");
        }
        return Ok(());
    }

    if !simulation_passed {
        println!("\n🚫 Execution aborted because simulation did not pass. Resolve the issues above and retry.");
        return Ok(());
    }

    // Load private key from environment
    let private_key =
        std::env::var("PRIVATE_KEY").wrap_err("PRIVATE_KEY environment variable not set")?;

    let signer: PrivateKeySigner = private_key
        .parse()
        .wrap_err("Failed to parse private key")?;

    let wallet = EthereumWallet::from(signer.clone());
    let from_address = signer.address();

    println!("📍 Wallet Address: {}", from_address);

    // Recreate provider with wallet for signing
    let client = ClientBuilder::default().http(rpc_url.parse()?);

    let provider_with_wallet = ProviderBuilder::new().wallet(wallet).connect_client(client);

    // Get wallet balance
    let eth_balance = provider_with_wallet.get_balance(from_address).await?;
    println!(
        "💰 ETH Balance: {} ({:.6} ETH)",
        eth_balance,
        format_ether(eth_balance)
    );

    // Ask for confirmation
    println!("\n🔴 WARNING: You are about to execute a real arbitrage transaction!");
    println!("   Press Ctrl+C now to cancel, or wait 5 seconds to continue...");
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    println!("\n🚀 Executing arbitrage...\n");

    match execute_arbitrage(&opportunity, provider_with_wallet).await {
        Ok(_) => {
            println!("\n✅ Arbitrage execution completed!");
        }
        Err(e) => {
            error!(target: "execute.main", error = %e, "Arbitrage execution failed");
            return Err(e);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_path() {
        let path_str = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0x1858d52cf57c07a018171d7a1e68dc081f17144f(fee_bps=500)";

        let result = ArbitrageOpportunity::parse_path_from_log(path_str);
        assert!(result.is_ok());

        let path = result.unwrap();
        assert_eq!(path.hops.len(), 1);
        assert_eq!(path.hops[0].fee_bps, 500);
    }

    #[test]
    fn test_parse_multi_hop_path() {
        let path_str = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0x1858d52cf57c07a018171d7a1e68dc081f17144f(fee_bps=500)|0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500)";

        let result = ArbitrageOpportunity::parse_path_from_log(path_str);
        assert!(result.is_ok());

        let path = result.unwrap();
        assert_eq!(path.hops.len(), 2);
        assert_eq!(path.hops[0].fee_bps, 500);
        assert_eq!(path.hops[1].fee_bps, 500);
    }
}
