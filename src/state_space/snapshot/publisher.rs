//! Atomic MarketSnapshot publication with explicit readiness (WHI-510).
//!
//! Consumers must only quote from [`SnapshotStatus::Ready`]. On any new head,
//! gap, fork, or read failure the publisher leaves Ready immediately. The last
//! good snapshot is retained only as a recovery baseline — never as an
//! executable quote source while the canonical head is unresolved.

use std::sync::Arc;

use tokio::sync::RwLock;

use super::continuity::{classify_head, AssembleKind, HeadDecision, HeadObservation};
use super::status::{HaltReason, SnapshotStatus};
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
    ///
    /// Does **not** write `last_tip`. Continuity tip is only established by a successful
    /// live [`publish`]. Writing `snapshot.id` here would re-seed a discovery Ready
    /// (intentionally `last_tip = None`) and turn a failed first-head assemble into a
    /// permanent Gap/Halt against a stale tip before M1-7 exists.
    async fn demote_ready_to_baseline(&self, status: &mut SnapshotStatus) {
        if let SnapshotStatus::Ready(snapshot) = status {
            *self.recovery_baseline.write().await = Some(Arc::clone(snapshot));
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

    /// Atomically publish a complete snapshot as Ready **and** establish continuity tip.
    ///
    /// Call only after all reads / coverage / identity checks for this snapshot succeeded.
    /// Use this on the live subscribe path after a head is fully assembled.
    /// This is the **only** path that seeds `last_tip`.
    pub async fn publish(&self, snapshot: MarketSnapshot) {
        let arc = snapshot.into_arc();
        *self.last_tip.write().await = Some(arc.id);
        *self.recovery_baseline.write().await = Some(Arc::clone(&arc));
        *self.status.write().await = SnapshotStatus::Ready(arc);
    }

    /// Publish Ready for quoting **without** seeding a continuity tip.
    ///
    /// Cold-start discovery uses this: the discovery tip is typically already stale by
    /// the time the WS subscription delivers its first head. Seeding `last_tip` to that
    /// tip would classify the first head as Gap/Fork and Halt quoting before M1-7
    /// backfill exists. Leaving `last_tip` empty makes the next [`observe_head`] a
    /// Bootstrap assemble; only the subsequent [`publish`] establishes the tip.
    ///
    /// `begin_sync` / `fail_read` must not re-seed `last_tip` from this Ready either
    /// (see [`Self::demote_ready_to_baseline`]), or a failed first-head assemble would
    /// reintroduce the stale discovery tip.
    pub async fn publish_ready_awaiting_head(&self, snapshot: MarketSnapshot) {
        let arc = snapshot.into_arc();
        *self.recovery_baseline.write().await = Some(Arc::clone(&arc));
        *self.last_tip.write().await = None;
        *self.status.write().await = SnapshotStatus::Ready(arc);
    }

    /// Apply a mid-assembly failure: leave Ready/Syncing → Halted, baseline intact.
    pub async fn fail_read(&self, message: impl Into<String>) {
        self.halt(HaltReason::ReadFailure(message.into())).await;
    }

    /// Classify `observed` against the last tip and update readiness accordingly.
    ///
    /// - [`HeadObservation::Duplicate`]: status unchanged.
    /// - [`HeadObservation::Assemble`]: status is Syncing; caller must [`publish`] or
    ///   [`fail_read`].
    /// - [`HeadObservation::Halted`]: status is Halted with the returned reason (fork or
    ///   gap). Callers should surface that reason directly — no status re-read needed.
    pub async fn observe_head(&self, observed: &ObservedHead) -> HeadObservation {
        let last = *self.last_tip.read().await;
        let decision = classify_head(last.as_ref(), observed);

        match decision {
            HeadDecision::Duplicate => HeadObservation::Duplicate,
            HeadDecision::Bootstrap => {
                self.begin_sync().await;
                HeadObservation::Assemble(AssembleKind::Bootstrap)
            }
            HeadDecision::Advance => {
                self.begin_sync().await;
                HeadObservation::Assemble(AssembleKind::Advance)
            }
            HeadDecision::Fork(kind) => {
                let previous = last.unwrap_or_else(|| {
                    SnapshotId::new(observed.chain_id, 0, alloy::primitives::B256::ZERO)
                });
                let reason = HaltReason::Fork {
                    previous,
                    observed_number: observed.number,
                    observed_hash: observed.hash,
                    observed_parent: observed.parent_hash,
                    kind,
                };
                self.halt(reason.clone()).await;
                HeadObservation::Halted(reason)
            }
            HeadDecision::Gap {
                last_number,
                observed_number,
            } => {
                let reason = HaltReason::Gap {
                    last_number,
                    observed_number,
                };
                self.halt(reason.clone()).await;
                HeadObservation::Halted(reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy::primitives::B256;

    use super::*;
    use crate::state_space::snapshot::status::ForkKind;
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
        assert_eq!(pub_.last_tip().await.unwrap().block_number, 10);
    }

    #[tokio::test]
    async fn discovery_ready_does_not_seed_tip_so_first_head_bootstraps() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish_ready_awaiting_head(snapshot(10, 1, 0)).await;

        // Quotable immediately from the discovery snapshot…
        assert!(pub_.allows_execution().await);
        assert!(pub_.last_tip().await.is_none());
        assert_eq!(
            pub_.ready_snapshot().await.unwrap().block_hash(),
            h(1)
        );

        // …but the first live head is Bootstrap even if far ahead of discovery tip.
        // With a seeded tip this would be Gap → Halt (M1-7 not implemented).
        let observation = pub_.observe_head(&head(15, 5, 1)).await;
        assert_eq!(
            observation,
            HeadObservation::Assemble(AssembleKind::Bootstrap)
        );
        assert!(!pub_.allows_execution().await);
        assert!(matches!(pub_.status().await, SnapshotStatus::Syncing));
        // demote must not re-seed the stale discovery tip while assembling.
        assert!(pub_.last_tip().await.is_none());

        // After a successful live assemble, tip is established and gap detection works.
        pub_.publish(snapshot(15, 5, 1)).await;
        assert_eq!(pub_.last_tip().await.unwrap().block_number, 15);
        let gap = pub_.observe_head(&head(20, 9, 5)).await;
        assert!(matches!(
            gap,
            HeadObservation::Halted(HaltReason::Gap { .. })
        ));
    }

    #[tokio::test]
    async fn failed_first_head_does_not_seed_stale_discovery_tip() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish_ready_awaiting_head(snapshot(10, 1, 0)).await;

        // First head starts Bootstrap assemble…
        assert_eq!(
            pub_.observe_head(&head(15, 5, 1)).await,
            HeadObservation::Assemble(AssembleKind::Bootstrap)
        );
        // …then mid-assembly failure (get_logs / sync error).
        pub_.fail_read("get_logs failed").await;
        assert!(!pub_.allows_execution().await);
        assert!(matches!(
            pub_.status().await,
            SnapshotStatus::Halted(HaltReason::ReadFailure(_))
        ));
        // Recovery baseline kept for resync; continuity tip still unset.
        assert_eq!(
            pub_.recovery_baseline().await.unwrap().id.block_number,
            10
        );
        assert!(pub_.last_tip().await.is_none());

        // Next head still Bootstraps (does not Gap against stale tip 10).
        assert_eq!(
            pub_.observe_head(&head(16, 6, 5)).await,
            HeadObservation::Assemble(AssembleKind::Bootstrap)
        );
        assert!(pub_.last_tip().await.is_none());
    }

    #[tokio::test]
    async fn mid_read_failure_keeps_baseline_and_leaves_ready() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;
        let baseline_hash = pub_.ready_snapshot().await.unwrap().block_hash();

        // New head starts assembly.
        let observation = pub_.observe_head(&head(11, 2, 1)).await;
        assert_eq!(
            observation,
            HeadObservation::Assemble(AssembleKind::Advance)
        );
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

        let observation = pub_.observe_head(&head(10, 9, 0)).await;
        match observation {
            HeadObservation::Halted(HaltReason::Fork { kind, .. }) => {
                assert_eq!(kind, ForkKind::SameHeightReplacement);
            }
            other => panic!("expected fork halt observation, got {other:?}"),
        }
        assert!(!pub_.allows_execution().await);
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

        let observation = pub_.observe_head(&head(11, 2, 0xEE)).await;
        assert!(matches!(
            observation,
            HeadObservation::Halted(HaltReason::Fork {
                kind: ForkKind::WrongParent,
                ..
            })
        ));
        assert!(!pub_.allows_execution().await);
    }

    #[tokio::test]
    async fn exact_duplicate_is_idempotent() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let observation = pub_.observe_head(&head(10, 1, 0)).await;
        assert_eq!(observation, HeadObservation::Duplicate);
        assert!(pub_.allows_execution().await);
        assert_eq!(pub_.ready_snapshot().await.unwrap().block_hash(), h(1));
    }

    #[tokio::test]
    async fn gap_halts_quoting() {
        let pub_ = SnapshotPublisher::new();
        pub_.publish(snapshot(10, 1, 0)).await;

        let observation = pub_.observe_head(&head(15, 5, 1)).await;
        assert_eq!(
            observation,
            HeadObservation::Halted(HaltReason::Gap {
                last_number: 10,
                observed_number: 15
            })
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
