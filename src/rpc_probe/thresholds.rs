//! Pass/fail thresholds for the RPC qualification probe (WHI-744).
//!
//! Every numeric gate lives here so tightening later is a one-file change and
//! the JSON report can print threshold-vs-measured side by side.

/// Probe schema / report version (independent of crate version).
pub const PROBE_VERSION: &str = "1.0.0";

/// Default number of consecutive blocks to sample for continuity / headers /
/// receipts. ≥128 so a short provider glitch is not masked by a lucky window.
pub const DEFAULT_BLOCKS: u64 = 128;

/// Default sustained WS subscription duration (seconds). A gate run needs hours;
/// 300s is the minimum useful stability sample (not a 30s smoke).
pub const DEFAULT_DURATION_SECS: u64 = 300;

/// Default recent-block window for multi-address `eth_getLogs` (Check A).
/// Large enough to exercise the filter, small enough for public endpoints.
pub const DEFAULT_LOGS_BLOCK_WINDOW: u64 = 8;

/// Mantle mainnet target block time (~2s). Used for stall detection.
pub const MANTLE_BLOCK_TIME_SECS: u64 = 2;

/// Silent-stall threshold: no new head past this many seconds fails Check E.
/// Justified as ~2× Mantle block time so one missed slot is tolerated, two is not.
pub const WS_STALL_THRESHOLD_SECS: u64 = MANTLE_BLOCK_TIME_SECS * 2;

/// Maximum allowed disconnects during the sustained WS window (Check E).
/// Zero: a production gate cannot afford reconnect storms on the tip feed.
pub const MAX_WS_DISCONNECTS: u64 = 0;

/// Maximum allowed silent stalls during the sustained WS window (Check E).
pub const MAX_WS_STALLS: u64 = 0;

/// Maximum allowed HTTP 429 responses across Check A attempts.
/// Any rate-limit response is disqualifying for a multi-protocol bot.
pub const MAX_HTTP_429: u64 = 0;

/// Maximum allowed HTTP 413 / response-too-large rejections on Check A.
pub const MAX_HTTP_413: u64 = 0;

/// Maximum allowed multi-address `eth_getLogs` failures (non-success outcomes).
/// The full merged address set must succeed in one shot — splitting is not a pass.
pub const MAX_MULTI_ADDRESS_LOG_FAILURES: u64 = 0;

/// Minimum fraction of sampled blocks that must yield complete headers (Check D).
/// 1.0: partial headers break snapshot identity in `state_space`.
pub const MIN_HEADER_COMPLETENESS_RATIO: f64 = 1.0;

/// Minimum fraction of sampled heights where HTTP and WS observe the same
/// `(number, hash, parent_hash)` (Check C cross-transport).
pub const MIN_HTTP_WS_HASH_AGREEMENT_RATIO: f64 = 1.0;

/// Maximum allowed continuity gaps (non-consecutive number or parent_hash break)
/// on either transport alone.
pub const MAX_CONTINUITY_GAPS: u64 = 0;

/// Check B must observe at least this many type-`0x7e` receipts in the sample.
/// Mantle deposit/system txs are ubiquitous; zero means the provider is omitting
/// them or the sample is not Mantle.
pub const MIN_TYPE_0X7E_RECEIPTS: u64 = 1;

/// Maximum allowed receipt decode / fetch failures (Check B).
pub const MAX_RECEIPT_FAILURES: u64 = 0;

/// When true, alloy Ethereum-typed `get_block_receipts` failures fail Check B.
///
/// Default **false**: Mantle deposit receipts use type `0x7e`, which Ethereum
/// `TxType` rejects on every provider. The bot readiness path already uses raw
/// `eth_getBlockReceipts` for that reason (`legacy_service_support`). Provider
/// qualification therefore gates on raw delivery + structural completeness;
/// typed failures remain measured. Flip to true only after the stack adopts
/// OP-stack receipt types for Mantle.
pub const REQUIRE_ALLOY_TYPED_RECEIPT_DECODE: bool = false;

/// Default address-set multiplier for Check A headroom probing.
pub const DEFAULT_ADDRESS_MULTIPLIER: f64 = 1.0;
