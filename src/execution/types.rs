use super::fee_context::{BlockFeeContext, BlockFeeContextCache, FeePlan};
use super::gas_profile::{BinCrossingBucket, RouteKey, TickCrossingBucket};
use super::gas_runtime::RuntimeGasProfile;
use super::intent::IntentAuthority;
use crate::state_space::{BlockHeaderContext, SnapshotId};
use alloy::primitives::{aliases::U112, keccak256, Address, B256, U160, U256};
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
    /// Seconds added to the candidate header timestamp for on-chain deadline.
    pub execution_deadline_secs: u64,
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
            execution_deadline_secs: 60,
        }
    }
}

/// Replacement / cancel policy for the nonce-intent state machine (WHI-519).
///
/// Fee caps have no implicit defaults: construction of the state machine fails
/// if either cap is unset. Other fields carry documented defaults.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IntentPolicy {
    pub stuck_after_blocks: u64,
    pub drop_confirm_blocks: u64,
    pub fee_bump_bps: u16,
    pub max_attempts_per_intent: u32,
    pub max_cancel_attempts: u32,
    pub confirmation_depth: u64,
    pub execution_deadline_secs: u64,
    pub cancel_gas_limit: u64,
    pub reorg_track_blocks: u64,
    /// Mandatory Execute fee cap (wei). No default.
    pub max_fee_cap_wei: u128,
    /// Mandatory cancel emergency fee cap (wei). No default.
    pub cancel_fee_cap_wei: u128,
}

impl IntentPolicy {
    pub fn with_caps(max_fee_cap_wei: u128, cancel_fee_cap_wei: u128) -> Self {
        Self {
            stuck_after_blocks: 3,
            drop_confirm_blocks: 2,
            fee_bump_bps: 1_250,
            max_attempts_per_intent: 3,
            max_cancel_attempts: 3,
            confirmation_depth: 1,
            execution_deadline_secs: 60,
            cancel_gas_limit: 21_000,
            reorg_track_blocks: crate::state_space::CACHE_SIZE as u64,
            max_fee_cap_wei,
            cancel_fee_cap_wei,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let max_fee_cap_wei = std::env::var("MAX_FEE_CAP_WEI")
            .map_err(|_| "MAX_FEE_CAP_WEI is required".to_string())?
            .parse::<u128>()
            .map_err(|e| format!("MAX_FEE_CAP_WEI: {e}"))?;
        let cancel_fee_cap_wei = std::env::var("CANCEL_FEE_CAP_WEI")
            .map_err(|_| "CANCEL_FEE_CAP_WEI is required".to_string())?
            .parse::<u128>()
            .map_err(|e| format!("CANCEL_FEE_CAP_WEI: {e}"))?;
        let mut policy = Self::with_caps(max_fee_cap_wei, cancel_fee_cap_wei);
        if let Ok(v) = std::env::var("EXECUTION_DEADLINE_SECS") {
            policy.execution_deadline_secs = v
                .parse()
                .map_err(|e| format!("EXECUTION_DEADLINE_SECS: {e}"))?;
        }
        if let Ok(v) = std::env::var("STUCK_AFTER_BLOCKS") {
            policy.stuck_after_blocks =
                v.parse().map_err(|e| format!("STUCK_AFTER_BLOCKS: {e}"))?;
        }
        if let Ok(v) = std::env::var("DROP_CONFIRM_BLOCKS") {
            policy.drop_confirm_blocks =
                v.parse().map_err(|e| format!("DROP_CONFIRM_BLOCKS: {e}"))?;
        }
        if let Ok(v) = std::env::var("FEE_BUMP_BPS") {
            policy.fee_bump_bps = v.parse().map_err(|e| format!("FEE_BUMP_BPS: {e}"))?;
        }
        if let Ok(v) = std::env::var("MAX_ATTEMPTS_PER_INTENT") {
            policy.max_attempts_per_intent = v
                .parse()
                .map_err(|e| format!("MAX_ATTEMPTS_PER_INTENT: {e}"))?;
        }
        if let Ok(v) = std::env::var("MAX_CANCEL_ATTEMPTS") {
            policy.max_cancel_attempts =
                v.parse().map_err(|e| format!("MAX_CANCEL_ATTEMPTS: {e}"))?;
        }
        if let Ok(v) = std::env::var("CONFIRMATION_DEPTH") {
            policy.confirmation_depth =
                v.parse().map_err(|e| format!("CONFIRMATION_DEPTH: {e}"))?;
        }
        if let Ok(v) = std::env::var("CANCEL_GAS_LIMIT") {
            policy.cancel_gas_limit = v.parse().map_err(|e| format!("CANCEL_GAS_LIMIT: {e}"))?;
        }
        if let Ok(v) = std::env::var("REORG_TRACK_BLOCKS") {
            policy.reorg_track_blocks =
                v.parse().map_err(|e| format!("REORG_TRACK_BLOCKS: {e}"))?;
        }
        policy.validate().map_err(|e| e.to_string())?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), super::intent::IntentError> {
        use super::intent::IntentError;
        let cache = crate::state_space::CACHE_SIZE as u64;
        if self.confirmation_depth == 0 {
            return Err(IntentError::InvalidPolicy(
                "confirmation_depth must be >= 1".into(),
            ));
        }
        if self.reorg_track_blocks == 0 {
            return Err(IntentError::InvalidPolicy(
                "reorg_track_blocks must be >= 1".into(),
            ));
        }
        if self.reorg_track_blocks > cache {
            return Err(IntentError::InvalidPolicy(format!(
                "reorg_track_blocks {} exceeds CACHE_SIZE {cache}",
                self.reorg_track_blocks
            )));
        }
        if self.confirmation_depth > self.reorg_track_blocks {
            return Err(IntentError::InvalidPolicy(format!(
                "confirmation_depth {} > reorg_track_blocks {}",
                self.confirmation_depth, self.reorg_track_blocks
            )));
        }
        if self.max_fee_cap_wei == 0 {
            return Err(IntentError::InvalidPolicy(
                "max_fee_cap_wei must be set (>0)".into(),
            ));
        }
        if self.cancel_fee_cap_wei == 0 {
            return Err(IntentError::InvalidPolicy(
                "cancel_fee_cap_wei must be set (>0)".into(),
            ));
        }
        let min_cancel =
            super::fee_context::bump_fee_value_ceil(self.max_fee_cap_wei, self.fee_bump_bps)
                .map_err(|e| {
                    IntentError::InvalidPolicy(format!("cancel fee ceil overflow: {e}"))
                })?;
        if self.cancel_fee_cap_wei < min_cancel {
            return Err(IntentError::InvalidPolicy(format!(
                "cancel_fee_cap_wei {} < required minimum {min_cancel}",
                self.cancel_fee_cap_wei
            )));
        }
        if self.cancel_gas_limit == 0 {
            return Err(IntentError::InvalidPolicy(
                "cancel_gas_limit must be > 0".into(),
            ));
        }
        if self.max_attempts_per_intent == 0 || self.max_cancel_attempts == 0 {
            return Err(IntentError::InvalidPolicy(
                "attempt budgets must be >= 1".into(),
            ));
        }
        Ok(())
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
        let deployed_wmnt =
            super::contract::IArbitrageExecutor::new(executor_contract, provider.clone())
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

/// Opaque, SM-minted authorization to sign/submit one attempt.
///
/// Fields are private and the type is non-exhaustive. Construction requires an
/// [`IntentAuthority`] token that only the intent module can mint.
///
/// A permit is linear: it cannot be cloned or consumed twice.
/// ```compile_fail
/// use amms::execution::ExecutionPermit;
/// fn consume(_: ExecutionPermit) {}
/// fn reuse(permit: ExecutionPermit) {
///     consume(permit);
///     consume(permit);
/// }
/// ```
///
/// External code cannot construct one.
/// ```compile_fail
/// use amms::execution::ExecutionPermit;
/// fn forge() -> ExecutionPermit {
///     ExecutionPermit { ..panic!("private authority") }
/// }
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecutionPermit {
    authority: IntentAuthority,
    signer_address: Address,
    route_key: RouteKey,
    block_fee_context: BlockFeeContext,
    nonce: u64,
    snapshot_id: SnapshotId,
    header: BlockHeaderContext,
    pool_universe_fingerprint: B256,
}

impl ExecutionPermit {
    pub fn new(
        authority: IntentAuthority,
        signer_address: Address,
        route_key: RouteKey,
        block_fee_context: BlockFeeContext,
        nonce: u64,
        snapshot_id: SnapshotId,
        header: BlockHeaderContext,
        pool_universe_fingerprint: B256,
    ) -> Self {
        Self {
            authority,
            signer_address,
            route_key,
            block_fee_context,
            nonce,
            snapshot_id,
            header,
            pool_universe_fingerprint,
        }
    }

    pub fn route_key(&self) -> &RouteKey {
        &self.route_key
    }

    pub fn block_fee_context(&self) -> &BlockFeeContext {
        &self.block_fee_context
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    pub fn snapshot_id(&self) -> SnapshotId {
        self.snapshot_id
    }

    pub fn header(&self) -> BlockHeaderContext {
        self.header
    }

    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    pub fn pool_universe_fingerprint(&self) -> B256 {
        self.pool_universe_fingerprint
    }

    pub(crate) fn into_authorized_parts(
        self,
    ) -> (
        Address,
        RouteKey,
        BlockFeeContext,
        u64,
        SnapshotId,
        BlockHeaderContext,
        B256,
    ) {
        let Self {
            authority: _authority,
            signer_address,
            route_key,
            block_fee_context,
            nonce,
            snapshot_id,
            header,
            pool_universe_fingerprint,
        } = self;
        (
            signer_address,
            route_key,
            block_fee_context,
            nonce,
            snapshot_id,
            header,
            pool_universe_fingerprint,
        )
    }
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
