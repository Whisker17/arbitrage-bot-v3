//! Shared multi-protocol service scaffolding (WHI-727 / WHI-728).
//!
//! Fifth library module (alongside `amms`, `arbitrage`, `execution`,
//! `state_space` / `signing`). Additive surface extracted from the three
//! surviving `*_monitor_executor_service` examples. Existing example binaries
//! stay untouched and continue to compile against their local helpers;
//! WHI-527.3 / WHI-728 wires this module into a single multi-protocol binary.

pub mod block_loop;
pub mod config;
pub mod discovery;
pub mod error;
pub mod fixture;
pub mod gas;
pub mod pool_universe;
pub mod protocol;
pub mod select;
pub mod shadow_row;
pub mod startup;

pub use block_loop::{
    new_job_slot, require_matching_ready_tip, ExecutionJob, JobSlot, JOB_POLL_INTERVAL,
};
pub use config::{
    normalize_ws_endpoint, read_address_from_env, read_min_profit_threshold, resolve_http_endpoint,
    resolve_ws_endpoint, ServiceConfig, ServiceConfigOpts, DEFAULT_HTTP_MAINNET,
    DEFAULT_HTTP_SEPOLIA, DEFAULT_WMNT, DEFAULT_WS, MOE_MIN_PROFIT_FLOOR_WEI,
    V2_MIN_PROFIT_FLOOR_WEI, V3_MIN_PROFIT_FLOOR_WEI,
};
pub use discovery::{
    assert_signerless_invariant, attempt_discovered_via_job_slot, discover_for_protocols,
    discover_opportunities, factories_for_selection, path_is_cross_protocol,
    simulate_mixed_path_with_route_key, DiscoveryConfig, DiscoveredOpportunity,
};
pub use error::{PoolUniverseSourceError, ProtocolError};
pub use fixture::{
    cross_protocol_fixture_pools, fixture_settlement_asset, fixture_manual_roundtrip_profit,
};
pub use gas::{default_gas_safety_margin, gas_config_for_base_fee, GasConfig};
pub use pool_universe::{
    CsvPoolUniverseSource, LoadedPoolUniverse, MoeCsvPoolUniverseSource, PoolUniverseSource,
};
pub use protocol::{
    AgniV2Protocol, AgniV3Protocol, Candidate, ExecutionAttempt, MoeProtocol, Protocol,
    ServiceExecutionContext, V2_FEE, MOE_BINS_BATCH_SIZE, MOE_BINS_RADIUS,
};
pub use select::{
    filter_pools_by_protocols, parse_protocols_flag, protocol_kind_of_amm, SelectedProtocol,
};
pub use shadow_row::{
    collect_expected_states, format_roi_percent, hops_description, CandidateLedgerRow,
    GrossCandidate, PositiveCandidate, BEST_PATH_LOG_HEADERS, POSITIVE_PATH_LOG_HEADERS,
};
pub use startup::{
    build_execution_runtime, build_execution_runtime_or_monitor_only, build_shadow_execution_context,
    production_send_allowed, resolve_shadow_ledger_path, shadow_ledger_path, shadow_mode_enabled,
};
