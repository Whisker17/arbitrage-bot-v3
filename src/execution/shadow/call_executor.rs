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

use std::sync::Arc;

use alloy::eips::BlockId;
use alloy::providers::Provider;
use alloy::rpc::types::state::StateOverride;

use super::super::final_request::{final_request_digest, FinalRequest};
use super::super::preflight::{
    classify_call_error, BlockTag, CallOutcome, SemanticCallError, SemanticCallExecutor,
};
use super::ledger::ShadowLedgerWriter;
use super::manifest::PoolProvenanceOutcome;

/// Shadow-mode [`SemanticCallExecutor`]: identical to `ProviderSemanticCallExecutor`
/// except the `eth_call` carries a pre-built [`StateOverride`], and every call records
/// its candidate's pool provenance to the ledger first.
pub struct ShadowSemanticCallExecutor<P> {
    provider: P,
    state_override: StateOverride,
    ledger: Arc<ShadowLedgerWriter>,
    provenance: PoolProvenanceOutcome,
}

impl<P> ShadowSemanticCallExecutor<P> {
    pub(crate) fn new(
        provider: P,
        state_override: StateOverride,
        ledger: Arc<ShadowLedgerWriter>,
        provenance: PoolProvenanceOutcome,
    ) -> Self {
        Self {
            provider,
            state_override,
            ledger,
            provenance,
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
            if let Err(error) = self
                .ledger
                .record_provenance(digest, self.provenance.clone())
            {
                tracing::error!(
                    target: "execution.shadow",
                    ?error,
                    "failed to record shadow pool provenance to the ledger"
                );
            }
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
    use crate::execution::intent::PreparedPayload;
    use crate::execution::preflight::{PreflightOutcome, RpcErrorClass};
    use crate::state_space::{BlockHeaderContext, SnapshotId};

    use super::super::ledger::LedgerRunHeader;
    use super::super::manifest::ShadowOverrideManifest;

    /// Returns the writer alongside its owning `TempDir` -- the caller must keep the
    /// `TempDir` bound for the test's duration so the ledger file isn't cleaned up out
    /// from under an in-progress write.
    fn fixture_ledger() -> (Arc<ShadowLedgerWriter>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let manifest = ShadowOverrideManifest {
            storage_layout_digest: B256::repeat_byte(0x11),
            wmnt_descriptor_digest: B256::repeat_byte(0x22),
            moe_allowlist_digest: B256::repeat_byte(0x33),
            identity_digest: B256::repeat_byte(0x44),
        };
        let header = LedgerRunHeader::from_manifest(&manifest, 1_700_000_000);
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
            route: route_key,
            fee_context: block_fee_context,
            gas_profile_identity: "test-profile".to_string(),
        };

        FinalRequest::new(
            transaction,
            fee_plan,
            PreparedPayload::Cancel {
                to: Address::ZERO,
                gas_limit: 21_000,
            },
            B256::ZERO,
            7,
            submitted_at,
            from,
            identity,
            U256::ZERO,
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
            PoolProvenanceOutcome::Create2CheckSkipped,
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
            PoolProvenanceOutcome::Create2CheckSkipped,
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
            PoolProvenanceOutcome::Create2CheckSkipped,
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
        let manifest = ShadowOverrideManifest {
            storage_layout_digest: B256::repeat_byte(0x11),
            wmnt_descriptor_digest: B256::repeat_byte(0x22),
            moe_allowlist_digest: B256::repeat_byte(0x33),
            identity_digest: B256::repeat_byte(0x44),
        };
        let header = LedgerRunHeader::from_manifest(&manifest, 1_700_000_000);
        let ledger = Arc::new(ShadowLedgerWriter::open(&path, header).unwrap());

        let executor = ShadowSemanticCallExecutor::new(
            provider,
            StateOverride::default(),
            Arc::clone(&ledger),
            PoolProvenanceOutcome::MoeAllowlisted,
        );
        let request = fixture_request();
        let digest = final_request_digest(&request).unwrap();

        executor.call(&request, BlockTag::Latest).await.unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        let rows: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["row_type"], "run_header");
        assert_eq!(rows[1]["row_type"], "provenance");
        assert_eq!(rows[1]["digest"], digest.0.to_string());
        assert_eq!(rows[1]["outcome"], "moe_allowlisted");
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
