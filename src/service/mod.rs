//! Shared multi-protocol service scaffolding (WHI-727 / WHI-527.2).
//!
//! Fifth library module (alongside `amms`, `arbitrage`, `execution`,
//! `state_space` / `signing`). Additive surface extracted from the three
//! surviving `*_monitor_executor_service` examples. Existing example binaries
//! stay untouched and continue to compile against their local helpers;
//! WHI-527.3 wires this module into a single multi-protocol binary.

pub mod block_loop;
pub mod config;
pub mod error;
pub mod gas;
pub mod pool_universe;
pub mod protocol;
pub mod shadow_row;
pub mod startup;

pub use block_loop::{
    new_job_slot, require_matching_ready_tip, ExecutionJob, JobSlot, JOB_POLL_INTERVAL,
};
pub use config::{
    read_address_from_env, read_min_profit_threshold, resolve_http_endpoint, resolve_ws_endpoint,
    normalize_ws_endpoint, ServiceConfig, ServiceConfigOpts, DEFAULT_HTTP_MAINNET,
    DEFAULT_HTTP_SEPOLIA, DEFAULT_WMNT, DEFAULT_WS, MOE_MIN_PROFIT_FLOOR_WEI,
    V2_MIN_PROFIT_FLOOR_WEI, V3_MIN_PROFIT_FLOOR_WEI,
};
pub use error::{PoolUniverseSourceError, ProtocolError};
pub use gas::GasConfig;
pub use pool_universe::{CsvPoolUniverseSource, LoadedPoolUniverse, PoolUniverseSource};
pub use protocol::{
    AgniV2Protocol, AgniV3Protocol, Candidate, ExecutionAttempt, MoeProtocol, Protocol,
    ServiceExecutionContext,
};
pub use shadow_row::{
    CandidateLedgerRow, BEST_PATH_LOG_HEADERS, POSITIVE_PATH_LOG_HEADERS, V2_OPPORTUNITY_LOG_HEADERS,
};
pub use startup::{
    build_execution_runtime, build_execution_runtime_or_monitor_only, build_shadow_execution_context,
    production_send_allowed, resolve_shadow_ledger_path, shadow_ledger_path, shadow_mode_enabled,
};
