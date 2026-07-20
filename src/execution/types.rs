use super::fee_context::{BlockFeeContext, BlockFeeContextCache, FeePlan};
use super::gas_profile::{BinCrossingBucket, RouteKey, TickCrossingBucket};
use super::gas_runtime::RuntimeGasProfile;
use alloy::primitives::{aliases::U112, keccak256, Address, U160, U256};
use alloy::providers::{DynProvider, Provider};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Pool type enumeration
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PoolType {
    /// Uniswap V2 style (e.g., MoeLP)
    UniV2,
    /// Uniswap V3 style (e.g., Agni)
    UniV3,
    /// Moe Liquidity Book style
    MoeLB,
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
    /// For MoeLB: swap direction (true = swap for Y, false = swap for X)
    pub swap_for_y: Option<bool>,
    /// For MoeLB: bin step (e.g. 15)
    pub bin_step: Option<u16>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorConfig {
    pub chain_id: u64,
    pub v3_router_address: Option<Address>,
    pub moe_router_address: Option<Address>,
    pub slippage_tolerance: f64, // e.g. 0.1 means willing to lose 10% of expected profit
    pub default_priority_fee_wei: u128,
    pub block_gas_limit_reserve: u64,
    pub receipt_gas_limit_utilization_bps: u16,
    /// Minimum required net profit (in MNT wei) after gas; 0 means any positive is fine
    pub min_net_profit_mnt_wei: U256,
    /// If true, ensure min_amount_out covers amount_in + gas cost (no net loss)
    pub include_gas_cost_in_min_out: bool,
    /// If true, enforce non-loss even if not including gas cost
    pub enforce_non_loss: bool,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        let chain_id = 5000;
        Self {
            chain_id,
            v3_router_address: None,
            moe_router_address: None,
            slippage_tolerance: 0.10,
            default_priority_fee_wei: 100_000, // 0.0001 gwei in Mantle wei units
            block_gas_limit_reserve: 1,
            receipt_gas_limit_utilization_bps: 9_500,
            min_net_profit_mnt_wei: U256::from(0u64),
            include_gas_cost_in_min_out: true,
            enforce_non_loss: true,
        }
    }
}

#[derive(Clone)]
pub struct ExecutionContext {
    pub(crate) provider: DynProvider,
    pub(crate) executor_contract: Address,
    pub(crate) wmnt_address: Address,
    pub(crate) gas_profile: RuntimeGasProfile,
    pub(crate) block_fee_contexts: Arc<BlockFeeContextCache>,
}

impl ExecutionContext {
    pub async fn from_provider<P: Provider + Clone + 'static>(
        provider: P,
        executor_contract: Address,
        wmnt_address: Address,
        gas_profile: RuntimeGasProfile,
        block_fee_contexts: Arc<BlockFeeContextCache>,
    ) -> eyre::Result<Self> {
        let expected_identity = gas_profile.executor_identity();
        let observed_chain_id = provider.get_chain_id().await?;
        if observed_chain_id != expected_identity.chain_id {
            eyre::bail!(
                "executor chain identity mismatch: expected {}, observed {}",
                expected_identity.chain_id,
                observed_chain_id
            );
        }
        let code = provider.get_code_at(executor_contract).await?;
        if code.is_empty() {
            eyre::bail!("executor address has no deployed bytecode: {executor_contract}");
        }
        let observed_code_hash = format!("{}", keccak256(code.as_ref()));
        if observed_code_hash != expected_identity.code_hash {
            eyre::bail!(
                "executor code hash mismatch: expected {}, observed {}",
                expected_identity.code_hash,
                observed_code_hash
            );
        }
        let deployed_wmnt = super::contract::IArbitrageExecutor::new(
            executor_contract,
            provider.clone(),
        )
            .WMNT()
            .call()
            .await?;
        if deployed_wmnt != wmnt_address {
            eyre::bail!(
                "executor WMNT mismatch: expected {}, observed {}",
                wmnt_address,
                deployed_wmnt
            );
        }
        Ok(Self {
            provider: provider.erased(),
            executor_contract,
            wmnt_address,
            gas_profile,
            block_fee_contexts,
        })
    }

}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPermit {
    pub route_key: RouteKey,
    pub block_fee_context: BlockFeeContext,
}

#[derive(Clone, Debug)]
pub struct SubmittedExecution {
    pub(crate) tx_hash: alloy::primitives::TxHash,
    pub(crate) route_key: RouteKey,
    pub(crate) fee_plan: FeePlan,
}

impl SubmittedExecution {
    pub fn tx_hash(&self) -> alloy::primitives::TxHash {
        self.tx_hash
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedCrossingBuckets {
    pub(crate) v3_tick_crossings: Option<TickCrossingBucket>,
    pub(crate) moe_bin_crossings: Option<BinCrossingBucket>,
}

#[derive(Clone, Debug)]
pub struct ExecutionParams {
    pub amount_in: U256,
    pub route_key: RouteKey,
    pub(crate) crossing_buckets_verified: bool,
    pub token_path: Vec<Address>,
    pub pool_addresses: Vec<Address>,
    /// On-chain poolType per hop: 0=V2, 1=V3, 2=MoeLB (must match registered venue).
    pub pool_types: Vec<u8>,
    /// Registered (token0/tokenX, token1/tokenY) per pool for direction checks.
    pub pool_tokens: Vec<(Address, Address)>,
    pub expected_reserves_u112: Vec<U112>,
    pub step_amounts_out: Vec<U256>,
    pub min_amount_out: U256,
    /// Expected net profit (after gas) in MNT wei for this opportunity
    pub expected_net_profit_mnt_wei: U256,
}
