// Note: This executor is designed for ArbitrageOpportunity from logic module
// For direct swap execution, use SwapExecutor instead

// use crate::logic::types::ArbitrageOpportunity;
use alloy::network::ReceiptResponse;
use alloy::primitives::{Address, TxHash, U256};
use alloy::providers::Provider;
use eyre::Result;

use super::contract::{IAgniPool, IArbitrageExecutor, IMoeLBPair, IMoePair};
use super::fee_context::{FeePlanError, FeePolicy};
use super::gas_profile::ProtocolKind;
use super::params::ParamsBuilder;
use super::types::{
    ExecutionContext, ExecutionParams, ExecutionPermit, ExecutorConfig, PoolType,
    SubmittedExecution, VerifiedCrossingBuckets,
};

// Placeholder for ArbitrageOpportunity when logic module is not available
#[derive(Clone, Debug)]
pub struct ArbitrageOpportunity {
    pub optimal_input_amount: U256,
    pub gas_cost_mnt_wei: U256,
    pub net_profit_mnt_wei: U256,
    pub path: SwapPath,
}

#[derive(Clone, Debug)]
pub struct SwapPath {
    pub tokens: Vec<Token>,
    pub pools: Vec<Pool>,
}

#[derive(Clone, Debug)]
pub struct Token {
    address: Address,
}

impl Token {
    pub fn get_address(&self) -> Address {
        self.address
    }
}

#[derive(Clone, Debug)]
pub struct Pool {
    address: Address,
    /// When set, used directly; otherwise `detect_pool_meta` probes on-chain.
    pub pool_type: Option<PoolType>,
}

impl Pool {
    pub fn get_address(&self) -> Address {
        self.address
    }

    pub fn with_type(address: Address, pool_type: PoolType) -> Self {
        Self {
            address,
            pool_type: Some(pool_type),
        }
    }
}

/// On-chain poolType byte matching `ArbitrageExecutor` constants.
pub fn pool_type_byte(pool_type: PoolType) -> u8 {
    match pool_type {
        PoolType::UniV2 => 0,
        PoolType::UniV3 => 1,
        PoolType::MoeLB => 2,
    }
}

pub(crate) fn protocol_kind_for_pool_type_byte(value: u8) -> Result<ProtocolKind> {
    match value {
        0 => Ok(ProtocolKind::V2),
        1 => Ok(ProtocolKind::V3),
        2 => Ok(ProtocolKind::Moe),
        _ => eyre::bail!("unknown pool type byte {value}"),
    }
}

#[cfg(test)]
mod pool_type_tests {
    use super::*;

    #[test]
    fn pool_type_bytes_match_solidity_constants() {
        assert_eq!(pool_type_byte(PoolType::UniV2), 0);
        assert_eq!(pool_type_byte(PoolType::UniV3), 1);
        assert_eq!(pool_type_byte(PoolType::MoeLB), 2);
        assert_eq!(
            protocol_kind_for_pool_type_byte(0).unwrap(),
            ProtocolKind::V2
        );
        assert_eq!(
            protocol_kind_for_pool_type_byte(1).unwrap(),
            ProtocolKind::V3
        );
        assert_eq!(
            protocol_kind_for_pool_type_byte(2).unwrap(),
            ProtocolKind::Moe
        );
    }
}

/// Resolve venue + token ends for one pool.
/// Prefer explicit `hint`; otherwise probe Moe → V3 → V2 (fail closed if none work).
pub async fn detect_pool_meta<P: Provider>(
    provider: &P,
    pool: Address,
    hint: Option<PoolType>,
) -> Result<(PoolType, Address, Address)> {
    if let Some(pt) = hint {
        return match pt {
            PoolType::UniV2 => {
                let pair = IMoePair::new(pool, provider);
                Ok((pt, pair.token0().call().await?, pair.token1().call().await?))
            }
            PoolType::UniV3 => {
                let p = IAgniPool::new(pool, provider);
                Ok((pt, p.token0().call().await?, p.token1().call().await?))
            }
            PoolType::MoeLB => {
                let p = IMoeLBPair::new(pool, provider);
                Ok((pt, p.getTokenX().call().await?, p.getTokenY().call().await?))
            }
        };
    }

    // Moe LB first: getTokenX is unique to LB pairs.
    let moe = IMoeLBPair::new(pool, provider);
    if let (Ok(x), Ok(y)) = (moe.getTokenX().call().await, moe.getTokenY().call().await) {
        if x != Address::ZERO && y != Address::ZERO && x != y {
            return Ok((PoolType::MoeLB, x, y));
        }
    }

    // V3: liquidity() + fee() present.
    let v3 = IAgniPool::new(pool, provider);
    if v3.liquidity().call().await.is_ok() {
        let t0 = v3.token0().call().await?;
        let t1 = v3.token1().call().await?;
        return Ok((PoolType::UniV3, t0, t1));
    }

    // V2 fallback.
    let v2 = IMoePair::new(pool, provider);
    let t0 = v2.token0().call().await?;
    let t1 = v2.token1().call().await?;
    Ok((PoolType::UniV2, t0, t1))
}

pub struct Executor {
    pub config: ExecutorConfig,
    pub context: ExecutionContext,
}

impl Executor {
    pub fn new(context: ExecutionContext, config: ExecutorConfig) -> Self {
        Self { config, context }
    }

    /// Execute the arbitrage via contract call with pre-flight checks
    pub async fn execute<P: Provider>(
        &self,
        _provider: &P,
        params: &ExecutionParams,
        permit: &ExecutionPermit,
    ) -> Result<SubmittedExecution> {
        self.execute_submission(_provider, params, permit).await
    }

    async fn execute_submission<P: Provider>(
        &self,
        _provider: &P,
        params: &ExecutionParams,
        permit: &ExecutionPermit,
    ) -> Result<SubmittedExecution> {
        if params.route_key != permit.route_key {
            eyre::bail!(
                "execution permit route {} does not match built route {}",
                permit.route_key.key_string(),
                params.route_key.key_string()
            );
        }
        let actual_protocols = params
            .pool_types
            .iter()
            .copied()
            .map(protocol_kind_for_pool_type_byte)
            .collect::<Result<Vec<_>>>()?;
        if actual_protocols != permit.route_key.protocols
            || actual_protocols.len() != permit.route_key.hop_count as usize
        {
            eyre::bail!(
                "execution permit route {} does not match calldata route",
                permit.route_key.key_string()
            );
        }
        if actual_protocols
            .iter()
            .any(|protocol| matches!(protocol, ProtocolKind::V3 | ProtocolKind::Moe))
            && !params.crossing_buckets_verified
        {
            eyre::bail!(
                "V3/Moe execution requires verified crossing-bucket evidence before signing"
            );
        }
        let quote = self.context.gas_profile.quote(&permit.route_key)?;
        let fee_context = self
            .context
            .block_fee_contexts
            .matching(&permit.block_fee_context)?;
        let fee_plan = FeePolicy::new(
            self.config.default_priority_fee_wei,
            self.config.block_gas_limit_reserve,
        )
        .build(&quote, &fee_context)?;
        let expected_net_profit = params
            .min_amount_out
            .checked_sub(params.amount_in)
            .and_then(|gross| gross.checked_sub(fee_plan.expected_gas_cost))
            .ok_or_else(|| eyre::eyre!("expected profit does not cover measured gas cost"))?;
        if self.config.enforce_non_loss && expected_net_profit.is_zero() {
            eyre::bail!("Abort execution: non-loss requirement not satisfied");
        }
        if expected_net_profit < self.config.min_net_profit_mnt_wei {
            eyre::bail!(
                "Skip execution: expected net profit {} < min required {}",
                expected_net_profit,
                self.config.min_net_profit_mnt_wei
            );
        }

        tracing::info!(
            profile = %fee_plan.profile_identity,
            block_number = fee_plan.block_fee_context.block_number,
            block_hash = %fee_plan.block_fee_context.block_hash,
            gas_limit = fee_plan.gas_limit,
            expected_gas_used = fee_plan.expected_gas_used,
            max_fee_per_gas = fee_plan.max_fee_per_gas,
            max_priority_fee_per_gas = fee_plan.max_priority_fee_per_gas,
            "Measured gas profile selected"
        );

        let mut outs = params.step_amounts_out.clone();
        let required_out =
            if self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss {
                params.min_amount_out.max(
                    params
                        .amount_in
                        .checked_add(fee_plan.expected_gas_cost)
                        .ok_or_else(|| eyre::eyre!("required output overflow"))?,
                )
            } else {
                params.min_amount_out
            };
        let Some(computed_last) = outs.last().copied() else {
            eyre::bail!("Skip execution: no per-hop output is available");
        };
        if computed_last < required_out {
            eyre::bail!(
                "Skip execution: expected last-hop out {} < required {}",
                computed_last,
                required_out
            );
        }
        let haircut = U256::from(1u64);
        let mut target_last = computed_last.saturating_sub(haircut);
        if target_last < required_out {
            target_last = required_out;
        }
        let Some(last) = outs.last_mut() else {
            eyre::bail!("Skip execution: no per-hop output is available");
        };
        *last = target_last;

        if let Err(e) = super::contract::validate_execute_path(
            self.context.wmnt_address,
            &params.token_path,
            &params.pool_addresses,
            &params.pool_types,
            &params.pool_tokens,
            Some(&outs),
        ) {
            eyre::bail!("Invalid execute path: {e}");
        }
        let min_profit = match required_out.checked_sub(params.amount_in) {
            Some(value) => value,
            None => U256::ZERO,
        };
        let deadline = U256::from(u64::MAX);

        let contract = IArbitrageExecutor::new(
            self.context.executor_contract,
            &self.context.provider,
        );
        let call = contract
            .executeArbitrage(
                params.amount_in,
                params.token_path.clone(),
                params.pool_addresses.clone(),
                params.pool_types.clone(),
                outs,
                min_profit,
                deadline,
            )
            .gas(fee_plan.gas_limit);

        self.context
            .block_fee_contexts
            .matching(&permit.block_fee_context)?;
        let current_quote = self.context.gas_profile.quote(&permit.route_key)?;
        let current_fee_plan = FeePolicy::new(
            self.config.default_priority_fee_wei,
            self.config.block_gas_limit_reserve,
        )
        .build(&current_quote, &permit.block_fee_context)?;
        if current_fee_plan != fee_plan {
            eyre::bail!("gas profile or fee context changed before signing");
        }
        let pending = call
            .max_fee_per_gas(fee_plan.max_fee_per_gas)
            .max_priority_fee_per_gas(fee_plan.max_priority_fee_per_gas)
            .send()
            .await?;
        Ok(SubmittedExecution {
            tx_hash: *pending.tx_hash(),
            route_key: permit.route_key.clone(),
            fee_plan,
        })
    }

    pub fn qualify_receipt_gas(
        &self,
        submitted: &SubmittedExecution,
        gas_used: u64,
    ) -> Result<()> {
        match submitted.fee_plan.qualify_receipt_gas(
            gas_used,
            self.config.receipt_gas_limit_utilization_bps,
        ) {
            Ok(()) => {
                tracing::info!(
                    profile = %submitted.fee_plan.profile_identity,
                    receipt_gas_used = gas_used,
                    "Measured gas receipt recorded"
                );
                Ok(())
            }
            Err(error @ FeePlanError::ReceiptGasThresholdExceeded { .. }) => {
                tracing::error!(
                    profile = %submitted.fee_plan.profile_identity,
                    receipt_gas_used = gas_used,
                    error = %error,
                    "Measured gas profile requires re-qualification"
                );
                self.context.gas_profile.invalidate(&submitted.route_key)?;
                Err(error.into())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub async fn observe_receipt(
        &self,
        submitted: &SubmittedExecution,
    ) -> Result<()> {
        let receipt = self
            .context
            .provider
            .get_transaction_receipt(submitted.tx_hash)
            .await?
            .ok_or_else(|| eyre::eyre!("receipt for {} is not available", submitted.tx_hash))?;
        let qualification = self.qualify_receipt_gas(submitted, receipt.gas_used());
        if !receipt.status() || receipt.block_hash().is_none() {
            eyre::bail!(
                "transaction {} did not produce a canonical successful receipt",
                submitted.tx_hash
            );
        }
        qualification
    }

    /// Convenience method: build params then execute
    pub async fn execute_opportunity<P: Provider>(
        &self,
        _provider: &P,
        opportunity: &ArbitrageOpportunity,
        permit: &ExecutionPermit,
    ) -> Result<SubmittedExecution> {
        let params = ParamsBuilder {
            context: &self.context,
            config: &self.config,
            crossing_buckets: None,
        }
        .build(&self.context.provider, opportunity)
        .await?;
        self.execute(&self.context.provider, &params, permit).await
    }

    pub(crate) async fn execute_opportunity_with_crossing_buckets<P: Provider>(
        &self,
        _provider: &P,
        opportunity: &ArbitrageOpportunity,
        permit: &ExecutionPermit,
        crossing_buckets: VerifiedCrossingBuckets,
    ) -> Result<SubmittedExecution> {
        let params = ParamsBuilder {
            context: &self.context,
            config: &self.config,
            crossing_buckets: Some(crossing_buckets),
        }
        .build(&self.context.provider, opportunity)
        .await?;
        self.execute(&self.context.provider, &params, permit).await
    }
}
