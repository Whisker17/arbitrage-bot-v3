//! Canonical Execute send ordering (WHI-520 Revision 5).
//!
//! `build -> validate -> preflight slot -> validate -> begin_send(Execute) ->
//! acquire lease -> final validate -> sign -> durable hook ->
//! record_submission -> RPC handoff -> release`.

use std::sync::Arc;

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use eyre::{eyre, Result};

use super::fee_context::BlockFeeContext;
use super::final_request::{final_request_digest, FinalRequest, FinalRequestDigest, FinalRequestParams};
use super::identity::{ExecutionIdentityLease, ExecutionIdentitySource};
use super::intent::{CandidateRef, ChainNonceView, IntentError, IntentStateMachine, SignedSubmission};
use super::pause::{AttemptKind, PauseGate, Paused, SendGuard};
use super::types::ExecutionPermit;
use super::Executor;
use crate::state_space::SnapshotStatus;

/// Wallet-free construction/revalidation seam for a [`FinalRequest`].
///
/// Implementation-independent: production [`Executor`] implements this by delegating to
/// its existing inherent methods; a future shadow builder (WHI-549) can implement it
/// without constructing an `Executor` or a wallet.
pub trait ExecutionRequestBuilder {
    fn build_final_request(
        &self,
        params: FinalRequestParams,
        permit: ExecutionPermit,
    ) -> Result<FinalRequest>;

    fn revalidate_final_request(&self, request: &FinalRequest) -> Result<()>;
}

impl ExecutionRequestBuilder for Executor {
    fn build_final_request(
        &self,
        params: FinalRequestParams,
        permit: ExecutionPermit,
    ) -> Result<FinalRequest> {
        Executor::build_final_request(self, params, permit)
    }

    fn revalidate_final_request(&self, request: &FinalRequest) -> Result<()> {
        Executor::revalidate_final_request(self, request)
    }
}

/// WHI-521 plugs semantic preflight here. Default is a no-op pass.
#[allow(async_fn_in_trait)]
pub trait PreflightSlot: Send + Sync {
    async fn preflight(&self, request: &FinalRequest) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopPreflight;

impl PreflightSlot for NoopPreflight {
    async fn preflight(&self, _request: &FinalRequest) -> Result<()> {
        Ok(())
    }
}

/// WHI-524 plugs durable attempt recording here. Default is a no-op pass.
pub trait DurableSubmissionHook: Send + Sync {
    fn on_signed(&self, signed: &SignedSubmission, min_profit: U256, deadline: U256) -> Result<()>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NoopDurableHook;

impl DurableSubmissionHook for NoopDurableHook {
    fn on_signed(
        &self,
        _signed: &SignedSubmission,
        _min_profit: U256,
        _deadline: U256,
    ) -> Result<()> {
        Ok(())
    }
}

/// Opaque handles spanning sign → record → RPC handoff.
///
/// Field order is intentional: Rust drops fields in declaration order, so
/// `lease` precedes `pause` to release in reverse of acquisition
/// (`begin_send` then `acquire_send_lease`).
pub struct ExecuteSendGuards {
    pub lease: ExecutionIdentityLease,
    pub pause: SendGuard,
}

/// Metadata extracted from the exact FinalRequest consumed by signing.
///
/// Private fields prevent callers from supplying values that can diverge from
/// the authenticated Execute calldata.
pub struct AuthenticatedAttemptMeta {
    min_profit: U256,
    deadline: U256,
}

#[derive(Debug, thiserror::Error)]
pub enum PipelineError {
    #[error(transparent)]
    Pause(#[from] Paused),
    #[error(transparent)]
    Identity(#[from] super::identity::IdentityError),
    #[error(transparent)]
    Intent(#[from] IntentError),
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

/// Wallet-free build + live revalidation before any pause/lease/wallet use.
pub async fn build_and_validate_final_request(
    builder: &impl ExecutionRequestBuilder,
    identity_source: &impl ExecutionIdentitySource,
    params: FinalRequestParams,
    permit: ExecutionPermit,
) -> Result<FinalRequest, PipelineError> {
    let request = builder.build_final_request(params, permit)?;
    identity_source.validate(request.identity()).await?;
    builder.revalidate_final_request(&request)?;
    Ok(request)
}

/// Wallet-free head of the Execute pipeline, stopped before any pause/lease/wallet
/// use. Non-`Clone`: the only way to reach a terminal outcome is a consuming
/// continuation (e.g. [`PreparedPipelineHead::into_closed_outcome`]), so exactly one
/// continuation ever owns cleanup for a given preparation.
///
/// The head **owns** the state-machine handle and chain-nonce view that minted its
/// nonce, so cleanup can never target a different `IntentStateMachine` than the one
/// that reserved it, and [`Drop`] can run real cleanup for a head that is neither
/// consumed nor explicitly released.
#[must_use]
pub struct PreparedPipelineHead {
    sm: Arc<IntentStateMachine>,
    chain: ChainNonceView,
    nonce: u64,
    request: FinalRequest,
    digest: FinalRequestDigest,
    /// Set by the consuming continuation so [`Drop`] does not re-run cleanup.
    consumed: bool,
}

impl PreparedPipelineHead {
    pub fn digest(&self) -> FinalRequestDigest {
        self.digest
    }

    pub fn request(&self) -> &FinalRequest {
        &self.request
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Consume under the closed send gate: abort the in-flight preparation and
    /// reconcile the nonce back to Reserved/released against the same SM and chain
    /// view that reserved it. Never touches pause/lease/wallet/signing/broadcast.
    ///
    /// Public so out-of-crate continuations (WHI-525's E2E gate) can complete a head
    /// obtained from [`prepare_pipeline_head`] instead of leaking it. Ownership is
    /// marked consumed before cleanup runs, so a cleanup error surfaces to the caller
    /// exactly once rather than being retried by [`Drop`].
    pub fn into_closed_outcome(mut self) -> Result<HeadOutcome, PipelineError> {
        self.consumed = true;
        self.sm.abort_prepare(self.nonce)?;
        self.sm.reconcile(self.chain.clone())?;
        Ok(HeadOutcome {
            digest: self.digest,
        })
    }

    /// Consume for an open-send continuation that will actually sign and
    /// broadcast (WHI-555's E2E capability layer): marks the head consumed
    /// without aborting or reconciling, so the `Preparing` intent stays live.
    /// The caller becomes responsible for eventually calling
    /// `IntentStateMachine::record_submission_with_min_profit` (after
    /// signing) or `abort_prepare` + `reconcile` (on failure) against the
    /// returned `sm`/`chain` — the same obligations the production Execute
    /// tail's `finalize_execute_submission` already discharges. Unlike
    /// [`Self::into_closed_outcome`], this never touches the SM: it only
    /// hands back clones of the exact handle/view that reserved this
    /// preparation's nonce, so a continuation can never target a different
    /// `IntentStateMachine`.
    pub fn into_open_parts(mut self) -> (Arc<IntentStateMachine>, ChainNonceView, u64) {
        self.consumed = true;
        (self.sm.clone(), self.chain.clone(), self.nonce)
    }
}

/// Last-resort cleanup for a head that was dropped without a continuation.
///
/// A `Preparing` intent holds a live nonce; leaking one stalls the nonce lane until
/// restart. Dropping is a caller bug, so this both repairs the SM (best effort) and
/// logs at error level.
impl Drop for PreparedPipelineHead {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        let abort = self.sm.abort_prepare(self.nonce);
        let reconcile = self.sm.reconcile(self.chain.clone());
        tracing::error!(
            target: "execution.pipeline",
            nonce = self.nonce,
            abort_error = ?abort.err(),
            reconcile_error = ?reconcile.err(),
            "PreparedPipelineHead dropped without a consuming continuation; ran \
             best-effort abort_prepare + reconcile so the Preparing intent does not \
             leak its nonce"
        );
    }
}

/// Outcome of a closed-send pipeline-head run: the sender-bound digest that would
/// have been signed, had the send gate been open.
pub struct HeadOutcome {
    pub digest: FinalRequestDigest,
}

/// `observe_snapshot -> reserve -> begin_prepare -> build_and_validate_final_request ->
/// preflight -> validate -> revalidate`, stopping before any pause/lease/wallet/signing/
/// broadcast. Never aborts or reconciles on success — exactly one consuming
/// continuation (e.g. [`run_pipeline_head_closed`]) owns cleanup for the returned
/// [`PreparedPipelineHead`].
///
/// On any failure after `reserve` has minted a nonce, best-effort cleanup
/// (`abort_prepare` + `reconcile`) is attempted so a failed preparation doesn't leak a
/// live Reserved/Preparing intent; cleanup errors are ignored so they never mask the
/// original failure.
///
/// Takes an owned `Arc<IntentStateMachine>` so the returned head can carry the exact
/// SM that minted its nonce into its continuation (and its `Drop` fallback).
#[allow(clippy::too_many_arguments)]
pub async fn prepare_pipeline_head(
    sm: Arc<IntentStateMachine>,
    candidate: CandidateRef,
    status: &SnapshotStatus,
    fee_ctx: BlockFeeContext,
    builder: &impl ExecutionRequestBuilder,
    identity_source: &impl ExecutionIdentitySource,
    preflight: &impl PreflightSlot,
    params: FinalRequestParams,
    chain: ChainNonceView,
) -> Result<PreparedPipelineHead, PipelineError> {
    sm.observe_snapshot(status)?;
    let (nonce, permit) = sm.reserve(candidate, status, fee_ctx)?;

    // Best-effort cleanup for every failure after the nonce is minted. Errors are
    // dropped so they never mask the original failure.
    let cleanup = |sm: &IntentStateMachine| {
        let _ = sm.abort_prepare(nonce);
        let _ = sm.reconcile(chain.clone());
    };

    if let Err(error) = sm.begin_prepare(nonce) {
        cleanup(&sm);
        return Err(error.into());
    }

    let request =
        match build_and_validate_final_request(builder, identity_source, params, permit).await {
            Ok(request) => request,
            Err(error) => {
                cleanup(&sm);
                return Err(error);
            }
        };

    if let Err(error) = preflight.preflight(&request).await {
        cleanup(&sm);
        return Err(error.into());
    }

    if let Err(error) = identity_source.validate(request.identity()).await {
        cleanup(&sm);
        return Err(error.into());
    }

    if let Err(error) = builder.revalidate_final_request(&request) {
        cleanup(&sm);
        return Err(error.into());
    }

    let digest = match final_request_digest(&request) {
        Ok(digest) => digest,
        Err(error) => {
            cleanup(&sm);
            return Err(error.into());
        }
    };
    Ok(PreparedPipelineHead {
        sm,
        chain,
        nonce,
        request,
        digest,
        consumed: false,
    })
}

/// Closed-send wrapper over [`prepare_pipeline_head`]: prepares, then immediately
/// consumes the result via `abort_prepare` + `reconcile` — no pause/lease/sign/
/// broadcast ever occurs. Used by all four monitor services while the production
/// send gate stays false.
#[allow(clippy::too_many_arguments)]
pub async fn run_pipeline_head_closed(
    sm: Arc<IntentStateMachine>,
    candidate: CandidateRef,
    status: &SnapshotStatus,
    fee_ctx: BlockFeeContext,
    builder: &impl ExecutionRequestBuilder,
    identity_source: &impl ExecutionIdentitySource,
    preflight: &impl PreflightSlot,
    params: FinalRequestParams,
    chain: ChainNonceView,
) -> Result<HeadOutcome, PipelineError> {
    let head = prepare_pipeline_head(
        sm,
        candidate,
        status,
        fee_ctx,
        builder,
        identity_source,
        preflight,
        params,
        chain,
    )
    .await?;
    head.into_closed_outcome()
}

/// Acquire pause guard then identity lease (fixed order) and perform final validation.
pub async fn acquire_execute_send_guards(
    pause_gate: &impl PauseGate,
    identity_source: &impl ExecutionIdentitySource,
    executor: &Executor,
    sm: &IntentStateMachine,
    request: &FinalRequest,
) -> Result<ExecuteSendGuards, PipelineError> {
    let nonce = request.nonce;
    let pause = match pause_gate.begin_send(AttemptKind::Execute) {
        Ok(guard) => guard,
        Err(paused) => {
            let _ = sm.abort_prepare(nonce);
            return Err(paused.into());
        }
    };
    let lease = match identity_source.acquire_send_lease(request.identity()).await {
        Ok(lease) => lease,
        Err(error) => {
            let _ = sm.abort_prepare(nonce);
            return Err(error.into());
        }
    };
    if let Err(error) = executor.revalidate_final_request(request) {
        let _ = sm.abort_prepare(nonce);
        return Err(error.into());
    }
    if let Err(error) = identity_source.validate(request.identity()).await {
        let _ = sm.abort_prepare(nonce);
        return Err(error.into());
    }
    Ok(ExecuteSendGuards { lease, pause })
}

/// Sign under acquired guards. Caller must already hold pause+lease.
pub async fn sign_under_guards(
    executor: &Executor,
    request: FinalRequest,
    wallet: &EthereumWallet,
    _guards: &ExecuteSendGuards,
) -> Result<(SignedSubmission, AuthenticatedAttemptMeta), PipelineError> {
    let meta = AuthenticatedAttemptMeta {
        min_profit: request.min_profit(),
        deadline: request.deadline(),
    };
    let signed = executor.sign_final_request(request, wallet).await?;
    Ok((signed, meta))
}

/// Durable hook → SM record. Guards must still be held until RPC handoff returns.
pub fn record_signed_submission(
    sm: &IntentStateMachine,
    signed: &SignedSubmission,
    meta: AuthenticatedAttemptMeta,
    durable: &impl DurableSubmissionHook,
) -> Result<(), PipelineError> {
    durable.on_signed(signed, meta.min_profit, meta.deadline)?;
    sm.record_submission_with_min_profit(signed, meta.min_profit)?;
    Ok(())
}

/// Full Execute tail after FinalRequest construction (preflight slot included).
///
/// Returns the signed submission plus RAII guards. Drop the guards only after
/// the bounded `eth_sendRawTransaction` handoff completes.
pub async fn finalize_execute_submission(
    executor: &Executor,
    sm: &IntentStateMachine,
    identity_source: &impl ExecutionIdentitySource,
    pause_gate: &impl PauseGate,
    preflight: &impl PreflightSlot,
    durable: &impl DurableSubmissionHook,
    request: FinalRequest,
    wallet: &EthereumWallet,
) -> Result<(SignedSubmission, ExecuteSendGuards), PipelineError> {
    let nonce = request.nonce;
    identity_source.validate(request.identity()).await?;
    preflight.preflight(&request).await?;
    identity_source.validate(request.identity()).await?;
    executor.revalidate_final_request(&request)?;

    let guards =
        acquire_execute_send_guards(pause_gate, identity_source, executor, sm, &request).await?;
    let (signed, meta) = match sign_under_guards(executor, request, wallet, &guards).await {
        Ok(result) => result,
        Err(error) => {
            let _ = sm.abort_prepare(nonce);
            return Err(error);
        }
    };
    record_signed_submission(sm, &signed, meta, durable)?;
    Ok((signed, guards))
}

/// Pure policy for production signer-role separation (WHI-520 §6 + WHI-524 guardian).
pub fn execution_signer_roles_ok(
    admin: Address,
    guardian: Address,
    signer: Address,
    is_hot: bool,
) -> Result<()> {
    if !is_hot {
        return Err(eyre!("execution signer {signer} is not a hot executor"));
    }
    if signer == admin {
        return Err(eyre!(
            "execution signer {signer} must not equal admin(); cold-admin material is forbidden"
        ));
    }
    if guardian == Address::ZERO {
        return Err(eyre!("guardian() must be nonzero"));
    }
    if guardian == admin {
        return Err(eyre!("guardian() must not equal admin()"));
    }
    if guardian == signer {
        return Err(eyre!("guardian() must not equal execution signer"));
    }
    Ok(())
}

/// Production signer-role separation (Revision 5 §6 / WHI-524).
///
/// `admin()` / `guardian()` / `isHotExecutor` are read at the same hash-pinned block.
pub async fn verify_execution_signer_roles<P: Provider>(
    provider: &P,
    executor: Address,
    signer: Address,
    block_hash: alloy::primitives::B256,
) -> Result<()> {
    let block = crate::state_space::hash_pinned_state_block_id(block_hash);
    let contract = super::contract::IArbitrageExecutor::new(executor, provider);
    let admin = contract
        .admin()
        .call()
        .block(block)
        .await
        .map_err(|e| eyre!("admin() read failed: {e}"))?;
    let guardian = contract
        .guardian()
        .call()
        .block(block)
        .await
        .map_err(|e| eyre!("guardian() read failed: {e}"))?;
    let is_hot = contract
        .isHotExecutor(signer)
        .call()
        .block(block)
        .await
        .map_err(|e| eyre!("isHotExecutor() read failed: {e}"))?;
    execution_signer_roles_ok(admin, guardian, signer, is_hot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::pause::AlwaysAllow;

    #[test]
    fn execute_send_order_matches_revision_5_contract() {
        assert_eq!(
            crate::execution::EXECUTE_SEND_ORDER,
            [
                "build",
                "validate",
                "preflight",
                "validate",
                "begin_send",
                "acquire_lease",
                "final_validate",
                "sign",
                "durable_hook",
                "record_submission",
                "rpc_handoff",
                "release",
            ]
        );
        assert!(AlwaysAllow.begin_send(AttemptKind::Execute).is_ok());
        assert!(AlwaysAllow.begin_send(AttemptKind::Cancel).is_ok());
    }

    #[test]
    fn signer_role_policy_rejects_non_hot_admin_and_bad_guardian() {
        let admin = Address::repeat_byte(1);
        let hot = Address::repeat_byte(2);
        let guardian = Address::repeat_byte(3);
        assert!(execution_signer_roles_ok(admin, guardian, hot, true).is_ok());
        assert!(execution_signer_roles_ok(admin, guardian, hot, false).is_err());
        assert!(execution_signer_roles_ok(admin, guardian, admin, true)
            .unwrap_err()
            .to_string()
            .contains("must not equal admin"));
        assert!(execution_signer_roles_ok(admin, Address::ZERO, hot, true).is_err());
        assert!(execution_signer_roles_ok(admin, admin, hot, true).is_err());
        assert!(execution_signer_roles_ok(admin, hot, hot, true).is_err());
    }

    #[test]
    fn rust_drops_struct_fields_in_declaration_order() {
        use std::cell::RefCell;
        use std::rc::Rc;

        struct Trace(Rc<RefCell<Vec<&'static str>>>, &'static str);
        impl Drop for Trace {
            fn drop(&mut self) {
                self.0.borrow_mut().push(self.1);
            }
        }
        struct Ordered {
            first: Trace,
            second: Trace,
        }

        let log = Rc::new(RefCell::new(Vec::new()));
        drop(Ordered {
            first: Trace(Rc::clone(&log), "lease"),
            second: Trace(Rc::clone(&log), "pause"),
        });
        assert_eq!(log.borrow().as_slice(), ["lease", "pause"]);
    }
}
