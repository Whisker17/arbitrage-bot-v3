/// Execute Arbitrage Transaction
///
/// This script executes a specific arbitrage opportunity with the following features:
/// - Reads PRIVATE_KEY and RPC_URL from .env file
/// - Verifies the opportunity is still valid
/// - Executes the multi-hop swap transaction
/// - Provides detailed logging and safety checks
///
/// Usage:
/// ```bash
/// # Set environment variables in .env:
/// # PRIVATE_KEY=your_private_key_here
/// # RPC_URL=https://rpc.mantle.xyz
///
/// cargo run --example execute_arbitrage
/// ```
use alloy::primitives::{address, utils::format_ether, Address, I256, U160, U256};
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
    consts::U256_1,
    uniswap_v2::UniswapV2Pool,
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

use amms::execution::{
    gas_schedule::gas_limit_for_hops, ExecutorConfig, PoolType, SwapExecutor, SwapStep,
};
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MIN_SQRT_RATIO};

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

// ERC20 interface
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function symbol() external view returns (string memory);
    }
}

/// Arbitrage opportunity structure
#[derive(Debug, Clone)]
struct ArbitrageOpportunity {
    block: u64,
    path_index: usize,
    optimal_input: U256,
    expected_output: U256,
    expected_profit: U256,
    roi_percentage: f64,
    path: ArbitragePath,
}

impl ArbitrageOpportunity {
    /// Parse path from log format
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

/// Load pool metadata
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
        target: "execute.metadata",
        pool_count = %pools.len(),
        "Loaded pool metadata"
    );

    Ok(pools)
}

/// Fetch pool state at current block
async fn fetch_pool_state<N, P>(
    pool_address: Address,
    provider: P,
    metadata: &HashMap<Address, PoolMetadata>,
) -> Result<AMM>
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    info!(
        target: "execute.fetch_pool",
        pool = %pool_address,
        "Fetching pool state"
    );

    // Determine pool type from metadata
    let meta = metadata
        .get(&pool_address)
        .ok_or_else(|| eyre::eyre!("Pool {} not found in metadata", pool_address))?;

    info!(
        target: "execute.fetch_pool",
        pool = %pool_address,
        protocol = %meta.protocol,
        "Detected pool protocol"
    );

    match meta.protocol.as_str() {
        "Agni" => {
            let pool = AgniPool::new(pool_address)
                .init_basic(BlockId::latest(), provider)
                .await?;
            Ok(AMM::AgniPool(pool))
        }
        "UniswapV2" | "UniswapV2-like" => {
            let fee = meta.fee_tier.unwrap_or(3000) / 10;
            let pool = UniswapV2Pool::new(pool_address, fee as usize)
                .init(BlockId::latest(), provider)
                .await?;
            Ok(AMM::UniswapV2Pool(pool))
        }
        _ => {
            // Try Agni first
            match AgniPool::new(pool_address)
                .init(BlockId::latest(), provider.clone())
                .await
            {
                Ok(pool) => Ok(AMM::AgniPool(pool)),
                Err(_) => {
                    // Try UniswapV2
                    if let Ok(pool) = UniswapV2Pool::new(pool_address, 300)
                        .init(BlockId::latest(), provider.clone())
                        .await
                    {
                        return Ok(AMM::UniswapV2Pool(pool));
                    }
                    Err(eyre::eyre!("Could not initialize pool as any known type"))
                }
            }
        }
    }
}

/// Verify the path is still profitable
async fn verify_opportunity<N, P>(
    opportunity: &ArbitrageOpportunity,
    provider: P,
    metadata: &HashMap<Address, PoolMetadata>,
) -> Result<bool>
where
    N: alloy::network::Network,
    P: Provider<N> + Clone,
{
    println!("\n🔬 Verifying opportunity is still valid...");

    // Fetch all pool states
    let mut pools = Vec::new();
    for hop in &opportunity.path.hops {
        let pool = fetch_pool_state::<N, P>(hop.pool_address, provider.clone(), metadata).await?;
        pools.push(pool);
    }

    // Simulate the swap
    let simulation_result = simulate_path(&opportunity.path, &pools, opportunity.optimal_input)?;

    match simulation_result {
        Some(result) => {
            let actual_profit = result.expected_profit;
            let actual_output = result.output_amount;

            println!("  ✅ Simulation successful:");
            println!("     Input:          {}", opportunity.optimal_input);
            println!("     Expected Output: {}", opportunity.expected_output);
            println!("     Actual Output:   {}", actual_output);
            println!("     Expected Profit: {}", opportunity.expected_profit);
            println!("     Actual Profit:   {}", actual_profit);

            // Check if profit is still valid (allow 5% tolerance)
            let min_acceptable_profit =
                opportunity.expected_profit * U256::from(95) / U256::from(100);

            if actual_profit >= min_acceptable_profit {
                println!("  ✅ Profit is still within acceptable range!");
                Ok(true)
            } else {
                println!("  ❌ Profit has decreased below acceptable threshold");
                println!("     Minimum acceptable: {}", min_acceptable_profit);
                println!("     Actual profit:      {}", actual_profit);
                Ok(false)
            }
        }
        None => {
            println!("  ❌ Simulation failed - path is no longer profitable");
            Ok(false)
        }
    }
}

/// Execute the arbitrage
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

    println!("\n🚀 Executing arbitrage transaction...");
    println!("   From: {}", from_address);

    // Get the start token
    let start_token = opportunity.path.hops[0].token_in;

    // Get token info
    let token_contract = IERC20::new(start_token, provider.clone());
    let balance = token_contract.balanceOf(from_address).call().await?;

    // Try to get token metadata (may fail for some tokens)
    let symbol = match token_contract.symbol().call().await {
        Ok(s) => s,
        Err(_) => "UNKNOWN".to_string(),
    };

    println!("\n💰 Token balance check:");
    println!("   Token: {} ({})", symbol, start_token);
    println!("   Balance: {}", balance);
    println!("   Required: {}", opportunity.optimal_input);

    if balance < opportunity.optimal_input {
        return Err(eyre::eyre!(
            "Insufficient balance: have {}, need {}",
            balance,
            opportunity.optimal_input
        ));
    }

    println!("   ✅ Sufficient balance available");

    // Approve tokens for each pool
    println!("\n📝 Approving tokens for pools...");
    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        let token = hop.token_in;
        let pool = hop.pool_address;

        let token_contract = IERC20::new(token, provider.clone());
        let allowance = token_contract.allowance(from_address, pool).call().await?;

        if allowance < opportunity.optimal_input {
            println!("   Hop {}: Approving {} for pool {}", i + 1, token, pool);

            let pending_tx = token_contract.approve(pool, U256::MAX).send().await?;
            let tx_hash = pending_tx.watch().await?;

            println!("   ✅ Approved. Tx: {}", tx_hash);
        } else {
            println!("   ✅ Hop {}: Already approved", i + 1);
        }
    }

    // Execute swaps
    println!("\n🔄 Executing swaps...");
    let mut current_amount = opportunity.optimal_input;

    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        println!(
            "\n   Hop {}/{}: {} -> {}",
            i + 1,
            opportunity.path.hops.len(),
            hop.token_in,
            hop.token_out
        );
        println!("   Pool: {}", hop.pool_address);
        println!("   Fee: {} bps", hop.fee_bps);
        println!("   Input amount: {}", current_amount);

        // Determine swap direction
        let zero_for_one = hop.token_in < hop.token_out;
        println!("   Direction: zero_for_one = {}", zero_for_one);

        // Set sqrt price limit using Uniswap V3 constants
        // These are the correct min/max values that won't trigger SPL error
        let sqrt_price_limit_x96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1 // Minimum price for zero_for_one
        } else {
            MAX_SQRT_RATIO - U256_1 // Maximum price for one_for_zero
        };

        // Convert to U160 for the contract call
        let sqrt_price_limit: U160 = sqrt_price_limit_x96.to::<U160>();
        println!("   Sqrt price limit: {}", sqrt_price_limit);

        // For Uniswap V3 / Agni: negative amount means exactInput (we know how much we're putting in)
        // Convert U256 to I256 and make it negative
        let amount_specified = -I256::from_raw(current_amount);
        println!("   Amount specified (signed): {}", amount_specified);

        let mut swap_step = SwapExecutor::build_swap_step(
            provider,
            hop.pool_address,
            hop.token_in,
            hop.token_out,
            current_amount,
        )
        .await?;

        swap_step.sqrt_price_limit = Some(sqrt_price_limit);
        swap_step.zero_for_one = Some(zero_for_one);
        swap_step.fee = Some(hop.fee_bps);

        let mut exec_config = ExecutorConfig::default();
        exec_config.v3_router_address = Some(address!("e38cfa32cCd918d94E2e20230dFaD1A4Fd8aEF16"));

        let amount_out =
            SwapExecutor::execute_swap(provider, &swap_step, from_address, &exec_config).await?;

        current_amount = amount_out;

        println!("   Output amount: {}", current_amount);
    }

    // Calculate final profit
    let final_balance = current_amount;
    let actual_profit = if final_balance > opportunity.optimal_input {
        final_balance - opportunity.optimal_input
    } else {
        U256::ZERO
    };

    println!("\n{}", "=".repeat(80));
    println!("🎉 Arbitrage Execution Complete!");
    println!("{}", "=".repeat(80));
    println!(
        "Initial Amount:   {} ({:.6} tokens)",
        opportunity.optimal_input,
        format_ether(opportunity.optimal_input)
    );
    println!(
        "Final Amount:     {} ({:.6} tokens)",
        final_balance,
        format_ether(final_balance)
    );
    println!(
        "Actual Profit:    {} ({:.6} tokens)",
        actual_profit,
        format_ether(actual_profit)
    );
    println!(
        "Expected Profit:  {} ({:.6} tokens)",
        opportunity.expected_profit,
        format_ether(opportunity.expected_profit)
    );

    if actual_profit >= opportunity.expected_profit {
        println!("\n✅ SUCCESS! Profit achieved or exceeded expectations!");
    } else if actual_profit > U256::ZERO {
        println!("\n⚠️  Completed with lower profit than expected (slippage/state change)");
    } else {
        println!("\n❌ WARNING: No profit or loss occurred!");
    }
    println!("{}", "=".repeat(80));

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true)
        .init();

    // Load environment variables from .env
    dotenv::dotenv().ok();

    println!("\n{}", "=".repeat(80));
    println!("🤖 Arbitrage Execution Script");
    println!("{}", "=".repeat(80));

    // Parse the arbitrage opportunity from the log
    // This is the opportunity from your monitoring:
    // Block: 85727151, path_index=21, ROI=36.0946%
    let opportunity = ArbitrageOpportunity {
        block: 85727151,
        path_index: 21,
        optimal_input: U256::from_str("520610318980548384")?,
        expected_output: U256::from_str("708522691891027181")?,
        expected_profit: U256::from_str("187912372910478797")?,
        roi_percentage: 36.0946,
        path: ArbitrageOpportunity::parse_path_from_log(
            "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0x1858d52cf57c07a018171d7a1e68dc081f17144f(fee_bps=500)|0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500)|0xcda86a272531e8640cd7f1a92c01839911b90bb0->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0x4b96994181cb694f506bdf24a218fe7af64147cb(fee_bps=2500)"
        ).wrap_err("Failed to parse path from log")?,
    };

    // Print opportunity details
    println!("\n📊 Arbitrage Opportunity:");
    println!("   Block: {}", opportunity.block);
    println!("   Path Index: {}", opportunity.path_index);
    println!("   ROI: {:.4}%", opportunity.roi_percentage);
    println!("   Optimal Input: {} wei", opportunity.optimal_input);
    println!("   Expected Output: {} wei", opportunity.expected_output);
    println!(
        "   Expected Profit: {} wei ({:.6} tokens)",
        opportunity.expected_profit,
        format_ether(opportunity.expected_profit)
    );

    println!("\n📍 Path Details:");
    for (i, hop) in opportunity.path.hops.iter().enumerate() {
        println!("   Hop {}: {} -> {}", i + 1, hop.token_in, hop.token_out);
        println!("      Pool: {}", hop.pool_address);
        println!(
            "      Fee: {} bps ({:.2}%)",
            hop.fee_bps,
            hop.fee_bps as f64 / 100.0
        );
    }

    // Load pool metadata
    println!("\n📚 Loading pool metadata...");
    let metadata = match load_pool_metadata() {
        Ok(m) => m,
        Err(e) => {
            warn!(
                target: "execute.main",
                error = %e,
                "Failed to load pool metadata, continuing without it"
            );
            HashMap::new()
        }
    };

    // Get RPC URL from environment
    let rpc_url = std::env::var("RPC_URL")
        .or_else(|_| std::env::var("MANTLE_RPC_URL"))
        .or_else(|_| std::env::var("MANTLE_PROVIDER_URL"))
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string());

    info!(target: "execute.main", rpc_url = %rpc_url, "Using RPC endpoint");
    println!("\n🔌 Connecting to RPC: {}", rpc_url);

    // Create provider for verification
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(100))
        .layer(RetryBackoffLayer::new(5, 200, 1000))
        .http(rpc_url.parse()?);

    let provider = ProviderBuilder::new().connect_client(client);

    // Get current block
    let current_block = provider.get_block_number().await?;
    println!("   Current block: {}", current_block);
    println!(
        "   Target block: {} ({} blocks ago)",
        opportunity.block,
        current_block.saturating_sub(opportunity.block)
    );

    // Verify the opportunity is still valid
    let is_valid = verify_opportunity::<alloy::network::Ethereum, _>(
        &opportunity,
        provider.clone(),
        &metadata,
    )
    .await?;

    if !is_valid {
        println!("\n❌ Opportunity is no longer valid. Aborting execution.");
        println!("\n💡 Possible reasons:");
        println!("   - Pool states have changed");
        println!("   - Someone else already took this arbitrage");
        println!("   - Price impact has changed");
        return Ok(());
    }

    // Load private key
    let private_key =
        std::env::var("PRIVATE_KEY").wrap_err("PRIVATE_KEY not found in .env file")?;

    let signer: PrivateKeySigner = private_key
        .parse()
        .wrap_err("Failed to parse private key")?;

    let wallet = EthereumWallet::from(signer.clone());
    let from_address = signer.address();

    println!("\n👤 Wallet Information:");
    println!("   Address: {}", from_address);

    // Create provider with wallet
    let client = ClientBuilder::default()
        .layer(ThrottleLayer::new(100))
        .http(rpc_url.parse()?);

    let provider_with_wallet = ProviderBuilder::new().wallet(wallet).connect_client(client);

    // Get wallet balance
    let eth_balance = provider_with_wallet.get_balance(from_address).await?;
    println!(
        "   Native Balance: {} ({:.6} tokens)",
        eth_balance,
        format_ether(eth_balance)
    );

    // Safety confirmation
    println!("\n{}", "=".repeat(80));
    println!("⚠️  WARNING: You are about to execute a REAL arbitrage transaction!");
    println!("⚠️  This will use REAL funds from your wallet!");
    println!("{}", "=".repeat(80));
    println!("\nPress Ctrl+C NOW to cancel, or wait 5 seconds to continue...");

    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Execute the arbitrage
    match execute_arbitrage(&opportunity, provider_with_wallet).await {
        Ok(_) => {
            println!("\n✅ Execution completed successfully!");
        }
        Err(e) => {
            error!(target: "execute.main", error = %e, "Execution failed");
            println!("\n❌ Execution failed: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
