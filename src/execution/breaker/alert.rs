//! Minimal alert sink for breaker/pause/operator events (WHI-524).
//! Prometheus extension is WHI-532.

use tracing::error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlertEvent {
    BreakerTrip { reason: String },
    Pause { reason: String },
    Unpause { actor: String },
    Init { actor: String },
    Recovery { actor: String },
    Tamper { detail: String },
    InventoryViolation { detail: String },
    LedgerReversal { detail: String },
    RestartValidationFailure { detail: String },
    IncompleteFeeAccounting { detail: String },
    NeedsOperator { detail: String },
    Halted { detail: String },
    AnomalousCancel { detail: String },
    UnattributableActivity { detail: String },
    DrainTimeout { detail: String },
}

pub trait AlertSink: Send + Sync {
    fn alert(&self, event: AlertEvent);
}

#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAlertSink;

impl AlertSink for TracingAlertSink {
    fn alert(&self, event: AlertEvent) {
        error!(?event, "breaker_alert");
    }
}

/// Prometheus sink for breaker alerts (WHI-532). Maps **variant name only** —
/// `reason`/`detail`/`actor` payload strings must never become label values.
#[derive(Debug, Default, Clone, Copy)]
pub struct MetricsAlertSink;

impl AlertSink for MetricsAlertSink {
    fn alert(&self, event: AlertEvent) {
        crate::metrics::record_breaker_alert(&event);
    }
}

/// Fan-out sink: tracing + metrics (WHI-532).
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingAndMetricsAlertSink;

impl AlertSink for TracingAndMetricsAlertSink {
    fn alert(&self, event: AlertEvent) {
        TracingAlertSink.alert(event.clone());
        MetricsAlertSink.alert(event);
    }
}

/// Test double that records alerts in memory.
#[derive(Debug, Default)]
pub struct RecordingAlertSink {
    pub events: std::sync::Mutex<Vec<AlertEvent>>,
}

impl AlertSink for RecordingAlertSink {
    fn alert(&self, event: AlertEvent) {
        self.events.lock().expect("alert lock").push(event);
    }
}

impl RecordingAlertSink {
    pub fn snapshot(&self) -> Vec<AlertEvent> {
        self.events.lock().expect("alert lock").clone()
    }
}
