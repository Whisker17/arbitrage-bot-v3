//! `ShadowInvariantSink` — wraps any [`PreflightAttemptSink`] and enforces the Shadow-stage
//! invariant `preflight::classify` guarantees: under `ExecutionStage::Shadow`,
//! `RiskTieredPreflight` always takes the `Mandatory` branch
//! (`if self.stage != ExecutionStage::Production { return Policy::Mandatory; }`), so a
//! recorded [`PreflightAttempt`] can never legitimately carry `SkippedApproved` or
//! `SampledOut` while shadow mode is running. Seeing either here means either
//! `classify`'s Shadow branch regressed or this sink was wired to a non-Shadow
//! `RiskTieredPreflight` by mistake -- both are programming errors to abort on, not
//! runtime conditions to recover from. `PreflightAttemptSink::record` has no `Result` to
//! propagate a soft failure through, so this panics rather than silently dropping or
//! miscategorizing the row.

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
        assert!(
            !matches!(
                attempt.outcome,
                PreflightOutcome::SkippedApproved | PreflightOutcome::SampledOut
            ),
            "shadow-stage invariant violated: RiskTieredPreflight under \
             ExecutionStage::Shadow must always issue a Mandatory call (see \
             preflight::classify), but this attempt recorded {:?} -- this indicates a \
             policy regression, not a recoverable runtime condition",
            attempt.outcome
        );
        self.inner.record(attempt);
    }
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

    #[test]
    #[should_panic(expected = "shadow-stage invariant violated")]
    fn panics_on_skipped_approved() {
        let sink = ShadowInvariantSink::new(RecordingSink::default());
        sink.record(attempt(PreflightOutcome::SkippedApproved));
    }

    #[test]
    #[should_panic(expected = "shadow-stage invariant violated")]
    fn panics_on_sampled_out() {
        let sink = ShadowInvariantSink::new(RecordingSink::default());
        sink.record(attempt(PreflightOutcome::SampledOut));
    }
}
