//! Hash-pinned state and log reads for a single MarketSnapshot (WHI-510).
//!
//! Preferred path: EIP-1898 block-hash pin with `requireCanonical` for state calls,
//! and a block-hash log filter (not a number-only range).
//!
//! Fallback path (RPC cannot hash-pin state): fix every call to the target **block
//! number**, compare the canonical hash at that height immediately before and after
//! all reads, and reject if either differs from the expected snapshot hash. The
//! middle of a fallback session must never use `latest`.

use alloy::eips::BlockId;
use alloy::primitives::B256;
use alloy::rpc::types::Filter;
use thiserror::Error;

use super::types::SnapshotId;

/// Errors from hash-pin / fallback identity enforcement.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PinError {
    #[error(
        "canonical hash before reads {before:?} does not match expected snapshot hash {expected:?}"
    )]
    HashBeforeMismatch { before: B256, expected: B256 },
    #[error(
        "canonical hash after reads {after:?} does not match expected snapshot hash {expected:?}"
    )]
    HashAfterMismatch { after: B256, expected: B256 },
    #[error(
        "canonical hash changed during fallback reads: before {before:?}, after {after:?}"
    )]
    HashChangedDuringReads { before: B256, after: B256 },
    #[error("fallback state call requested BlockId::Latest, which is forbidden")]
    LatestForbidden,
    #[error("read bound to block hash {got:?} but snapshot expects {expected:?}")]
    MixedBlockHash { got: B256, expected: B256 },
    #[error("read bound to block number {got} but snapshot expects {expected}")]
    MixedBlockNumber { got: u64, expected: u64 },
}

/// EIP-1898 state `BlockId` pinned to `hash` with canonicality required.
pub fn hash_pinned_state_block_id(block_hash: B256) -> BlockId {
    BlockId::hash_canonical(block_hash)
}

/// State `BlockId` for [`SnapshotId`] via hash pin (preferred path).
pub fn snapshot_state_block_id(id: &SnapshotId) -> BlockId {
    hash_pinned_state_block_id(id.block_hash)
}

/// Attach a block-hash filter for logs belonging to exactly one block.
///
/// Spec: query logs with a block-hash filter, not a number-only range.
pub fn hash_pinned_logs_filter(base: Filter, block_hash: B256) -> Filter {
    base.at_block_hash(block_hash)
}

/// Number-pinned fallback session for RPCs that cannot hash-pin state calls.
///
/// Lifecycle:
/// 1. [`NumberPinnedSession::begin`] with the pre-read canonical hash at `block_number`
/// 2. Issue every state call with [`NumberPinnedSession::state_block_id`] (never latest)
/// 3. [`NumberPinnedSession::finish`] with the post-read canonical hash
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberPinnedSession {
    block_number: u64,
    expected_hash: B256,
    hash_before: B256,
}

impl NumberPinnedSession {
    /// Start a fallback session. Fails if the pre-read hash ≠ expected snapshot hash.
    pub fn begin(
        block_number: u64,
        expected_hash: B256,
        hash_before: B256,
    ) -> Result<Self, PinError> {
        if hash_before != expected_hash {
            return Err(PinError::HashBeforeMismatch {
                before: hash_before,
                expected: expected_hash,
            });
        }
        Ok(Self {
            block_number,
            expected_hash,
            hash_before,
        })
    }

    pub fn from_snapshot_id(
        id: &SnapshotId,
        hash_before: B256,
    ) -> Result<Self, PinError> {
        Self::begin(id.block_number, id.block_hash, hash_before)
    }

    /// Block id for every state call in this session — fixed number, never `latest`.
    pub fn state_block_id(&self) -> BlockId {
        BlockId::number(self.block_number)
    }

    pub fn block_number(&self) -> u64 {
        self.block_number
    }

    pub fn expected_hash(&self) -> B256 {
        self.expected_hash
    }

    /// Reject a caller-supplied BlockId that would violate the pin contract.
    pub fn assert_allowed_block_id(&self, block_id: BlockId) -> Result<(), PinError> {
        match block_id {
            BlockId::Number(n) => {
                if n.is_latest() {
                    return Err(PinError::LatestForbidden);
                }
                if let Some(num) = n.as_number() {
                    if num != self.block_number {
                        return Err(PinError::MixedBlockNumber {
                            got: num,
                            expected: self.block_number,
                        });
                    }
                }
                Ok(())
            }
            BlockId::Hash(rpc_hash) => {
                if rpc_hash.block_hash != self.expected_hash {
                    return Err(PinError::MixedBlockHash {
                        got: rpc_hash.block_hash,
                        expected: self.expected_hash,
                    });
                }
                Ok(())
            }
        }
    }

    /// Complete the session. Fails if the post-read hash drifted or ≠ expected.
    pub fn finish(self, hash_after: B256) -> Result<(), PinError> {
        if hash_after != self.expected_hash {
            return Err(PinError::HashAfterMismatch {
                after: hash_after,
                expected: self.expected_hash,
            });
        }
        if hash_after != self.hash_before {
            return Err(PinError::HashChangedDuringReads {
                before: self.hash_before,
                after: hash_after,
            });
        }
        Ok(())
    }
}

/// Guard that records every block hash used while assembling one snapshot.
///
/// Used by tests and assemblers to prove a snapshot cannot mix two hashes.
#[derive(Debug, Default, Clone)]
pub struct AssemblyHashGuard {
    expected: Option<B256>,
}

impl AssemblyHashGuard {
    pub fn new(expected: B256) -> Self {
        Self {
            expected: Some(expected),
        }
    }

    pub fn record(&mut self, block_hash: B256) -> Result<(), PinError> {
        match self.expected {
            None => {
                self.expected = Some(block_hash);
                Ok(())
            }
            Some(expected) if expected == block_hash => Ok(()),
            Some(expected) => Err(PinError::MixedBlockHash {
                got: block_hash,
                expected,
            }),
        }
    }

    pub fn expected(&self) -> Option<B256> {
        self.expected
    }
}

#[cfg(test)]
mod tests {
    use alloy::eips::{BlockId, BlockNumberOrTag};
    use alloy::primitives::B256;
    use alloy::rpc::types::Filter;

    use super::*;
    use crate::state_space::snapshot::types::SnapshotId;

    fn h(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn preferred_state_block_id_is_hash_canonical() {
        let id = SnapshotId::new(5000, 42, h(7));
        let block_id = snapshot_state_block_id(&id);
        match block_id {
            BlockId::Hash(rpc) => {
                assert_eq!(rpc.block_hash, h(7));
                assert_eq!(rpc.require_canonical, Some(true));
            }
            other => panic!("expected hash pin, got {other:?}"),
        }
    }

    #[test]
    fn logs_filter_uses_block_hash_not_number_range() {
        let filter = hash_pinned_logs_filter(Filter::new(), h(3));
        assert_eq!(filter.get_block_hash(), Some(h(3)));
        assert!(filter.get_from_block().is_none());
        assert!(filter.get_to_block().is_none());
    }

    #[test]
    fn fallback_rejects_hash_change_before_reads() {
        let err = NumberPinnedSession::begin(10, h(1), h(2)).unwrap_err();
        assert_eq!(
            err,
            PinError::HashBeforeMismatch {
                before: h(2),
                expected: h(1)
            }
        );
    }

    #[test]
    fn fallback_rejects_hash_change_after_reads() {
        let session = NumberPinnedSession::begin(10, h(1), h(1)).unwrap();
        let err = session.finish(h(2)).unwrap_err();
        assert_eq!(
            err,
            PinError::HashAfterMismatch {
                after: h(2),
                expected: h(1)
            }
        );
    }

    #[test]
    fn fallback_middle_calls_use_number_never_latest() {
        let session = NumberPinnedSession::begin(10, h(1), h(1)).unwrap();
        assert_eq!(session.state_block_id(), BlockId::number(10));
        assert!(session
            .assert_allowed_block_id(BlockId::Number(BlockNumberOrTag::Latest))
            .is_err());
        assert!(session
            .assert_allowed_block_id(BlockId::number(10))
            .is_ok());
        session.finish(h(1)).unwrap();
    }

    #[test]
    fn assembly_guard_rejects_mixed_hashes() {
        let mut guard = AssemblyHashGuard::new(h(1));
        guard.record(h(1)).unwrap();
        let err = guard.record(h(2)).unwrap_err();
        assert_eq!(
            err,
            PinError::MixedBlockHash {
                got: h(2),
                expected: h(1)
            }
        );
    }

    #[test]
    fn fallback_happy_path_keeps_single_hash() {
        let id = SnapshotId::new(5000, 99, h(9));
        let session = NumberPinnedSession::from_snapshot_id(&id, h(9)).unwrap();
        let mut guard = AssemblyHashGuard::new(id.block_hash);
        // Simulate two state reads pinned to the session block number.
        session
            .assert_allowed_block_id(session.state_block_id())
            .unwrap();
        guard.record(id.block_hash).unwrap();
        session
            .assert_allowed_block_id(session.state_block_id())
            .unwrap();
        guard.record(id.block_hash).unwrap();
        session.finish(h(9)).unwrap();
        assert_eq!(guard.expected(), Some(h(9)));
    }
}
