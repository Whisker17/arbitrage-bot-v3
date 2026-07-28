//! `ShadowInvariantSink` — wraps any [`PreflightAttemptSink`] and enforces the Shadow-stage
//! invariant `preflight::classify` guarantees: under `ExecutionStage::Shadow`,
//! `RiskTieredPreflight` always takes the `Mandatory` branch
//! (`if self.stage != ExecutionStage::Production { return Policy::Mandatory; }`), so a
//! recorded [`PreflightAttempt`] can never legitimately carry `SkippedApproved` or
//! `SampledOut` while shadow mode is running. Seeing either here means either
//! `classify`'s Shadow branch regressed or this sink was wired to a non-Shadow
//! `RiskTieredPreflight` by mistake -- both are programming errors to abort on, not
//! runtime conditions to recover from.
//!
//! `record` runs on a spawned pipeline task (see `pipeline::run_pipeline_head_closed`), so
//! a `panic!`/`assert!` here only unwinds that one task -- callable and catchable through
//! its `JoinHandle`, letting the rest of the shadow run continue past a policy regression
//! it must never observe. `std::process::abort()` cannot be caught by `catch_unwind` or a
//! `JoinHandle` and terminates the whole process immediately, which is what "abort" means
//! for this invariant.

use crate::execution::preflight::{PreflightAttempt, PreflightAttemptSink, PreflightOutcome};

pub struct ShadowInvariantSink<S> {
    inner: S,
}

impl<S> ShadowInvariantSink<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S: PreflightAttemptSink> PreflightAttemptSink for ShadowInvariantSink<S> {
    fn record(&self, attempt: PreflightAttempt) {
        if matches!(
            attempt.outcome,
            PreflightOutcome::SkippedApproved | PreflightOutcome::SampledOut
        ) {
            abort_on_shadow_stage_invariant_violation(&attempt.outcome);
        }
        self.inner.record(attempt);
    }
}

/// Terminates the whole process (never returns) on a shadow-stage invariant violation.
/// See this module's doc comment for why `std::process::abort()` is used instead of
/// `panic!`.
fn abort_on_shadow_stage_invariant_violation(outcome: &PreflightOutcome) -> ! {
    eprintln!(
        "shadow-stage invariant violated: RiskTieredPreflight under ExecutionStage::Shadow \
         must always issue a Mandatory call (see preflight::classify), but this attempt \
         recorded {outcome:?} -- this indicates a policy regression, not a recoverable \
         runtime condition. Aborting the process."
    );
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::final_request::FinalRequestDigest;
    use crate::execution::preflight::{BlockTag, PolicyKey};
    use alloy::primitives::B256;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Default, Clone)]
    struct RecordingSink {
        attempts: Arc<Mutex<Vec<PreflightAttempt>>>,
    }

    impl PreflightAttemptSink for RecordingSink {
        fn record(&self, attempt: PreflightAttempt) {
            self.attempts.lock().unwrap().push(attempt);
        }
    }

    fn attempt(outcome: PreflightOutcome) -> PreflightAttempt {
        PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome,
            digest: FinalRequestDigest(B256::repeat_byte(0x01)),
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(1)),
            detail: None,
        }
    }

    #[test]
    fn passes_non_skip_outcomes_through_to_the_inner_sink() {
        let inner = RecordingSink::default();
        let sink = ShadowInvariantSink::new(inner.clone());

        sink.record(attempt(PreflightOutcome::Pass));
        sink.record(attempt(PreflightOutcome::Revert(
            "insufficient liquidity".into(),
        )));

        assert_eq!(inner.attempts.lock().unwrap().len(), 2);
    }

    /// Not run by the normal harness (`#[ignore]`) -- invoked only via a re-exec'd child
    /// process below, so its `std::process::abort()` call terminates that child, not the
    /// whole test run.
    #[test]
    #[ignore]
    fn abort_child_skipped_approved() {
        let sink = ShadowInvariantSink::new(RecordingSink::default());
        sink.record(attempt(PreflightOutcome::SkippedApproved));
    }

    #[test]
    #[ignore]
    fn abort_child_sampled_out() {
        let sink = ShadowInvariantSink::new(RecordingSink::default());
        sink.record(attempt(PreflightOutcome::SampledOut));
    }

    /// Confirms `record` terminates the *entire process*, not just the calling task, on a
    /// shadow-stage invariant violation. `std::process::abort()` cannot be caught by
    /// `catch_unwind`/a `JoinHandle`, so the only way to observe it is to run the
    /// violating call in a child process and check it died by signal (SIGABRT), not a
    /// normal panic-unwind exit.
    #[cfg(unix)]
    fn assert_child_aborts(test_name: &str) {
        use std::os::unix::process::ExitStatusExt;
        use std::process::Command;

        let exe = std::env::current_exe().expect("test binary path must be available");
        let status = Command::new(exe)
            .args(["--exact", "--ignored", "--nocapture", test_name])
            .status()
            .expect("child test process must spawn");

        assert!(
            !status.success(),
            "child running {test_name} must not exit successfully"
        );
        assert_eq!(
            status.signal(),
            Some(libc::SIGABRT),
            "child running {test_name} must be killed by SIGABRT (std::process::abort), \
             got status {status:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn aborts_the_whole_process_on_skipped_approved() {
        assert_child_aborts("execution::shadow::invariant::tests::abort_child_skipped_approved");
    }

    #[cfg(unix)]
    #[test]
    fn aborts_the_whole_process_on_sampled_out() {
        assert_child_aborts("execution::shadow::invariant::tests::abort_child_sampled_out");
    }
}
