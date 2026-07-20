use alloy::primitives::B256;
use amms::execution::{
    BlockFeeContext, BlockFeeContextCache, FeePolicy, ProtocolKind, RouteKey, RuntimeGasProfile,
    RuntimeProfileConfig,
};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2])?;
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let profile = RuntimeGasProfile::load(
        &profile_path,
        RuntimeProfileConfig::mantle_mainnet(vec![route_key.clone()]),
    )?;
    let quote = profile.quote(&route_key)?;
    let cache = BlockFeeContextCache::default();
    let block_context = BlockFeeContext {
        block_number: 98_158_262,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50_000_000_000,
        block_gas_limit: 60_000_000,
    };
    cache.publish(block_context.clone())?;
    let current_context = cache.matching(&block_context)?;
    let fee_plan = FeePolicy::new(100_000, 1).build(&quote, &current_context)?;

    println!("profile={}", fee_plan.profile_identity);
    println!("gas_limit={}", fee_plan.gas_limit);
    println!("expected_gas_cost={}", fee_plan.expected_gas_cost);
    println!("max_fee_per_gas={}", fee_plan.max_fee_per_gas);
    Ok(())
}
