//! `ShadowSemanticCallExecutor` — the shadow-mode [`SemanticCallExecutor`] implementation.
//!
//! Issues the exact same `.call(tx).block(block)` shape as
//! [`ProviderSemanticCallExecutor`](super::super::preflight::ProviderSemanticCallExecutor),
//! plus a `.overrides(state_override)` — no new RPC method, no new error classification.
//! The `StateOverride` is baked in at construction time rather than derived inside
//! `call()`: a [`FinalRequest`] carries only opaque, already-encoded transaction bytes
//! (see `final_request.rs`) and no decoded candidate context (pool, pool type, tokens,
//! venue), so there is nothing in `call()`'s own arguments to build a per-candidate
//! override from. The caller (`ShadowExecutionContext`, task #11) builds one
//! `ShadowSemanticCallExecutor` per candidate, using `overrides::build_shadow_state_override`
//! fed from the candidate context it still has on hand *before* the pipeline erases it
//! into a `FinalRequest`.
//!
//! For the same reason, the candidate's already-computed [`PoolProvenanceOutcome`] (see
//! `overrides::check_pool_provenance`/`combine_provenance_outcomes`) is also baked in at
//! construction, and `call()` records it to the ledger keyed by `final_request_digest`
//! *before* issuing the `eth_call` — `final_request_digest` is a pure function of the
//! already-fully-built `FinalRequest` `call()` receives, so recomputing it here yields
//! exactly the digest `RiskTieredPreflight::preflight` and `prepare_pipeline_head` derive
//! from the same request, satisfying `ShadowLedgerWriter::record_provenance`'s ordering
//! requirement (provenance row before the matching candidate row) with no pipeline change.
//!
//! A [`PoolProvenanceOutcome::Rejected`] candidate's `eth_call` is never issued at all:
//! `call()` short-circuits to `CallOutcome::EnvUnsupported` right after recording
//! provenance. An unverifiable/rejected pool is an environment limitation — candidate
//! routes cannot manufacture their own registration authority — not an on-chain revert,
//! so it must not be conflated with `Revert`. Without this short-circuit, a rejected
//! pool's provenance was only ever logged — the real `eth_call` still ran and could pass.

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::primitives::U256;
use alloy::providers::Provider;
use alloy::rpc::types::state::StateOverride;

use super::super::final_request::{final_request_digest, FinalRequest};
use super::super::intent::PreparedPayload;
use super::super::preflight::{
    classify_call_error, BlockTag, CallOutcome, SemanticCallError, SemanticCallExecutor,
};
use super::context::NoSend;
use super::ledger::{ProfitBasis, ShadowLedgerWriter};
use super::manifest::PoolProvenanceOutcome;
use super::overrides::ShadowRouteSummary;

/// Shadow-mode [`SemanticCallExecutor`]: identical to `ProviderSemanticCallExecutor`
/// except the `eth_call` carries a pre-built [`StateOverride`], and every call records
/// its candidate's pool provenance to the ledger first.
pub struct ShadowSemanticCallExecutor<P> {
    provider: P,
    state_override: StateOverride,
    ledger: Arc<ShadowLedgerWriter>,
    _capability: NoSend,
    /// The combined (worst-case) outcome across every hop — used for the
    /// `EnvUnsupported` short-circuit below.
    provenance: PoolProvenanceOutcome,
    /// Every hop's own outcome, in route order — recorded to the ledger alongside the
    /// combined `provenance` so a multi-hop route's coverage is independently
    /// auditable, not just its worst hop.
    hop_provenance: Vec<PoolProvenanceOutcome>,
    /// Which route this candidate is, independent of the transaction built for it — see
    /// [`ShadowRouteSummary`].
    route: ShadowRouteSummary,
}

impl<P> ShadowSemanticCallExecutor<P> {
    pub(crate) fn new(
        provider: P,
        state_override: StateOverride,
        ledger: Arc<ShadowLedgerWriter>,
        provenance: PoolProvenanceOutcome,
        hop_provenance: Vec<PoolProvenanceOutcome>,
        route: ShadowRouteSummary,
        capability: NoSend,
    ) -> Self {
        Self {
            provider,
            state_override,
            ledger,
            _capability: capability,
            provenance,
            hop_provenance,
            route,
        }
    }
}

impl<P: Provider + Send + Sync> SemanticCallExecutor for ShadowSemanticCallExecutor<P> {
    async fn call(
        &self,
        request: &FinalRequest,
        tag: BlockTag,
    ) -> Result<CallOutcome, SemanticCallError> {
        // Best-effort: a ledger write failure is an operational fault in an
        // already-non-crash-safe append log (see `ledger.rs`'s doc comment on its
        // `flush()`-only durability), never a reason to mask the real semantic-call
        // outcome this method exists to produce.
        if let Ok(digest) = final_request_digest(request) {
            if let Err(error) = self.ledger.record_provenance(
                digest,
                self.provenance.clone(),
                self.hop_provenance.clone(),
            ) {
                tracing::error!(
                    target: "execution.shadow",
                    ?error,
                    "failed to record shadow pool provenance to the ledger"
                );
            }
            let (gross_profit, net_profit) = match &request.payload {
                PreparedPayload::Execute { params, .. } => {
                    let final_amount_out = params
                        .step_amounts_out
                        .last()
                        .copied()
                        .unwrap_or(U256::ZERO);
                    (
                        final_amount_out.saturating_sub(params.amount_in),
                        params.expected_net_profit_mnt_wei,
                    )
                }
                PreparedPayload::Cancel { .. } => (U256::ZERO, U256::ZERO),
            };
            if let Err(error) = self.ledger.record_context(
                digest,
                request.identity(),
                &self.route,
                gross_profit,
                net_profit,
                ProfitBasis::Simulated,
            ) {
                tracing::error!(
                    target: "execution.shadow",
                    ?error,
                    "failed to record shadow execution context to the ledger"
                );
            }
        }

        if let PoolProvenanceOutcome::Rejected(reason) = &self.provenance {
            return Ok(CallOutcome::EnvUnsupported(reason.clone()));
        }

        let block = match tag {
            BlockTag::Latest => BlockId::latest(),
            BlockTag::Pending => BlockId::pending(),
        };
        match self
            .provider
            .call(request.transaction.clone())
            .block(block)
            .overrides(self.state_override.clone())
            .await
        {
            Ok(_) => Ok(CallOutcome::Success),
            Err(err) => classify_call_error(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::network::TransactionBuilder;
    use alloy::primitives::{Address, Bytes, B256, U256};
    use alloy::providers::ProviderBuilder;
    use alloy::rpc::types::TransactionRequest;
    use alloy::transports::mock::Asserter;

    use crate::execution::fee_context::{BlockFeeContext, FeePlan};
    use crate::execution::gas_profile::{ProtocolKind, RouteKey};
    use crate::execution::identity::ExecutionIdentity;
    use crate::execution::intent::{CandidateRef, PreparedPayload};
    use crate::execution::preflight::{PreflightOutcome, RpcErrorClass};
    use crate::state_space::{BlockHeaderContext, SnapshotId};

    use super::super::approved_pools::ApprovedPoolProtocol;
    use super::super::context::test_capability;
    use super::super::ledger::{LedgerRunHeader, RunMetadata};
    use super::super::manifest::{Create2Proof, ShadowOverrideManifest};

    fn sample_route() -> ShadowRouteSummary {
        ShadowRouteSummary {
            opportunity_id: B256::repeat_byte(0x99),
            ordered_pools: vec![Address::repeat_byte(0xEE)],
            amount_in: U256::from(1_000u64),
        }
    }

    fn sample_create2_proof() -> Create2Proof {
        Create2Proof {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory: Address::repeat_byte(0x33),
            init_code_hash: B256::repeat_byte(0x44),
            salt: B256::repeat_byte(0x55),
        }
    }

    fn sample_manifest() -> ShadowOverrideManifest {
        ShadowOverrideManifest {
            storage_layout_digest: B256::repeat_byte(0x11),
            wmnt_descriptor_digest: B256::repeat_byte(0x22),
            moe_allowlist_digest: B256::repeat_byte(0x33),
            identity_digest: B256::repeat_byte(0x44),
            approved_pools_digest: B256::repeat_byte(0x55),
            threshold_config_digest: B256::repeat_byte(0x66),
            profile_digest: B256::repeat_byte(0x77),
            override_digest: B256::repeat_byte(0x88),
        }
    }

    fn sample_metadata(started_at_unix: u64) -> RunMetadata {
        RunMetadata {
            run_id: "test-run".to_string(),
            git_commit: "deadbeef".to_string(),
            chain_id: 5000,
            service: "test-service".to_string(),
            executor_contract: Address::repeat_byte(0xAB),
            wmnt_address: Address::repeat_byte(0xCD),
            started_at_unix,
        }
    }

    /// Returns the writer alongside its owning `TempDir` -- the caller must keep the
    /// `TempDir` bound for the test's duration so the ledger file isn't cleaned up out
    /// from under an in-progress write.
    fn fixture_ledger() -> (Arc<ShadowLedgerWriter>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        (
            Arc::new(ShadowLedgerWriter::open(&path, header).unwrap()),
            dir,
        )
    }

    fn fixture_request() -> FinalRequest {
        let from = Address::repeat_byte(0x11);
        let to = Address::repeat_byte(0xAB);

        let transaction = TransactionRequest::default()
            .with_chain_id(5000)
            .with_nonce(7)
            .with_max_priority_fee_per_gas(1_500_000_000)
            .with_max_fee_per_gas(3_000_000_000)
            .with_gas_limit(500_000)
            .with_to(to)
            .with_value(U256::ZERO)
            .with_input(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        let block_fee_context = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1_000_000_000,
            block_gas_limit: 30_000_000,
        };
        let fee_plan = FeePlan {
            block_fee_context: block_fee_context.clone(),
            gas_limit: 500_000,
            expected_gas_used: 400_000,
            expected_gas_cost: U256::from(1_500_000_000_000_000u128),
            max_fee_per_gas: 3_000_000_000,
            max_priority_fee_per_gas: 1_500_000_000,
            profile_identity: "test-profile".to_string(),
        };
        let route_key = RouteKey::new(vec![ProtocolKind::V2]).expect("valid route key");
        let submitted_at = SnapshotId::new(5000, 1, B256::ZERO);
        let identity = ExecutionIdentity {
            snapshot_id: submitted_at,
            header: BlockHeaderContext::new(B256::ZERO, 0),
            pool_universe_fingerprint: B256::ZERO,
            route: route_key.clone(),
            fee_context: block_fee_context,
            gas_profile_identity: "test-profile".to_string(),
        };

        FinalRequest::new(
            transaction,
            fee_plan,
            PreparedPayload::Execute {
                params: crate::execution::types::ExecutionParams {
                    amount_in: U256::from(1_000u64),
                    route_key: route_key.clone(),
                    crossing_buckets_verified: false,
                    token_path: vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)],
                    pool_addresses: vec![Address::repeat_byte(0x03)],
                    pool_types: vec![0],
                    pool_tokens: vec![(Address::repeat_byte(0x01), Address::repeat_byte(0x02))],
                    expected_reserves_u112: vec![],
                    step_amounts_out: vec![U256::from(1_100u64)],
                    min_amount_out: U256::from(1_100u64),
                    expected_net_profit_mnt_wei: U256::from(77u64),
                },
                candidate: CandidateRef {
                    snapshot_id: submitted_at,
                    header: BlockHeaderContext::new(B256::ZERO, 0),
                    pool_universe_fingerprint: B256::ZERO,
                    route_key: route_key.clone(),
                    amount_in: U256::from(1_000u64),
                },
            },
            B256::ZERO,
            7,
            submitted_at,
            from,
            identity,
            U256::from(50u64),
            U256::ZERO,
        )
    }

    #[tokio::test]
    async fn call_reports_success_when_the_overridden_call_does_not_revert() {
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::new());
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let (ledger, _dir) = fixture_ledger();
        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            ledger,
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            vec![PoolProvenanceOutcome::Verified(sample_create2_proof())],
            sample_route(),
            test_capability(),
        );
        let outcome = executor
            .call(&fixture_request(), BlockTag::Latest)
            .await
            .unwrap();

        assert!(matches!(outcome, CallOutcome::Success));
    }

    #[tokio::test]
    async fn call_classifies_a_revert_through_the_shared_classifier() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("execution reverted: insufficient liquidity");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let (ledger, _dir) = fixture_ledger();
        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            ledger,
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            vec![PoolProvenanceOutcome::Verified(sample_create2_proof())],
            sample_route(),
            test_capability(),
        );
        let outcome = executor
            .call(&fixture_request(), BlockTag::Latest)
            .await
            .unwrap();

        match outcome {
            CallOutcome::Revert(reason) => {
                assert!(reason.contains("insufficient liquidity"));
            }
            other => panic!("expected Revert, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn call_reports_a_transport_error_distinctly_from_a_revert() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("internal error");
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let (ledger, _dir) = fixture_ledger();
        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            ledger,
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            vec![PoolProvenanceOutcome::Verified(sample_create2_proof())],
            sample_route(),
            test_capability(),
        );
        let error = executor
            .call(&fixture_request(), BlockTag::Latest)
            .await
            .unwrap_err();

        match error {
            SemanticCallError::Rpc { class, .. } => {
                assert_eq!(class, RpcErrorClass::ErrorResponse);
            }
        }
    }

    #[tokio::test]
    async fn call_records_the_baked_in_provenance_before_issuing_the_eth_call() {
        let asserter = Asserter::new();
        asserter.push_success(&Bytes::new());
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let ledger = Arc::new(ShadowLedgerWriter::open(&path, header).unwrap());

        let route = sample_route();
        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            Arc::clone(&ledger),
            PoolProvenanceOutcome::MoeAllowlisted,
            vec![PoolProvenanceOutcome::MoeAllowlisted],
            route.clone(),
            test_capability(),
        );
        let request = fixture_request();
        let digest = final_request_digest(&request).unwrap();

        executor.call(&request, BlockTag::Latest).await.unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["row_type"], "run_header");
        assert_eq!(rows[1]["row_type"], "provenance");
        assert_eq!(rows[1]["digest"], digest.0.to_string());
        assert_eq!(rows[1]["outcome"], "moe_allowlisted");
        assert_eq!(
            rows[1]["hop_outcomes"],
            serde_json::json!(["moe_allowlisted"])
        );
        assert_eq!(rows[2]["row_type"], "context");
        assert_eq!(rows[2]["digest"], digest.0.to_string());
        assert_eq!(rows[2]["opportunity_id"], route.opportunity_id.to_string());
        assert_eq!(
            rows[2]["ordered_pools"],
            serde_json::json!([route.ordered_pools[0].to_string()])
        );
        assert_eq!(rows[2]["amount_in"], route.amount_in.to_string());
        let expected_gross = U256::from(100u64);
        let expected_net = U256::from(77u64);
        assert_eq!(rows[2]["gross_profit"], expected_gross.to_string());
        assert_eq!(rows[2]["net_profit"], expected_net.to_string());
        assert_eq!(rows[2]["profit_basis"], "simulated");
    }

    #[tokio::test]
    async fn call_short_circuits_on_rejected_provenance_without_issuing_the_eth_call() {
        // No responses queued at all: if `call()` ever reached the real `eth_call`, the
        // mocked transport would panic/error on an empty queue, failing this test.
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let (ledger, _dir) = fixture_ledger();
        let rejection_reason = "pool not present on the Moe LB allowlist".to_string();
        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            ledger,
            PoolProvenanceOutcome::Rejected(rejection_reason.clone()),
            vec![PoolProvenanceOutcome::Rejected(rejection_reason.clone())],
            sample_route(),
            test_capability(),
        );

        let outcome = executor
            .call(&fixture_request(), BlockTag::Latest)
            .await
            .unwrap();

        match outcome {
            CallOutcome::EnvUnsupported(reason) => assert_eq!(reason, rejection_reason),
            other => panic!("expected EnvUnsupported, got {other:?}"),
        }
    }

    // Guards the invariant this executor relies on structurally: nothing in this module
    // (or `SemanticCallExecutor`) ever produces `SkippedApproved`/`SampledOut` — those are
    // policy-layer outcomes for `RiskTieredPreflight`, never a `CallOutcome`. Recorded here
    // as a type-level sanity check rather than a runtime assertion.
    #[allow(dead_code)]
    fn call_outcome_is_not_a_preflight_outcome(_: CallOutcome) -> Option<PreflightOutcome> {
        None
    }
}
