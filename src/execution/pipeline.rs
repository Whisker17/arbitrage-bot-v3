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
