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
use super::final_request::{FinalRequest, FinalRequestParams};
use super::gas_profile::ProtocolKind;
use super::identity::ExecutionIdentity;
use super::intent::{PrepareRequest, PreparedPayload, ReceiptOutcome, SignedSubmission};
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
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::SolCall;
    use alloy::transports::mock::Asserter;
    use std::{path::PathBuf, str::FromStr, sync::Arc};

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
        block_fee_contexts
            .publish(block_fee_context.clone())
            .unwrap();
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
            Address::ZERO,
            route_key,
            block_fee_context,
            0,
            crate::state_space::SnapshotId::new(5000, 42, alloy::primitives::B256::ZERO),
            crate::state_space::BlockHeaderContext::new(
                alloy::primitives::B256::ZERO,
                1_700_000_000,
            ),
            alloy::primitives::B256::repeat_byte(1),
        );

        let error = executor
            .execute(&provider, &params, &permit)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("fused Executor::execute is retired"));
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn final_request_build_and_revalidation_are_wallet_free_and_wrong_wallet_fails() {
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
        let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
        let fee_context = BlockFeeContext {
            block_number: 42,
            block_hash: B256::repeat_byte(1),
            base_fee_per_gas: 50_000_000_000,
            block_gas_limit: 60_000_000,
        };
        let fee_contexts = Arc::new(BlockFeeContextCache::default());
        fee_contexts.publish(fee_context.clone()).unwrap();
        let executor = Executor::new(
            ExecutionContext {
                provider: provider.erased(),
                executor_contract: Address::ZERO,
                wmnt_address: Address::ZERO,
                gas_profile,
                block_fee_contexts: fee_contexts,
            },
            ExecutorConfig::default(),
        );
        let signer = PrivateKeySigner::from_str(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        let wrong_signer = PrivateKeySigner::from_str(
            "0000000000000000000000000000000000000000000000000000000000000002",
        )
        .unwrap();
        let snapshot_id = crate::state_space::SnapshotId::new(5000, 42, fee_context.block_hash);
        let header = crate::state_space::BlockHeaderContext::new(B256::ZERO, 1_700_000_000);
        let fingerprint = B256::repeat_byte(9);
        let permit = ExecutionPermit::new(
            crate::execution::intent::test_support::authority(),
            signer.address(),
            route_key.clone(),
            fee_context,
            0,
            snapshot_id,
            header,
            fingerprint,
        );
        let fee_plan = executor.build_execute_fee_plan(&permit).unwrap();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);
        let final_out = amount_in + fee_plan.expected_gas_cost + U256::from(1_000u64);
        let params = ExecutionParams {
            amount_in,
            route_key: route_key.clone(),
            crossing_buckets_verified: false,
            token_path: vec![Address::ZERO; 3],
            pool_addresses: vec![Address::repeat_byte(3), Address::repeat_byte(4)],
            pool_types: vec![0, 0],
            pool_tokens: vec![(Address::ZERO, Address::ZERO); 2],
            expected_reserves_u112: vec![alloy::primitives::aliases::U112::ZERO; 4],
            step_amounts_out: vec![amount_in, final_out],
            min_amount_out: final_out,
            expected_net_profit_mnt_wei: U256::from(1_000u64),
        };
        let candidate = crate::execution::CandidateRef {
            snapshot_id,
            header,
            pool_universe_fingerprint: fingerprint,
            route_key,
            amount_in,
        };
        let final_request = executor
            .build_final_request(
                FinalRequestParams {
                    params,
                    candidate,
                    fee_plan,
                    deadline: U256::from(1_700_000_060u64),
                },
                permit,
            )
            .unwrap();
        executor.revalidate_final_request(&final_request).unwrap();
        assert_eq!(final_request.deadline(), U256::from(1_700_000_060u64));
        let input = final_request
            .transaction
            .input
            .input()
            .cloned()
            .expect("final request must carry calldata");
        let decoded = IArbitrageExecutor::executeArbitrageCall::abi_decode(input.as_ref())
            .expect("executeArbitrage calldata must decode");
        assert_eq!(decoded.minProfit, final_request.min_profit());
        assert_eq!(decoded.deadline, final_request.deadline());
        let error = executor
            .sign_final_request(final_request, &EthereumWallet::from(wrong_signer))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("wrong wallet"));
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
        permit: ExecutionPermit,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        match request {
            PrepareRequest::Execute {
                params,
                candidate,
                fee_plan,
                deadline,
            } => {
                let request = self.build_final_request(
                    FinalRequestParams {
                        params,
                        candidate,
                        fee_plan,
                        deadline,
                    },
                    permit,
                )?;
                self.sign_final_request(request, wallet).await
            }
            PrepareRequest::Cancel {
                to,
                gas_limit,
                fee_plan,
            } => {
                self.prepare_cancel(to, gas_limit, fee_plan, permit, wallet)
                    .await
            }
        }
    }

    async fn sign_and_wrap(
        &self,
        tx: TransactionRequest,
        wallet: &EthereumWallet,
        fee_plan: FeePlan,
        payload: PreparedPayload,
        calldata_digest: B256,
        nonce: u64,
        submitted_at: crate::state_space::SnapshotId,
    ) -> Result<SignedSubmission> {
        let envelope =
            <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::sign_request(wallet, tx)
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
            nonce,
            submitted_at,
        })
    }

    /// Build the exact Execute wire request without touching a wallet.
    ///
    /// Consuming the SM-minted permit makes reuse impossible.
    pub fn build_final_request(
        &self,
        request: FinalRequestParams,
        permit: ExecutionPermit,
    ) -> Result<FinalRequest> {
        build_final_request_impl(&self.context, &self.config, request, permit)
    }

    /// Wallet-free revalidation immediately before pause/lease/signing.
    pub fn revalidate_final_request(&self, request: &FinalRequest) -> Result<()> {
        revalidate_final_request_impl(&self.context, request)
    }

    /// Sign an already-finalized request. No fields are rebuilt with wallet state.
    pub async fn sign_final_request(
        &self,
        request: FinalRequest,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        self.revalidate_final_request(&request)?;
        let wallet_address =
            <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::default_signer_address(
                wallet,
            );
        if wallet_address != request.from {
            eyre::bail!(
                "wrong wallet: FinalRequest requires {}, wallet is {}",
                request.from,
                wallet_address
            );
        }
        self.sign_and_wrap(
            request.transaction,
            wallet,
            request.fee_plan,
            request.payload,
            request.calldata_digest,
            request.nonce,
            request.submitted_at,
        )
        .await
    }

    async fn prepare_cancel(
        &self,
        to: Address,
        gas_limit: u64,
        fee_plan: FeePlan,
        permit: ExecutionPermit,
        wallet: &EthereumWallet,
    ) -> Result<SignedSubmission> {
        if fee_plan.profile_identity != FeePlan::CANCEL_PROFILE_IDENTITY {
            eyre::bail!("cancel fee plan must use the cancel-intrinsic profile identity");
        }
        if gas_limit != fee_plan.gas_limit {
            eyre::bail!("cancel gas_limit mismatch with fee plan");
        }
        // No snapshot readiness / route profile checks for cancel.
        let (
            signer_address,
            _route,
            _permit_fee_context,
            nonce,
            snapshot_id,
            _header,
            _fingerprint,
        ) = permit.into_authorized_parts();
        if <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::default_signer_address(
            wallet,
        ) != signer_address
        {
            eyre::bail!("wrong wallet for cancel permit");
        }
        let calldata = Bytes::new();
        let calldata_digest = keccak256(calldata.as_ref());
        let tx = TransactionRequest::default()
            .with_to(to)
            .with_from(signer_address)
            .with_input(calldata)
            .with_nonce(nonce)
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
            nonce,
            snapshot_id,
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
    pub async fn fetch_receipt_outcome(&self, tx_hash: B256) -> Result<Option<ReceiptOutcome>> {
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
    pub fn build_execute_fee_plan(&self, permit: &ExecutionPermit) -> Result<FeePlan> {
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

    pub fn qualify_receipt_gas(&self, submitted: &SubmittedExecution, gas_used: u64) -> Result<()> {
        match submitted
            .fee_plan
            .qualify_receipt_gas(gas_used, self.config.receipt_gas_limit_utilization_bps)
        {
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

    pub async fn observe_receipt(&self, submitted: &SubmittedExecution) -> Result<()> {
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

/// Pure logic behind [`Executor::build_final_request`], parameterized over
/// `&ExecutionContext`/`&ExecutorConfig` rather than `&self` so a shadow-mode builder
/// (WHI-549) can call it without owning a production [`Executor`] or issuing any RPC.
/// Consuming the SM-minted permit makes reuse impossible.
pub(crate) fn build_final_request_impl(
    context: &ExecutionContext,
    config: &ExecutorConfig,
    request: FinalRequestParams,
    permit: ExecutionPermit,
) -> Result<FinalRequest> {
    let FinalRequestParams {
        params,
        candidate,
        fee_plan,
        deadline,
    } = request;
    let (
        signer_address,
        permit_route,
        permit_fee_context,
        nonce,
        permit_snapshot,
        permit_header,
        permit_fingerprint,
    ) = permit.into_authorized_parts();
    if params.route_key != permit_route {
        eyre::bail!(
            "execution permit route {} does not match built route {}",
            permit_route.key_string(),
            params.route_key.key_string()
        );
    }
    if candidate.snapshot_id != permit_snapshot || candidate.header != permit_header {
        eyre::bail!("candidate snapshot does not match permit");
    }
    if candidate.pool_universe_fingerprint != permit_fingerprint {
        eyre::bail!("candidate topology does not match permit");
    }
    let actual_protocols = params
        .pool_types
        .iter()
        .copied()
        .map(protocol_kind_for_pool_type_byte)
        .collect::<Result<Vec<_>>>()?;
    if actual_protocols != permit_route.protocols
        || actual_protocols.len() != permit_route.hop_count as usize
    {
        eyre::bail!(
            "execution permit route {} does not match calldata route",
            permit_route.key_string()
        );
    }
    if actual_protocols
        .iter()
        .any(|protocol| matches!(protocol, ProtocolKind::V3 | ProtocolKind::Moe))
        && !params.crossing_buckets_verified
    {
        eyre::bail!("V3/Moe execution requires verified crossing-bucket evidence before signing");
    }
    // Re-validate fee/profile against current cache.
    context.block_fee_contexts.matching(&permit_fee_context)?;
    let current_quote = context.gas_profile.quote(&permit_route)?;
    let current_fee_plan = FeePolicy::new(
        config.default_priority_fee_wei,
        config.block_gas_limit_reserve,
    )
    .build(&current_quote, &permit_fee_context)?;
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
    if config.enforce_non_loss && expected_net_profit.is_zero() {
        eyre::bail!("Abort execution: non-loss requirement not satisfied");
    }
    if expected_net_profit < config.min_net_profit_mnt_wei {
        eyre::bail!(
            "Skip execution: expected net profit {} < min required {}",
            expected_net_profit,
            config.min_net_profit_mnt_wei
        );
    }

    let mut outs = params.step_amounts_out.clone();
    let required_out = if config.include_gas_cost_in_min_out || config.enforce_non_loss {
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
        context.wmnt_address,
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

    let contract = IArbitrageExecutor::new(context.executor_contract, &context.provider);
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
        .with_to(context.executor_contract)
        .with_from(signer_address)
        .with_input(calldata)
        .with_nonce(nonce)
        .with_gas_limit(fee_plan.gas_limit)
        .with_max_fee_per_gas(fee_plan.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fee_plan.max_priority_fee_per_gas)
        .with_chain_id(config.chain_id)
        .with_value(U256::ZERO);
    let identity = ExecutionIdentity {
        snapshot_id: permit_snapshot,
        header: permit_header,
        pool_universe_fingerprint: permit_fingerprint,
        route: permit_route,
        fee_context: permit_fee_context,
        gas_profile_identity: fee_plan.profile_identity.clone(),
    };
    Ok(FinalRequest::new(
        tx,
        fee_plan,
        PreparedPayload::Execute { params, candidate },
        calldata_digest,
        nonce,
        permit_snapshot,
        signer_address,
        identity,
        min_profit,
        deadline,
    ))
}

/// Pure logic behind [`Executor::revalidate_final_request`], parameterized over
/// `&ExecutionContext` rather than `&self` so a shadow-mode builder (WHI-549) can call
/// it without owning a production [`Executor`].
pub(crate) fn revalidate_final_request_impl(
    context: &ExecutionContext,
    request: &FinalRequest,
) -> Result<()> {
    context
        .block_fee_contexts
        .matching(&request.identity().fee_context)?;
    let quote = context.gas_profile.quote(&request.identity().route)?;
    if quote.profile_identity != request.identity().gas_profile_identity {
        eyre::bail!("gas profile identity changed after FinalRequest build");
    }
    let PreparedPayload::Execute { params, candidate } = &request.payload else {
        eyre::bail!("FinalRequest must contain Execute payload");
    };
    if candidate.snapshot_id != request.identity().snapshot_id
        || candidate.header != request.identity().header
        || candidate.pool_universe_fingerprint != request.identity().pool_universe_fingerprint
        || params.route_key != request.identity().route
    {
        eyre::bail!("FinalRequest execution identity no longer matches payload");
    }
    Ok(())
}
