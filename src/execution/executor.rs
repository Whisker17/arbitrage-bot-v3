// Note: This executor is designed for ArbitrageOpportunity from logic module
// For direct swap execution, use SwapExecutor instead

// use crate::logic::types::ArbitrageOpportunity;
use alloy::eips::Encodable2718;
use alloy::network::{EthereumWallet, NetworkWallet, ReceiptResponse, TransactionBuilder};
use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::TransactionRequest;
use eyre::Result;

use super::contract::{IAgniPool, IArbitrageExecutor, IMoeLBPair, IMoePair};
use super::fee_context::{deadline_from_header_timestamp, FeePlan, FeePlanError, FeePolicy};
use super::gas_profile::ProtocolKind;
use super::intent::{
    PrepareRequest, PreparedPayload, ReceiptOutcome, SignedSubmission,
};
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

#[cfg(test)]
mod execution_rpc_tests {
    use super::*;
    use crate::execution::{
        load_artifact, BlockFeeContext, BlockFeeContextCache, RuntimeGasProfile,
        RuntimeProfileConfig,
    };
    use alloy::providers::{Provider, ProviderBuilder};
    use alloy::transports::mock::Asserter;
    use std::{path::PathBuf, sync::Arc};

    #[tokio::test]
    async fn measured_fee_selection_makes_no_gas_sizing_rpc_calls() {
        let route_key = crate::execution::RouteKey::new(vec![
            crate::execution::ProtocolKind::V2,
            crate::execution::ProtocolKind::V2,
        ])
        .unwrap();
        let artifact_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/gas_profiles/mantle_mainnet_v1.json");
        let gas_profile = RuntimeGasProfile::from_artifact(
            load_artifact(&artifact_path).unwrap(),
            RuntimeProfileConfig::mantle_mainnet(vec![route_key.clone()]),
        )
        .unwrap();

        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let block_fee_context = BlockFeeContext {
            block_number: 42,
            block_hash: alloy::primitives::B256::ZERO,
            base_fee_per_gas: 50_000_000_000,
            block_gas_limit: 60_000_000,
        };
        let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
        block_fee_contexts.publish(block_fee_context.clone()).unwrap();
        let context = ExecutionContext {
            provider: provider.clone().erased(),
            executor_contract: Address::ZERO,
            wmnt_address: Address::ZERO,
            gas_profile,
            block_fee_contexts,
        };
        let executor = Executor::new(context, ExecutorConfig::default());
        let params = ExecutionParams {
            amount_in: U256::from(1_000_000u64),
            route_key: route_key.clone(),
            crossing_buckets_verified: false,
            token_path: vec![Address::ZERO; 3],
            pool_addresses: vec![Address::ZERO; 2],
            pool_types: vec![pool_type_byte(PoolType::UniV2); 2],
            pool_tokens: vec![(Address::ZERO, Address::ZERO); 2],
            expected_reserves_u112: vec![alloy::primitives::aliases::U112::ZERO; 4],
            step_amounts_out: vec![U256::ZERO; 2],
            min_amount_out: U256::from(1_000_000u64),
            expected_net_profit_mnt_wei: U256::ZERO,
        };
        let permit = ExecutionPermit::new(
            crate::execution::intent::test_support::authority(),
            route_key,
            block_fee_context,
            0,
            crate::state_space::SnapshotId::new(5000, 42, alloy::primitives::B256::ZERO),
            crate::state_space::BlockHeaderContext::new(
                alloy::primitives::B256::ZERO,
                1_700_000_000,
            ),
        );

        let error = executor.execute(&provider, &params, &permit).await.unwrap_err();
        assert!(error
            .to_string()
            .contains("fused Executor::execute is retired"));
        assert!(asserter.read_q().is_empty());
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
        _params: &ExecutionParams,
        _permit: &ExecutionPermit,
    ) -> Result<SubmittedExecution> {
        eyre::bail!(
            "fused Executor::execute is retired for live sends; use prepare_submission + broadcast via IntentStateMachine"
        )
    }

    /// Locally sign a kind-explicit submission with no network I/O.
    pub async fn prepare_submission(
        &self,
        request: PrepareRequest,
        permit: &ExecutionPermit,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        match request {
            PrepareRequest::Execute {
                params,
                candidate,
                fee_plan,
                deadline,
            } => {
                self.prepare_execute(params, candidate, fee_plan, deadline, permit, wallet)
                    .await
            }
            PrepareRequest::Cancel {
                to,
                gas_limit,
                fee_plan,
            } => self.prepare_cancel(to, gas_limit, fee_plan, permit, wallet).await,
        }
    }


    async fn sign_and_wrap(
        &self,
        tx: TransactionRequest,
        wallet: &EthereumWallet,
        fee_plan: FeePlan,
        payload: PreparedPayload,
        calldata_digest: B256,
        permit: &ExecutionPermit,
    ) -> Result<SignedSubmission> {
        let envelope = <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::sign_request(
            wallet, tx,
        )
        .await
        .map_err(|e| eyre::eyre!("local sign failed: {e}"))?;
        let tx_hash = *envelope.tx_hash();
        let raw = Bytes::from(envelope.encoded_2718());
        Ok(SignedSubmission {
            raw,
            tx_hash,
            fee_plan,
            payload,
            calldata_digest,
            nonce: permit.nonce(),
            submitted_at: permit.snapshot_id(),
        })
    }

    async fn prepare_execute(
        &self,
        params: ExecutionParams,
        candidate: super::intent::CandidateRef,
        fee_plan: FeePlan,
        deadline: U256,
        permit: &ExecutionPermit,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        if &params.route_key != permit.route_key() {
            eyre::bail!(
                "execution permit route {} does not match built route {}",
                permit.route_key().key_string(),
                params.route_key.key_string()
            );
        }
        if candidate.snapshot_id != permit.snapshot_id() {
            eyre::bail!("candidate snapshot does not match permit");
        }
        let actual_protocols = params
            .pool_types
            .iter()
            .copied()
            .map(protocol_kind_for_pool_type_byte)
            .collect::<Result<Vec<_>>>()?;
        if actual_protocols != permit.route_key().protocols
            || actual_protocols.len() != permit.route_key().hop_count as usize
        {
            eyre::bail!(
                "execution permit route {} does not match calldata route",
                permit.route_key().key_string()
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
        // Re-validate fee/profile against current cache.
        self.context
            .block_fee_contexts
            .matching(permit.block_fee_context())?;
        let current_quote = self.context.gas_profile.quote(permit.route_key())?;
        let current_fee_plan = FeePolicy::new(
            self.config.default_priority_fee_wei,
            self.config.block_gas_limit_reserve,
        )
        .build(&current_quote, permit.block_fee_context())?;
        // Allow caller-provided fee_plan only when it still matches re-quoted base
        // gas limit/identity; replacements may raise fees above the base plan.
        if current_fee_plan.gas_limit != fee_plan.gas_limit
            || current_fee_plan.expected_gas_used != fee_plan.expected_gas_used
            || current_fee_plan.profile_identity != fee_plan.profile_identity
            || current_fee_plan.block_fee_context != fee_plan.block_fee_context
        {
            eyre::bail!("gas profile or fee context changed before signing");
        }
        if fee_plan.max_fee_per_gas < current_fee_plan.max_fee_per_gas
            || fee_plan.max_priority_fee_per_gas < current_fee_plan.max_priority_fee_per_gas
        {
            eyre::bail!("replacement fee plan must not undercut the re-quoted base fees");
        }

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

        let contract = IArbitrageExecutor::new(
            self.context.executor_contract,
            &self.context.provider,
        );
        let call = contract.executeArbitrage(
            params.amount_in,
            params.token_path.clone(),
            params.pool_addresses.clone(),
            params.pool_types.clone(),
            outs,
            min_profit,
            deadline,
        );
        let calldata = call.calldata().clone();
        let calldata_digest = keccak256(calldata.as_ref());
        let tx = TransactionRequest::default()
            .with_to(self.context.executor_contract)
            .with_input(calldata)
            .with_nonce(permit.nonce())
            .with_gas_limit(fee_plan.gas_limit)
            .with_max_fee_per_gas(fee_plan.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fee_plan.max_priority_fee_per_gas)
            .with_chain_id(self.config.chain_id)
            .with_value(U256::ZERO);
        self.sign_and_wrap(
            tx,
            wallet,
            fee_plan,
            PreparedPayload::Execute { params, candidate },
            calldata_digest,
            permit,
        )
        .await
    }

    async fn prepare_cancel(
        &self,
        to: Address,
        gas_limit: u64,
        fee_plan: FeePlan,
        permit: &ExecutionPermit,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        if fee_plan.profile_identity != FeePlan::CANCEL_PROFILE_IDENTITY {
            eyre::bail!("cancel fee plan must use the cancel-intrinsic profile identity");
        }
        if gas_limit != fee_plan.gas_limit {
            eyre::bail!("cancel gas_limit mismatch with fee plan");
        }
        // No snapshot readiness / route profile checks for cancel.
        let calldata = Bytes::new();
        let calldata_digest = keccak256(calldata.as_ref());
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_input(calldata)
            .with_nonce(permit.nonce())
            .with_gas_limit(fee_plan.gas_limit)
            .with_max_fee_per_gas(fee_plan.max_fee_per_gas)
            .with_max_priority_fee_per_gas(fee_plan.max_priority_fee_per_gas)
            .with_chain_id(self.config.chain_id)
            .with_value(U256::ZERO);
        self.sign_and_wrap(
            tx,
            wallet,
            fee_plan,
            PreparedPayload::Cancel { to, gas_limit },
            calldata_digest,
            permit,
        )
        .await
    }

    /// Broadcast a previously recorded signed submission.
    pub async fn broadcast(&self, signed: &SignedSubmission) -> Result<B256> {
        let pending = self
            .context
            .provider
            .send_raw_transaction(signed.raw.as_ref())
            .await?;
        Ok(*pending.tx_hash())
    }

    /// Outcome-returning receipt read for the intent receipt tracker.
    pub async fn fetch_receipt_outcome(
        &self,
        tx_hash: B256,
    ) -> Result<Option<ReceiptOutcome>> {
        let Some(receipt) = self
            .context
            .provider
            .get_transaction_receipt(tx_hash)
            .await?
        else {
            return Ok(None);
        };
        let Some(block_hash) = receipt.block_hash() else {
            return Ok(None);
        };
        let Some(block_number) = receipt.block_number() else {
            return Ok(None);
        };
        // Mantle/OP-stack L1 fee is not always exposed on the generic receipt.
        // When absent, mark execution-layer-only so downstream cost accounting
        // does not treat max_fee_per_gas as paid cost.
        let l1_fee = None;
        Ok(Some(ReceiptOutcome {
            success: receipt.status(),
            block_number,
            block_hash,
            gas_used: receipt.gas_used(),
            effective_gas_price: receipt.effective_gas_price(),
            l1_fee,
            execution_layer_only: l1_fee.is_none(),
        }))
    }

    /// Build an Execute fee plan from the measured profile + permit context.
    pub fn build_execute_fee_plan(
        &self,
        permit: &ExecutionPermit,
    ) -> Result<FeePlan> {
        let quote = self.context.gas_profile.quote(permit.route_key())?;
        let fee_context = self
            .context
            .block_fee_contexts
            .matching(permit.block_fee_context())?;
        Ok(FeePolicy::new(
            self.config.default_priority_fee_wei,
            self.config.block_gas_limit_reserve,
        )
        .build(&quote, &fee_context)?)
    }

    /// Finite deadline from the permit's header timestamp (never wall clock).
    pub fn deadline_from_permit(&self, permit: &ExecutionPermit) -> Result<U256> {
        deadline_from_header_timestamp(
            permit.header().block_timestamp,
            self.config.execution_deadline_secs,
        )
        .map_err(|_| {
            eyre::eyre!(
                "deadline overflow for header timestamp {} + {}",
                permit.header().block_timestamp,
                self.config.execution_deadline_secs
            )
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
        if !receipt.status() || receipt.block_hash().is_none() {
            eyre::bail!(
                "transaction {} did not produce a canonical successful receipt",
                submitted.tx_hash
            );
        }
        self.qualify_receipt_gas(submitted, receipt.gas_used())
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
            // TODO(M2-1): replace this with the verified crossing-bucket pipeline before live senders are enabled.
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
