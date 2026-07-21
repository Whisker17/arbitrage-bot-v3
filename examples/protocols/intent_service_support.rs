//! Shared WHI-519 intent-state-machine helpers for active monitor services.
//!
//! Production broadcast remains fail-closed. With the gate closed, services still
//! exercise the SM through reserve → prepare-marker → abort/release so the
//! discovery path is non-blocking and does not call `.watch()`.

use amms::execution::{
    CandidateRef, ChainNonceView, IntentPolicy, IntentStateMachine, LatestWinsSlot,
};
use amms::state_space::{BlockHeaderContext, SnapshotId};
use alloy::primitives::{Address, B256, U256};
use eyre::{eyre, Result};
use std::sync::Arc;

pub fn production_send_allowed() -> bool {
    // WHI-519 keeps production sends disabled; WHI-526 owns enablement.
    false
}

pub fn intent_policy_from_env_or_defaults() -> Result<IntentPolicy> {
    match IntentPolicy::from_env() {
        Ok(policy) => Ok(policy),
        Err(_) => {
            // Local/dev defaults for dry-run services when caps are unset.
            // Live operators must set MAX_FEE_CAP_WEI / CANCEL_FEE_CAP_WEI.
            Ok(IntentPolicy::with_caps(
                200_000_000_000, // 200 gwei
                400_000_000_000, // 400 gwei
            ))
        }
    }
}

pub fn build_intent_sm(signer: Address) -> Result<Arc<IntentStateMachine>> {
    let policy = intent_policy_from_env_or_defaults()?;
    Ok(Arc::new(IntentStateMachine::new(
        signer,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        policy,
        production_send_allowed(),
    )?))
}

pub fn candidate_ref(
    snapshot_id: SnapshotId,
    header: BlockHeaderContext,
    route_protocols_v2_hops: usize,
    amount_in: U256,
) -> Result<CandidateRef> {
    use amms::execution::{ProtocolKind, RouteKey};
    let hops = route_protocols_v2_hops.max(1);
    let protocols = vec![ProtocolKind::V2; hops];
    // RouteKey construction is protocol-kind based for SM identity only when the
    // service has not yet threaded measured RouteKey (M2-2).
    let route_key = RouteKey::new(protocols).map_err(|e| eyre!("{e}"))?;
    Ok(CandidateRef {
        snapshot_id,
        header,
        route_key,
        amount_in,
    })
}

/// Non-blocking pre-broadcast exercise of the SM while production send is gated.
pub fn exercise_sm_prebroadcast(
    sm: &IntentStateMachine,
    candidate: CandidateRef,
) -> Result<()> {
    let (nonce, _permit) = sm.reserve(candidate)?;
    sm.begin_prepare(nonce)?;
    // Gate closed: never sign/broadcast. Abort prepare and release trailing reserved.
    sm.abort_prepare(nonce)?;
    let _ = sm.reconcile(ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    })?;
    Ok(())
}

pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

pub fn header_from_block(parent_hash: B256, timestamp: u64) -> BlockHeaderContext {
    BlockHeaderContext::new(parent_hash, timestamp)
}

pub fn finite_deadline(header: &BlockHeaderContext, deadline_secs: u64) -> Result<U256> {
    let ts = header
        .block_timestamp
        .checked_add(deadline_secs)
        .ok_or_else(|| eyre!("deadline overflow"))?;
    Ok(U256::from(ts))
}
