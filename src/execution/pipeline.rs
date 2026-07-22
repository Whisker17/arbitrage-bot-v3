//! Canonical Execute send ordering (WHI-520 Revision 5).
//!
//! `build -> validate -> preflight slot -> validate -> begin_send(Execute) ->
//! acquire lease -> final validate -> sign -> durable hook ->
//! record_submission -> RPC handoff -> release`.

use alloy::network::EthereumWallet;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use eyre::{eyre, Result};

use super::final_request::{FinalRequest, FinalRequestParams};
use super::identity::{ExecutionIdentityLease, ExecutionIdentitySource};
use super::intent::{IntentError, IntentStateMachine, SignedSubmission};
use super::pause::{AttemptKind, PauseGate, Paused, SendGuard};
use super::types::ExecutionPermit;
use super::Executor;

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
    fn on_signed(
        &self,
        signed: &SignedSubmission,
        min_profit: U256,
        deadline: U256,
    ) -> Result<()>;
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
pub struct ExecuteSendGuards {
    pub pause: SendGuard,
    pub lease: ExecutionIdentityLease,
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
    executor: &Executor,
    identity_source: &impl ExecutionIdentitySource,
    params: FinalRequestParams,
    permit: ExecutionPermit,
) -> Result<FinalRequest, PipelineError> {
    let request = executor.build_final_request(params, permit)?;
    identity_source.validate(request.identity()).await?;
    executor.revalidate_final_request(&request)?;
    Ok(request)
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
    Ok(ExecuteSendGuards { pause, lease })
}

/// Sign under acquired guards. Caller must already hold pause+lease.
pub async fn sign_under_guards(
    executor: &Executor,
    request: FinalRequest,
    wallet: &EthereumWallet,
    _guards: &ExecuteSendGuards,
) -> Result<SignedSubmission, PipelineError> {
    Ok(executor.sign_final_request(request, wallet).await?)
}

/// Durable hook → SM record. Guards must still be held until RPC handoff returns.
pub fn record_signed_submission(
    sm: &IntentStateMachine,
    signed: &SignedSubmission,
    min_profit: U256,
    deadline: U256,
    durable: &impl DurableSubmissionHook,
) -> Result<(), PipelineError> {
    durable.on_signed(signed, min_profit, deadline)?;
    sm.record_submission(signed)?;
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
    let min_profit = request.min_profit();
    let deadline = request.deadline();
    let signed = match sign_under_guards(executor, request, wallet, &guards).await {
        Ok(signed) => signed,
        Err(error) => {
            let _ = sm.abort_prepare(nonce);
            return Err(error);
        }
    };
    record_signed_submission(sm, &signed, min_profit, deadline, durable)?;
    Ok((signed, guards))
}

/// Pure policy for production signer-role separation (Revision 5 §6).
pub fn execution_signer_roles_ok(admin: Address, signer: Address, is_hot: bool) -> Result<()> {
    if !is_hot {
        return Err(eyre!(
            "execution signer {signer} is not a hot executor"
        ));
    }
    if signer == admin {
        return Err(eyre!(
            "execution signer {signer} must not equal admin(); cold-admin material is forbidden"
        ));
    }
    Ok(())
}

/// Production signer-role separation (Revision 5 §6).
pub async fn verify_execution_signer_roles<P: Provider>(
    provider: &P,
    executor: Address,
    signer: Address,
) -> Result<()> {
    let contract = super::contract::IArbitrageExecutor::new(executor, provider);
    let admin = contract
        .admin()
        .call()
        .await
        .map_err(|e| eyre!("admin() read failed: {e}"))?;
    let is_hot = contract
        .isHotExecutor(signer)
        .call()
        .await
        .map_err(|e| eyre!("isHotExecutor() read failed: {e}"))?;
    execution_signer_roles_ok(admin, signer, is_hot)
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
    fn signer_role_policy_rejects_non_hot_and_admin_signer() {
        let admin = Address::repeat_byte(1);
        let hot = Address::repeat_byte(2);
        assert!(execution_signer_roles_ok(admin, hot, true).is_ok());
        assert!(execution_signer_roles_ok(admin, hot, false).is_err());
        assert!(execution_signer_roles_ok(admin, admin, true)
            .unwrap_err()
            .to_string()
            .contains("must not equal admin"));
    }
}
