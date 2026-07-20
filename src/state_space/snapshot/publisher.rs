//! Atomic MarketSnapshot publication with explicit readiness (WHI-510).
//!
//! Consumers must only quote from [`SnapshotStatus::Ready`]. On any new head,
//! gap, fork, or read failure the publisher leaves Ready immediately. The last
//! good snapshot is retained only as a recovery baseline — never as an
//! executable quote source while the canonical head is unresolved.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::continuity::{classify_head, HeadDecision};
use super::status::{ForkKind, HaltReason, SnapshotStatus};
use super::types::{MarketSnapshot, ObservedHead, SnapshotId};

/// Publishes complete snapshots atomically and exposes readiness to consumers.
#[derive(Debug, Clone)]
pub struct SnapshotPublisher {
    status: Arc<RwLock<SnapshotStatus>>,
    /// Last successfully published snapshot; kept for resync/unwind baselines only.
    recovery_baseline: Arc<RwLock<Option<Arc<MarketSnapshot>>>>,
    /// Last tip identity accepted for continuity checks (Ready or recovery).
    last_tip: Arc<RwLock<Option<SnapshotId>>>,
}

impl Default for SnapshotPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotPublisher {
    /// Starts in [`SnapshotStatus::Syncing`] with no baseline.
    pub fn new() -> Self {
        Self {
            status: Arc::new(RwLock::new(SnapshotStatus::Syncing)),
            recovery_baseline: Arc::new(RwLock::new(None)),
            last_tip: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn status(&self) -> SnapshotStatus {
        self.status.read().await.clone()
    }

    /// Executable snapshot only when Ready.
    pub async fn ready_snapshot(&self) -> Option<Arc<MarketSnapshot>> {
        self.status.read().await.ready_snapshot().cloned()
    }

    pub async fn allows_execution(&self) -> bool {
        self.status.read().await.allows_execution()
    }

    /// Last good snapshot for recovery; not a quote source while not Ready.
    pub async fn recovery_baseline(&self) -> Option<Arc<MarketSnapshot>> {
        self.recovery_baseline.read().await.clone()
    }

    pub async fn last_tip(&self) -> Option<SnapshotId> {
        *self.last_tip.read().await
    }

    /// If currently Ready, park that snapshot as the recovery baseline (not quotable).
    async fn demote_ready_to_baseline(&self, status: &mut SnapshotStatus) {
        if let SnapshotStatus::Ready(snapshot) = status {
            *self.recovery_baseline.write().await = Some(Arc::clone(snapshot));
            *self.last_tip.write().await = Some(snapshot.id);
        }
    }

    /// Leave Ready immediately when a new head is being assembled.
    ///
    /// The previous Ready snapshot becomes the recovery baseline.
    pub async fn begin_sync(&self) {
        let mut status = self.status.write().await;
        self.demote_ready_to_baseline(&mut status).await;
        *status = SnapshotStatus::Syncing;
    }

    /// Halt quoting. Preserves recovery baseline; never publishes partial state.
    pub async fn halt(&self, reason: HaltReason) {
        let mut status = self.status.write().await;
        self.demote_ready_to_baseline(&mut status).await;
        *status = SnapshotStatus::Halted(reason);
    }

    /// Atomically publish a complete snapshot as Ready.
    ///
    /// Call only after all reads / coverage / identity checks for this snapshot succeeded.
    pub async fn publish(&self, snapshot: MarketSnapshot) {
        let arc = snapshot.into_arc();
        *self.last_tip.write().await = Some(arc.id);
        *self.recovery_baseline.write().await = Some(Arc::clone(&arc));
        *self.status.write().await = SnapshotStatus::Ready(arc);
    }

    /// Apply a mid-assembly failure: leave Ready/Syncing → Halted, baseline intact.
    pub async fn fail_read(&self, message: impl Into<String>) {
        self.halt(HaltReason::ReadFailure(message.into())).await;
    }

    /// Classify `observed` against the last tip and update readiness accordingly.
    ///
    /// Returns the decision so callers can route gaps to backfill (M1-7) or forks
    /// to resync. On [`HeadDecision::Advance`] / [`HeadDecision::Bootstrap`] the
    /// publisher enters Syncing and the caller must assemble then [`publish`] or
    /// [`fail_read`]. Duplicates leave status unchanged.
    pub async fn observe_head(&self, observed: &ObservedHead) -> HeadDecision {
        let last = *self.last_tip.read().await;
        let decision = classify_head(last.as_ref(), observed);

        match &decision {
            HeadDecision::Duplicate => {
                // Idempotent: do not leave Ready, do not re-publish.
            }
            HeadDecision::Bootstrap | HeadDecision::Advance => {
                self.begin_sync().await;
            }
            HeadDecision::Fork(kind) => {
                let previous = last.unwrap_or_else(|| {
                    SnapshotId::new(observed.chain_id, 0, alloy::primitives::B256::ZERO)
                });
                self.halt(HaltReason::Fork {
                    previous,
                    observed_number: observed.number,
                    observed_hash: observed.hash,
                    observed_parent: observed.parent_hash,
                    kind: *kind,
                })
                .await;
            }
            HeadDecision::Gap {
                last_number,
                observed_number,
            } => {
                self.halt(HaltReason::Gap {
                    last_number: *last_number,
                    observed_number: *observed_number,
                })
                .await;
            }
        }

        decision
    }

    /// Convenience: whether a fork decision requires resync (always true for Fork).
    pub fn fork_requires_resync(kind: ForkKind) -> bool {
        matches!(
            kind,
            ForkKind::SameHeightReplacement
                | ForkKind::HeightRollback
                | ForkKind::WrongParent
                | ForkKind::ChainIdMismatch
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::B256;

    use super::*;
    use crate::state_space::snapshot::types::{BlockHeaderContext, ProtocolCoverage};

    fn h(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn snapshot(number: u64, hash: u8, parent: u8) -> MarketSnapshot {
        MarketSnapshot::new(
            SnapshotId::new(5000, number, h(hash)),
            BlockHeaderContext::new(h(parent), 1_700_000_000 + number),
            HashMap::new(),
            ProtocolCoverage::default(),
        )
    }

    fn head(number: u64, hash: u8, parent: u8) -> ObservedHead {
        ObservedHead::new(5000, number, h(hash), h(parent), 1_700_000_000 + number)
    }

    #[tokio::test]
    async fn publish_makes_ready_and_allows_execution() {
        let pub_ = SnapshotPublisher::new();
        assert!(!pub_.allows_execution().await);
        pub_.publish(snapshot(10, 1, 0)).await;
        assert!(pub_.allows_execution().await);
        let ready = pub_.ready_snapshot().await.unwrap();
        assert_eq!(ready.id.block_number, 10);
        assert_eq!(ready.block_hash(), h(1));
    }

    #[tokio::test]
    async fn mid_read_failure_keeps_baseline_and_leaves_ready() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;
        let baseline_hash = pub_.ready_snapshot().await.unwrap().block_hash();

        // New head starts assembly.
        let decision = pub_.observe_head(&head(11, 2, 1)).await;
        assert_eq!(decision, HeadDecision::Advance);
        assert!(!pub_.allows_execution().await);
        assert!(matches!(pub_.status().await, SnapshotStatus::Syncing));

        // Failure mid-assembly.
        pub_.fail_read("slot0 eth_call failed").await;
        assert!(!pub_.allows_execution().await);
        assert!(matches!(
            pub_.status().await,
            SnapshotStatus::Halted(HaltReason::ReadFailure(_))
        ));

        // Previous published snapshot intact as recovery baseline only.
        let baseline = pub_.recovery_baseline().await.unwrap();
        assert_eq!(baseline.block_hash(), baseline_hash);
        assert_eq!(baseline.id.block_number, 10);
        // Not executable until a complete current snapshot is published.
        assert!(pub_.ready_snapshot().await.is_none());
    }

    #[tokio::test]
    async fn same_height_replacement_halts_and_requires_resync() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let decision = pub_.observe_head(&head(10, 9, 0)).await;
        assert_eq!(
            decision,
            HeadDecision::Fork(ForkKind::SameHeightReplacement)
        );
        assert!(!pub_.allows_execution().await);
        match pub_.status().await {
            SnapshotStatus::Halted(HaltReason::Fork { kind, .. }) => {
                assert!(SnapshotPublisher::fork_requires_resync(kind));
                assert_eq!(kind, ForkKind::SameHeightReplacement);
            }
            other => panic!("expected fork halt, got {other:?}"),
        }
        // Recovery baseline still the last good snapshot.
        assert_eq!(
            pub_.recovery_baseline().await.unwrap().block_hash(),
            h(1)
        );
    }

    #[tokio::test]
    async fn wrong_parent_child_halts_and_requires_resync() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let decision = pub_.observe_head(&head(11, 2, 0xEE)).await;
        assert_eq!(decision, HeadDecision::Fork(ForkKind::WrongParent));
        assert!(!pub_.allows_execution().await);
        assert!(matches!(
            pub_.status().await,
            SnapshotStatus::Halted(HaltReason::Fork {
                kind: ForkKind::WrongParent,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn exact_duplicate_is_idempotent() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let decision = pub_.observe_head(&head(10, 1, 0)).await;
        assert_eq!(decision, HeadDecision::Duplicate);
        assert!(pub_.allows_execution().await);
        assert_eq!(pub_.ready_snapshot().await.unwrap().block_hash(), h(1));
    }

    #[tokio::test]
    async fn gap_halts_quoting() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let decision = pub_.observe_head(&head(15, 5, 1)).await;
        assert_eq!(
            decision,
            HeadDecision::Gap {
                last_number: 10,
                observed_number: 15
            }
        );
        assert!(!pub_.allows_execution().await);
    }

    #[tokio::test]
    async fn successful_publish_after_failure_restores_ready() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;
        pub_.observe_head(&head(11, 2, 1)).await;
        pub_.fail_read("transient").await;
        assert!(!pub_.allows_execution().await);

        pub_.publish(snapshot(11, 2, 1)).await;
        assert!(pub_.allows_execution().await);
        assert_eq!(pub_.ready_snapshot().await.unwrap().id.block_number, 11);
    }

    #[tokio::test]
    async fn replayed_candidates_share_one_block_hash() {
        use crate::state_space::snapshot::{
            hash_pinned_logs_filter, snapshot_state_block_id, AssemblyHashGuard,
        };
        use alloy::rpc::types::Filter;

        let pub_ = SnapshotPublisher::new();

        // Simulate assembling one snapshot from multiple hash-pinned inputs
        // (state call + logs + a second state call), then deriving candidates.
        let id = SnapshotId::new(5000, 42, h(7));
        let mut guard = AssemblyHashGuard::new(id.block_hash);

        // Preferred path: EIP-1898 state pin + block-hash log filter.
        let state_id = snapshot_state_block_id(&id);
        match state_id {
            alloy::eips::BlockId::Hash(rpc) => {
                guard.record(rpc.block_hash).unwrap();
                assert_eq!(rpc.require_canonical, Some(true));
            }
            other => panic!("expected hash pin, got {other:?}"),
        }
        let logs_filter = hash_pinned_logs_filter(Filter::new(), id.block_hash);
        guard.record(logs_filter.get_block_hash().unwrap()).unwrap();
        guard.record(id.block_hash).unwrap(); // second state read

        let snap = MarketSnapshot::new(
            id,
            BlockHeaderContext::new(h(6), 1_700_000_042),
            HashMap::new(),
            ProtocolCoverage::default(),
        );
        let block_hash = snap.block_hash();
        pub_.publish(snap).await;

        let ready = pub_.ready_snapshot().await.unwrap();
        // Every candidate input identity must match the single assembled hash.
        #[derive(Clone, Copy)]
        struct CandidateInput {
            snapshot_id: SnapshotId,
        }
        let candidates: Vec<CandidateInput> = (0..5)
            .map(|_| CandidateInput {
                snapshot_id: ready.snapshot_id(),
            })
            .collect();
        assert!(candidates
            .iter()
            .all(|c| c.snapshot_id.block_hash == block_hash));
        assert!(candidates.iter().all(|c| c.snapshot_id == ready.id));
        assert_eq!(guard.expected(), Some(block_hash));
    }
}
