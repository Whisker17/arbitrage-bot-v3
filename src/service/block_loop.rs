//! Shared block-subscription job-slot / worker pattern (WHI-727).
//!
//! Standardizes the latest-wins slot used by v3/1559 and moe. Wiring v2 onto
//! this loop (deliberate behavior change: v2 currently runs execution inline)
//! is WHI-527.3's job — this module only provides the reusable primitives.

use crate::execution::LatestWinsSlot;
use crate::state_space::{SnapshotId, SnapshotStatus};
use alloy::primitives::B256;
use eyre::{eyre, Result};
use std::sync::Arc;
use std::time::Duration;

/// Poll interval used by the v3/moe execution workers when the slot is empty.
pub const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Latest-wins job slot shared between the block loop and the execution worker.
pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

/// Create an empty latest-wins job slot.
pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

/// Protocol-agnostic execution job envelope (v3/moe shape).
///
/// `C` is the protocol's candidate type (typically [`super::protocol::Candidate`]).
#[derive(Clone, Debug)]
pub struct ExecutionJob<C> {
    pub candidate: C,
    pub block_number: u64,
    pub header: crate::state_space::BlockHeaderContext,
    pub pool_universe_fingerprint: B256,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

/// Reject queued work unless the live tip is still Ready at the candidate SnapshotId.
///
/// Relocated from `examples/protocols/intent_service_support.rs`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_space::{
        BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
    };
    use alloy::primitives::{b256, B256};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn ready_tip(id: SnapshotId) -> SnapshotStatus {
        let header = BlockHeaderContext::new(B256::ZERO, 1);
        SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
            id,
            header,
            HashMap::new(),
            ProtocolCoverage::default(),
        )))
    }

    #[test]
    fn matching_ready_tip_accepts() {
        let id = SnapshotId::new(
            5000,
            10,
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        );
        let tip = require_matching_ready_tip(Some(ready_tip(id)), id).unwrap();
        match tip {
            SnapshotStatus::Ready(s) => assert_eq!(s.id, id),
            other => panic!("unexpected tip: {other:?}"),
        }
    }

    #[test]
    fn stale_ready_tip_rejects() {
        let candidate = SnapshotId::new(
            5000,
            10,
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        );
        let live = SnapshotId::new(
            5000,
            11,
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
        );
        let err = require_matching_ready_tip(Some(ready_tip(live)), candidate).unwrap_err();
        assert!(err.to_string().contains("stale queued opportunity"));
    }

    #[test]
    fn absent_tip_rejects() {
        let id = SnapshotId::new(5000, 1, B256::ZERO);
        let err = require_matching_ready_tip(None, id).unwrap_err();
        assert!(err.to_string().contains("no live Ready snapshot tip"));
    }

    #[test]
    fn job_slot_latest_wins() {
        let slot = new_job_slot::<u32>();
        slot.publish(1);
        slot.publish(2);
        assert_eq!(slot.take(), Some(2));
        assert_eq!(slot.take(), None);
    }
}
