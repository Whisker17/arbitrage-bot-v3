//! `ShadowExecutionContext` — signerless shadow mode's wallet-free, RPC-free execution
//! context: wraps a plain [`ExecutionContext`] (built via a direct struct literal, never
//! [`ExecutionContext::from_provider`]'s live chain-id/codehash/WMNT RPC checks) plus the
//! override engine's approved-registration/allowlist/storage-layout config and the ledger.
//!
//! There is no owned `Executor` here — `build_final_request`/`revalidate_final_request`
//! are satisfied by delegating to `executor::build_final_request_impl`/
//! `revalidate_final_request_impl`, the same free functions the production `Executor`
//! itself now delegates to, so shadow mode shares that logic without constructing a
//! wallet-adjacent production type.
//!
//! No generic provider parameter: [`ExecutionContext`] already stores a type-erased
//! `DynProvider`, and [`ExecutionContext::provider`] hands out a cheap clone of it —
//! exactly what [`ShadowExecutionContext::build_preflight`] needs to construct a
//! [`ShadowSemanticCallExecutor`].

use std::path::Path;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use serde_json::Value;

use crate::execution::executor::{build_final_request_impl, revalidate_final_request_impl};
use crate::execution::fee_context::BlockFeeContextCache;
use crate::execution::final_request::{FinalRequest, FinalRequestParams};
use crate::execution::gas_profile::GasProfileArtifact;
use crate::execution::gas_runtime::{RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig};
use crate::execution::pipeline::ExecutionRequestBuilder;
use crate::execution::preflight::{ExecutionStage, RiskTieredPreflight};
use crate::execution::runtime_identity::{
    resolve_immutable_plan, BuildEvidence, ImmutableInputs, RuntimeIdentityError,
    VerifiedRuntimeIdentity,
};
use crate::execution::types::{ExecutionContext, ExecutionContextView, ExecutionPermit, ExecutorConfig};

use super::approved_pools::ApprovedPoolsConfig;
use super::call_executor::ShadowSemanticCallExecutor;
use super::invariant::ShadowInvariantSink;
use super::ledger::{LedgerError, LedgerRunHeader, ShadowLedgerWriter};
use super::manifest::ShadowOverrideManifest;
use super::moe_allowlist::MoeAllowlist;
use super::overrides::{
    build_shadow_state_override, check_pool_provenance, combine_provenance_outcomes,
    ShadowOverrideInputs,
};
use super::slots::SlotsError;
use super::wmnt_descriptor::{self, WmntDescriptor, WmntDescriptorError};

#[derive(Debug, thiserror::Error)]
pub enum ShadowContextError {
    #[error("shadow gas profile: {0}")]
    GasProfile(#[from] RuntimeGasProfileError),
    #[error("shadow runtime identity: {0}")]
    RuntimeIdentity(#[from] RuntimeIdentityError),
    #[error("shadow wmnt descriptor: {0}")]
    WmntDescriptor(#[from] WmntDescriptorError),
    #[error("shadow ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("shadow state override: {0}")]
    Slots(#[from] SlotsError),
}

/// Signerless shadow execution context (WHI-549): zero RPC at construction, no owned
/// `Executor`. Holds an [`ExecutionContext`] directly, and the override engine's inputs
/// (storage layout, patched runtime bytes, WMNT descriptor, Moe allowlist, approved
/// CREATE2 registrations, the pinned config-generation manifest) plus the ledger.
pub struct ShadowExecutionContext {
    context: ExecutionContext,
    config: ExecutorConfig,
    storage_layout: Value,
    patched_runtime: Vec<u8>,
    wmnt_descriptor: WmntDescriptor,
    manifest: ShadowOverrideManifest,
    moe_allowlist: MoeAllowlist,
    approved_pools: ApprovedPoolsConfig,
    ledger: Arc<ShadowLedgerWriter>,
}

impl ShadowExecutionContext {
    /// Builds a shadow context with **zero RPC calls**: `provider` is erased and stored
    /// for `eth_call` use only, never queried for chain id / deployed code / a live WMNT
    /// read (that's `ExecutionContext::from_provider`'s job for the production path).
    /// Instead, the executor's patched runtime bytes come from re-deriving the immutable
    /// plan from `evidence` (for
    /// [`crate::execution::runtime_identity::ValidatedImmutablePlan::patched_bytes`],
    /// injected into the shadow `eth_call`'s state override), while the gas profile is
    /// built from the caller-supplied, already-pinned `verified_identity` — matching what
    /// `manifest` was built from, with no redundant re-verification.
    #[allow(clippy::too_many_arguments)]
    pub fn new<P: Provider + Clone + 'static>(
        provider: P,
        executor_contract: Address,
        wmnt_address: Address,
        artifact: GasProfileArtifact,
        profile_config: RuntimeProfileConfig,
        evidence: &BuildEvidence,
        verified_identity: &VerifiedRuntimeIdentity,
        block_fee_contexts: Arc<BlockFeeContextCache>,
        executor_config: ExecutorConfig,
        wmnt_descriptor: WmntDescriptor,
        manifest: ShadowOverrideManifest,
        moe_allowlist: MoeAllowlist,
        approved_pools: ApprovedPoolsConfig,
        ledger_path: &Path,
        started_at_unix: u64,
    ) -> Result<Self, ShadowContextError> {
        wmnt_descriptor::check_wmnt_balance_slot_drift(&wmnt_descriptor)?;

        let plan = resolve_immutable_plan(
            evidence,
            ImmutableInputs { wmnt: wmnt_address },
            executor_config.chain_id,
        )?;

        let gas_profile = RuntimeGasProfile::from_artifact_with_identity(
            artifact,
            profile_config,
            verified_identity,
        )?;

        let context = ExecutionContext {
            provider: provider.erased(),
            executor_contract,
            wmnt_address,
            gas_profile,
            block_fee_contexts,
        };

        let header = LedgerRunHeader::from_manifest(&manifest, started_at_unix);
        let ledger = Arc::new(ShadowLedgerWriter::open(ledger_path, header)?);

        Ok(Self {
            context,
            config: executor_config,
            storage_layout: evidence.storage_layout().clone(),
            patched_runtime: plan.patched_bytes().to_vec(),
            wmnt_descriptor,
            manifest,
            moe_allowlist,
            approved_pools,
            ledger,
        })
    }

    pub fn manifest(&self) -> &ShadowOverrideManifest {
        &self.manifest
    }

    /// The executor config this context was built with — needed by call sites (e.g.
    /// [`crate::execution::pipeline::run_pipeline_head_closed`] wiring) that must read
    /// fee-policy fields (`default_priority_fee_wei`, `block_gas_limit_reserve`,
    /// `execution_deadline_secs`) without an owned `Executor` to read them from.
    pub fn config(&self) -> &ExecutorConfig {
        &self.config
    }

    /// Builds one candidate's semantic preflight: a [`RiskTieredPreflight`] wired to a
    /// fresh [`ShadowSemanticCallExecutor`] carrying this candidate's `StateOverride` and
    /// baked-in pool provenance, sharing the one underlying ledger file across every
    /// candidate's attempt via a cloned `Arc<ShadowLedgerWriter>`. Always constructed
    /// with `ExecutionStage::Shadow` and no approval config, since Shadow never consults
    /// one.
    pub fn build_preflight(
        &self,
        inputs: &ShadowOverrideInputs,
    ) -> Result<
        RiskTieredPreflight<ShadowSemanticCallExecutor<DynProvider>, ShadowInvariantSink<Arc<ShadowLedgerWriter>>>,
        ShadowContextError,
    > {
        let state_override = build_shadow_state_override(
            self.context.wmnt_address(),
            self.wmnt_descriptor.storage_shape,
            &self.storage_layout,
            &self.patched_runtime,
            inputs,
        )?;
        let provenance = combine_provenance_outcomes(inputs.pools.iter().map(|pool| {
            check_pool_provenance(pool, &self.moe_allowlist, &self.approved_pools)
        }));
        let call_executor = ShadowSemanticCallExecutor::new(
            self.context.provider(),
            state_override,
            Arc::clone(&self.ledger),
            provenance,
        );
        Ok(RiskTieredPreflight::with_sink(
            call_executor,
            ShadowInvariantSink::new(Arc::clone(&self.ledger)),
            ExecutionStage::Shadow,
            None,
        ))
    }

    pub fn ledger(&self) -> &Arc<ShadowLedgerWriter> {
        &self.ledger
    }

    /// Proof that this call site is running in shadow mode — see [`NoSend`].
    pub fn capability(&self) -> NoSend {
        NoSend(())
    }
}

impl ExecutionContextView for ShadowExecutionContext {
    fn executor_contract(&self) -> Address {
        self.context.executor_contract()
    }

    fn wmnt_address(&self) -> Address {
        self.context.wmnt_address()
    }

    fn gas_profile(&self) -> &RuntimeGasProfile {
        self.context.gas_profile()
    }

    fn block_fee_contexts(&self) -> &BlockFeeContextCache {
        self.context.block_fee_contexts()
    }
}

impl ExecutionRequestBuilder for ShadowExecutionContext {
    fn build_final_request(
        &self,
        params: FinalRequestParams,
        permit: ExecutionPermit,
    ) -> eyre::Result<FinalRequest> {
        build_final_request_impl(&self.context, &self.config, params, permit)
    }

    fn revalidate_final_request(&self, request: &FinalRequest) -> eyre::Result<()> {
        revalidate_final_request_impl(&self.context, request)
    }
}

/// Zero-sized proof that a call site holds no [`alloy::network::EthereumWallet`] and
/// therefore cannot reach [`crate::execution::executor::Executor::sign_final_request`]/
/// [`crate::execution::executor::Executor::prepare_submission`] — the only two `Executor`
/// methods that turn a [`FinalRequest`] into a signed, broadcastable transaction, and both
/// require a `&EthereumWallet` argument that shadow mode never constructs (no service ever
/// reads a private-key env var on the shadow branch of `main()`). This mirrors
/// `pipeline::{NoopPreflight, NoopDurableHook}`'s pattern of encoding a guarantee in the
/// type used rather than a runtime flag: the private tuple field means the only way to
/// obtain a `NoSend` is [`ShadowExecutionContext::capability`], so a `NoSend` argument in a
/// function signature is a compile-time witness that the caller is on the shadow path, not
/// the production one.
///
/// ```compile_fail
/// use amms::execution::NoSend;
/// fn forge() -> NoSend {
///     // The tuple field is private to this module: no external caller can construct one.
///     NoSend(())
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct NoSend(());

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn no_send_is_zero_sized_and_copy() {
        assert_eq!(std::mem::size_of::<NoSend>(), 0);
        let token = NoSend(());
        let _copy = token;
        let _original_still_usable = token;
    }
}
