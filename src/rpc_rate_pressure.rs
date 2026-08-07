//! Process-global recent rate-limit signal (WHI-921).
//!
//! `CreateContractSizeLimit` under 429 pressure wants the opposite response from
//! a genuinely oversized CREATE batch (slow down vs split). The production RPC
//! retry layer notes 429 / -32016 events here; Moe sync re-samples via
//! [`under_rpc_rate_pressure`].
//!
//! This is control-plane state, not a Prometheus metric — kept as a tiny crate
//! root module so both `service` and `amms` can share it without layering
//! inversion through the metrics facade.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Unix-ms of the most recent rate-limit / HTTP 429 observation (0 = never).
static LAST_RATE_LIMIT_UNIX_MS: AtomicU64 = AtomicU64::new(0);

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Record that a rate-limit (HTTP 429 / JSON-RPC -32016) was observed.
pub fn note_rpc_rate_limit() {
    LAST_RATE_LIMIT_UNIX_MS.store(unix_now_ms(), Ordering::Relaxed);
}

/// True if a rate-limit event was noted within `window`.
pub fn under_rpc_rate_pressure(window: Duration) -> bool {
    let last = LAST_RATE_LIMIT_UNIX_MS.load(Ordering::Relaxed);
    if last == 0 {
        return false;
    }
    let now = unix_now_ms();
    now.saturating_sub(last) <= window.as_millis() as u64
}

/// Test helper: clear the rate-pressure signal.
#[cfg(test)]
pub fn clear_rpc_rate_limit_for_test() {
    LAST_RATE_LIMIT_UNIX_MS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_false_when_never_noted() {
        clear_rpc_rate_limit_for_test();
        assert!(!under_rpc_rate_pressure(Duration::from_secs(10)));
    }

    #[test]
    fn pressure_true_immediately_after_note() {
        clear_rpc_rate_limit_for_test();
        note_rpc_rate_limit();
        assert!(under_rpc_rate_pressure(Duration::from_secs(10)));
        clear_rpc_rate_limit_for_test();
    }

    #[test]
    fn pressure_false_outside_window() {
        clear_rpc_rate_limit_for_test();
        // Store a timestamp far in the past.
        LAST_RATE_LIMIT_UNIX_MS.store(1, Ordering::Relaxed);
        assert!(!under_rpc_rate_pressure(Duration::from_millis(1)));
        clear_rpc_rate_limit_for_test();
    }
}
