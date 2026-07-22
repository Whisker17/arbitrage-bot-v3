//! Send-time pause seam. WHI-524 will provide the real policy.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptKind {
    Execute,
    Cancel,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("attempt is paused")]
pub struct Paused;

/// RAII token proving a pause decision remained admitted for preparation.
#[derive(Debug)]
pub struct SendGuard {
    _private: (),
}

pub trait PauseGate: Send + Sync {
    fn begin_send(&self, kind: AttemptKind) -> Result<SendGuard, Paused>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysAllow;

impl PauseGate for AlwaysAllow {
    fn begin_send(&self, _kind: AttemptKind) -> Result<SendGuard, Paused> {
        Ok(SendGuard { _private: () })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_allow_returns_noop_guards_for_both_attempt_kinds() {
        assert!(AlwaysAllow.begin_send(AttemptKind::Execute).is_ok());
        assert!(AlwaysAllow.begin_send(AttemptKind::Cancel).is_ok());
    }
}
