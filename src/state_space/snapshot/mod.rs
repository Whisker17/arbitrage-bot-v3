//! MarketSnapshot consistency and readiness protocol (WHI-510 / M1-1).
//!
//! A `MarketSnapshot` is not a version tag: all reads for one snapshot are pinned
//! to one canonical block hash, carry full chain/header identity, publish
//! atomically, and expose explicit [`SnapshotStatus`] so consumers cannot keep
//! quoting an old snapshot during replacement, rollback, gap recovery, or a
//! failed new-head sync.

mod continuity;
mod pin;
mod publisher;
mod status;
mod types;

pub use continuity::{classify_head, AssembleKind, HeadDecision, HeadObservation};
pub use pin::{
    hash_pinned_logs_filter, hash_pinned_state_block_id, snapshot_state_block_id,
    AssemblyHashGuard, NumberPinnedSession, PinError,
};
pub use publisher::SnapshotPublisher;
pub use status::{ForkKind, HaltReason, SnapshotStatus};
pub use types::{
    BlockHeaderContext, MarketSnapshot, ObservedHead, ProtocolCoverage, SnapshotId,
};
