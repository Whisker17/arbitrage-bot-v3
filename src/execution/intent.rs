//! Nonce-intent execution state machine (WHI-519 / M2-1).
//!
//! One signer account is owned by at most one process (one state-machine
//! instance) at a time. Within a process the SM is the per-account singleton
//! nonce authority. Running two services concurrently requires distinct signer
//! accounts. Detection of external nonce activity is best-effort only and
//! cannot prevent two independent processes from racing the same account.
//!
//! Discovery, signing/broadcast, and receipt tracking are decoupled: no hot
//! path blocks on per-tx `watch()`.

use super::fee_context::{
    bump_prior_fees, deadline_from_header_timestamp, BlockFeeContext, FeePlan, FeePlanError,
    PriorFees,
};
use super::gas_profile::RouteKey;
use super::nonce::NonceManager;
use super::types::{ExecutionParams, ExecutionPermit, IntentPolicy};
use crate::state_space::{BlockHeaderContext, SnapshotId, SnapshotStatus};
use alloy::primitives::{Address, Bytes, B256, U256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Zero-sized authority token required to construct an [`ExecutionPermit`].
///
/// No public fields, no `Default`/`Clone` derive, constructor private to this
/// module — only the intent state machine can mint permits.
#[derive(Debug)]
pub struct IntentAuthority {
    _private: (),
}

impl IntentAuthority {
    fn mint() -> Self {
        Self { _private: () }
    }
}

/// Identifies the quote a live attempt was built from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateRef {
    pub snapshot_id: SnapshotId,
    pub header: BlockHeaderContext,
    pub pool_universe_fingerprint: B256,
    pub route_key: RouteKey,
    pub amount_in: U256,
}

/// Kind-explicit payload for a broadcast attempt.
#[derive(Clone, Debug)]
pub enum AttemptPayload {
    Execute {
        params: ExecutionParams,
        candidate: CandidateRef,
    },
    Cancel {
        to: Address,
        gas_limit: u64,
    },
}

impl AttemptPayload {
    pub fn is_cancel(&self) -> bool {
        matches!(self, Self::Cancel { .. })
    }
}

/// One signed/broadcast attempt under a nonce intent.
#[derive(Clone, Debug)]
pub struct Attempt {
    pub tx_hash: B256,
    pub fee_plan: FeePlan,
    pub calldata_digest: B256,
    pub submitted_at: SnapshotId,
    pub payload: AttemptPayload,
    /// Calldata `minProfit` retained at build/sign time (WHI-520/524).
    pub onchain_min_profit: U256,
    /// Set once a receipt is observed on a canonical inclusion.
    pub inclusion: Option<InclusionRecord>,
    pub superseded: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InclusionRecord {
    pub block_number: u64,
    pub block_hash: B256,
}

/// Why an intent entered non-terminal `NeedsOperator` quarantine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NeedsOperatorReason {
    CancelUnpriceable,
    CancelBudgetExhausted,
    FeeContextUnavailable,
    ExternalNonceActivity,
    DeepReorgHalt,
}

/// Lifecycle of one nonce intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentState {
    Reserved,
    Preparing,
    Submitted,
    IncludedUnconfirmed { block_number: u64, block_hash: B256 },
    Finalized,
    RevertedFinalized,
    CancelFinalized,
    Released,
    NeedsOperator { reason: NeedsOperatorReason },
}

impl IntentState {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finalized | Self::RevertedFinalized | Self::CancelFinalized | Self::Released
        )
    }

    pub fn is_zero_broadcast_releasable(&self) -> bool {
        matches!(self, Self::Reserved)
    }
}

/// One reserved nonce and its attempt history.
#[derive(Clone, Debug)]
pub struct NonceIntent {
    pub nonce: u64,
    pub candidate: CandidateRef,
    pub attempts: Vec<Attempt>,
    pub state: IntentState,
    /// Blocks the latest attempt has been missing from the mempool/chain.
    pub drop_absent_blocks: u64,
    /// Blocks since the latest attempt was submitted without a receipt.
    pub pending_blocks: u64,
    /// Whether a fee-recovery cancel retry is still allowed from NeedsOperator.
    pub fee_recovery_retry_available: bool,
    /// Inclusion retained for reorg tracking after terminalization.
    pub retained_inclusion: Option<InclusionRecord>,
    pub terminal_at_block: Option<u64>,
}

impl NonceIntent {
    pub fn has_broadcast_attempt(&self) -> bool {
        !self.attempts.is_empty()
    }

    pub fn latest_attempt(&self) -> Option<&Attempt> {
        self.attempts.last()
    }

    pub fn execute_attempt_count(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| !a.payload.is_cancel())
            .count()
    }

    pub fn cancel_attempt_count(&self) -> usize {
        self.attempts
            .iter()
            .filter(|a| a.payload.is_cancel() && !a.superseded)
            .count()
    }

    pub fn highest_prior_fees(&self) -> Option<PriorFees> {
        self.attempts
            .iter()
            .map(|a| {
                PriorFees::new(
                    a.fee_plan.max_priority_fee_per_gas,
                    a.fee_plan.max_fee_per_gas,
                )
            })
            .max_by_key(|fees| (fees.priority_fee, fees.max_fee))
    }
}

/// Kind-explicit prepare input.
#[derive(Clone, Debug)]
pub enum PrepareRequest {
    Execute {
        params: ExecutionParams,
        candidate: CandidateRef,
        fee_plan: FeePlan,
        deadline: U256,
    },
    Cancel {
        to: Address,
        gas_limit: u64,
        fee_plan: FeePlan,
    },
}

/// Kind-explicit prepare result payload (pre-broadcast).
#[derive(Clone, Debug)]
pub enum PreparedPayload {
    Execute {
        params: ExecutionParams,
        candidate: CandidateRef,
    },
    Cancel {
        to: Address,
        gas_limit: u64,
    },
}

/// Locally signed raw transaction ready for `eth_sendRawTransaction`.
#[derive(Clone, Debug)]
pub struct SignedSubmission {
    pub raw: Bytes,
    pub tx_hash: B256,
    pub fee_plan: FeePlan,
    pub payload: PreparedPayload,
    pub calldata_digest: B256,
    pub nonce: u64,
    pub submitted_at: SnapshotId,
}

/// Outcome of a receipt read used by the tracker (not `observe_receipt`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptOutcome {
    pub success: bool,
    pub block_number: u64,
    pub block_hash: B256,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    pub l1_fee: Option<U256>,
    /// True when L1 fee was unavailable; cost is execution-layer only.
    pub execution_layer_only: bool,
}

impl ReceiptOutcome {
    pub fn actual_cost(&self) -> U256 {
        let exec = U256::from(self.gas_used).saturating_mul(U256::from(self.effective_gas_price));
        match self.l1_fee {
            Some(l1) => exec.saturating_add(l1),
            None => exec,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum IntentError {
    #[error("intent policy invalid: {0}")]
    InvalidPolicy(String),
    #[error("accounting persist failed: {0}")]
    AccountingPersistFailed(String),
    #[error("illegal intent transition from {from:?} to {to:?}")]
    IllegalTransition { from: IntentState, to: IntentState },
    #[error("nonce {0} has no live intent")]
    UnknownNonce(u64),
    #[error("broadcast-order invariant: nonce {nonce} cannot broadcast while lower nonce {blocker} is zero-broadcast")]
    BroadcastOrder { nonce: u64, blocker: u64 },
    #[error("stale candidate: attempt snapshot {attempt:?} != current {current:?}")]
    StaleCandidate {
        attempt: SnapshotId,
        current: SnapshotId,
    },
    #[error("stale topology: candidate {candidate:?} != current {current:?}")]
    StaleTopology {
        candidate: Option<B256>,
        current: Option<B256>,
    },
    #[error("snapshot status does not allow Execute signing")]
    SnapshotNotReady,
    #[error("external nonce activity detected at chain nonce {0}")]
    ExternalNonceActivity(u64),
    #[error("intent state machine halted: {0}")]
    Halted(String),
    #[error("replacement requires a freshly re-simulated candidate")]
    StaleReplacement,
    #[error("cancel only allowed after at least one broadcast attempt")]
    CancelWithoutBroadcast,
    #[error("fee plan error: {0}")]
    Fee(String),
    #[error("intent is quarantined for operator recovery: {0:?}")]
    NeedsOperator(NeedsOperatorReason),
    #[error("production send is disabled")]
    ProductionSendDisabled,
    #[error("deadline overflow for header timestamp {timestamp} + {horizon}")]
    DeadlineOverflow { timestamp: u64, horizon: u64 },
    #[error("intent state machine lock poisoned")]
    LockPoisoned,
}

impl From<FeePlanError> for IntentError {
    fn from(value: FeePlanError) -> Self {
        Self::Fee(value.to_string())
    }
}

/// Events emitted by the state machine for services / operators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentEvent {
    Reserved {
        nonce: u64,
    },
    Submitted {
        nonce: u64,
        tx_hash: B256,
    },
    IncludedUnconfirmed {
        nonce: u64,
        tx_hash: B256,
        block_number: u64,
        block_hash: B256,
    },
    Finalized {
        nonce: u64,
        tx_hash: B256,
        success: bool,
        actual_cost: U256,
        execution_layer_only: bool,
    },
    CancelFinalized {
        nonce: u64,
        tx_hash: B256,
        success: bool,
        actual_cost: U256,
        execution_layer_only: bool,
        anomaly: bool,
    },
    Released {
        nonce: u64,
    },
    NeedsOperator {
        nonce: u64,
        reason: NeedsOperatorReason,
    },
    Reopened {
        nonce: u64,
    },
    Halted {
        reason: String,
    },
    Superseded {
        nonce: u64,
        tx_hash: B256,
    },
    /// Execute receipt gas used crossed the re-qualification threshold.
    GasProfileRequalification {
        nonce: u64,
        tx_hash: B256,
        gas_used: u64,
        gas_limit: u64,
        utilization_bps: u16,
    },
}

/// Canonical-chain view used by reconcile / receipt tracking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainNonceView {
    /// Account transaction count at latest (confirmed) block.
    pub latest_nonce: u64,
    /// Account transaction count including pending pool txs.
    pub pending_nonce: u64,
}

/// Canonical block identity for reorg checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalBlock {
    pub number: u64,
    pub hash: B256,
}

fn transition_allowed(from: &IntentState, to: &IntentState) -> bool {
    use IntentState::*;
    match (from, to) {
        (Reserved, Preparing) => true,
        (Preparing, Reserved) => true,
        (Preparing, Submitted) => true,
        (Reserved, Released) => true,
        (Submitted, IncludedUnconfirmed { .. }) => true,
        (IncludedUnconfirmed { .. }, Finalized) => true,
        (IncludedUnconfirmed { .. }, RevertedFinalized) => true,
        (IncludedUnconfirmed { .. }, CancelFinalized) => true,
        (Submitted, Finalized) => true,
        (Submitted, RevertedFinalized) => true,
        (Submitted, CancelFinalized) => true,
        (Submitted, NeedsOperator { .. }) => true,
        (IncludedUnconfirmed { .. }, NeedsOperator { .. }) => true,
        (NeedsOperator { .. }, Submitted) => true,
        (NeedsOperator { .. }, IncludedUnconfirmed { .. }) => true,
        (NeedsOperator { .. }, Finalized) => true,
        (NeedsOperator { .. }, RevertedFinalized) => true,
        (NeedsOperator { .. }, CancelFinalized) => true,
        // Reorg reopen
        (IncludedUnconfirmed { .. }, Submitted) => true,
        (Finalized, Submitted) | (RevertedFinalized, Submitted) | (CancelFinalized, Submitted) => {
            true
        }
        // Replacement / cancel re-submit stay in Submitted
        (Submitted, Submitted) => true,
        (a, b) if a == b => true,
        _ => false,
    }
}

/// The per-account nonce-intent state machine.
///
/// Deployment precondition: one signer account is owned by at most one process
/// (one SM instance) at a time. Cross-process mutual exclusion is out of scope.
pub struct IntentStateMachine {
    inner: Mutex<IntentStateMachineInner>,
    policy: IntentPolicy,
    signer_address: Address,
    production_send_allowed: bool,
}

struct IntentStateMachineInner {
    nonces: NonceManager,
    live: BTreeMap<u64, NonceIntent>,
    /// Nonces observed on-chain that we never reserved (in-flight foreign txs).
    held_external: HashSet<u64>,
    /// Latest Ready snapshot tip known to the SM (None until first publish).
    current_snapshot: Option<SnapshotId>,
    halted: Option<String>,
    events: Vec<IntentEvent>,
    /// Legacy counters retained for back-compat; authoritative loss/streak live
    /// in the WHI-524 ledger when `accounting` is attached.
    pub revert_count: u64,
    pub cancel_count: u64,
    pub realized_loss_wei: U256,
    accounting: Option<std::sync::Arc<dyn crate::execution::breaker::AccountingCommit>>,
}

impl IntentStateMachine {
    pub fn new(
        signer_address: Address,
        initial_chain: ChainNonceView,
        policy: IntentPolicy,
        production_send_allowed: bool,
    ) -> Result<Self, IntentError> {
        policy.validate()?;
        if initial_chain.pending_nonce < initial_chain.latest_nonce {
            return Err(IntentError::InvalidPolicy(
                "pending nonce cannot be lower than latest nonce".into(),
            ));
        }
        let mut held_external = HashSet::new();
        // Hold unknown in-flight nonces between latest and pending.
        for n in initial_chain.latest_nonce..initial_chain.pending_nonce {
            held_external.insert(n);
        }
        // Start reserving at pending so we never blind-reuse in-flight nonces.
        let nonces = NonceManager::new(initial_chain.pending_nonce);
        Ok(Self {
            inner: Mutex::new(IntentStateMachineInner {
                nonces,
                live: BTreeMap::new(),
                held_external,
                current_snapshot: None,
                halted: None,
                events: Vec::new(),
                revert_count: 0,
                cancel_count: 0,
                realized_loss_wei: U256::ZERO,
                accounting: None,
            }),
            policy,
            signer_address,
            production_send_allowed,
        })
    }

    pub fn policy(&self) -> &IntentPolicy {
        &self.policy
    }

    pub fn signer_address(&self) -> Address {
        self.signer_address
    }

    pub fn production_send_allowed(&self) -> bool {
        self.production_send_allowed
    }

    /// Attach the WHI-524 durable accounting sink. Receipt terminalization will
    /// refuse to proceed if persistence fails.
    pub fn attach_accounting(
        &self,
        accounting: std::sync::Arc<dyn crate::execution::breaker::AccountingCommit>,
    ) -> Result<(), IntentError> {
        self.lock()?.accounting = Some(accounting);
        Ok(())
    }

    /// Ledger-backed stats when accounting is attached; otherwise legacy counters.
    pub fn breaker_stats(&self) -> Result<crate::execution::breaker::BreakerStats, IntentError> {
        let g = self.lock()?;
        if let Some(acc) = g.accounting.as_ref() {
            return Ok(acc.stats());
        }
        Ok(crate::execution::breaker::BreakerStats {
            consecutive_reverts: g.revert_count as u32,
            window_loss_wei: g.realized_loss_wei,
            charged_entries: g.revert_count.saturating_add(g.cancel_count),
            paused: g.halted.is_some(),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, IntentStateMachineInner>, IntentError> {
        self.inner.lock().map_err(|_| IntentError::LockPoisoned)
    }

    pub fn is_halted(&self) -> Result<bool, IntentError> {
        Ok(self.lock()?.halted.is_some())
    }

    pub fn halt_reason(&self) -> Result<Option<String>, IntentError> {
        Ok(self.lock()?.halted.clone())
    }

    pub fn drain_events(&self) -> Result<Vec<IntentEvent>, IntentError> {
        let mut g = self.lock()?;
        let events = std::mem::take(&mut g.events);
        drop(g);
        self.emit_event_metrics(&events);
        Ok(events)
    }

    /// Emit Prometheus series for drained intent events (WHI-532).
    ///
    /// Called from every path that returns events out of the SM so counters do
    /// not depend on callers remembering `drain_events`.
    fn emit_event_metrics(&self, events: &[IntentEvent]) {
        for event in events {
            crate::metrics::record_intent_event(event);
        }
        if events.is_empty() {
            return;
        }
        if let (Ok(live), Ok(next)) = (self.live_intents(), self.peek_next_nonce()) {
            crate::metrics::record_intent_gauges(live.len(), next);
        }
        if let Ok(stats) = self.breaker_stats() {
            crate::metrics::record_breaker_stats(&stats);
        }
    }

    pub fn peek_next_nonce(&self) -> Result<u64, IntentError> {
        Ok(self.lock()?.nonces.peek())
    }

    /// Publish the latest readiness tip so Execute gates can compare SnapshotIds.
    ///
    /// Only [`SnapshotStatus::Ready`] updates the current tip. Syncing / Halted
    /// leave the last Ready tip in place so cancels can still proceed.
    pub fn observe_snapshot(&self, status: &SnapshotStatus) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        if let SnapshotStatus::Ready(snapshot) = status {
            g.current_snapshot = Some(snapshot.snapshot_id());
        }
        Ok(())
    }

    pub fn current_snapshot_id(&self) -> Result<Option<SnapshotId>, IntentError> {
        Ok(self.lock()?.current_snapshot)
    }

    fn ensure_execute_snapshot_gate(
        candidate: &CandidateRef,
        status: &SnapshotStatus,
    ) -> Result<(), IntentError> {
        if !status.allows_execution() {
            return Err(IntentError::SnapshotNotReady);
        }
        let Some(ready) = status.ready_snapshot() else {
            return Err(IntentError::SnapshotNotReady);
        };
        let ready_id = ready.snapshot_id();
        if candidate.snapshot_id != ready_id {
            return Err(IntentError::StaleCandidate {
                attempt: candidate.snapshot_id,
                current: ready_id,
            });
        }
        let current = ready.coverage.pool_universe_fingerprint;
        if current != Some(candidate.pool_universe_fingerprint) {
            return Err(IntentError::StaleTopology {
                candidate: Some(candidate.pool_universe_fingerprint),
                current,
            });
        }
        Ok(())
    }

    pub fn live_intents(&self) -> Result<Vec<NonceIntent>, IntentError> {
        let g = self.lock()?;
        Ok(g.live.values().cloned().collect())
    }

    /// Non-terminal intents that already have a broadcast attempt — pause→cancel targets.
    pub fn intents_needing_pause_cancel(&self) -> Result<Vec<NonceIntent>, IntentError> {
        Ok(self
            .live_intents()?
            .into_iter()
            .filter(|i| !i.state.is_terminal() && i.has_broadcast_attempt())
            .collect())
    }

    /// Pause→pending-cancellation sweep (Rev5 §5): purge a queued candidate slot and
    /// return every live broadcast intent that must be best-effort cancelled.
    ///
    /// Callers drive cancel through the normal prepare/sign/broadcast path (Cancel is
    /// allowed while paused). Service-loop auto-wiring remains DI-12.
    pub fn begin_pause_cancel_sweep<T>(
        &self,
        queue: &LatestWinsSlot<T>,
    ) -> Result<Vec<NonceIntent>, IntentError> {
        let _ = queue.take();
        self.intents_needing_pause_cancel()
    }

    pub fn intent(&self, nonce: u64) -> Result<Option<NonceIntent>, IntentError> {
        let g = self.lock()?;
        Ok(g.live.get(&nonce).cloned())
    }

    /// Reserve a nonce and create a live intent for the candidate.
    ///
    /// Execute reservations require `SnapshotStatus::Ready` and a candidate
    /// whose full `SnapshotId` equals the Ready tip (and the SM's last observed
    /// Ready tip when one has been published).
    pub fn reserve(
        &self,
        candidate: CandidateRef,
        status: &SnapshotStatus,
        block_fee_context: BlockFeeContext,
    ) -> Result<(u64, ExecutionPermit), IntentError> {
        let mut g = self.lock()?;
        if let Some(reason) = g.halted.clone() {
            return Err(IntentError::Halted(reason));
        }
        if !g.held_external.is_empty() {
            let n = *g.held_external.iter().next().unwrap();
            return Err(IntentError::ExternalNonceActivity(n));
        }
        Self::ensure_execute_snapshot_gate(&candidate, status)?;
        // Keep the SM tip in sync with the gate we just accepted.
        if let SnapshotStatus::Ready(snapshot) = status {
            g.current_snapshot = Some(snapshot.snapshot_id());
        }
        let nonce = g.nonces.reserve();
        let intent = NonceIntent {
            nonce,
            candidate: candidate.clone(),
            attempts: Vec::new(),
            state: IntentState::Reserved,
            drop_absent_blocks: 0,
            pending_blocks: 0,
            fee_recovery_retry_available: true,
            retained_inclusion: None,
            terminal_at_block: None,
        };
        g.live.insert(nonce, intent);
        g.events.push(IntentEvent::Reserved { nonce });
        let permit = ExecutionPermit::new(
            IntentAuthority::mint(),
            self.signer_address,
            candidate.route_key.clone(),
            block_fee_context,
            nonce,
            candidate.snapshot_id,
            candidate.header,
            candidate.pool_universe_fingerprint,
        );
        Ok((nonce, permit))
    }

    /// Replace a live intent's Execute candidate after a fresh re-simulation.
    ///
    /// Rejects stale candidates / non-Ready snapshots. Zero-broadcast intents
    /// that become unprofitable are released without constructing a cancel.
    pub fn replace(
        &self,
        nonce: u64,
        fresh_candidate: CandidateRef,
        status: &SnapshotStatus,
        block_fee_context: BlockFeeContext,
        still_profitable: bool,
    ) -> Result<ExecutionPermit, IntentError> {
        let mut g = self.lock()?;
        if let Some(reason) = g.halted.clone() {
            return Err(IntentError::Halted(reason));
        }
        Self::ensure_execute_snapshot_gate(&fresh_candidate, status)?;
        if let SnapshotStatus::Ready(snapshot) = status {
            g.current_snapshot = Some(snapshot.snapshot_id());
        }
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        if intent.state.is_terminal() {
            return Err(IntentError::IllegalTransition {
                from: intent.state.clone(),
                to: IntentState::Preparing,
            });
        }
        // Replacement must re-simulate against a newer/current tip, not reuse
        // the original reservation candidate identity blindly.
        if fresh_candidate.snapshot_id == intent.candidate.snapshot_id
            && fresh_candidate.amount_in == intent.candidate.amount_in
            && fresh_candidate.route_key == intent.candidate.route_key
            && intent.has_broadcast_attempt()
        {
            return Err(IntentError::StaleReplacement);
        }
        if !still_profitable {
            if !intent.has_broadcast_attempt() {
                // Zero-broadcast: release without cancel.
                apply_transition(intent, IntentState::Released)?;
                g.live.remove(&nonce);
                g.events.push(IntentEvent::Released { nonce });
                return Err(IntentError::CancelWithoutBroadcast);
            }
            // Caller must prepare a cancel attempt for the live nonce.
            return Err(IntentError::StaleReplacement);
        }
        intent.candidate = fresh_candidate.clone();
        Ok(ExecutionPermit::new(
            IntentAuthority::mint(),
            self.signer_address,
            fresh_candidate.route_key,
            block_fee_context,
            nonce,
            fresh_candidate.snapshot_id,
            fresh_candidate.header,
            fresh_candidate.pool_universe_fingerprint,
        ))
    }

    /// Mark intent Preparing under the SM lock (non-releasable).
    ///
    /// While Halted, only live intents that already broadcast may re-enter
    /// Preparing (cancel / emergency replacement). Fresh Execute prep is blocked.
    pub fn begin_prepare(&self, nonce: u64) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        let halted = g.halted.clone();
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        if let Some(reason) = halted {
            if !intent.has_broadcast_attempt() {
                return Err(IntentError::Halted(reason));
            }
        }
        apply_transition(intent, IntentState::Preparing)?;
        Ok(())
    }

    /// Return Preparing → Reserved after a pre-hash preparation failure.
    pub fn abort_prepare(&self, nonce: u64) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        if !matches!(intent.state, IntentState::Preparing) {
            return Err(IntentError::IllegalTransition {
                from: intent.state.clone(),
                to: IntentState::Reserved,
            });
        }
        apply_transition(intent, IntentState::Reserved)?;
        Ok(())
    }

    /// Record a signed attempt and transition to Submitted **before** broadcast.
    pub fn record_submission(&self, signed: &SignedSubmission) -> Result<(), IntentError> {
        self.record_submission_with_min_profit(signed, U256::ZERO)
    }

    /// Record a signed submission, retaining the exact calldata `minProfit`.
    pub fn record_submission_with_min_profit(
        &self,
        signed: &SignedSubmission,
        onchain_min_profit: U256,
    ) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        if let Some(reason) = g.halted.clone() {
            // Cancel may still be signed while Halted; Execute stays blocked.
            if !matches!(signed.payload, PreparedPayload::Cancel { .. }) {
                return Err(IntentError::Halted(reason));
            }
        }
        for (&lower, lower_intent) in g.live.range(..signed.nonce) {
            if !lower_intent.state.is_terminal() && !lower_intent.has_broadcast_attempt() {
                return Err(IntentError::BroadcastOrder {
                    nonce: signed.nonce,
                    blocker: lower,
                });
            }
        }
        if !g.live.contains_key(&signed.nonce) {
            return Err(IntentError::UnknownNonce(signed.nonce));
        }
        // Collect superseded hashes first.
        let prior_hashes: Vec<B256> = g
            .live
            .get(&signed.nonce)
            .map(|intent| intent.attempts.iter().map(|a| a.tx_hash).collect())
            .unwrap_or_default();
        for h in &prior_hashes {
            g.events.push(IntentEvent::Superseded {
                nonce: signed.nonce,
                tx_hash: *h,
            });
        }
        let intent = g
            .live
            .get_mut(&signed.nonce)
            .ok_or(IntentError::UnknownNonce(signed.nonce))?;
        if !matches!(
            intent.state,
            IntentState::Preparing | IntentState::Submitted | IntentState::NeedsOperator { .. }
        ) {
            return Err(IntentError::IllegalTransition {
                from: intent.state.clone(),
                to: IntentState::Submitted,
            });
        }
        for prior in intent.attempts.iter_mut() {
            prior.superseded = true;
        }
        let payload = match &signed.payload {
            PreparedPayload::Execute { params, candidate } => {
                intent.candidate = candidate.clone();
                AttemptPayload::Execute {
                    params: params.clone(),
                    candidate: candidate.clone(),
                }
            }
            PreparedPayload::Cancel { to, gas_limit } => AttemptPayload::Cancel {
                to: *to,
                gas_limit: *gas_limit,
            },
        };
        intent.attempts.push(Attempt {
            tx_hash: signed.tx_hash,
            fee_plan: signed.fee_plan.clone(),
            calldata_digest: signed.calldata_digest,
            submitted_at: signed.submitted_at,
            payload,
            onchain_min_profit,
            inclusion: None,
            superseded: false,
        });
        intent.drop_absent_blocks = 0;
        intent.pending_blocks = 0;
        apply_transition(intent, IntentState::Submitted)?;
        g.events.push(IntentEvent::Submitted {
            nonce: signed.nonce,
            tx_hash: signed.tx_hash,
        });
        Ok(())
    }

    /// Mint a replacement/cancel permit reusing the intent nonce.
    pub fn permit_for_nonce(
        &self,
        nonce: u64,
        route_key: RouteKey,
        block_fee_context: BlockFeeContext,
        snapshot_id: SnapshotId,
        header: BlockHeaderContext,
        pool_universe_fingerprint: B256,
    ) -> Result<ExecutionPermit, IntentError> {
        let g = self.lock()?;
        if !g.live.contains_key(&nonce) {
            return Err(IntentError::UnknownNonce(nonce));
        }
        Ok(ExecutionPermit::new(
            IntentAuthority::mint(),
            self.signer_address,
            route_key,
            block_fee_context,
            nonce,
            snapshot_id,
            header,
            pool_universe_fingerprint,
        ))
    }

    /// Release the highest contiguous zero-broadcast Reserved suffix, then
    /// recompute next_nonce = max(chain_pending, 1 + highest remaining
    /// non-terminal intent nonce). This is the only counter-lowering path.
    pub fn reconcile(&self, chain: ChainNonceView) -> Result<Vec<u64>, IntentError> {
        let mut g = self.lock()?;
        // Detect external consumption of unreserved nonces.
        if chain.latest_nonce > g.nonces.peek() {
            // Chain advanced past nonces we never reserved.
            for n in g.nonces.peek()..chain.latest_nonce {
                if !g.live.contains_key(&n) {
                    g.held_external.insert(n);
                }
            }
        }
        // Drop external holds that have settled.
        g.held_external.retain(|n| *n >= chain.latest_nonce);

        // Release highest contiguous zero-broadcast Reserved suffix.
        let mut released = Vec::new();
        let reserved_suffix: Vec<u64> = g
            .live
            .iter()
            .rev()
            .take_while(|(_, intent)| {
                intent.state.is_zero_broadcast_releasable() && !intent.has_broadcast_attempt()
            })
            .map(|(&n, _)| n)
            .collect();
        // reserved_suffix is highest→lowest; release highest first is fine.
        // Ensure contiguity from the top.
        let mut expected: Option<u64> = None;
        for n in reserved_suffix {
            if let Some(exp) = expected {
                if n + 1 != exp {
                    break;
                }
            }
            expected = Some(n);
            if let Some(mut intent) = g.live.remove(&n) {
                let _ = apply_transition(&mut intent, IntentState::Released);
                released.push(n);
                g.events.push(IntentEvent::Released { nonce: n });
            }
        }

        // Recompute next nonce (may lower).
        let highest_live = g
            .live
            .iter()
            .filter(|(_, i)| !i.state.is_terminal())
            .map(|(&n, _)| n)
            .max();
        let from_intents = highest_live.map(|n| n.saturating_add(1)).unwrap_or(0);
        let next = chain.pending_nonce.max(from_intents);
        // Never lower below unsettled external holds.
        let next = g
            .held_external
            .iter()
            .max()
            .map(|n| next.max(n + 1))
            .unwrap_or(next);
        g.nonces.set_next(next);
        Ok(released)
    }

    pub fn mark_needs_operator(
        &self,
        nonce: u64,
        reason: NeedsOperatorReason,
    ) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        apply_transition(
            intent,
            IntentState::NeedsOperator {
                reason: reason.clone(),
            },
        )?;
        g.events.push(IntentEvent::NeedsOperator { nonce, reason });
        Ok(())
    }

    /// Observe a new canonical block: reorg check, receipt mapping, stuck/drop.
    pub fn on_new_block(
        &self,
        head: CanonicalBlock,
        // number → canonical hash for the reorg window
        canonical: &HashMap<u64, B256>,
        // tx_hash → outcome (None means still pending / absent)
        receipts: &HashMap<B256, Option<ReceiptOutcome>>,
        // tx_hash present in mempool/chain (Some(true)=present, Some(false)=absent, None=unknown)
        tx_presence: &HashMap<B256, bool>,
        chain: ChainNonceView,
    ) -> Result<Vec<IntentEvent>, IntentError> {
        let mut actions = Vec::new();
        {
            let mut g = self.lock()?;
            if g.halted.is_some() {
                let events = g.events.clone();
                drop(g);
                self.emit_event_metrics(&events);
                return Ok(events);
            }

            // 1) Inclusion-record reorg check first.
            let nonces: Vec<u64> = g.live.keys().copied().collect();
            for nonce in nonces {
                let Some(intent) = g.live.get(&nonce).cloned() else {
                    continue;
                };
                let inclusion = intent.retained_inclusion.clone().or_else(|| {
                    if let IntentState::IncludedUnconfirmed {
                        block_number,
                        block_hash,
                    } = &intent.state
                    {
                        Some(InclusionRecord {
                            block_number: *block_number,
                            block_hash: *block_hash,
                        })
                    } else {
                        intent
                            .attempts
                            .iter()
                            .rev()
                            .find_map(|a| a.inclusion.clone())
                    }
                });
                let Some(inc) = inclusion else {
                    continue;
                };
                let depth = head.number.saturating_sub(inc.block_number);
                if depth > self.policy.reorg_track_blocks {
                    // Beyond window — if hash diverges, halt for full resync.
                    if let Some(canon) = canonical.get(&inc.block_number) {
                        if *canon != inc.block_hash {
                            let reason = format!(
                                "reorg deeper than window at block {} (window {})",
                                inc.block_number, self.policy.reorg_track_blocks
                            );
                            g.halted = Some(reason.clone());
                            g.events.push(IntentEvent::Halted {
                                reason: reason.clone(),
                            });
                            let events = std::mem::take(&mut g.events);
                            drop(g);
                            self.emit_event_metrics(&events);
                            return Ok(events);
                        }
                    }
                    continue;
                }
                if let Some(canon) = canonical.get(&inc.block_number) {
                    if *canon != inc.block_hash {
                        // Inclusion invalidated. Prefer a sibling attempt with a
                        // canonical receipt before reopening.
                        let mut adopted: Option<(B256, ReceiptOutcome, AttemptPayload)> = None;
                        for attempt in &intent.attempts {
                            if let Some(Some(outcome)) = receipts.get(&attempt.tx_hash) {
                                if let Some(h) = canonical.get(&outcome.block_number) {
                                    if *h == outcome.block_hash {
                                        adopted = Some((
                                            attempt.tx_hash,
                                            outcome.clone(),
                                            attempt.payload.clone(),
                                        ));
                                        break;
                                    }
                                }
                            }
                        }
                        if let Some((tx_hash, outcome, payload)) = adopted {
                            let min_profit = intent
                                .attempts
                                .iter()
                                .find(|a| a.tx_hash == tx_hash)
                                .map(|a| a.onchain_min_profit)
                                .unwrap_or(U256::ZERO);
                            let old_hash = intent
                                .attempts
                                .iter()
                                .find(|a| a.inclusion.as_ref() == Some(&inc))
                                .map(|a| a.tx_hash);
                            if let Some(acc) = g.accounting.clone() {
                                if let Some(old_hash) = old_hash {
                                    if old_hash != tx_hash {
                                        persist_inclusion_reversal(
                                            acc.as_ref(),
                                            nonce,
                                            old_hash,
                                            &inc,
                                            "sibling_adoption",
                                        )?;
                                    }
                                }
                                persist_terminal_accounting(
                                    acc.as_ref(),
                                    nonce,
                                    tx_hash,
                                    &outcome,
                                    &payload,
                                    min_profit,
                                )?;
                            }
                            drop(intent);
                            if let Some(intent) = g.live.get_mut(&nonce) {
                                let (ev, rd, cd, loss) = apply_receipt_mapping(
                                    intent,
                                    &tx_hash,
                                    &outcome,
                                    &payload,
                                    head.number,
                                )?;
                                g.events.extend(ev);
                                g.revert_count = g.revert_count.saturating_add(rd);
                                g.cancel_count = g.cancel_count.saturating_add(cd);
                                g.realized_loss_wei = g.realized_loss_wei.saturating_add(loss);
                            }
                        } else {
                            let old_hash = intent
                                .attempts
                                .iter()
                                .find(|a| a.inclusion.as_ref() == Some(&inc))
                                .map(|a| a.tx_hash);
                            if let (Some(acc), Some(tx_hash)) = (g.accounting.clone(), old_hash) {
                                persist_inclusion_reversal(
                                    acc.as_ref(),
                                    nonce,
                                    tx_hash,
                                    &inc,
                                    "reorg_reopen",
                                )?;
                            }
                            if let Some(intent) = g.live.get_mut(&nonce) {
                                for a in intent.attempts.iter_mut() {
                                    a.inclusion = None;
                                }
                                intent.retained_inclusion = None;
                                apply_transition(intent, IntentState::Submitted)?;
                                g.events.push(IntentEvent::Reopened { nonce });
                            }
                            actions.push(nonce);
                        }
                    }
                }
            }
        }

        // Reconcile after any reopen.
        if !actions.is_empty() {
            let _ = self.reconcile(chain.clone())?;
        }

        // 2) Map new receipts by attempt kind.
        {
            let mut g = self.lock()?;
            let nonces: Vec<u64> = g.live.keys().copied().collect();
            for nonce in nonces {
                let intent_snapshot = match g.live.get(&nonce) {
                    Some(i) => i.clone(),
                    None => continue,
                };
                if intent_snapshot.state.is_terminal()
                    && !matches!(
                        intent_snapshot.state,
                        IntentState::Finalized
                            | IntentState::RevertedFinalized
                            | IntentState::CancelFinalized
                    )
                {
                    // Released etc.
                    continue;
                }
                for attempt in intent_snapshot.attempts.iter().rev() {
                    if let Some(Some(outcome)) = receipts.get(&attempt.tx_hash) {
                        // Verify inclusion block is canonical.
                        if let Some(h) = canonical.get(&outcome.block_number) {
                            if *h != outcome.block_hash {
                                continue;
                            }
                        } else {
                            continue;
                        }
                        let conf = head.number.saturating_sub(outcome.block_number);
                        if conf < self.policy.confirmation_depth {
                            // Mark included unconfirmed if not yet.
                            if let Some(intent) = g.live.get_mut(&nonce) {
                                if matches!(
                                    intent.state,
                                    IntentState::Submitted | IntentState::NeedsOperator { .. }
                                ) {
                                    if let Some(a) = intent
                                        .attempts
                                        .iter_mut()
                                        .find(|a| a.tx_hash == attempt.tx_hash)
                                    {
                                        a.inclusion = Some(InclusionRecord {
                                            block_number: outcome.block_number,
                                            block_hash: outcome.block_hash,
                                        });
                                    }
                                    apply_transition(
                                        intent,
                                        IntentState::IncludedUnconfirmed {
                                            block_number: outcome.block_number,
                                            block_hash: outcome.block_hash,
                                        },
                                    )?;
                                    g.events.push(IntentEvent::IncludedUnconfirmed {
                                        nonce,
                                        tx_hash: attempt.tx_hash,
                                        block_number: outcome.block_number,
                                        block_hash: outcome.block_hash,
                                    });
                                }
                            }
                            break;
                        }
                        let accounting = g.accounting.clone();
                        if let Some(intent) = g.live.get_mut(&nonce) {
                            if let Some(acc) = accounting {
                                persist_terminal_accounting(
                                    acc.as_ref(),
                                    nonce,
                                    attempt.tx_hash,
                                    outcome,
                                    &attempt.payload,
                                    attempt.onchain_min_profit,
                                )?;
                            }
                            let (ev, rd, cd, loss) = apply_receipt_mapping(
                                intent,
                                &attempt.tx_hash,
                                outcome,
                                &attempt.payload,
                                head.number,
                            )?;
                            g.events.extend(ev);
                            g.revert_count = g.revert_count.saturating_add(rd);
                            g.cancel_count = g.cancel_count.saturating_add(cd);
                            g.realized_loss_wei = g.realized_loss_wei.saturating_add(loss);
                        }
                        break;
                    }
                }
            }

            // 3) Stuck / drop detection for still-pending intents.
            let nonces: Vec<u64> = g.live.keys().copied().collect();
            for nonce in nonces {
                let Some(intent) = g.live.get_mut(&nonce) else {
                    continue;
                };
                if !matches!(intent.state, IntentState::Submitted) {
                    continue;
                }
                let Some(latest) = intent.latest_attempt().map(|a| a.tx_hash) else {
                    continue;
                };
                intent.pending_blocks = intent.pending_blocks.saturating_add(1);
                match tx_presence.get(&latest) {
                    Some(false) => {
                        intent.drop_absent_blocks = intent.drop_absent_blocks.saturating_add(1);
                    }
                    Some(true) => {
                        intent.drop_absent_blocks = 0;
                    }
                    None => {}
                }
            }
        }

        // Prune terminal intents outside reorg window.
        {
            let mut g = self.lock()?;
            let prune: Vec<u64> = g
                .live
                .iter()
                .filter_map(|(&n, i)| {
                    if i.state.is_terminal() {
                        if let Some(at) = i.terminal_at_block {
                            if head.number.saturating_sub(at) > self.policy.reorg_track_blocks {
                                return Some(n);
                            }
                        }
                    }
                    None
                })
                .collect();
            for n in prune {
                g.live.remove(&n);
            }
        }

        let mut g = self.lock()?;
        let events = std::mem::take(&mut g.events);
        drop(g);
        self.emit_event_metrics(&events);
        Ok(events)
    }

    /// Whether the latest attempt is stuck (no receipt after N new blocks).
    pub fn is_stuck(&self, nonce: u64) -> Result<bool, IntentError> {
        let g = self.lock()?;
        let intent = g.live.get(&nonce).ok_or(IntentError::UnknownNonce(nonce))?;
        Ok(matches!(intent.state, IntentState::Submitted)
            && intent.pending_blocks >= self.policy.stuck_after_blocks)
    }

    /// Whether the latest attempt is debounced-dropped.
    pub fn is_dropped(&self, nonce: u64) -> Result<bool, IntentError> {
        let g = self.lock()?;
        let intent = g.live.get(&nonce).ok_or(IntentError::UnknownNonce(nonce))?;
        Ok(matches!(intent.state, IntentState::Submitted)
            && intent.drop_absent_blocks >= self.policy.drop_confirm_blocks)
    }

    pub fn should_cancel_for_budget(&self, nonce: u64) -> Result<bool, IntentError> {
        let g = self.lock()?;
        let intent = g.live.get(&nonce).ok_or(IntentError::UnknownNonce(nonce))?;
        Ok(intent.execute_attempt_count() >= self.policy.max_attempts_per_intent as usize)
    }

    pub fn cancel_budget_exhausted(&self, nonce: u64) -> Result<bool, IntentError> {
        let g = self.lock()?;
        let intent = g.live.get(&nonce).ok_or(IntentError::UnknownNonce(nonce))?;
        Ok(intent.cancel_attempt_count() >= self.policy.max_cancel_attempts as usize)
    }

    /// Automatic one-shot exit from `NeedsOperator{CancelUnpriceable}`.
    ///
    /// Spec: when a cancel was unpriceable and a later `BlockFeeContext` makes it
    /// priceable within the cancel cap **and** cancel budget remains, re-arm
    /// exactly one cancel retry. Budget exhaustion never auto-exits.
    pub fn try_rearm_fee_recovery_cancel(
        &self,
        nonce: u64,
        latest_context: &BlockFeeContext,
        block_gas_reserve: u64,
    ) -> Result<Option<FeePlan>, IntentError> {
        let mut g = self.lock()?;
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        match &intent.state {
            IntentState::NeedsOperator {
                reason: NeedsOperatorReason::CancelUnpriceable,
            } => {}
            IntentState::NeedsOperator { reason } => {
                return Err(IntentError::NeedsOperator(reason.clone()));
            }
            other => {
                return Err(IntentError::IllegalTransition {
                    from: other.clone(),
                    to: IntentState::Submitted,
                });
            }
        }
        if !intent.fee_recovery_retry_available {
            return Ok(None);
        }
        if intent.cancel_attempt_count() >= self.policy.max_cancel_attempts as usize {
            return Ok(None);
        }
        let prior = intent
            .highest_prior_fees()
            .ok_or(IntentError::CancelWithoutBroadcast)?;
        match FeePlan::for_cancel(
            self.policy.cancel_gas_limit,
            prior,
            self.policy.fee_bump_bps,
            self.policy.cancel_fee_cap_wei,
            latest_context,
            block_gas_reserve,
        ) {
            Ok(plan) => {
                intent.fee_recovery_retry_available = false;
                apply_transition(intent, IntentState::Submitted)?;
                Ok(Some(plan))
            }
            Err(FeePlanError::CancelFeeCapExceeded { .. })
            | Err(FeePlanError::CancelFeeBelowBase { .. }) => Ok(None),
            Err(e) => Err(IntentError::from(e)),
        }
    }

    /// Explicit in-process operator recovery for NeedsOperator.
    ///
    /// `extra_cancel_attempts` raises the effective cancel budget for this intent
    /// by superseding prior cancel attempts (they remain in history for audit but
    /// no longer count against `max_cancel_attempts`).
    pub fn operator_recover(
        &self,
        nonce: u64,
        expected_hashes: &[B256],
        extra_cancel_attempts: u32,
    ) -> Result<(), IntentError> {
        let mut g = self.lock()?;
        let intent = g
            .live
            .get_mut(&nonce)
            .ok_or(IntentError::UnknownNonce(nonce))?;
        match &intent.state {
            IntentState::NeedsOperator { .. } => {}
            other => {
                return Err(IntentError::IllegalTransition {
                    from: other.clone(),
                    to: IntentState::Submitted,
                });
            }
        }
        let have: HashSet<B256> = intent.attempts.iter().map(|a| a.tx_hash).collect();
        for h in expected_hashes {
            if !have.contains(h) {
                return Err(IntentError::InvalidPolicy(format!(
                    "operator recovery hash {h} not known for nonce {nonce}"
                )));
            }
        }
        if extra_cancel_attempts > 0 {
            let mut remaining = extra_cancel_attempts;
            for attempt in intent.attempts.iter_mut() {
                if remaining == 0 {
                    break;
                }
                if attempt.payload.is_cancel() && !attempt.superseded {
                    attempt.superseded = true;
                    remaining = remaining.saturating_sub(1);
                }
            }
        }
        intent.fee_recovery_retry_available = true;
        apply_transition(intent, IntentState::Submitted)?;
        Ok(())
    }

    /// Compute finite deadline from header timestamp + configured horizon.
    pub fn deadline_for_header(&self, header: &BlockHeaderContext) -> Result<U256, IntentError> {
        let secs = self.policy.execution_deadline_secs;
        deadline_from_header_timestamp(header.block_timestamp, secs).map_err(|_| {
            IntentError::DeadlineOverflow {
                timestamp: header.block_timestamp,
                horizon: secs,
            }
        })
    }

    /// Apply a fee bump over highest prior attempt fees.
    pub fn bump_fees(
        &self,
        prior: PriorFees,
        base_fee: u128,
        cap: u128,
    ) -> Result<PriorFees, IntentError> {
        let bumped = bump_prior_fees(prior, self.policy.fee_bump_bps, base_fee)?;
        if bumped.max_fee > cap {
            return Err(IntentError::Fee(format!(
                "bumped max fee {} exceeds cap {cap}",
                bumped.max_fee
            )));
        }
        Ok(bumped)
    }
}

fn apply_transition(intent: &mut NonceIntent, to: IntentState) -> Result<(), IntentError> {
    if !transition_allowed(&intent.state, &to) {
        return Err(IntentError::IllegalTransition {
            from: intent.state.clone(),
            to,
        });
    }
    intent.state = to;
    Ok(())
}

/// Default receipt-utilization threshold matching `ExecutorConfig` (9_500 bps).
const DEFAULT_RECEIPT_GAS_UTILIZATION_BPS: u16 = 9_500;

fn terminal_kind_from_payload(payload: &AttemptPayload) -> crate::execution::breaker::TerminalKind {
    if payload.is_cancel() {
        crate::execution::breaker::TerminalKind::Cancel
    } else {
        crate::execution::breaker::TerminalKind::Execute
    }
}

fn persist_terminal_accounting(
    accounting: &dyn crate::execution::breaker::AccountingCommit,
    nonce: u64,
    tx_hash: B256,
    outcome: &ReceiptOutcome,
    payload: &AttemptPayload,
    onchain_min_profit: U256,
) -> Result<(), IntentError> {
    accounting
        .persist_terminal(crate::execution::breaker::AccountingRecord {
            nonce,
            tx_hash,
            block_number: outcome.block_number,
            block_hash: outcome.block_hash,
            kind: terminal_kind_from_payload(payload),
            success: outcome.success,
            actual_cost: outcome.actual_cost(),
            execution_layer_only: outcome.execution_layer_only,
            onchain_min_profit,
        })
        .map_err(|e| IntentError::AccountingPersistFailed(e.to_string()))
}

fn persist_inclusion_reversal(
    accounting: &dyn crate::execution::breaker::AccountingCommit,
    nonce: u64,
    tx_hash: B256,
    inclusion: &InclusionRecord,
    reason: &str,
) -> Result<(), IntentError> {
    accounting
        .persist_reversal(crate::execution::breaker::ReversalRecord {
            nonce,
            tx_hash,
            block_number: inclusion.block_number,
            block_hash: inclusion.block_hash,
            reason: reason.into(),
        })
        .map_err(|e| IntentError::AccountingPersistFailed(e.to_string()))
}

fn apply_receipt_mapping(
    intent: &mut NonceIntent,
    tx_hash: &B256,
    outcome: &ReceiptOutcome,
    payload: &AttemptPayload,
    head_number: u64,
) -> Result<(Vec<IntentEvent>, u64, u64, U256), IntentError> {
    let mut events = Vec::new();
    let mut revert_delta = 0u64;
    let mut cancel_delta = 0u64;
    let mut loss = U256::ZERO;
    let attempt_fee_plan = intent
        .attempts
        .iter()
        .find(|a| a.tx_hash == *tx_hash)
        .map(|a| a.fee_plan.clone());
    if let Some(a) = intent.attempts.iter_mut().find(|a| a.tx_hash == *tx_hash) {
        a.inclusion = Some(InclusionRecord {
            block_number: outcome.block_number,
            block_hash: outcome.block_hash,
        });
    }
    intent.retained_inclusion = Some(InclusionRecord {
        block_number: outcome.block_number,
        block_hash: outcome.block_hash,
    });
    intent.terminal_at_block = Some(head_number);
    let cost = outcome.actual_cost();
    match payload {
        AttemptPayload::Execute { .. } => {
            if outcome.success {
                if let Some(fee_plan) = attempt_fee_plan.as_ref() {
                    // Cancel never qualifies; Execute success always runs the
                    // measured-profile receipt check and surfaces re-qualification.
                    if let Err(FeePlanError::ReceiptGasThresholdExceeded {
                        gas_used,
                        gas_limit,
                        ..
                    }) = fee_plan
                        .qualify_receipt_gas(outcome.gas_used, DEFAULT_RECEIPT_GAS_UTILIZATION_BPS)
                    {
                        events.push(IntentEvent::GasProfileRequalification {
                            nonce: intent.nonce,
                            tx_hash: *tx_hash,
                            gas_used,
                            gas_limit,
                            utilization_bps: DEFAULT_RECEIPT_GAS_UTILIZATION_BPS,
                        });
                    }
                }
                apply_transition(intent, IntentState::Finalized)?;
                events.push(IntentEvent::Finalized {
                    nonce: intent.nonce,
                    tx_hash: *tx_hash,
                    success: true,
                    actual_cost: cost,
                    execution_layer_only: outcome.execution_layer_only,
                });
            } else {
                apply_transition(intent, IntentState::RevertedFinalized)?;
                revert_delta = 1;
                loss = cost;
                events.push(IntentEvent::Finalized {
                    nonce: intent.nonce,
                    tx_hash: *tx_hash,
                    success: false,
                    actual_cost: cost,
                    execution_layer_only: outcome.execution_layer_only,
                });
            }
        }
        AttemptPayload::Cancel { .. } => {
            apply_transition(intent, IntentState::CancelFinalized)?;
            cancel_delta = 1;
            loss = cost;
            events.push(IntentEvent::CancelFinalized {
                nonce: intent.nonce,
                tx_hash: *tx_hash,
                success: outcome.success,
                actual_cost: cost,
                execution_layer_only: outcome.execution_layer_only,
                anomaly: !outcome.success,
            });
        }
    }
    for a in intent.attempts.iter_mut() {
        if a.tx_hash != *tx_hash {
            a.superseded = true;
        }
    }
    Ok((events, revert_delta, cancel_delta, loss))
}

/// Shared latest-wins job slot for discovery → execution handoff.
#[derive(Debug)]
pub struct LatestWinsSlot<T> {
    inner: Mutex<Option<T>>,
}

impl<T> Default for LatestWinsSlot<T> {
    fn default() -> Self {
        Self {
            inner: Mutex::new(None),
        }
    }
}

impl<T> LatestWinsSlot<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, job: T) {
        if let Ok(mut g) = self.inner.lock() {
            *g = Some(job);
        }
    }

    pub fn take(&self) -> Option<T> {
        self.inner.lock().ok().and_then(|mut g| g.take())
    }
}

/// Shared handle.
pub type SharedIntentStateMachine = Arc<IntentStateMachine>;

#[cfg(test)]
#[path = "intent_tests.rs"]
mod intent_tests;

#[cfg(test)]
pub mod test_support {
    use super::IntentAuthority;
    pub fn authority() -> IntentAuthority {
        IntentAuthority::mint()
    }
}
