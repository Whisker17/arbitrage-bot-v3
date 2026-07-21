//! Shared WHI-519 intent-state-machine helpers for active monitor services.
//!
//! Production broadcast remains fail-closed (WHI-526). With the gate closed,
//! services still route candidates through a process-lifetime SM singleton and
//! exercise reserve → begin_prepare → abort/release using real candidate
//! SnapshotIds. Local signing is covered by unit tests / measured Executor
//! paths; services do not invent fake Measured Executor state here.

use amms::execution::{
    CandidateRef, ChainNonceView, IntentPolicy, IntentStateMachine, LatestWinsSlot,
};
use amms::state_space::{
    BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
};
use alloy::primitives::{Address, B256, U256};
use eyre::{eyre, Result};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

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

/// Process-lifetime SM singleton for one service process / signer account.
pub fn process_intent_sm(signer: Address) -> Result<Arc<IntentStateMachine>> {
    static SM: OnceLock<Result<Arc<IntentStateMachine>, String>> = OnceLock::new();
    // OnceLock stores Result so construction errors surface once.
    // Note: signer is fixed to the first caller in this process (services use one account).
    let cell = SM.get_or_init(|| {
        let policy = intent_policy_from_env_or_defaults().map_err(|e| e.to_string())?;
        IntentStateMachine::new(
            signer,
            ChainNonceView {
                latest_nonce: 0,
                pending_nonce: 0,
            },
            policy,
            production_send_allowed(),
        )
        .map(Arc::new)
        .map_err(|e| e.to_string())
    });
    match cell {
        Ok(sm) => {
            if sm.signer_address() != signer {
                return Err(eyre!(
                    "intent SM singleton already bound to signer {}, refused {}",
                    sm.signer_address(),
                    signer
                ));
            }
            Ok(Arc::clone(sm))
        }
        Err(e) => Err(eyre!("intent SM init failed: {e}")),
    }
}

/// Back-compat alias used by older call sites.
pub fn build_intent_sm(signer: Address) -> Result<Arc<IntentStateMachine>> {
    process_intent_sm(signer)
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

/// Ready tip matching the candidate's full SnapshotId (service dry-run gate).
pub fn ready_status_for_candidate(
    candidate: &CandidateRef,
) -> SnapshotStatus {
    SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        candidate.snapshot_id,
        candidate.header,
        HashMap::new(),
        ProtocolCoverage::default(),
    )))
}

/// Fee context bound to the candidate block identity for permit minting.
pub fn fee_context_for_candidate(
    candidate: &CandidateRef,
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> amms::execution::BlockFeeContext {
    amms::execution::BlockFeeContext {
        block_number: candidate.snapshot_id.block_number,
        block_hash: candidate.snapshot_id.block_hash,
        base_fee_per_gas,
        block_gas_limit,
    }
}

/// Non-blocking pre-broadcast exercise of the process SM while production send is gated.
///
/// Uses the candidate's real SnapshotId and a Ready gate. Does not sign/broadcast.
pub fn exercise_sm_prebroadcast(
    sm: &IntentStateMachine,
    candidate: CandidateRef,
) -> Result<()> {
    let status = ready_status_for_candidate(&candidate);
    sm.observe_snapshot(&status)?;
    let fee_ctx = fee_context_for_candidate(&candidate, 50_000_000_000, 60_000_000);
    let (nonce, _permit) = sm.reserve(candidate, &status, fee_ctx)?;
    sm.begin_prepare(nonce)?;
    // Gate closed: never sign/broadcast. Abort prepare and release trailing reserved.
    sm.abort_prepare(nonce)?;
    let _ = sm.reconcile(ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    })?;
    Ok(())
}

/// Shared helper: route a resized candidate through the process SM prebroadcast path.
pub fn route_candidate_through_sm(
    signer: Address,
    snapshot_id: SnapshotId,
    header: BlockHeaderContext,
    hops: usize,
    amount_in: U256,
) -> Result<()> {
    let cand = candidate_ref(snapshot_id, header, hops, amount_in)?;
    let sm = process_intent_sm(signer)?;
    exercise_sm_prebroadcast(&sm, cand)
}

pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

pub fn header_from_block(parent_hash: B256, timestamp: u64) -> BlockHeaderContext {
    BlockHeaderContext::new(parent_hash, timestamp)
}

pub fn finite_deadline(header: &BlockHeaderContext, deadline_secs: u64) -> Result<U256> {
    amms::execution::deadline_from_header_timestamp(header.block_timestamp, deadline_secs)
        .map_err(|_| eyre!("deadline overflow"))
}
