//! Chain continuity classification for new-head notifications (WHI-510).

use super::status::{ForkKind, HaltReason};
use super::types::{ObservedHead, SnapshotId};

/// Pure classification of an observed head relative to the last accepted tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadDecision {
    /// No previous tip: accept as the bootstrap snapshot identity.
    Bootstrap,
    /// Normal progress: `number == last + 1` and `parent_hash == last.hash`.
    Advance,
    /// Exact duplicate `(number, hash)` — ignore (idempotent).
    Duplicate,
    /// Branch change requiring halt + resync (not gap backfill).
    Fork(ForkKind),
    /// Numeric gap; route to M1-7 backfill. Quoting must stop.
    Gap { last_number: u64, observed_number: u64 },
}

/// Publisher-facing result of applying a head: either assemble, ignore, or halt.
///
/// Carries the authoritative [`HaltReason`] so callers do not re-derive it from status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadObservation {
    /// Exact duplicate — status unchanged.
    Duplicate,
    /// Status is now [`super::status::SnapshotStatus::Syncing`]; assemble then publish/fail.
    Assemble(AssembleKind),
    /// Status is already [`super::status::SnapshotStatus::Halted`] with this reason.
    Halted(HaltReason),
}

/// Why assembly was requested after a head observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssembleKind {
    Bootstrap,
    Advance,
}

/// Classify `observed` against the last accepted snapshot identity.
///
/// Rules (spec 02 Sync module / WHI-510):
/// - exact same `(number, hash)` → [`HeadDecision::Duplicate`]
/// - same height, different hash → [`ForkKind::SameHeightReplacement`]
/// - lower height → [`ForkKind::HeightRollback`]
/// - `number == last + 1` and parent matches → [`HeadDecision::Advance`]
/// - `number == last + 1` and parent mismatches → [`ForkKind::WrongParent`]
/// - `number > last + 1` → [`HeadDecision::Gap`]
/// - chain id mismatch → [`ForkKind::ChainIdMismatch`]
pub fn classify_head(last: Option<&SnapshotId>, observed: &ObservedHead) -> HeadDecision {
    let Some(last) = last else {
        return HeadDecision::Bootstrap;
    };

    if observed.chain_id != last.chain_id {
        return HeadDecision::Fork(ForkKind::ChainIdMismatch);
    }

    if observed.number == last.block_number {
        if observed.hash == last.block_hash {
            return HeadDecision::Duplicate;
        }
        return HeadDecision::Fork(ForkKind::SameHeightReplacement);
    }

    if observed.number < last.block_number {
        return HeadDecision::Fork(ForkKind::HeightRollback);
    }

    // observed.number > last.block_number
    if observed.number == last.block_number + 1 {
        if observed.parent_hash == last.block_hash {
            return HeadDecision::Advance;
        }
        return HeadDecision::Fork(ForkKind::WrongParent);
    }

    HeadDecision::Gap {
        last_number: last.block_number,
        observed_number: observed.number,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;

    use super::*;

    fn hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn tip(number: u64, h: u8) -> SnapshotId {
        SnapshotId::new(5000, number, hash(h))
    }

    fn head(number: u64, h: u8, parent: u8) -> ObservedHead {
        ObservedHead::new(5000, number, hash(h), hash(parent), 1_700_000_000)
    }

    #[test]
    fn bootstrap_when_no_last() {
        assert_eq!(
            classify_head(None, &head(10, 1, 0)),
            HeadDecision::Bootstrap
        );
    }

    #[test]
    fn exact_duplicate_is_idempotent() {
        let last = tip(10, 1);
        let observed = ObservedHead::new(5000, 10, hash(1), hash(9), 99);
        assert_eq!(
            classify_head(Some(&last), &observed),
            HeadDecision::Duplicate
        );
    }

    #[test]
    fn same_height_different_hash_is_replacement_fork() {
        let last = tip(10, 1);
        assert_eq!(
            classify_head(Some(&last), &head(10, 2, 9)),
            HeadDecision::Fork(ForkKind::SameHeightReplacement)
        );
    }

    #[test]
    fn height_rollback_is_fork() {
        let last = tip(10, 1);
        assert_eq!(
            classify_head(Some(&last), &head(9, 9, 8)),
            HeadDecision::Fork(ForkKind::HeightRollback)
        );
    }

    #[test]
    fn advance_requires_parent_link() {
        let last = tip(10, 1);
        assert_eq!(
            classify_head(Some(&last), &head(11, 2, 1)),
            HeadDecision::Advance
        );
    }

    #[test]
    fn wrong_parent_child_is_fork() {
        let last = tip(10, 1);
        assert_eq!(
            classify_head(Some(&last), &head(11, 2, 0xAB)),
            HeadDecision::Fork(ForkKind::WrongParent)
        );
    }

    #[test]
    fn numeric_gap_is_not_advance() {
        let last = tip(10, 1);
        assert_eq!(
            classify_head(Some(&last), &head(13, 4, 1)),
            HeadDecision::Gap {
                last_number: 10,
                observed_number: 13
            }
        );
    }

    #[test]
    fn chain_id_mismatch_is_fork() {
        let last = tip(10, 1);
        let foreign = ObservedHead::new(1, 11, hash(2), hash(1), 0);
        assert_eq!(
            classify_head(Some(&last), &foreign),
            HeadDecision::Fork(ForkKind::ChainIdMismatch)
        );
    }
}
