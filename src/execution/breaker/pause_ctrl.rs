//! Kind-aware pause controller with bounded send sections (WHI-524).

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::alert::{AlertEvent, AlertSink};
use crate::execution::pause::{AttemptKind, PauseGate, Paused, SendGuard};

/// Real pause gate. Paused denies Execute; Cancel and OnChainPause remain allowed.
#[derive(Clone)]
pub struct PauseController {
    inner: Arc<Inner>,
}

struct Inner {
    paused: AtomicBool,
    initialized: AtomicBool,
    in_flight: AtomicUsize,
    drain: Mutex<()>,
    drain_cv: Condvar,
    alerts: Arc<dyn AlertSink>,
    broadcast_timeout: Duration,
}

impl PauseController {
    pub fn new(alerts: Arc<dyn AlertSink>, broadcast_timeout_ms: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                // Missing durable state always starts control-only paused.
                paused: AtomicBool::new(true),
                initialized: AtomicBool::new(false),
                in_flight: AtomicUsize::new(0),
                drain: Mutex::new(()),
                drain_cv: Condvar::new(),
                alerts,
                broadcast_timeout: Duration::from_millis(broadcast_timeout_ms.max(1)),
            }),
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner.paused.load(Ordering::Acquire)
    }

    pub fn is_initialized(&self) -> bool {
        self.inner.initialized.load(Ordering::Acquire)
    }

    pub fn mark_initialized(&self) {
        self.inner.initialized.store(true, Ordering::Release);
    }

    pub fn in_flight(&self) -> usize {
        self.inner.in_flight.load(Ordering::Acquire)
    }

    pub fn pause(&self, reason: impl Into<String>) {
        let reason = reason.into();
        self.inner.paused.store(true, Ordering::Release);
        self.inner.alerts.alert(AlertEvent::Pause {
            reason: reason.clone(),
        });
        self.drain_in_flight();
    }

    /// Operator unpause. Fails closed if not initialized.
    pub fn unpause(&self, actor: impl Into<String>) -> Result<(), Paused> {
        if !self.is_initialized() {
            return Err(Paused);
        }
        self.inner.paused.store(false, Ordering::Release);
        self.inner.alerts.alert(AlertEvent::Unpause {
            actor: actor.into(),
        });
        Ok(())
    }

    fn drain_in_flight(&self) {
        let guard = self.inner.drain.lock().expect("drain lock");
        let timeout = self.inner.broadcast_timeout;
        let inner = Arc::clone(&self.inner);
        let (_guard, result) = self
            .inner
            .drain_cv
            .wait_timeout_while(guard, timeout, |_| inner.in_flight.load(Ordering::Acquire) > 0)
            .expect("drain wait");
        if result.timed_out() && self.inner.in_flight.load(Ordering::Acquire) > 0 {
            self.inner.alerts.alert(AlertEvent::DrainTimeout {
                detail: format!(
                    "{} in-flight send(s) exceeded {:?}",
                    self.inner.in_flight.load(Ordering::Acquire),
                    timeout
                ),
            });
        }
    }

    fn admit(&self, kind: AttemptKind) -> Result<(), Paused> {
        match kind {
            AttemptKind::Execute if self.is_paused() => Err(Paused),
            _ => Ok(()),
        }
    }
}

impl PauseGate for PauseController {
    fn begin_send(&self, kind: AttemptKind) -> Result<SendGuard, Paused> {
        self.admit(kind)?;
        self.inner.in_flight.fetch_add(1, Ordering::AcqRel);
        if let Err(paused) = self.admit(kind) {
            self.inner.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.inner.drain_cv.notify_all();
            return Err(paused);
        }
        let inner = Arc::clone(&self.inner);
        Ok(SendGuard::tracked(Arc::new(move || {
            inner.in_flight.fetch_sub(1, Ordering::AcqRel);
            inner.drain_cv.notify_all();
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::breaker::alert::RecordingAlertSink;
    use crate::execution::pause::PauseGate;

    #[test]
    fn paused_rejects_execute_allows_cancel_and_onchain_pause() {
        let alerts = Arc::new(RecordingAlertSink::default());
        let gate = PauseController::new(alerts, 50);
        assert!(gate.begin_send(AttemptKind::Execute).is_err());
        assert!(gate.begin_send(AttemptKind::Cancel).is_ok());
        assert!(gate.begin_send(AttemptKind::OnChainPause).is_ok());
    }

    #[test]
    fn unpause_fails_before_init() {
        let alerts = Arc::new(RecordingAlertSink::default());
        let gate = PauseController::new(alerts, 50);
        assert!(gate.unpause("actor").is_err());
        gate.mark_initialized();
        assert!(gate.unpause("actor").is_ok());
        assert!(gate.begin_send(AttemptKind::Execute).is_ok());
    }

    #[test]
    fn drop_releases_inflight_guard() {
        let alerts = Arc::new(RecordingAlertSink::default());
        let gate = PauseController::new(alerts, 50);
        gate.mark_initialized();
        gate.unpause("actor").unwrap();
        let g = gate.begin_send(AttemptKind::Execute).unwrap();
        assert_eq!(gate.in_flight(), 1);
        drop(g);
        assert_eq!(gate.in_flight(), 0);
    }
}
