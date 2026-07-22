mod alert;
mod config;
mod coordinator;
mod durable_hook;
mod ledger;
mod operator;
mod pause_ctrl;
mod store;
mod wal;

pub use alert::{AlertEvent, AlertSink, TracingAlertSink};
pub use config::BreakerConfig;
pub use coordinator::{
    BreakerRuntime, CanonicalChainView, CoordinatorError, DurableIntentCoordinator, InitAnchor,
    ScopeId,
};
pub use durable_hook::WalDurableHook;
pub use ledger::{
    AccountingRecord, BreakerStats, LedgerState, ReversalRecord, StreakEntry, TerminalKind,
};
pub use operator::{
    ControlCommand, ControlKind, OperatorCommand, OperatorVerifier, SignedOperatorCommand,
    StaticOperatorVerifier, sign_command,
};
pub use pause_ctrl::PauseController;
pub use store::{SecureStore, StoreError};
pub use wal::{
    decode_frame, encode_frame, frame_digest, WalPayload, WalRecord, WalTag, WAL_DOMAIN,
    WAL_VERSION,
};

/// Sink invoked before SM terminalization so the WAL owns accounting (WHI-524).
pub trait AccountingCommit: Send + Sync {
    fn persist_terminal(&self, record: AccountingRecord) -> Result<(), CoordinatorError>;
    fn persist_reversal(&self, record: ReversalRecord) -> Result<(), CoordinatorError>;
    fn stats(&self) -> BreakerStats;
}

impl AccountingCommit for DurableIntentCoordinator {
    fn persist_terminal(&self, record: AccountingRecord) -> Result<(), CoordinatorError> {
        self.commit_terminal(record).map(|_| ())
    }

    fn persist_reversal(&self, record: ReversalRecord) -> Result<(), CoordinatorError> {
        self.commit_reversal(record).map(|_| ())
    }

    fn stats(&self) -> BreakerStats {
        self.breaker_stats()
    }
}
