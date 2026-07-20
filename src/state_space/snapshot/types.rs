//! Core identity types for the MarketSnapshot consistency protocol (WHI-510).

use std::collections::HashMap;
use std::sync::Arc;

use alloy::primitives::{Address, B256};

use crate::amms::amm::AMM;

/// Full chain identity of a canonical block used as a quote source.
///
/// Every pool read, log query, balance fetch, and candidate derived from a snapshot
/// must share this identity. Equality is by `(chain_id, block_number, block_hash)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SnapshotId {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
}

impl SnapshotId {
    pub const fn new(chain_id: u64, block_number: u64, block_hash: B256) -> Self {
        Self {
            chain_id,
            block_number,
            block_hash,
        }
    }
}

/// Immutable header context bound to the same block as [`SnapshotId`].
///
/// `parent_hash` is required for chain continuity checks; `block_timestamp` is the
/// only time source Moe (and any time-dependent) quotes may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockHeaderContext {
    pub parent_hash: B256,
    pub block_timestamp: u64,
}

impl BlockHeaderContext {
    pub const fn new(parent_hash: B256, block_timestamp: u64) -> Self {
        Self {
            parent_hash,
            block_timestamp,
        }
    }
}

/// Protocol-level coverage metadata attached to a snapshot.
///
/// Concrete V3 word/tick ranges and Moe queried-range sets are filled by later
/// M1 issues (WHI-512 / WHI-513). Presence of this field on every snapshot keeps
/// the identity contract stable for consumers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProtocolCoverage {
    /// Opaque fingerprint of coverage completeness for this snapshot generation.
    /// Empty means "not yet asserted"; incomplete coverage must fail closed at quote time.
    pub fingerprint: Option<B256>,
}

/// A single canonical market state: one block hash, one header, one pool map.
///
/// Construction must only happen after all reads for `id.block_hash` succeed.
/// Partial assemblies must never be published as [`super::status::SnapshotStatus::Ready`].
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    pub id: SnapshotId,
    pub header: BlockHeaderContext,
    pub pools: HashMap<Address, AMM>,
    pub coverage: ProtocolCoverage,
}

impl MarketSnapshot {
    pub fn new(
        id: SnapshotId,
        header: BlockHeaderContext,
        pools: HashMap<Address, AMM>,
        coverage: ProtocolCoverage,
    ) -> Self {
        Self {
            id,
            header,
            pools,
            coverage,
        }
    }

    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Identity every candidate / quote derived from this snapshot must carry.
    pub const fn snapshot_id(&self) -> SnapshotId {
        self.id
    }

    pub const fn block_hash(&self) -> B256 {
        self.id.block_hash
    }

    pub const fn block_timestamp(&self) -> u64 {
        self.header.block_timestamp
    }
}

/// Observed new-head notification (WS block header or equivalent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedHead {
    pub chain_id: u64,
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
}

impl ObservedHead {
    pub const fn new(
        chain_id: u64,
        number: u64,
        hash: B256,
        parent_hash: B256,
        timestamp: u64,
    ) -> Self {
        Self {
            chain_id,
            number,
            hash,
            parent_hash,
            timestamp,
        }
    }

    pub const fn to_snapshot_id(&self) -> SnapshotId {
        SnapshotId::new(self.chain_id, self.number, self.hash)
    }

    pub const fn to_header_context(&self) -> BlockHeaderContext {
        BlockHeaderContext::new(self.parent_hash, self.timestamp)
    }
}
