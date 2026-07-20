//! Validate Moe LB multi-hop swap simulation against on-chain getSwapOut calls.
//!
//! This script loads the pools involved in a fixed arbitrage path, syncs their
//! state, runs the local simulator for each hop, and then queries the Moe LB
//! pair contracts to compare results with `getSwapOut`. The final output prints
//! per-hop details plus aggregate ROI numbers for both simulated and on-chain
//! calculations.

use std::{collections::HashMap, str::FromStr};

use alloy::{
    eips::BlockId,
    network::primitives::{BlockResponse, HeaderResponse},
    primitives::{address, Address, U256},
    providers::{Provider, ProviderBuilder},
};
use amms::amms::{
    amm::AMM,
    moe::{
        default_moe_pool_list_path, sync_moe_snapshots_batch, sync_token_decimals, MoeLbPair,
        MoePoolList, MoeSnapshotContext, MoeSnapshotSyncConfig,
    },
};
use amms::execution::contract::IMoeLBPair;
use eyre::{bail, ContextCompat, Result};
use itertools::Itertools;
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

const BINS_RADIUS: u32 = 200; // Increase to cover more bins for swaps

/// Swap hop description.
struct Hop {
    pair: Address,
    token_in: Address,
    token_out: Address,
    fee_bps: u16,
}

/// Predefined arbitrage path.
fn arbitrage_path() -> Vec<Hop> {
    vec![
        Hop {
            pair: address!("0xccf72369a2eD02f1185e2055544a89B2D48269b8"),
            token_in: address!("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"), // WMNT
            token_out: address!("0xE6829d9a7eE3040e1276Fa75293Bde931859e8fA"), // CMETH
            fee_bps: 20,
        },
        Hop {
            pair: address!("0xF0601AA87a7341a38034B49f9517dd3adC2DdeC4"),
            token_in: address!("0xE6829d9a7eE3040e1276Fa75293Bde931859e8fA"), // CMETH
            token_out: address!("0xdEAddEaDdeadDEadDEADDEAddEADDEAddead1111"), // WETH
            fee_bps: 1,
        },
        Hop {
            pair: address!("0x1606C79bE3EBD70D8d40bAc6287e23005CfBefA2"),
            token_in: address!("0xdEAddEaDdeadDEadDEADDEAddEADDEAddead1111"), // WETH
            token_out: address!("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"), // WMNT
            fee_bps: 10,
        },
    ]
}

fn init_tracing() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .finish();
    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn mantle_rpc_url() -> String {
    std::env::var("MANTLE_HTTP_URL").unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string())
}

async fn init_provider() -> Result<impl Provider + Clone> {
    let url = mantle_rpc_url();
    let provider = ProviderBuilder::new().connect_http(url.parse()?);
    Ok(provider)
}

#[derive(Debug)]
struct PoolMeta {
    name: String,
}

fn load_pool_metadata() -> Result<HashMap<Address, PoolMeta>> {
    let list = MoePoolList::load_path(default_moe_pool_list_path())?;
    let mut map = HashMap::new();
    for entry in list.entries {
        let name = format!("{:?}/{:?}@{}", entry.token_x, entry.token_y, entry.bin_step);
        map.insert(entry.pool, PoolMeta { name });
    }
    Ok(map)
}

async fn get_swap_out(
    provider: impl Provider + Clone,
    pair: Address,
    amount_in: U256,
    swap_for_y: bool,
) -> Result<(U256, U256, U256)> {
    let contract = IMoeLBPair::new(pair, provider);
    let amount_in_native: u128 = amount_in
        .try_into()
        .map_err(|_| eyre::eyre!("amount too large for uint128"))?;

    let ret = contract
        .getSwapOut(amount_in_native, swap_for_y)
        .call()
        .await?;

    Ok((
        U256::from(ret.amountInLeft),
        U256::from(ret.amountOut),
        U256::from(ret.fee),
    ))
}

fn format_amount(amount: U256, decimals: u8) -> String {
    if amount.is_zero() {
        return "0".into();
    }
    let factor = U256::from(10u128.pow(decimals as u32));
    let whole = amount.checked_div(factor).unwrap_or(U256::ZERO);
    let remainder = amount.checked_rem(factor).unwrap_or(U256::ZERO);
    format!("{}.{:0width$}", whole, remainder, width = decimals as usize)
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let provider = init_provider().await?;
    let path = arbitrage_path();
    let metadata = load_pool_metadata().unwrap_or_default();

    let unique_pairs = path.iter().map(|hop| hop.pair).unique().collect_vec();

    // Initialize pools
    let mut amms: Vec<AMM> = unique_pairs
        .iter()
        .map(|addr| AMM::MoeLbPair(MoeLbPair::new(*addr)))
        .collect();

    let block_number = provider.get_block_number().await?;
    let header = provider
        .get_block_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("missing block {block_number}"))?;
    let context = MoeSnapshotContext::new(header.header().hash(), header.header().timestamp);
    let block_id = BlockId::hash_canonical(context.block_hash);
    info!("Using Mantle block {}", block_number);

    sync_moe_snapshots_batch(
        &mut amms,
        block_id,
        provider.clone(),
        context,
        MoeSnapshotSyncConfig {
            bins_radius: BINS_RADIUS,
            bins_per_request: 15,
        },
    )
    .await?;
    sync_token_decimals(&mut amms, provider.clone()).await?;

    let mut pools: HashMap<Address, MoeLbPair> = HashMap::new();
    for amm in amms {
        if let AMM::MoeLbPair(pair) = amm {
            pools.insert(pair.address, pair);
        }
    }

    info!("Synced {} Moe LB pools", pools.len());

    // Verify bins data against chain for the first pool
    info!("\n=== VERIFYING BINS DATA AGAINST CHAIN ===");
    let first_pair_addr = path[0].pair;
    if let Some(pair) = pools.get(&first_pair_addr) {
        info!(
            "Checking pool {} (Active ID: {})",
            first_pair_addr, pair.active_id
        );

        let pair_contract = IMoeLBPair::new(first_pair_addr, provider.clone());

        // Check bins around active_id
        let check_range = 20u32;
        info!(
            "Comparing local vs chain for bins {} to {}",
            pair.active_id.saturating_sub(check_range),
            pair.active_id.saturating_add(check_range)
        );

        let mut mismatches = 0;
        let mut local_empty = 0;
        let mut chain_empty = 0;
        let mut both_empty = 0;

        for offset in -(check_range as i32)..=(check_range as i32) {
            let bin_id = (pair.active_id as i32 + offset) as u32;

            // Get local data
            let local_bin = pair.bins.get(&bin_id);
            let (local_x, local_y) = if let Some(bin) = local_bin {
                (bin.reserve_x, bin.reserve_y)
            } else {
                (0, 0)
            };

            // Get chain data
            match pair_contract.getBin(U256::from(bin_id).to()).call().await {
                Ok(bin_data) => {
                    let chain_x = bin_data.binReserveX;
                    let chain_y = bin_data.binReserveY;

                    let is_empty_local = local_x == 0 && local_y == 0;
                    let is_empty_chain = chain_x == 0 && chain_y == 0;

                    if is_empty_local && is_empty_chain {
                        both_empty += 1;
                    } else if is_empty_local && !is_empty_chain {
                        local_empty += 1;
                        info!(
                            "  ⚠️  Bin {}: LOCAL MISSING! Chain has x={}, y={}",
                            bin_id, chain_x, chain_y
                        );
                    } else if !is_empty_local && is_empty_chain {
                        chain_empty += 1;
                        info!(
                            "  ⚠️  Bin {}: CHAIN EMPTY! Local has x={}, y={}",
                            bin_id, local_x, local_y
                        );
                    } else if local_x != chain_x || local_y != chain_y {
                        mismatches += 1;
                        info!(
                            "  ❌ Bin {}: MISMATCH! Local x={}, y={} | Chain x={}, y={}",
                            bin_id, local_x, local_y, chain_x, chain_y
                        );
                    } else {
                        // Match - only log non-empty bins
                        if !is_empty_local {
                            info!("  ✅ Bin {}: x={}, y={}", bin_id, local_x, local_y);
                        }
                    }
                }
                Err(e) => {
                    info!("  ⚠️  Bin {}: Chain call failed: {}", bin_id, e);
                }
            }
        }

        info!("\n=== VERIFICATION SUMMARY ===");
        info!("Both empty: {}", both_empty);
        info!("Local missing (chain has data): {}", local_empty);
        info!("Chain empty (local has data): {}", chain_empty);
        info!("Mismatches: {}", mismatches);

        if local_empty > 0 {
            info!(
                "\n⚠️  WARNING: {} bins are missing from local sync but exist on chain!",
                local_empty
            );
            info!("This will cause incorrect swap simulation!");
        }
    }

    let initial_input = U256::from(3_694_167_771_597_537u64);
    let mut sim_amount = initial_input;
    let mut chain_amount = initial_input;

    println!("\n{:^150}", "MOE LB PATH COMPARISON");
    println!(
        "{:<4} {:<44} {:>12} {:>18} {:>18} {:>18} {:>18} {:>18}",
        "Hop", "Pair", "Direction", "Sim In", "Sim Out", "Chain In", "Chain Out", "Chain Fee"
    );

    for (idx, hop) in path.iter().enumerate() {
        let pair = pools
            .get_mut(&hop.pair)
            .with_context(|| format!("pool {} not synced", hop.pair))?;

        let name = metadata
            .get(&hop.pair)
            .map(|meta| meta.name.clone())
            .unwrap_or_else(|| "Unknown".into());

        let (swap_for_y, token_in_decimals, token_out_decimals) =
            if hop.token_in == pair.token_x.address {
                (true, pair.token_x.decimals, pair.token_y.decimals)
            } else if hop.token_in == pair.token_y.address {
                (false, pair.token_y.decimals, pair.token_x.decimals)
            } else {
                bail!("token {} not part of pair {}", hop.token_in, hop.pair);
            };

        let sim_in = sim_amount;
        let chain_in = chain_amount;

        println!("\n{}", "=".repeat(80));
        println!(
            "Hop {} - {} ({} -> {})",
            idx + 1,
            name,
            if swap_for_y { "TokenX" } else { "TokenY" },
            if swap_for_y { "TokenY" } else { "TokenX" }
        );
        println!("{}", "=".repeat(80));

        // Get on-chain slot0 data
        let slot = pair.get_slot0(provider.clone()).await?;
        info!(
            "Chain slot0 - active_id: {}, timestamp: {}",
            slot.active_id, slot.timestamp
        );
        info!(
            "Local state - active_id: {}, bins: {}",
            pair.active_id,
            pair.bins.len()
        );

        // Verify active bin data
        if let Some(active_bin) = pair.bins.get(&pair.active_id) {
            info!(
                "Active bin {} - reserve_x: {}, reserve_y: {}",
                pair.active_id, active_bin.reserve_x, active_bin.reserve_y
            );
        }

        // Get chain price for active bin
        let pair_contract = IMoeLBPair::new(hop.pair, provider.clone());
        let chain_price = pair_contract
            .getPriceFromId(U256::from(pair.active_id).to())
            .call()
            .await?;
        info!(
            "Chain price from active_id {}: {}",
            pair.active_id, chain_price
        );

        // Simulate locally
        println!("\n--- LOCAL SIMULATION ---");
        let sim_out = pair.simulate_swap_precise(swap_for_y, sim_in, slot.timestamp.to::<u64>())?;
        println!("Local result: in={}, out={}", sim_in, sim_out);

        // Get chain result
        println!("\n--- CHAIN CALL ---");
        let (leftover, chain_out, fee) =
            get_swap_out(provider.clone(), hop.pair, chain_in, swap_for_y).await?;
        println!(
            "Chain result: in={}, out={}, fee={}, leftover={}",
            chain_in, chain_out, fee, leftover
        );

        // Compare
        println!("\n--- COMPARISON ---");
        let direction = if swap_for_y { "X->Y" } else { "Y->X" };
        println!("Direction: {}", direction);
        println!(
            "Input  - Local: {:<20} Chain: {:<20} Match: {}",
            format_amount(sim_in, token_in_decimals),
            format_amount(chain_in, token_in_decimals),
            sim_in == chain_in
        );
        println!(
            "Output - Local: {:<20} Chain: {:<20} Match: {}",
            format_amount(sim_out, token_out_decimals),
            format_amount(chain_out, token_out_decimals),
            sim_out == chain_out
        );

        if !leftover.is_zero() {
            println!("⚠️  WARNING: Chain has leftover amount = {}", leftover);
        }

        if sim_out != chain_out {
            let diff = if sim_out > chain_out {
                sim_out - chain_out
            } else {
                chain_out - sim_out
            };
            let pct = if chain_out.is_zero() {
                U256::ZERO
            } else {
                (diff * U256::from(10000u64)) / chain_out
            };
            println!(
                "❌ MISMATCH: diff={} ({}.{}%)",
                diff,
                pct / U256::from(100u64),
                pct % U256::from(100u64)
            );
        } else {
            println!("✅ Results match!");
        }

        sim_amount = sim_out;
        chain_amount = chain_out;
    }

    let base_decimals = pools
        .values()
        .find(|pair| {
            pair.token_x.address == path[0].token_in || pair.token_y.address == path[0].token_in
        })
        .map(|pair| {
            if pair.token_x.address == path[0].token_in {
                pair.token_x.decimals
            } else {
                pair.token_y.decimals
            }
        })
        .unwrap_or(18);

    let sim_profit = sim_amount.saturating_sub(initial_input);
    let chain_profit = chain_amount.saturating_sub(initial_input);

    let roi = |profit: U256| {
        if initial_input.is_zero() {
            return 0.0;
        }
        let profit_f = profit.to::<u128>() as f64 / 10f64.powi(base_decimals as i32);
        let input_f = initial_input.to::<u128>() as f64 / 10f64.powi(base_decimals as i32);
        (profit_f / input_f) * 100.0
    };

    println!(
        "\nSimulated final output : {}",
        format_amount(sim_amount, base_decimals)
    );
    println!(
        "Simulated profit       : {}",
        format_amount(sim_profit, base_decimals)
    );
    println!("Simulated ROI (%)      : {:.6}", roi(sim_profit));

    println!(
        "\nOn-chain final output  : {}",
        format_amount(chain_amount, base_decimals)
    );
    println!(
        "On-chain profit        : {}",
        format_amount(chain_profit, base_decimals)
    );
    println!("On-chain ROI (%)       : {:.6}", roi(chain_profit));

    Ok(())
}
