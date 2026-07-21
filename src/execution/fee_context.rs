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
        highest_prior_attempt_fees: (u128, u128),
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
        let (prior_prio, prior_max) = highest_prior_attempt_fees;
        let bps = u128::from(fee_bump_bps);
        let bump = |v: u128| -> Result<u128, FeePlanError> {
            v.checked_mul(10_000u128 + bps)
                .and_then(|x| x.checked_div(10_000))
                .ok_or(FeePlanError::Overflow)
        };
        let max_priority_fee_per_gas = bump(prior_prio)?;
        let mut max_fee_per_gas = bump(prior_max)?;
        let min_max = latest_context
            .base_fee_per_gas
            .checked_add(max_priority_fee_per_gas)
            .ok_or(FeePlanError::Overflow)?;
        if max_fee_per_gas < min_max {
            max_fee_per_gas = min_max;
        }
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
