//! Typed metric emit helpers (WHI-532).
//!
//! Call sites outside this module must not use `metrics::` macros directly — label
//! spelling and enum exhaustiveness live here.

use std::time::Duration;

use alloy::primitives::U256;
use metrics::{
    counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};

use super::names::*;
use crate::execution::breaker::{AlertEvent, BreakerStats};
use crate::execution::intent::{IntentEvent, NeedsOperatorReason};
use crate::execution::preflight::{
    BlockTag, PolicyKey, PreflightAttempt, PreflightOutcome,
};
use crate::state_space::{ForkKind, HaltReason, HeadDecision, SnapshotStatus};

/// Pipeline stage label values for `arbbot_pipeline_stage_duration_seconds`.
pub mod stage {
    pub const SNAPSHOT_ASSEMBLE: &str = "snapshot_assemble";
    pub const DISCOVERY: &str = "discovery";
    pub const OPTIMIZE: &str = "optimize";
    pub const PREFLIGHT: &str = "preflight";
    pub const SIGN_AND_BROADCAST: &str = "sign_and_broadcast";
}

/// `arbbot_block_to_submit_duration_seconds` outcome labels.
pub mod block_outcome {
    pub const SUBMITTED: &str = "submitted";
    pub const GATE_BLOCKED: &str = "gate_blocked";
    pub const PREFLIGHT_FAILED: &str = "preflight_failed";
    pub const STALE_TIP: &str = "stale_tip";
}

/// Discovery rejection reason labels.
pub mod reject_reason {
    pub const POOL_LOOKUP: &str = "pool_lookup";
    pub const NO_OPTIMUM: &str = "no_optimum";
    pub const ZERO_PROFIT: &str = "zero_profit";
    pub const MIXED_SIM_ERROR: &str = "mixed_sim_error";
    pub const GROSS_UNDERFLOW: &str = "gross_underflow";
    pub const HOP_CAP: &str = "hop_cap";
    pub const GAS_SCREEN: &str = "gas_screen";
    pub const NET_PROFIT: &str = "net_profit";
    pub const EXPECTED_STATES: &str = "expected_states";
}

/// Register HELP/TYPE for every series. Safe to call more than once.
pub fn describe_all() {
    describe_gauge!(
        BUILD_INFO,
        "Build identity (const 1). Labels carry version/git_sha/protocols/production_send_allowed. Exemplars: bot"
    );
    describe_histogram!(
        BLOCK_TO_SUBMIT_DURATION_SECONDS,
        "End-to-end seconds from head observation to terminal job outcome. Exemplars: service.block_loop"
    );
    describe_histogram!(
        PIPELINE_STAGE_DURATION_SECONDS,
        "Per-stage pipeline latency in seconds. Exemplars: bot.discovery, execution.pipeline"
    );
    describe_counter!(
        BLOCKS_OBSERVED_TOTAL,
        "Heads classified by SnapshotPublisher::observe_head. Exemplars: state_space.snapshot"
    );
    describe_counter!(
        SNAPSHOT_HALTS_TOTAL,
        "Snapshot publisher halts by reason. Exemplars: state_space.snapshot"
    );
    describe_counter!(
        SNAPSHOT_FORKS_TOTAL,
        "Fork classifications by kind. Exemplars: state_space.snapshot"
    );
    describe_gauge!(
        SNAPSHOT_GAP_BLOCKS,
        "Size of the most recent numeric gap (observed - last). Exemplars: state_space.snapshot"
    );
    describe_gauge!(
        SNAPSHOT_STATUS,
        "One series per SnapshotStatus variant; exactly one is 1. Exemplars: state_space.snapshot"
    );
    describe_gauge!(
        SNAPSHOT_BLOCK_NUMBER,
        "Block number of the latest Ready publication. Exemplars: state_space.snapshot"
    );
    describe_counter!(
        SNAPSHOT_PUBLICATIONS_TOTAL,
        "Successful Ready publications. Exemplars: state_space.snapshot"
    );
    describe_gauge!(
        DISCOVERY_POOLS_LOADED,
        "Pools loaded into the discovery universe per protocol. Exemplars: bot.discovery"
    );
    describe_counter!(
        DISCOVERY_CYCLES_FOUND_TOTAL,
        "Closed settlement cycles returned by PathFinder::find_cycles. Exemplars: bot.discovery"
    );
    describe_counter!(
        DISCOVERY_CANDIDATES_TOTAL,
        "Candidates that survived all discovery screens. Exemplars: bot.discovery"
    );
    describe_counter!(
        DISCOVERY_REJECTED_TOTAL,
        "Candidates rejected during discovery by reason. Exemplars: bot.discovery"
    );
    describe_gauge!(
        DISCOVERY_BEST_NET_PROFIT_MNT,
        "Best net profit of the discovery pass in MNT (lossy wei→f64). Exemplars: bot.discovery"
    );
    describe_counter!(
        PREFLIGHT_ATTEMPTS_TOTAL,
        "Semantic preflight attempts. Exemplars: execution.preflight"
    );
    describe_histogram!(
        PREFLIGHT_DURATION_SECONDS,
        "Semantic preflight latency in seconds (observed only when a call was issued). Exemplars: execution.preflight"
    );
    describe_counter!(
        INTENT_EVENTS_TOTAL,
        "Intent state-machine events by variant. Exemplars: execution.runtime"
    );
    describe_counter!(
        INTENT_FINALIZED_TOTAL,
        "Finalized execute/cancel intents by success. Exemplars: execution.runtime"
    );
    describe_counter!(
        INTENT_NEEDS_OPERATOR_TOTAL,
        "Intents entering NeedsOperator by reason. Exemplars: execution.runtime"
    );
    describe_gauge!(
        INTENT_LIVE_COUNT,
        "Non-drained live intents currently tracked. Exemplars: execution.runtime"
    );
    describe_gauge!(
        INTENT_NEXT_NONCE,
        "Next nonce the IntentStateMachine would reserve. Exemplars: execution.runtime"
    );
    describe_gauge!(
        GAS_BASE_FEE_WEI,
        "Latest observed base fee in wei. Exemplars: service.gas"
    );
    describe_counter!(
        GAS_PROFILE_QUOTE_TOTAL,
        "Runtime gas profile quote hits/misses. Exemplars: execution.runtime"
    );
    describe_counter!(
        GAS_USED_TOTAL,
        "Gas used from receipts / requalification. Exemplars: execution.runtime"
    );
    describe_counter!(
        GAS_COST_MNT_TOTAL,
        "Actual gas cost accumulated in micro-MNT units (1e-6 MNT; metrics facade counters are integer-only). Exemplars: execution.runtime"
    );
    describe_gauge!(
        SETTLEMENT_BALANCE_MNT,
        "Settlement-token balance bound to a snapshot, in MNT (lossy). Exemplars: execution.runtime"
    );
    describe_gauge!(
        BREAKER_PAUSED,
        "1 when the circuit breaker is paused. Exemplars: execution.breaker"
    );
    describe_gauge!(
        BREAKER_CONSECUTIVE_REVERTS,
        "Consecutive execute reverts tracked by the breaker. Exemplars: execution.breaker"
    );
    describe_gauge!(
        BREAKER_WINDOW_LOSS_MNT,
        "Windowed realized loss in MNT (lossy). Exemplars: execution.breaker"
    );
    describe_gauge!(
        BREAKER_CHARGED_ENTRIES,
        "Charged ledger entries counted by the breaker. Exemplars: execution.breaker"
    );
    describe_counter!(
        BREAKER_ALERTS_TOTAL,
        "Breaker alert events by variant name only. Exemplars: execution.breaker"
    );
    describe_counter!(
        RPC_RETRIES_TOTAL,
        "Transient RPC transport retries by error class. Exemplars: service.rpc"
    );
    describe_counter!(
        WATCH_REBASELINES_TOTAL,
        "Watch-loop large-gap re-baselines by kind (cold_start|mid_run). Exemplars: service.block_loop"
    );
    describe_histogram!(
        HTTP_TIP_WAIT_DURATION_SECONDS,
        "Seconds spent waiting for HTTP to serve an announced block hash. Exemplars: service.block_loop"
    );
    describe_counter!(
        HTTP_TIP_TIMEOUTS_TOTAL,
        "Heads skipped because HTTP never served the announced hash within the wait deadline. Exemplars: service.block_loop"
    );
    describe_counter!(
        WATCH_BLOCK_SKIPS_TOTAL,
        "Watch-loop block skips by reason (pinned hash unavailable, continuity halt, …). Exemplars: service.block_loop (WHI-762)"
    );
    describe_counter!(
        WATCH_SKIP_RATIO_WARNINGS_TOTAL,
        "Times the rolling skip-ratio threshold was breached. Exemplars: service.block_loop (WHI-762)"
    );
}

/// Lossy wei → MNT (`/ 1e18`) as `f64`.
///
/// Integers above `2^53` lose exact representation; practical inventory sizes stay
/// accurate well past sub-milli-MNT. **Never** use the raw wei integer as a metric value
/// for money quantities.
pub fn wei_to_mnt_f64(wei: U256) -> f64 {
    const WEI_PER_MNT: f64 = 1e18;
    match u128::try_from(wei) {
        Ok(v) => (v as f64) / WEI_PER_MNT,
        Err(_) => {
            // Split into whole MNT + remainder to keep magnitude for huge balances.
            let scale = U256::from(10u64).pow(U256::from(18u64));
            let whole = wei / scale;
            let rem = wei % scale;
            let whole_f = u128::try_from(whole).map(|v| v as f64).unwrap_or(f64::MAX);
            let rem_f = u128::try_from(rem).map(|v| v as f64).unwrap_or(0.0) / WEI_PER_MNT;
            whole_f + rem_f
        }
    }
}

pub fn record_build_info(
    version: &str,
    git_sha: &str,
    protocols: &str,
    production_send_allowed: bool,
) {
    gauge!(
        BUILD_INFO,
        LABEL_VERSION => version.to_string(),
        LABEL_GIT_SHA => git_sha.to_string(),
        LABEL_PROTOCOLS => protocols.to_string(),
        LABEL_PRODUCTION_SEND_ALLOWED => if production_send_allowed { "true" } else { "false" },
    )
    .set(1.0);
}

pub fn record_head_decision(d: &HeadDecision) {
    let decision = match d {
        HeadDecision::Bootstrap => "bootstrap",
        HeadDecision::Advance => "advance",
        HeadDecision::Duplicate => "duplicate",
        HeadDecision::Fork(_) => "fork",
        HeadDecision::Gap { .. } => "gap",
    };
    counter!(BLOCKS_OBSERVED_TOTAL, LABEL_DECISION => decision).increment(1);
    if let HeadDecision::Gap {
        last_number,
        observed_number,
    } = d
    {
        let gap = observed_number.saturating_sub(*last_number);
        gauge!(SNAPSHOT_GAP_BLOCKS).set(gap as f64);
    }
}

pub fn record_halt(reason: &HaltReason) {
    let label = match reason {
        HaltReason::Fork { .. } => "fork",
        HaltReason::Gap { .. } => "gap",
        HaltReason::ReadFailure(_) => "read_failure",
        HaltReason::IdentityMismatch(_) => "identity_mismatch",
        HaltReason::ResyncRequired => "resync_required",
    };
    counter!(SNAPSHOT_HALTS_TOTAL, LABEL_REASON => label).increment(1);
    if let HaltReason::Fork { kind, .. } = reason {
        record_fork_kind(*kind);
    }
    if let HaltReason::Gap {
        last_number,
        observed_number,
    } = reason
    {
        let gap = observed_number.saturating_sub(*last_number);
        gauge!(SNAPSHOT_GAP_BLOCKS).set(gap as f64);
    }
    record_snapshot_status(&SnapshotStatus::Halted(reason.clone()));
}

fn record_fork_kind(kind: ForkKind) {
    let label = match kind {
        ForkKind::SameHeightReplacement => "same_height_replacement",
        ForkKind::HeightRollback => "height_rollback",
        ForkKind::WrongParent => "wrong_parent",
        ForkKind::ChainIdMismatch => "chain_id_mismatch",
    };
    counter!(SNAPSHOT_FORKS_TOTAL, LABEL_KIND => label).increment(1);
}

pub fn record_snapshot_status(s: &SnapshotStatus) {
    // Exactly one series is 1; others forced to 0 so scrapes stay unambiguous.
    let (ready, syncing, halted) = match s {
        SnapshotStatus::Ready(_) => (1.0, 0.0, 0.0),
        SnapshotStatus::Syncing => (0.0, 1.0, 0.0),
        SnapshotStatus::Halted(_) => (0.0, 0.0, 1.0),
    };
    gauge!(SNAPSHOT_STATUS, LABEL_STATUS => "ready").set(ready);
    gauge!(SNAPSHOT_STATUS, LABEL_STATUS => "syncing").set(syncing);
    gauge!(SNAPSHOT_STATUS, LABEL_STATUS => "halted").set(halted);
}

/// Publish Ready status + block number after a successful snapshot publish.
pub fn record_snapshot_ready(block_number: u64, status: &SnapshotStatus) {
    record_snapshot_status(status);
    gauge!(SNAPSHOT_BLOCK_NUMBER).set(block_number as f64);
    counter!(SNAPSHOT_PUBLICATIONS_TOTAL).increment(1);
}

pub fn record_pipeline_stage(stage: &'static str, protocol: &str, duration: Duration) {
    histogram!(
        PIPELINE_STAGE_DURATION_SECONDS,
        LABEL_STAGE => stage,
        LABEL_PROTOCOL => protocol.to_string(),
    )
    .record(duration.as_secs_f64());
}

pub fn record_block_to_submit(protocol: &str, outcome: &'static str, duration: Duration) {
    histogram!(
        BLOCK_TO_SUBMIT_DURATION_SECONDS,
        LABEL_PROTOCOL => protocol.to_string(),
        LABEL_OUTCOME => outcome,
    )
    .record(duration.as_secs_f64());
}

pub fn record_discovery_pools_loaded(protocol: &str, count: usize) {
    gauge!(DISCOVERY_POOLS_LOADED, LABEL_PROTOCOL => protocol.to_string()).set(count as f64);
}

pub fn record_discovery_cycles_found(count: usize) {
    counter!(DISCOVERY_CYCLES_FOUND_TOTAL).increment(count as u64);
}

pub fn record_discovery_rejected(reason: &'static str) {
    counter!(DISCOVERY_REJECTED_TOTAL, LABEL_REASON => reason).increment(1);
}

pub fn record_discovery_candidate(protocol_mix: &str) {
    counter!(
        DISCOVERY_CANDIDATES_TOTAL,
        LABEL_PROTOCOL_MIX => protocol_mix.to_string(),
    )
    .increment(1);
}

pub fn record_discovery_best_net_profit(protocol_mix: &str, net_profit_wei: U256) {
    gauge!(
        DISCOVERY_BEST_NET_PROFIT_MNT,
        LABEL_PROTOCOL_MIX => protocol_mix.to_string(),
    )
    .set(wei_to_mnt_f64(net_profit_wei));
}

pub fn record_preflight_attempt(a: &PreflightAttempt) {
    let outcome = preflight_outcome_label(&a.outcome);
    let policy = policy_key_label(a.policy_key);
    let block_tag = a
        .block_tag
        .map(block_tag_label)
        .unwrap_or("none");
    counter!(
        PREFLIGHT_ATTEMPTS_TOTAL,
        LABEL_OUTCOME => outcome,
        LABEL_POLICY_KEY => policy,
        LABEL_BLOCK_TAG => block_tag,
    )
    .increment(1);
    if let Some(latency) = a.latency {
        histogram!(
            PREFLIGHT_DURATION_SECONDS,
            LABEL_OUTCOME => outcome,
        )
        .record(latency.as_secs_f64());
    }
}

fn preflight_outcome_label(o: &PreflightOutcome) -> &'static str {
    match o {
        PreflightOutcome::Pass => "pass",
        PreflightOutcome::Revert(_) => "revert",
        PreflightOutcome::RpcError(_) => "rpc_error",
        PreflightOutcome::EnvUnsupported => "env_unsupported",
        PreflightOutcome::SkippedApproved => "skipped_approved",
        PreflightOutcome::SampledOut => "sampled_out",
    }
}

fn policy_key_label(p: PolicyKey) -> &'static str {
    match p {
        PolicyKey::Mandatory => "mandatory",
        PolicyKey::ApprovedStableDisabled => "approved_stable_disabled",
        PolicyKey::ApprovedStableSampled => "approved_stable_sampled",
    }
}

fn block_tag_label(t: BlockTag) -> &'static str {
    match t {
        BlockTag::Latest => "latest",
        BlockTag::Pending => "pending",
    }
}

pub fn record_intent_event(e: &IntentEvent) {
    let event = intent_event_label(e);
    counter!(INTENT_EVENTS_TOTAL, LABEL_EVENT => event).increment(1);

    match e {
        IntentEvent::Finalized {
            success,
            actual_cost,
            ..
        } => {
            counter!(
                INTENT_FINALIZED_TOTAL,
                LABEL_KIND => "execute",
                LABEL_SUCCESS => if *success { "true" } else { "false" },
            )
            .increment(1);
            // metrics::counter is integer-only; accumulate micro-MNT (1e-6 MNT).
            let micro_mnt = (wei_to_mnt_f64(*actual_cost) * 1_000_000.0).max(0.0).round() as u64;
            if micro_mnt > 0 {
                counter!(GAS_COST_MNT_TOTAL, LABEL_KIND => "execute").increment(micro_mnt);
            }
        }
        IntentEvent::CancelFinalized {
            success,
            actual_cost,
            ..
        } => {
            counter!(
                INTENT_FINALIZED_TOTAL,
                LABEL_KIND => "cancel",
                LABEL_SUCCESS => if *success { "true" } else { "false" },
            )
            .increment(1);
            let micro_mnt = (wei_to_mnt_f64(*actual_cost) * 1_000_000.0).max(0.0).round() as u64;
            if micro_mnt > 0 {
                counter!(GAS_COST_MNT_TOTAL, LABEL_KIND => "cancel").increment(micro_mnt);
            }
        }
        IntentEvent::NeedsOperator { reason, .. } => {
            record_needs_operator(reason);
        }
        IntentEvent::GasProfileRequalification { gas_used, .. } => {
            counter!(GAS_USED_TOTAL, LABEL_KIND => "execute").increment(*gas_used);
        }
        _ => {}
    }
}

fn intent_event_label(e: &IntentEvent) -> &'static str {
    match e {
        IntentEvent::Reserved { .. } => "reserved",
        IntentEvent::Submitted { .. } => "submitted",
        IntentEvent::IncludedUnconfirmed { .. } => "included_unconfirmed",
        IntentEvent::Finalized { .. } => "finalized",
        IntentEvent::CancelFinalized { .. } => "cancel_finalized",
        IntentEvent::Released { .. } => "released",
        IntentEvent::NeedsOperator { .. } => "needs_operator",
        IntentEvent::Reopened { .. } => "reopened",
        IntentEvent::Halted { .. } => "halted",
        IntentEvent::Superseded { .. } => "superseded",
        IntentEvent::GasProfileRequalification { .. } => "gas_profile_requalification",
    }
}

pub fn record_needs_operator(r: &NeedsOperatorReason) {
    let reason = match r {
        NeedsOperatorReason::CancelUnpriceable => "cancel_unpriceable",
        NeedsOperatorReason::CancelBudgetExhausted => "cancel_budget_exhausted",
        NeedsOperatorReason::FeeContextUnavailable => "fee_context_unavailable",
        NeedsOperatorReason::ExternalNonceActivity => "external_nonce_activity",
        NeedsOperatorReason::DeepReorgHalt => "deep_reorg_halt",
    };
    counter!(INTENT_NEEDS_OPERATOR_TOTAL, LABEL_REASON => reason).increment(1);
}

pub fn record_intent_gauges(live_count: usize, next_nonce: u64) {
    gauge!(INTENT_LIVE_COUNT).set(live_count as f64);
    gauge!(INTENT_NEXT_NONCE).set(next_nonce as f64);
}

pub fn record_breaker_stats(s: &BreakerStats) {
    gauge!(BREAKER_PAUSED).set(if s.paused { 1.0 } else { 0.0 });
    gauge!(BREAKER_CONSECUTIVE_REVERTS).set(s.consecutive_reverts as f64);
    gauge!(BREAKER_WINDOW_LOSS_MNT).set(wei_to_mnt_f64(s.window_loss_wei));
    gauge!(BREAKER_CHARGED_ENTRIES).set(s.charged_entries as f64);
}

pub fn record_breaker_alert(e: &AlertEvent) {
    let event = alert_event_label(e);
    counter!(BREAKER_ALERTS_TOTAL, LABEL_EVENT => event).increment(1);
}

fn alert_event_label(e: &AlertEvent) -> &'static str {
    match e {
        AlertEvent::BreakerTrip { .. } => "breaker_trip",
        AlertEvent::Pause { .. } => "pause",
        AlertEvent::Unpause { .. } => "unpause",
        AlertEvent::Init { .. } => "init",
        AlertEvent::Recovery { .. } => "recovery",
        AlertEvent::Tamper { .. } => "tamper",
        AlertEvent::InventoryViolation { .. } => "inventory_violation",
        AlertEvent::LedgerReversal { .. } => "ledger_reversal",
        AlertEvent::RestartValidationFailure { .. } => "restart_validation_failure",
        AlertEvent::IncompleteFeeAccounting { .. } => "incomplete_fee_accounting",
        AlertEvent::NeedsOperator { .. } => "needs_operator",
        AlertEvent::Halted { .. } => "halted",
        AlertEvent::AnomalousCancel { .. } => "anomalous_cancel",
        AlertEvent::UnattributableActivity { .. } => "unattributable_activity",
        AlertEvent::DrainTimeout { .. } => "drain_timeout",
    }
}

pub fn record_settlement_balance(holder: &'static str, wei: U256) {
    gauge!(SETTLEMENT_BALANCE_MNT, LABEL_HOLDER => holder).set(wei_to_mnt_f64(wei));
}

pub fn record_gas_profile_quote(hit: bool) {
    let result = if hit { "hit" } else { "miss" };
    counter!(GAS_PROFILE_QUOTE_TOTAL, LABEL_RESULT => result).increment(1);
}

pub fn record_gas_base_fee(wei: u128) {
    gauge!(GAS_BASE_FEE_WEI).set(wei as f64);
}

/// Count one transient RPC retry (WHI-786).
///
/// `error_class` is a stable low-cardinality label from
/// [`crate::service::rpc_provider::classify_retry_error`].
pub fn record_rpc_retry(error_class: &'static str) {
    counter!(RPC_RETRIES_TOTAL, LABEL_ERROR_CLASS => error_class).increment(1);
}

/// Count a watch-loop large-gap re-baseline (WHI-792).
///
/// `kind` is `cold_start` or `mid_run`.
pub fn record_watch_rebaseline(kind: &'static str) {
    counter!(WATCH_REBASELINES_TOTAL, LABEL_KIND => kind).increment(1);
}

/// Record how long the loop waited for HTTP to catch a WS tip (WHI-792).
pub fn record_http_tip_wait(duration: Duration) {
    histogram!(HTTP_TIP_WAIT_DURATION_SECONDS).record(duration.as_secs_f64());
}

/// Count one HTTP tip wait deadline expiry (WHI-792).
pub fn record_http_tip_timeout() {
    counter!(HTTP_TIP_TIMEOUTS_TOTAL).increment(1);
}

/// Count one watch-loop block skip (WHI-762).
///
/// `reason` is a stable low-cardinality label from [`crate::service::block_loop::BlockSkipReason`].
pub fn record_watch_block_skip(reason: &'static str) {
    counter!(WATCH_BLOCK_SKIPS_TOTAL, LABEL_REASON => reason).increment(1);
}

/// Count one rolling skip-ratio threshold breach (WHI-762).
pub fn record_watch_skip_ratio_warning() {
    counter!(WATCH_SKIP_RATIO_WARNINGS_TOTAL).increment(1);
}

/// Zero-init emit so every series appears in a scrape after `describe_all`.
pub fn emit_zero_init() {
    record_build_info("0", "unknown", "none", false);
    record_block_to_submit("none", block_outcome::STALE_TIP, Duration::from_secs(0));
    record_pipeline_stage(stage::DISCOVERY, "none", Duration::from_secs(0));
    counter!(BLOCKS_OBSERVED_TOTAL, LABEL_DECISION => "bootstrap").increment(0);
    counter!(SNAPSHOT_HALTS_TOTAL, LABEL_REASON => "resync_required").increment(0);
    counter!(SNAPSHOT_FORKS_TOTAL, LABEL_KIND => "wrong_parent").increment(0);
    gauge!(SNAPSHOT_GAP_BLOCKS).set(0.0);
    record_snapshot_status(&SnapshotStatus::Syncing);
    gauge!(SNAPSHOT_BLOCK_NUMBER).set(0.0);
    counter!(SNAPSHOT_PUBLICATIONS_TOTAL).increment(0);
    record_discovery_pools_loaded("none", 0);
    counter!(DISCOVERY_CYCLES_FOUND_TOTAL).increment(0);
    counter!(
        DISCOVERY_CANDIDATES_TOTAL,
        LABEL_PROTOCOL_MIX => "none",
    )
    .increment(0);
    counter!(DISCOVERY_REJECTED_TOTAL, LABEL_REASON => "no_optimum").increment(0);
    gauge!(
        DISCOVERY_BEST_NET_PROFIT_MNT,
        LABEL_PROTOCOL_MIX => "none",
    )
    .set(0.0);
    counter!(
        PREFLIGHT_ATTEMPTS_TOTAL,
        LABEL_OUTCOME => "pass",
        LABEL_POLICY_KEY => "mandatory",
        LABEL_BLOCK_TAG => "latest",
    )
    .increment(0);
    histogram!(PREFLIGHT_DURATION_SECONDS, LABEL_OUTCOME => "pass").record(0.0);
    counter!(INTENT_EVENTS_TOTAL, LABEL_EVENT => "reserved").increment(0);
    counter!(
        INTENT_FINALIZED_TOTAL,
        LABEL_KIND => "execute",
        LABEL_SUCCESS => "true",
    )
    .increment(0);
    counter!(
        INTENT_NEEDS_OPERATOR_TOTAL,
        LABEL_REASON => "deep_reorg_halt",
    )
    .increment(0);
    gauge!(INTENT_LIVE_COUNT).set(0.0);
    gauge!(INTENT_NEXT_NONCE).set(0.0);
    gauge!(GAS_BASE_FEE_WEI).set(0.0);
    counter!(GAS_PROFILE_QUOTE_TOTAL, LABEL_RESULT => "hit").increment(0);
    counter!(GAS_USED_TOTAL, LABEL_KIND => "execute").increment(0);
    counter!(GAS_COST_MNT_TOTAL, LABEL_KIND => "execute").increment(0);
    gauge!(SETTLEMENT_BALANCE_MNT, LABEL_HOLDER => "executor").set(0.0);
    gauge!(BREAKER_PAUSED).set(0.0);
    gauge!(BREAKER_CONSECUTIVE_REVERTS).set(0.0);
    gauge!(BREAKER_WINDOW_LOSS_MNT).set(0.0);
    gauge!(BREAKER_CHARGED_ENTRIES).set(0.0);
    counter!(BREAKER_ALERTS_TOTAL, LABEL_EVENT => "init").increment(0);
    counter!(RPC_RETRIES_TOTAL, LABEL_ERROR_CLASS => "http_429").increment(0);
    counter!(WATCH_REBASELINES_TOTAL, LABEL_KIND => "cold_start").increment(0);
    counter!(WATCH_REBASELINES_TOTAL, LABEL_KIND => "mid_run").increment(0);
    histogram!(HTTP_TIP_WAIT_DURATION_SECONDS).record(0.0);
    counter!(HTTP_TIP_TIMEOUTS_TOTAL).increment(0);
    counter!(WATCH_BLOCK_SKIPS_TOTAL, LABEL_REASON => "pinned_header_unavailable").increment(0);
    counter!(WATCH_SKIP_RATIO_WARNINGS_TOTAL).increment(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wei_to_mnt_f64_scales_by_1e18() {
        let one_mnt = U256::from(10u64).pow(U256::from(18u64));
        assert_eq!(wei_to_mnt_f64(one_mnt), 1.0);
    }
}
