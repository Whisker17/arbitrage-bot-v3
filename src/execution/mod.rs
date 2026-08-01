pub mod breaker;
pub mod contract;
pub mod e2e;
pub mod executor;
pub mod fee_context;
pub mod final_request;
pub mod gas_profile;
pub mod gas_runtime;
pub mod identity;
pub mod intent;
pub mod mainnet_fork_harness;
mod nonce;
mod params;
pub mod pause;
pub mod pipeline;
pub mod preflight;
pub mod principal;
pub mod provenance;
pub mod runtime_identity;
pub mod shadow;
pub mod shadow_bot_benchmark;
pub mod shadow_decision;
pub mod shadow_gate_plan;
pub mod shadow_report;
pub mod shadow_thresholds;
pub mod types;

pub use breaker::{
    AccountingCommit, AccountingRecord, AlertEvent, AlertSink, BreakerConfig, BreakerRuntime,
    MetricsAlertSink, TracingAndMetricsAlertSink,
    BreakerStats, CoordinatorError, DurableIntentCoordinator, PauseController, ReversalRecord,
    ScopeId, TracingAlertSink, WalDurableHook,
};
pub use contract::*;
pub use executor::*;
pub use fee_context::*;
pub use final_request::*;
pub use gas_profile::*;
pub use gas_runtime::*;
pub use identity::*;
pub use intent::{
    Attempt, AttemptPayload, CandidateRef, CanonicalBlock, ChainNonceView, IntentError,
    IntentEvent, IntentState, IntentStateMachine, LatestWinsSlot, NeedsOperatorReason, NonceIntent,
    PrepareRequest, PreparedPayload, ReceiptOutcome, SharedIntentStateMachine, SignedSubmission,
};
pub use mainnet_fork_harness::*;
pub use pause::*;
pub use pipeline::*;
pub use preflight::*;
pub use principal::*;
pub use provenance::*;
pub use runtime_identity::*;
pub use shadow::*;
// `shadow_decision` and `shadow_gate_plan` both define `build_envelope`/`sign`
// (same shape, different payload types) -- glob re-exporting both is an
// ambiguous-name warning, so each symbol here is listed explicitly and the
// colliding `build_envelope`/`sign` stay reachable only via their fully
// qualified module paths (no caller depends on the unqualified form).
pub use shadow_decision::{
    check_approve_eligibility, DecisionError, DecisionPayload, DecisionVerifier,
    ProductionDecisionVerifier, Verdict, GATE_DECISION_DOMAIN, GATE_DECISION_SCHEMA_VERSION,
};
pub use shadow_gate_plan::{
    digest_bytes, digest_file_bytes, GatePlanError, GatePlanPayload, GatePlanVerifier,
    ProductionGatePlanVerifier, ShadowGateScope, GATE_PLAN_DOMAIN, GATE_PLAN_SCHEMA_VERSION,
};
pub use shadow_bot_benchmark::{
    classify_event, compare, load_known_bot_events, render_markdown_report, BenchmarkError,
    BenchmarkReport, Bucket, BucketCounts, ClassifiedEvent, KnownBotEvent, LedgerBytes,
    ShadowLedgerIndex, ShadowOpportunity, BENCHMARK_REPORT_SCHEMA_VERSION,
};
pub use shadow_report::*;
pub use shadow_thresholds::*;
pub use types::*;

#[cfg(test)]
mod fee_context_tests;

#[cfg(test)]
mod gas_runtime_tests;

#[cfg(test)]
mod gas_runtime_sepolia_tests;
