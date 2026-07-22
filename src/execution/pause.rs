//! Send-time pause seam (WHI-520 stub + WHI-524 real policy).

use std::sync::Arc;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptKind {
    Execute,
    Cancel,
    OnChainPause,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
#[error("attempt is paused")]
pub struct Paused;

/// RAII token proving a pause decision remained admitted for preparation.
///
/// Drop releases any tracked in-flight counter (cancellation-safe).
pub struct SendGuard {
    on_drop: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl std::fmt::Debug for SendGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendGuard")
            .field("tracked", &self.on_drop.is_some())
            .finish()
    }
}

impl SendGuard {
    pub(crate) fn untracked() -> Self {
        Self { on_drop: None }
    }

    pub(crate) fn tracked(on_drop: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            on_drop: Some(on_drop),
        }
    }
}

impl Drop for SendGuard {
    fn drop(&mut self) {
        if let Some(cb) = self.on_drop.take() {
            cb();
        }
    }
}

pub trait PauseGate: Send + Sync {
    fn begin_send(&self, kind: AttemptKind) -> Result<SendGuard, Paused>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysAllow;

impl PauseGate for AlwaysAllow {
    fn begin_send(&self, _kind: AttemptKind) -> Result<SendGuard, Paused> {
        Ok(SendGuard::untracked())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_allow_returns_noop_guards_for_all_attempt_kinds() {
        assert!(AlwaysAllow.begin_send(AttemptKind::Execute).is_ok());
        assert!(AlwaysAllow.begin_send(AttemptKind::Cancel).is_ok());
        assert!(AlwaysAllow.begin_send(AttemptKind::OnChainPause).is_ok());
    }

    #[test]
    fn tracked_guard_runs_callback_on_drop() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let n = Arc::new(AtomicUsize::new(0));
        let n2 = Arc::clone(&n);
        let g = SendGuard::tracked(Arc::new(move || {
            n2.fetch_add(1, Ordering::SeqCst);
        }));
        drop(g);
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }
}
