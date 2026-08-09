//! Shared multi-protocol service scaffolding (WHI-727 / WHI-728).
//!
//! Fifth library module (alongside `amms`, `arbitrage`, `execution`,
//! `state_space` / `signing`). Additive surface extracted from the three
//! surviving `*_monitor_executor_service` examples. Existing example binaries
//! stay untouched and continue to compile against their local helpers;
//! WHI-527.3 / WHI-728 wires this module into a single multi-protocol binary.

pub mod arb_coverage;
pub mod block_loop;
pub mod block_summary;
pub mod capital_bound;
pub mod config;
pub mod discovery;
pub mod eligibility;
pub mod error;
pub mod fixture;
pub mod fee_scoring;
pub mod gas;
pub mod path_index;
pub mod pool_universe;
pub mod protocol;
pub mod rpc_provider;
pub mod select;
pub mod send_path;
pub mod shadow_row;
pub mod startup;
pub mod unified_universe;
pub mod universe_filter;
pub mod v3_venues;

pub use block_loop::{
    apply_gap_range_to_tip_refresh, backfill_gap, dirty_addresses_from_logs,
    load_canonical_header_with_wait, merged_gas_config, new_job_slot, poll_heads_http,
    process_observed_head, refresh_selected_tip_state, reorg_deeper_than_cache,
    require_matching_ready_tip, run_multi_protocol_watch_loop, subscribe_heads_once,
    tip_refresh_full_reason, tip_refresh_gap_log_range, tip_refresh_gap_size,
    tip_refresh_requires_full, tip_refresh_scope_for_head, union_tip_refresh_dirty,
    wait_for_shutdown_signal, BlockSkipReason, BlockTick, CanonicalHeaderLoad, ExecutionJob,
    HeadSource, HeadSubscription, JobSlot, NoopWatchHooks, ProcessHeadResult, RebaselineKind,
    SkipRatioTracker, TipRefreshFullReason, WatchLoopConfig, WatchLoopHooks, WatchLoopState,
    WatchLoopStats, DEFAULT_HTTP_POLL_INTERVAL, DEFAULT_HTTP_TIP_WAIT, DEFAULT_SKIP_FATAL_WINDOW,
    DEFAULT_SKIP_RATIO_THRESHOLD, DEFAULT_SKIP_RATIO_WINDOW, JOB_POLL_INTERVAL,
};
pub use config::{
    assert_expected_chain_id, assert_http_ws_chain_ids_agree, normalize_ws_endpoint,
    observe_and_assert_chain_id, read_address_from_env, read_min_profit_threshold,
    resolve_http_endpoint, resolve_ws_endpoint, ResolvedEndpoint, ServiceConfig, ServiceConfigOpts,
    DEFAULT_EXPECTED_CHAIN_ID, DEFAULT_HTTP_MAINNET, DEFAULT_HTTP_SEPOLIA, DEFAULT_WMNT, DEFAULT_WS,
    DEFAULT_WS_SEPOLIA, ENDPOINT_SOURCE_DEFAULT, MANTLE_MAINNET_CHAIN_ID, MANTLE_SEPOLIA_CHAIN_ID,
    MOE_MIN_PROFIT_FLOOR_WEI, V2_MIN_PROFIT_FLOOR_WEI, V3_MIN_PROFIT_FLOOR_WEI,
};
// Strategy hop cap lives in pathfinder; re-export so examples share one literal.
pub use crate::arbitrage::DEFAULT_MAX_HOPS;
pub use block_summary::{
    BlockSummary, BLOCK_SUMMARY_MESSAGE, BLOCK_SUMMARY_TARGET, MAX_INFO_LINES_PER_BLOCK,
};
pub use discovery::{
    assert_signerless_invariant, attempt_discovered_via_job_slot,
    attempt_discovered_via_job_slot_with_send, discover_for_protocols, discover_opportunities,
    discover_opportunities_with_scope, discover_pass, factories_for_selection,
    path_is_cross_protocol, simulate_mixed_path_with_route_key, validate_max_hops,
    walk_attempt_plan, AttemptIdentityContext, AttemptJobContext, AttemptWalkResult,
    DiscoveryConfig, DiscoveryPass, DiscoveryPassStats, DiscoveredOpportunity,
};
pub use capital_bound::{
    amount_survives_send_caps, apply_capital_domain_to_discovery, approved_canary_notional_from_env,
    approved_strategy_cap_from_env, pin_executor_balance_strategy_a, resolve_capital_domain,
    shadow_assumed_capital_from_env, CapitalDomain, CapitalEvidence, CapitalMode, CapitalPolicy,
    BALANCE_READ_STRATEGY, BALANCE_READ_STRATEGY_DESCRIPTION,
    DEFAULT_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI, ENV_APPROVED_CANARY_NOTIONAL_WMNT_WEI,
    ENV_APPROVED_STRATEGY_CAP_WMNT_WEI, ENV_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI,
};
pub use eligibility::{
    candidates_for_attempt, classify_opportunities, classify_with_send_runtime,
    evaluate_static_eligibility, next_attempt_decision, resolve_attempt_budget, AttemptBudget,
    AttemptSelectionOutcome, EligibilityBounds, EligibilityView, StaticEligibility,
    StaticIneligibility, DEFAULT_ATTEMPT_BUDGET, ENV_ATTEMPT_BUDGET_MS,
};
pub use path_index::{DiscoveryEngine, DiscoveryStats, PathIndex};
pub use error::{PoolUniverseSourceError, ProtocolError};
pub use fixture::{
    cross_protocol_fixture_pools, fixture_settlement_asset, fixture_manual_roundtrip_profit,
};
pub use fee_scoring::{
    discovery_fee_reject_reason, resolve_discovery_tip_fee_fields, DiscoveryFeeError,
    DiscoveryTipFeeError, DiscoveryTipFeeFields, MeasuredFeeScoring,
};
pub use gas::{default_gas_safety_margin, gas_config_for_base_fee, GasConfig};
pub use pool_universe::{
    assert_universe_freshness, enforce_freshness_if_present, enforce_universe_freshness,
    CsvPoolUniverseSource, LoadedPoolUniverse, MoeCsvPoolUniverseSource, PoolUniverseSource,
    DEFAULT_UNIVERSE_MAX_AGE_BLOCKS, REGENERATE_AGNI_POOL_LIST, REGENERATE_MOE_POOL_LIST,
    REGENERATE_V2_POOL_LIST,
};
pub use arb_coverage::{
    adapter_class, build_report, compute_coverage, coverage_path_for, dataset_label, format_report_text,
    greedy_rank, load_arbs_jsonl, load_census, load_held_pools_from_csv, normalize_address, pct,
    run_coverage_report, write_report, AdapterClass, ArbCoverageError, ArbCoverageReport, ArbPath,
    CoverageSummary, GreedyStep, ObservedArbCoverage, PoolCensusEntry,
    ARB_COVERAGE_REPORT_SCHEMA_VERSION, OBSERVED_ARB_COVERAGE_SCHEMA_VERSION,
};
pub use unified_universe::{
    build_meta, format_funnel_report, load_unified_meta, meta_path_for, protocol_label_to_pool_protocol,
    quarantine_path_for, read_unified_csv, selected_to_protocol_label, write_quarantine,
    write_unified_csv, write_unified_meta, FilterPolicyMeta, UnifiedCsvRow, UnifiedPoolUniverseSource,
    UnifiedUniverseMeta, ValuationMeta, DEFAULT_POOL_UNIVERSE_REL, REGENERATE_POOL_UNIVERSE,
    UNIFIED_SCHEMA_VERSION,
};
pub use universe_filter::{
    apply_universe_filters, count_by_protocol, default_max_hops, filter_settlement_cycles,
    pools_on_settlement_cycles, CandidatePool, FilterResult, FunnelCounts, QuarantineEntry,
    DEFAULT_MIN_TVL_WMNT_WEI, FILTER_POLICY_VERSION,
};
pub use v3_venues::{
    drop_in_v3_factories, drop_in_v3_funnel_counts, factory_for_seed_protocol_tag,
    format_v3_factory_funnel, is_quarantined_v3_factory, quarantine_reason_for_factory,
    quarantined_v3_by_factory, split_quarantined_v3_candidates, venue_by_factory, DropInV3Venue,
    QuarantinedV3Venue, AGNI_V3, BUTTER, CLEOPATRA_CL, DROP_IN_V3_VENUES, FLUXION_V3, FUSIONX_V3,
    QUARANTINED_V3_VENUES, UNISWAP_V3_MANTLE, V3FORK_636EA2, V3_UNIVERSE_PROTOCOL_LABEL,
};
pub use protocol::{
    plan_moe_tip_refresh, AgniV2Protocol, AgniV3Protocol, Candidate, ExecutionAttempt,
    MoeProtocol, MoeTipRefreshPlan, Protocol, ServiceExecutionContext, TipRefreshScope, V2_FEE,
    MOE_BINS_BATCH_SIZE, MOE_BINS_RADIUS,
};
pub use rpc_provider::{
    classify_retry_error, connect_http_provider, connect_ws_provider, is_mantle_transient,
    is_request_timeout_error, recommended_throttle_rps, timeout_error, ObservingRetryBackoffLayer,
    ObservingRetryPolicy, RequestTimeoutLayer, RpcProviderConfig, DEFAULT_HTTP_THROTTLE_RPS,
    DEFAULT_REQUEST_TIMEOUT_MS, DEFAULT_RETRY_CUPS, DEFAULT_RETRY_INITIAL_BACKOFF_MS,
    DEFAULT_RETRY_MAX, ENV_HTTP_THROTTLE_RPS, ENV_REQUEST_TIMEOUT_MS, ENV_RETRY_CUPS,
    ENV_RETRY_INITIAL_BACKOFF_MS, ENV_RETRY_MAX, THROTTLE_REF_POOL_COUNT, THROTTLE_REF_RPS,
};
// WHI-968: derivation helper next to throttle config. Call sites that need the
// process-global budget use `crate::rpc_pipeline` directly (amms / bins).
pub use crate::rpc_pipeline::pipelined_rpc_concurrency;
pub use select::{
    filter_pools_by_protocols, parse_protocols_flag, protocol_kind_of_amm, SelectedProtocol,
};
pub use shadow_row::{
    collect_expected_states, format_roi_percent, hops_description, CandidateLedgerRow,
    GrossCandidate, PositiveCandidate, BEST_PATH_LOG_HEADERS, POSITIVE_PATH_LOG_HEADERS,
};
pub use send_path::{
    arm_production_send_path, default_breaker_store, disarm_production_sends,
    enforce_inventory_caps, load_hot_executor_signer, sends_killed_env, sends_opt_in_requested,
    validate_send_preconditions, ArmSendPathRequest, ArmedSendRuntime, SendPathArmError,
    SendRuntime, ENV_ENABLE_SENDS, ENV_HOT_EXECUTOR_PRIVATE_KEY, ENV_SENDS_KILLED,
};
pub use startup::{
    build_execution_runtime, build_execution_runtime_or_monitor_only, build_shadow_execution_context,
    production_send_allowed, resolve_shadow_ledger_path, shadow_ledger_path, shadow_mode_enabled,
    validate_settlement_asset, validate_settlement_asset_config, MERGED_BOT_SHADOW_SERVICE,
};
