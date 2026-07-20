//! Explicit readiness token for consumers of market state (WHI-510).

use std::sync::Arc;

use alloy::primitives::B256;

use super::types::{MarketSnapshot, SnapshotId};

/// Why the snapshot publisher is not [`SnapshotStatus::Ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HaltReason {
    /// Branch change: same-height replacement, height rollback, or wrong parent.
    Fork {
        previous: SnapshotId,
        observed_number: u64,
        observed_hash: B256,
        observed_parent: B256,
        kind: ForkKind,
    },
    /// Numeric gap (`observed > last + 1`). Recovery is M1-7 backfill, not quoting.
    Gap {
        last_number: u64,
        observed_number: u64,
    },
    /// A read, coverage sync, or identity validation failed mid-assembly.
    ReadFailure(String),
    /// Header/state identity mixed across hashes or chain ids.
    IdentityMismatch(String),
    /// Explicit operator / internal request to resync before quoting again.
    ResyncRequired,
}

/// Classification of a discontinuous head relative to the last known good tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkKind {
    /// Same block number, different hash (replacement; no number gap).
    SameHeightReplacement,
    /// Observed height strictly below the last known tip.
    HeightRollback,
    /// `number == last + 1` but `parent_hash != last.hash`.
    WrongParent,
    /// Head belongs to a different chain id than the publisher was configured for.
    ChainIdMismatch,
}

/// Public readiness surface for quote / candidate / send gates.
///
/// - [`Ready`](Self::Ready): only state that may be used to create, size, sign, or send.
/// - [`Syncing`](Self::Syncing): a new head is being assembled; no quoting.
/// - [`Halted`](Self::Halted): fork, gap, or failure; no quoting until a full resync publishes.
#[derive(Debug, Clone)]
pub enum SnapshotStatus {
    Ready(Arc<MarketSnapshot>),
    Syncing,
    Halted(HaltReason),
}

impl SnapshotStatus {
    /// Executable quote source, if and only if status is Ready.
    pub fn ready_snapshot(&self) -> Option<&Arc<MarketSnapshot>> {
        match self {
            Self::Ready(snapshot) => Some(snapshot),
            Self::Syncing | Self::Halted(_) => None,
        }
    }

    pub fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }

    /// Candidate creation / sign / send stays disabled unless Ready.
    pub fn allows_execution(&self) -> bool {
        self.is_ready()
    }
}
