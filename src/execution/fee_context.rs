use super::gas_profile::GasQuote;
use alloy::primitives::{B256, U256};
use std::sync::RwLock;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlockFeeContext {
    pub block_number: u64,
    pub block_hash: B256,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

/// Priority + max fee pair from a prior attempt (positional-safe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PriorFees {
    pub priority_fee: u128,
    pub max_fee: u128,
}

impl PriorFees {
    pub const fn new(priority_fee: u128, max_fee: u128) -> Self {
        Self {
            priority_fee,
            max_fee,
        }
    }
}

/// Shared EIP-1559 fee bump (floor): `v * (10_000 + bps) / 10_000`.
///
/// Used when applying a bump to live fees — floor avoids over-paying by 1 wei
/// on non-divisible products.
pub fn bump_fee_value(v: u128, fee_bump_bps: u16) -> Result<u128, FeePlanError> {
    let bps = u128::from(fee_bump_bps);
    v.checked_mul(10_000u128 + bps)
        .and_then(|x| x.checked_div(10_000))
        .ok_or(FeePlanError::Overflow)
}

/// Ceil fee bump: `ceil(v * (10_000 + bps) / 10_000)`.
///
/// Used only for **policy lower-bound checks** (cancel fee cap must cover at
/// least one full bump above the execute cap). Floor would under-require by up
/// to 1 wei and violate the WHI-519 ceil formula.
pub fn bump_fee_value_ceil(v: u128, fee_bump_bps: u16) -> Result<u128, FeePlanError> {
    let bps = u128::from(fee_bump_bps);
    let numer = v
        .checked_mul(10_000u128 + bps)
        .ok_or(FeePlanError::Overflow)?;
    // ceil(n / 10000) = (n + 9999) / 10000
    numer
        .checked_add(9_999)
        .and_then(|x| x.checked_div(10_000))
        .ok_or(FeePlanError::Overflow)
}

/// Finite on-chain deadline from a header timestamp + horizon (never wall clock).
pub fn deadline_from_header_timestamp(
    block_timestamp: u64,
    horizon_secs: u64,
) -> Result<U256, FeePlanError> {
    let ts = block_timestamp
        .checked_add(horizon_secs)
        .ok_or(FeePlanError::Overflow)?;
    Ok(U256::from(ts))
}

/// Bump both fee legs and ensure max covers base + priority.
pub fn bump_prior_fees(
    prior: PriorFees,
    fee_bump_bps: u16,
    base_fee: u128,
) -> Result<PriorFees, FeePlanError> {
    let priority_fee = bump_fee_value(prior.priority_fee, fee_bump_bps)?;
    let mut max_fee = bump_fee_value(prior.max_fee, fee_bump_bps)?;
    let min_max = base_fee
        .checked_add(priority_fee)
        .ok_or(FeePlanError::Overflow)?;
    if max_fee < min_max {
        max_fee = min_max;
    }
    Ok(PriorFees {
        priority_fee,
        max_fee,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum FeePlanError {
    #[error("block header has no EIP-1559 base fee")]
    MissingBaseFee,
    #[error("block fee context has no usable block gas limit")]
    InvalidBlockGasLimit,
    #[error("profile gas limit {gas_limit} is not below the reserved block limit {available}")]
    GasLimitExceedsBlockReserve { gas_limit: u64, available: u64 },
    #[error("profile quote has an invalid gas range")]
    InvalidGasQuote,
    #[error("fee arithmetic overflow")]
    Overflow,
    #[error("receipt gas utilization threshold must be in 1..=10000 basis points")]
    InvalidReceiptThreshold,
    #[error(
        "receipt gas used {gas_used} reached profile re-qualification threshold {threshold} of limit {gas_limit}"
    )]
    ReceiptGasThresholdExceeded {
        gas_used: u64,
        threshold: u64,
        gas_limit: u64,
    },
    #[error("current fee context no longer matches the candidate block")]
    StaleBlockFeeContext,
    #[error("block fee context cache is poisoned")]
    CachePoisoned,
    #[error("cancel fee exceeds cap {cap}: got {fee}")]
    CancelFeeCapExceeded { fee: u128, cap: u128 },
    #[error("cancel max fee {fee} is below base fee {base_fee}")]
    CancelFeeBelowBase { fee: u128, base_fee: u128 },
}

#[derive(Default)]
pub struct BlockFeeContextCache {
    current: RwLock<Option<BlockFeeContext>>,
}

impl BlockFeeContextCache {
    pub fn publish_header(
        &self,
        header: &alloy::rpc::types::eth::Header,
    ) -> Result<(), FeePlanError> {
        let base_fee_per_gas = header
            .inner
            .base_fee_per_gas
            .ok_or(FeePlanError::MissingBaseFee)?;
        self.publish(BlockFeeContext {
            block_number: header.inner.number,
            block_hash: header.hash,
            base_fee_per_gas: u128::from(base_fee_per_gas),
            block_gas_limit: header.inner.gas_limit,
        })
    }

    pub fn publish(&self, context: BlockFeeContext) -> Result<(), FeePlanError> {
        if context.block_gas_limit == 0 {
            return Err(FeePlanError::InvalidBlockGasLimit);
        }
        let mut current = self
            .current
            .write()
            .map_err(|_| FeePlanError::CachePoisoned)?;
        *current = Some(context);
        Ok(())
    }

    pub fn matching(&self, candidate: &BlockFeeContext) -> Result<BlockFeeContext, FeePlanError> {
        let current = self
            .current
            .read()
            .map_err(|_| FeePlanError::CachePoisoned)?;
        match current.as_ref() {
            Some(context) if context == candidate => Ok(context.clone()),
            _ => Err(FeePlanError::StaleBlockFeeContext),
        }
    }

    pub fn current(&self) -> Result<Option<BlockFeeContext>, FeePlanError> {
        let current = self
            .current
            .read()
            .map_err(|_| FeePlanError::CachePoisoned)?;
        Ok(current.clone())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeePolicy {
    priority_fee_per_gas: u128,
    block_gas_reserve: u64,
}

impl FeePolicy {
    pub const fn new(priority_fee_per_gas: u128, block_gas_reserve: u64) -> Self {
        Self {
            priority_fee_per_gas,
            block_gas_reserve,
        }
    }

    pub fn build(
        self,
        quote: &GasQuote,
        context: &BlockFeeContext,
    ) -> Result<FeePlan, FeePlanError> {
        if quote.gas_limit == 0
            || quote.expected_gas_used == 0
            || quote.expected_gas_used >= quote.gas_limit
        {
            return Err(FeePlanError::InvalidGasQuote);
        }
        let available = context
            .block_gas_limit
            .checked_sub(self.block_gas_reserve)
            .ok_or(FeePlanError::InvalidBlockGasLimit)?;
        if quote.gas_limit >= available {
            return Err(FeePlanError::GasLimitExceedsBlockReserve {
                gas_limit: quote.gas_limit,
                available,
            });
        }
        let max_fee_per_gas = context
            .base_fee_per_gas
            .checked_add(self.priority_fee_per_gas)
            .ok_or(FeePlanError::Overflow)?;
        let expected_gas_cost = U256::from(quote.expected_gas_used)
            .checked_mul(U256::from(max_fee_per_gas))
            .ok_or(FeePlanError::Overflow)?;

        Ok(FeePlan {
            block_fee_context: context.clone(),
            gas_limit: quote.gas_limit,
            expected_gas_used: quote.expected_gas_used,
            expected_gas_cost,
            max_fee_per_gas,
            max_priority_fee_per_gas: self.priority_fee_per_gas,
            profile_identity: quote.profile_identity.clone(),
        })
    }

    pub const fn priority_fee_per_gas(self) -> u128 {
        self.priority_fee_per_gas
    }

    pub const fn block_gas_reserve(self) -> u64 {
        self.block_gas_reserve
    }
}

/// Shared discovery/send gas cost (WHI-949 / G-2).
///
/// Pure thin wrapper over [`FeePolicy::build`]: discovery ranking and send-time
/// admission must call this (or `FeePolicy::build` itself) so
/// `expected_gas_cost` is wei-identical. Inputs are the same as the executor
/// path: `GasQuote + BlockFeeContext + priority_fee + block_gas_reserve`
/// (encoded in `policy`).
///
/// Fail-closed on `InvalidGasQuote` / `GasLimitExceedsBlockReserve` / overflow —
/// same rejections the send path applies before costing.
#[inline]
pub fn fee_plan_cost(
    quote: &GasQuote,
    context: &BlockFeeContext,
    policy: FeePolicy,
) -> Result<U256, FeePlanError> {
    Ok(policy.build(quote, context)?.expected_gas_cost)
}

/// Fee-factor identity for gas re-score invalidation (WHI-949).
///
/// Any change to base fee, priority policy, or block gas limit / reserve must
/// re-screen cached gross quotes — not base fee alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeeScoreKey {
    pub base_fee_per_gas: u128,
    pub priority_fee_per_gas: u128,
    pub block_gas_limit: u64,
    pub block_gas_reserve: u64,
}

impl FeeScoreKey {
    pub const fn from_policy_and_context(policy: FeePolicy, context: &BlockFeeContext) -> Self {
        Self {
            base_fee_per_gas: context.base_fee_per_gas,
            priority_fee_per_gas: policy.priority_fee_per_gas(),
            block_gas_limit: context.block_gas_limit,
            block_gas_reserve: policy.block_gas_reserve(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeePlan {
    pub block_fee_context: BlockFeeContext,
    pub gas_limit: u64,
    pub expected_gas_used: u64,
    pub expected_gas_cost: U256,
    pub max_fee_per_gas: u128,
    pub max_priority_fee_per_gas: u128,
    pub profile_identity: String,
}

impl FeePlan {
    /// Reserved profile identity for cancel-intrinsic self-transfers.
    pub const CANCEL_PROFILE_IDENTITY: &'static str = "cancel-intrinsic";

    /// Build a cancel fee plan.
    ///
    /// Documented exemption from the route-quote invariant
    /// `expected_gas_used < gas_limit`: a fixed intrinsic transfer needs no
    /// headroom, so `gas_limit = expected_gas_used = cancel_gas_limit`.
    pub fn for_cancel(
        cancel_gas_limit: u64,
        highest_prior_attempt_fees: PriorFees,
        fee_bump_bps: u16,
        cancel_fee_cap_wei: u128,
        latest_context: &BlockFeeContext,
        block_gas_reserve: u64,
    ) -> Result<Self, FeePlanError> {
        if cancel_gas_limit == 0 {
            return Err(FeePlanError::InvalidGasQuote);
        }
        let available = latest_context
            .block_gas_limit
            .checked_sub(block_gas_reserve)
            .ok_or(FeePlanError::InvalidBlockGasLimit)?;
        if cancel_gas_limit >= available {
            return Err(FeePlanError::GasLimitExceedsBlockReserve {
                gas_limit: cancel_gas_limit,
                available,
            });
        }
        let bumped = bump_prior_fees(
            highest_prior_attempt_fees,
            fee_bump_bps,
            latest_context.base_fee_per_gas,
        )?;
        let max_priority_fee_per_gas = bumped.priority_fee;
        let max_fee_per_gas = bumped.max_fee;
        if max_fee_per_gas < latest_context.base_fee_per_gas {
            return Err(FeePlanError::CancelFeeBelowBase {
                fee: max_fee_per_gas,
                base_fee: latest_context.base_fee_per_gas,
            });
        }
        if max_fee_per_gas > cancel_fee_cap_wei {
            return Err(FeePlanError::CancelFeeCapExceeded {
                fee: max_fee_per_gas,
                cap: cancel_fee_cap_wei,
            });
        }
        let expected_gas_cost = U256::from(cancel_gas_limit)
            .checked_mul(U256::from(max_fee_per_gas))
            .ok_or(FeePlanError::Overflow)?;
        Ok(Self {
            block_fee_context: latest_context.clone(),
            gas_limit: cancel_gas_limit,
            expected_gas_used: cancel_gas_limit,
            expected_gas_cost,
            max_fee_per_gas,
            max_priority_fee_per_gas,
            profile_identity: Self::CANCEL_PROFILE_IDENTITY.to_string(),
        })
    }

    pub fn qualify_receipt_gas(
        &self,
        gas_used: u64,
        utilization_bps: u16,
    ) -> Result<(), FeePlanError> {
        if utilization_bps == 0 || utilization_bps > 10_000 {
            return Err(FeePlanError::InvalidReceiptThreshold);
        }
        let threshold = self
            .gas_limit
            .checked_mul(u64::from(utilization_bps))
            .ok_or(FeePlanError::Overflow)?
            / 10_000;
        if gas_used >= threshold {
            return Err(FeePlanError::ReceiptGasThresholdExceeded {
                gas_used,
                threshold,
                gas_limit: self.gas_limit,
            });
        }
        Ok(())
    }
}
