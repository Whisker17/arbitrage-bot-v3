//! `ShadowExecutionContext` — wires the real, wallet-free `Executor`/`ExecutionContext`
//! (see `executor.rs`, `types.rs`) to shadow mode's override engine and ledger.
//!
//! Holds one `Executor` for the whole run (its `ExecutionContext` is built once via
//! `ExecutionContext::from_provider`, performing the same chain-id/bytecode/WMNT
//! liveness checks production uses), plus everything needed to build a fresh
//! `RiskTieredPreflight` *per candidate*: each candidate's `StateOverride` is baked into
//! its own `ShadowSemanticCallExecutor` at construction time (see `call_executor.rs`), so
//! it cannot be shared across candidates the way the `Executor` itself is. The ledger is
//! held as an `Arc<ShadowLedgerWriter>` (see `ledger.rs`'s `PreflightAttemptSink` impl for
//! `Arc<ShadowLedgerWriter>`) so every per-candidate `RiskTieredPreflight` still writes
//! into the same underlying JSONL file.

use std::path::Path;
use std::sync::Arc;

use alloy::primitives::Address;
use alloy::providers::Provider;

use crate::execution::executor::Executor;
use crate::execution::fee_context::BlockFeeContextCache;
use crate::execution::gas_profile::GasProfileArtifact;
use crate::execution::gas_runtime::{
    RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig,
};
use crate::execution::preflight::{ExecutionStage, RiskTieredPreflight};
use crate::execution::runtime_identity::VerifiedRuntimeIdentity;
use crate::execution::types::{ExecutionContext, ExecutionContextView, ExecutorConfig};

use super::call_executor::ShadowSemanticCallExecutor;
use super::invariant::ShadowInvariantSink;
use super::ledger::{LedgerError, LedgerRunHeader, ShadowLedgerWriter};
use super::manifest::ShadowOverrideManifest;
use super::moe_allowlist::MoeAllowlist;
use super::overrides::{
    build_shadow_state_override, check_pool_provenance, combine_provenance_outcomes,
    ShadowOverrideInputs,
};
use super::slots::SlotError;
use super::wmnt_descriptor::WmntStorageShape;

#[derive(Debug, thiserror::Error)]
pub enum ShadowContextError {
    #[error("shadow gas profile: {0}")]
    GasProfile(#[from] RuntimeGasProfileError),
    #[error("shadow execution context: {0}")]
    ExecutionContext(String),
    #[error("shadow ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("shadow state override: {0}")]
    Override(#[from] SlotError),
}

/// Bundles the real, wallet-free `Executor` with everything shadow mode needs to build a
/// per-candidate semantic preflight: the compiled storage layout, the WMNT storage shape,
/// the pinned override manifest, and a shared ledger writer.
pub struct ShadowExecutionContext<P> {
    executor: Executor,
    provider: P,
    storage_layout: serde_json::Value,
    wmnt_storage_shape: WmntStorageShape,
    manifest: ShadowOverrideManifest,
    moe_allowlist: MoeAllowlist,
    ledger: Arc<ShadowLedgerWriter>,
}

impl<P: Provider + Clone + 'static> ShadowExecutionContext<P> {
    /// Builds the real `ExecutionContext` (live chain-id/bytecode/WMNT checks against
    /// `provider`), a non-compile-time-pinned `RuntimeGasProfile` verified against
    /// `verified_identity` (`RuntimeGasProfile::from_artifact_with_identity`), and opens
    /// the ledger with a header pinning `manifest`'s digests.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        provider: P,
        executor_contract: Address,
        wmnt_address: Address,
        artifact: GasProfileArtifact,
        profile_config: RuntimeProfileConfig,
        verified_identity: &VerifiedRuntimeIdentity,
        block_fee_contexts: Arc<BlockFeeContextCache>,
        executor_config: ExecutorConfig,
        storage_layout: serde_json::Value,
        wmnt_storage_shape: WmntStorageShape,
        manifest: ShadowOverrideManifest,
        moe_allowlist: MoeAllowlist,
        ledger_path: &Path,
        started_at_unix: u64,
    ) -> Result<Self, ShadowContextError> {
        let gas_profile = RuntimeGasProfile::from_artifact_with_identity(
            artifact,
            profile_config,
            verified_identity,
        )?;
        let context = ExecutionContext::from_provider(
            provider.clone(),
            executor_contract,
            wmnt_address,
            gas_profile,
            block_fee_contexts,
        )
        .await
        .map_err(|err| ShadowContextError::ExecutionContext(err.to_string()))?;
        let executor = Executor::new(context, executor_config);

        let header = LedgerRunHeader::from_manifest(&manifest, started_at_unix);
        let ledger = Arc::new(ShadowLedgerWriter::open(ledger_path, header)?);

        Ok(Self {
            executor,
            provider,
            storage_layout,
            wmnt_storage_shape,
            manifest,
            moe_allowlist,
            ledger,
        })
    }

    pub fn manifest(&self) -> &ShadowOverrideManifest {
        &self.manifest
    }

    /// The real, wallet-free `Executor` this shadow context wraps — needed to satisfy
    /// [`crate::execution::pipeline::run_pipeline_head_closed`]'s (and its
    /// `run_candidate_through_pipeline_head` service wrapper's) `executor: &Executor`
    /// parameter, exactly as the production path supplies it.
    pub fn executor(&self) -> &Executor {
        &self.executor
    }

    /// Builds a fresh, single-use `RiskTieredPreflight` for one candidate: its
    /// `StateOverride` (built from `inputs`, the candidate's still-decoded pool/token/
    /// venue context) is baked into a new `ShadowSemanticCallExecutor`, and the sink wraps
    /// a clone of the shared ledger `Arc` in a `ShadowInvariantSink` -- so every
    /// candidate's attempt still lands in the same underlying JSONL file, and a
    /// `SkippedApproved`/`SampledOut` outcome (which `preflight::classify` proves can
    /// never legitimately occur under `ExecutionStage::Shadow`) aborts loudly instead of
    /// being silently recorded. Always constructed with `ExecutionStage::Shadow` and no
    /// approval config, since Shadow never consults one.
    pub fn build_preflight(
        &self,
        inputs: &ShadowOverrideInputs,
    ) -> Result<
        RiskTieredPreflight<
            ShadowSemanticCallExecutor<P>,
            ShadowInvariantSink<Arc<ShadowLedgerWriter>>,
        >,
        ShadowContextError,
    > {
        let state_override = build_shadow_state_override(
            &self.storage_layout,
            self.executor.context.wmnt_address(),
            self.wmnt_storage_shape,
            inputs,
        )?;
        let provenance = combine_provenance_outcomes(
            inputs
                .pools
                .iter()
                .map(|pool| check_pool_provenance(pool, &self.moe_allowlist)),
        );
        let call_executor = ShadowSemanticCallExecutor::new(
            self.provider.clone(),
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

/// Zero-sized proof that a call site holds no [`alloy::network::EthereumWallet`] and
/// therefore cannot reach [`Executor::sign_final_request`]/[`Executor::prepare_submission`]
/// — the only two `Executor` methods that turn a [`super::super::final_request::FinalRequest`]
/// into a signed, broadcastable transaction, and both require a `&EthereumWallet` argument
/// that shadow mode never constructs (no service ever reads a private-key env var on the
/// shadow branch of `main()`). This mirrors `pipeline::{NoopPreflight, NoopDurableHook}`'s
/// pattern of encoding a guarantee in the type used rather than a runtime flag: the private
/// tuple field means the only way to obtain a `NoSend` is
/// [`ShadowExecutionContext::capability`], so a `NoSend` argument in a function signature is
/// a compile-time witness that the caller is on the shadow path, not the production one.
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
