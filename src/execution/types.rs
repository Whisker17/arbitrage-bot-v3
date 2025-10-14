use alloy::primitives::{aliases::U112, Address, U160, U256};
use serde::{Deserialize, Serialize};

/// Pool type enumeration
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PoolType {
    /// Uniswap V2 style (e.g., MoeLP)
    UniV2,
    /// Uniswap V3 style (e.g., Agni)
    UniV3,
}

/// Swap step configuration for a single pool
#[derive(Clone, Debug)]
pub struct SwapStep {
    pub pool_address: Address,
    pub pool_type: PoolType,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub expected_amount_out: Option<U256>,
    /// For V3: sqrt price limit
    pub sqrt_price_limit: Option<U160>,
    /// For V3: zero for one direction
    pub zero_for_one: Option<bool>,
    /// For V3: pool fee tier in basis points (e.g. 3000)
    pub fee: Option<u32>,
    /// Optional override for router address
    pub router_address: Option<Address>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorConfig {
    pub chain_id: u64,
    pub v3_router_address: Option<Address>,
    pub slippage_tolerance: f64, // e.g. 0.1 means willing to lose 10% of expected profit
    pub gas_limit: u64,
    pub default_priority_fee_wei: u128,
    /// Global hard cap for max_fee_per_gas (Mantle wei). Prevents overspending even with high profit
    pub global_fee_hard_cap_wei: u128,
    /// Fee mode: Legacy (gas_price) or EIP-1559 (max fee + priority fee)
    pub fee_mode: FeeMode,
    /// Minimum required net profit (in MNT wei) after gas; 0 means any positive is fine
    pub min_net_profit_mnt_wei: U256,
    /// If true, ensure min_amount_out covers amount_in + gas cost (no net loss)
    pub include_gas_cost_in_min_out: bool,
    /// If true, enforce non-loss even if not including gas cost
    pub enforce_non_loss: bool,
    /// If set, use this fixed gas price in wei instead of dynamic pricing
    pub fixed_gas_price_wei: Option<u128>,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        let chain_id = 5000;
        Self {
            chain_id,
            v3_router_address: None,
            slippage_tolerance: 0.10,
            gas_limit: 600_000_000,
            default_priority_fee_wei: 100_000, // 0.0001 gwei in Mantle wei units
            global_fee_hard_cap_wei: 500_000_000, // 0.5 gwei
            fee_mode: FeeMode::Eip1559,
            min_net_profit_mnt_wei: U256::from(0u64),
            include_gas_cost_in_min_out: true,
            enforce_non_loss: true,
            fixed_gas_price_wei: None, // Use dynamic max fee cap with fixed base fee + priority tip
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FeeMode {
    Legacy,
    Eip1559,
}

#[derive(Clone, Debug)]
pub struct ExecutionContext {
    pub executor_contract: Address,
    pub wmnt_address: Address,
}

#[derive(Clone, Debug)]
pub struct ExecutionParams {
    pub amount_in: U256,
    pub token_path: Vec<Address>,
    pub pool_addresses: Vec<Address>,
    pub expected_reserves_u112: Vec<U112>,
    pub step_amounts_out: Vec<U256>,
    pub min_amount_out: U256,
    /// Expected net profit (after gas) in MNT wei for this opportunity
    pub expected_net_profit_mnt_wei: U256,
}
