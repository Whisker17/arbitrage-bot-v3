//! Throttle-derived pipeline concurrency for pipelined eth_calls (WHI-968).
//!
//! The HTTP throttle and per-request timeout live in [`crate::service::rpc_provider`].
//! Pipelined eth_call sites in `amms` need the same throttle budget so they never
//! open more in-flight futures than the limiter can dequeue within the timeout.
//!
//! ## Timeout vs queue depth (WHI-968 decision)
//!
//! Production layer order is **retry → timeout → throttle → HTTP**. The
//! per-request timeout therefore **includes** time spent waiting on the
//! throttle. We intentionally keep that order: the timeout remains an
//! end-to-end deadline for a single attempt (including brief rate-limit wait).
//!
//! Unbounded fan-out behind that stack is what trips the 30 s timeout: many
//! futures submit at once, sit in the throttle queue past the deadline, and
//! fail even though the endpoint is healthy. The fix is **not** to stop the
//! timeout clock until dequeue (that would let queue depth grow without
//! bound). Instead, **pipeline concurrency is derived from the configured
//! throttle RPS** so in-flight work never exceeds the per-second budget and
//! queue wait stays well under the timeout.
//!
//! Process-global active RPS is set when the production HTTP provider is
//! built so `amms` can read it without depending on `service` (same pattern
//! as [`crate::rpc_rate_pressure`]).
//!
//! ## Pipelined eth_call sites (derivation inventory — WHI-968)
//!
//! | Site | Former constant | Now |
//! | --- | --- | --- |
//! | `amms/agni` fee/tickSpacing populate | `METADATA_CONCURRENCY = 16` | [`active_pipelined_rpc_concurrency`] |
//! | `amms/uniswap_v3` fee/tickSpacing populate | `METADATA_CONCURRENCY = 16` | same |
//! | `amms/moe/sync` bin CREATE wave | `BIN_WAVE = 4` | same |
//! | `amms/moe/pool_list` on-chain validate | `ON_CHAIN_VALIDATE_CONCURRENCY = 8` | same |
//! | `examples/**/*_monitor_executor_service` init fan-out | `MAX_INIT_CONCURRENCY = 8` | same |
//!
//! Batch-CREATE paths that process sequential groups (V3 tick bitmap/data,
//! Moe slot0 chunks) are intentionally not fan-out concurrency knobs — they
//! were already serialized against the same timeout/throttle failure class
//! (WHI-929).

use std::sync::atomic::{AtomicU32, Ordering};

/// Canonical default HTTP throttle RPS / pipeline concurrency budget.
///
/// Shared by [`crate::service::rpc_provider::DEFAULT_HTTP_THROTTLE_RPS`] so the
/// production throttle default and the process-global pipeline budget cannot
/// drift (WHI-968).
pub const DEFAULT_PIPELINE_THROTTLE_RPS: u32 = 8;

static ACTIVE_THROTTLE_RPS: AtomicU32 = AtomicU32::new(DEFAULT_PIPELINE_THROTTLE_RPS);

/// Record the HTTP throttle RPS used by the active production provider.
///
/// Call from [`crate::service::rpc_provider::connect_http_provider`] so
/// pipelined eth_call sites inherit the same budget as the throttle layer.
pub fn set_active_throttle_rps(rps: u32) {
    ACTIVE_THROTTLE_RPS.store(rps.max(1), Ordering::Relaxed);
}

/// Active HTTP throttle RPS (never zero).
pub fn active_throttle_rps() -> u32 {
    ACTIVE_THROTTLE_RPS.load(Ordering::Relaxed).max(1)
}

/// Max concurrent in-flight pipelined eth_calls for a given throttle RPS.
///
/// In-flight work beyond the throttle only queues; with the timeout wrapping
/// the throttle, that queue time counts toward the deadline. Concurrency must
/// not exceed the throttle budget (WHI-968).
pub fn pipelined_rpc_concurrency(throttle_rps: u32) -> usize {
    throttle_rps.max(1) as usize
}

/// [`pipelined_rpc_concurrency`] for the active production throttle.
pub fn active_pipelined_rpc_concurrency() -> usize {
    pipelined_rpc_concurrency(active_throttle_rps())
}

/// Test helper: restore the default active throttle.
#[cfg(test)]
pub fn reset_active_throttle_rps_for_test() {
    set_active_throttle_rps(DEFAULT_PIPELINE_THROTTLE_RPS);
}

/// Crate-wide mutex for tests that mutate the process-global active throttle.
#[cfg(test)]
pub static PIPELINE_THROTTLE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipelined_concurrency_never_exceeds_throttle_budget() {
        for throttle in [4u32, 8, 16] {
            let concurrency = pipelined_rpc_concurrency(throttle);
            assert!(
                (concurrency as u32) <= throttle,
                "concurrency {concurrency} exceeds throttle {throttle}"
            );
            assert!(concurrency >= 1);
        }
    }

    #[test]
    fn pipelined_concurrency_equals_throttle_budget() {
        // Derivation is 1:1 with the throttle RPS — no independent constant.
        for throttle in [1u32, 4, 8, 16, 40] {
            assert_eq!(pipelined_rpc_concurrency(throttle), throttle as usize);
        }
    }

    #[test]
    fn zero_throttle_clamps_to_one() {
        assert_eq!(pipelined_rpc_concurrency(0), 1);
    }

    #[test]
    fn active_tracks_set_throttle() {
        let _g = PIPELINE_THROTTLE_TEST_LOCK.lock().unwrap();
        set_active_throttle_rps(4);
        assert_eq!(active_throttle_rps(), 4);
        assert_eq!(active_pipelined_rpc_concurrency(), 4);
        set_active_throttle_rps(16);
        assert_eq!(active_pipelined_rpc_concurrency(), 16);
        reset_active_throttle_rps_for_test();
        assert_eq!(active_throttle_rps(), DEFAULT_PIPELINE_THROTTLE_RPS);
        assert_eq!(
            active_pipelined_rpc_concurrency(),
            DEFAULT_PIPELINE_THROTTLE_RPS as usize
        );
    }
}
