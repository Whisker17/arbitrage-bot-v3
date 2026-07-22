//! Shared WHI-519 intent-state-machine helpers for active monitor services.
//!
//! Production broadcast remains fail-closed (WHI-526). With the gate closed,
//! services still route candidates through a process-lifetime SM singleton and
//! exercise reserve → begin_prepare → abort/release using real candidate
//! SnapshotIds. Local signing is covered by unit tests / measured Executor
//! paths; services do not invent fake Measured Executor state here.

use alloy::primitives::{Address, B256, U256};
use amms::execution::{
    CandidateRef, ChainNonceView, IntentPolicy, IntentStateMachine, LatestWinsSlot, RouteKey,
};
use amms::state_space::{BlockHeaderContext, SnapshotId, SnapshotStatus};
#[cfg(test)]
use amms::state_space::{MarketSnapshot, ProtocolCoverage};
use eyre::{eyre, Result};
#[cfg(test)]
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Reject queued work unless the live tip is still Ready at the candidate SnapshotId.
pub fn require_matching_ready_tip(
    tip: Option<SnapshotStatus>,
    candidate_id: SnapshotId,
) -> Result<SnapshotStatus> {
    match tip {
        Some(SnapshotStatus::Ready(snapshot)) if snapshot.id == candidate_id => {
            Ok(SnapshotStatus::Ready(snapshot))
        }
        Some(SnapshotStatus::Ready(snapshot)) => Err(eyre!(
            "stale queued opportunity: candidate {:?} != live tip {:?}",
            candidate_id,
            snapshot.id
        )),
        Some(_) => Err(eyre!("execution gate has no live Ready snapshot tip")),
        None => Err(eyre!("execution gate has no live Ready snapshot tip")),
    }
}

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
    pool_universe_fingerprint: B256,
    route_key: RouteKey,
    amount_in: U256,
) -> Result<CandidateRef> {
    Ok(CandidateRef {
        snapshot_id,
        header,
        pool_universe_fingerprint,
        route_key,
        amount_in,
    })
}

/// Ready tip matching the candidate's full SnapshotId (service dry-run gate).
#[cfg(test)]
pub fn ready_status_for_candidate(candidate: &CandidateRef) -> SnapshotStatus {
    let mut coverage = ProtocolCoverage::default();
    coverage.pool_universe_fingerprint = Some(candidate.pool_universe_fingerprint);
    SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        candidate.snapshot_id,
        candidate.header,
        HashMap::new(),
        coverage,
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
    status: &SnapshotStatus,
) -> Result<()> {
    sm.observe_snapshot(status)?;
    let fee_ctx = fee_context_for_candidate(&candidate, 50_000_000_000, 60_000_000);
    let (nonce, _permit) = sm.reserve(candidate, status, fee_ctx)?;
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
    status: &SnapshotStatus,
    header: BlockHeaderContext,
    pool_universe_fingerprint: B256,
    route_key: RouteKey,
    amount_in: U256,
) -> Result<()> {
    let SnapshotStatus::Ready(snapshot) = status else {
        return Err(eyre!("execution gate requires a Ready market snapshot"));
    };
    if snapshot.header != header {
        return Err(eyre!(
            "candidate header does not match the Ready market snapshot"
        ));
    }
    if snapshot.coverage.pool_universe_fingerprint != Some(pool_universe_fingerprint) {
        return Err(eyre!(
            "candidate pool-universe fingerprint does not match the Ready market snapshot"
        ));
    }

    let candidate = candidate_ref(
        snapshot.id,
        header,
        pool_universe_fingerprint,
        route_key,
        amount_in,
    )?;
    let sm = process_intent_sm(signer)?;
    exercise_sm_prebroadcast(&sm, candidate, status)
}

pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

pub fn finite_deadline(header: &BlockHeaderContext, deadline_secs: u64) -> Result<U256> {
    amms::execution::deadline_from_header_timestamp(header.block_timestamp, deadline_secs)
        .map_err(|_| eyre!("deadline overflow"))
}

/// Shared WMNT balance read at a hash-pinned snapshot (WHI-524).
pub async fn executor_balance_at_snapshot<P: alloy::providers::Provider + Clone>(
    provider: &P,
    wmnt: Address,
    executor: Address,
    snapshot_id: SnapshotId,
) -> Result<amms::state_space::SnapshotBoundBalance> {
    use amms::execution::IERC20;
    use amms::state_space::{hash_pinned_state_block_id, SnapshotBoundBalance};
    let wmnt_contract = IERC20::new(wmnt, provider.clone());
    let amount = wmnt_contract
        .balanceOf(executor)
        .call()
        .block(hash_pinned_state_block_id(snapshot_id.block_hash))
        .await?;
    Ok(SnapshotBoundBalance::new(snapshot_id, amount))
}

/// Three-way per-tx cap: `min(balance, configured_quote_max, max_input_per_tx)`.
pub fn capped_max_input_for_snapshot(
    snapshot_id: SnapshotId,
    balance: amms::state_space::SnapshotBoundBalance,
    configured_quote_max: U256,
    max_input_per_tx_wmnt_wei: U256,
) -> Result<U256> {
    let configured = configured_quote_max.min(max_input_per_tx_wmnt_wei);
    amms::state_space::max_input_bound_for_snapshot(snapshot_id, balance, configured)
        .map_err(|e| eyre!("{e}"))
}

/// Inventory over-cap check. Returns Err when balance exceeds the mandatory cap.
pub fn check_inventory_cap(
    balance: U256,
    max_total_inventory_wmnt_wei: U256,
) -> Result<()> {
    if balance > max_total_inventory_wmnt_wei {
        return Err(eyre!(
            "executor inventory {balance} exceeds MAX_TOTAL_INVENTORY_WMNT_WEI {max_total_inventory_wmnt_wei}"
        ));
    }
    Ok(())
}
