use alloy::transports::TransportErrorKind;
use thiserror::Error;

use crate::amms::error::AMMError;

use super::snapshot::{HaltReason, PinError, SnapshotId};

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotBalanceError {
    #[error("pool state snapshot {pool:?} does not match balance snapshot {balance:?}")]
    MismatchedSnapshot {
        pool: SnapshotId,
        balance: SnapshotId,
    },
}

#[derive(Error, Debug)]
pub enum StateSpaceError {
    #[error(transparent)]
    AMMError(#[from] AMMError),
    #[error(transparent)]
    TransportError(#[from] alloy::transports::RpcError<TransportErrorKind>),
    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),
    #[error("Block Number Does not Exist")]
    MissingBlockNumber,
    #[error("Block hash missing from log or header")]
    MissingBlockHash,
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error("Snapshot halted: {0}")]
    SnapshotHalted(HaltReason),
    #[error("Snapshot not ready for execution (status is Syncing or Halted)")]
    SnapshotNotReady,
    #[error("Canonical tip block not found at number {0}")]
    MissingTipBlock(u64),
    #[error("Canonical block not found at number {0}")]
    MissingBlock(u64),
    #[error("Block identity mismatch: {0}")]
    IdentityMismatch(String),
}
