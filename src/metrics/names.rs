//! Metric name and label-key constants (WHI-532).
//!
//! Every series in the registry table has a `pub const` here. Label keys live
//! alongside so spelling cannot drift between emit sites.

// --- metric names ---

pub const BUILD_INFO: &str = "arbbot_build_info";
pub const BLOCK_TO_SUBMIT_DURATION_SECONDS: &str = "arbbot_block_to_submit_duration_seconds";
pub const PIPELINE_STAGE_DURATION_SECONDS: &str = "arbbot_pipeline_stage_duration_seconds";
pub const BLOCKS_OBSERVED_TOTAL: &str = "arbbot_blocks_observed_total";
pub const SNAPSHOT_HALTS_TOTAL: &str = "arbbot_snapshot_halts_total";
pub const SNAPSHOT_FORKS_TOTAL: &str = "arbbot_snapshot_forks_total";
pub const SNAPSHOT_GAP_BLOCKS: &str = "arbbot_snapshot_gap_blocks";
pub const SNAPSHOT_STATUS: &str = "arbbot_snapshot_status";
pub const SNAPSHOT_BLOCK_NUMBER: &str = "arbbot_snapshot_block_number";
pub const SNAPSHOT_PUBLICATIONS_TOTAL: &str = "arbbot_snapshot_publications_total";
pub const DISCOVERY_POOLS_LOADED: &str = "arbbot_discovery_pools_loaded";
pub const DISCOVERY_CYCLES_FOUND_TOTAL: &str = "arbbot_discovery_cycles_found_total";
pub const DISCOVERY_CANDIDATES_TOTAL: &str = "arbbot_discovery_candidates_total";
pub const DISCOVERY_REJECTED_TOTAL: &str = "arbbot_discovery_rejected_total";
pub const DISCOVERY_BEST_NET_PROFIT_MNT: &str = "arbbot_discovery_best_net_profit_mnt";
pub const PREFLIGHT_ATTEMPTS_TOTAL: &str = "arbbot_preflight_attempts_total";
pub const PREFLIGHT_DURATION_SECONDS: &str = "arbbot_preflight_duration_seconds";
pub const INTENT_EVENTS_TOTAL: &str = "arbbot_intent_events_total";
pub const INTENT_FINALIZED_TOTAL: &str = "arbbot_intent_finalized_total";
pub const INTENT_NEEDS_OPERATOR_TOTAL: &str = "arbbot_intent_needs_operator_total";
pub const INTENT_LIVE_COUNT: &str = "arbbot_intent_live_count";
pub const INTENT_NEXT_NONCE: &str = "arbbot_intent_next_nonce";
pub const GAS_BASE_FEE_WEI: &str = "arbbot_gas_base_fee_wei";
pub const GAS_PROFILE_QUOTE_TOTAL: &str = "arbbot_gas_profile_quote_total";
pub const GAS_USED_TOTAL: &str = "arbbot_gas_used_total";
pub const GAS_COST_MNT_TOTAL: &str = "arbbot_gas_cost_mnt_total";
pub const SETTLEMENT_BALANCE_MNT: &str = "arbbot_settlement_balance_mnt";
pub const BREAKER_PAUSED: &str = "arbbot_breaker_paused";
pub const BREAKER_CONSECUTIVE_REVERTS: &str = "arbbot_breaker_consecutive_reverts";
pub const BREAKER_WINDOW_LOSS_MNT: &str = "arbbot_breaker_window_loss_mnt";
pub const BREAKER_CHARGED_ENTRIES: &str = "arbbot_breaker_charged_entries";
pub const BREAKER_ALERTS_TOTAL: &str = "arbbot_breaker_alerts_total";

/// Every metric name constant — used by description completeness tests.
pub const ALL_METRIC_NAMES: &[&str] = &[
    BUILD_INFO,
    BLOCK_TO_SUBMIT_DURATION_SECONDS,
    PIPELINE_STAGE_DURATION_SECONDS,
    BLOCKS_OBSERVED_TOTAL,
    SNAPSHOT_HALTS_TOTAL,
    SNAPSHOT_FORKS_TOTAL,
    SNAPSHOT_GAP_BLOCKS,
    SNAPSHOT_STATUS,
    SNAPSHOT_BLOCK_NUMBER,
    SNAPSHOT_PUBLICATIONS_TOTAL,
    DISCOVERY_POOLS_LOADED,
    DISCOVERY_CYCLES_FOUND_TOTAL,
    DISCOVERY_CANDIDATES_TOTAL,
    DISCOVERY_REJECTED_TOTAL,
    DISCOVERY_BEST_NET_PROFIT_MNT,
    PREFLIGHT_ATTEMPTS_TOTAL,
    PREFLIGHT_DURATION_SECONDS,
    INTENT_EVENTS_TOTAL,
    INTENT_FINALIZED_TOTAL,
    INTENT_NEEDS_OPERATOR_TOTAL,
    INTENT_LIVE_COUNT,
    INTENT_NEXT_NONCE,
    GAS_BASE_FEE_WEI,
    GAS_PROFILE_QUOTE_TOTAL,
    GAS_USED_TOTAL,
    GAS_COST_MNT_TOTAL,
    SETTLEMENT_BALANCE_MNT,
    BREAKER_PAUSED,
    BREAKER_CONSECUTIVE_REVERTS,
    BREAKER_WINDOW_LOSS_MNT,
    BREAKER_CHARGED_ENTRIES,
    BREAKER_ALERTS_TOTAL,
];

// --- label keys ---

pub const LABEL_VERSION: &str = "version";
pub const LABEL_GIT_SHA: &str = "git_sha";
pub const LABEL_PROTOCOLS: &str = "protocols";
pub const LABEL_PRODUCTION_SEND_ALLOWED: &str = "production_send_allowed";
pub const LABEL_PROTOCOL: &str = "protocol";
pub const LABEL_OUTCOME: &str = "outcome";
pub const LABEL_STAGE: &str = "stage";
pub const LABEL_DECISION: &str = "decision";
pub const LABEL_REASON: &str = "reason";
pub const LABEL_KIND: &str = "kind";
pub const LABEL_STATUS: &str = "status";
pub const LABEL_PROTOCOL_MIX: &str = "protocol_mix";
pub const LABEL_POLICY_KEY: &str = "policy_key";
pub const LABEL_BLOCK_TAG: &str = "block_tag";
pub const LABEL_EVENT: &str = "event";
pub const LABEL_SUCCESS: &str = "success";
pub const LABEL_RESULT: &str = "result";
pub const LABEL_HOLDER: &str = "holder";
