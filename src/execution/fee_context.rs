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
