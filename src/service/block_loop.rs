//! Shared block-subscription job-slot / multi-protocol watch loop (WHI-727 / WHI-741).
//!
//! Job-slot primitives (latest-wins) remain the execution handoff. WHI-741 adds the
//! continuous multi-protocol subscribe/worker path: **one** block subscription drives
//! all selected protocols against a single shared [`StateSpace`].
//!
//! ## Block-hash pin invariant (WHI-762)
//!
//! For every head iteration the **announced block hash** is authoritative for the
//! whole block. No per-block read may widen the pin to `latest`, a bare block
//! number, or a neighbouring height:
//!
//! * header / base-fee load → `eth_getBlockByHash(announced)`
//! * log fetch for **state mutation** → hash filter only (no number-range fallback)
//! * Moe tip refresh → `BlockId::hash_canonical(announced)`
//!
//! When the HTTP node cannot serve that hash (lag, prune, wrong fork), the loop
//! **skips** the block — it never substitutes state from another identity. Skip
//! counters + a rolling skip-ratio warning make transport skew loud.
//!
//! ## Gap-aware Moe dirty set (WHI-893)
//!
//! On a small numeric gap (`gap ≤ CACHE_SIZE`; Backfill implies `gap ≥ 2`) the
//! loop may issue **one** number-range `eth_getLogs` over `previous+1 ..= head`
//! using the shared block filter. Those range logs are used **only** to widen
//! the tip-refresh dirty set (which pools need a hash-pinned bin re-read). They
//! are **never** passed to [`StateSpace::sync`] and never seed quotes. A
//! reorged or stale range log can therefore cause an unnecessary refresh, never
//! a wrong quote. Full tip refresh remains reserved for cold start, gaps larger
//! than `CACHE_SIZE`, and a failed range-log fetch (fail safe, not fail open).
//!
//! ## Reorg policy
//!
//! Log application goes through [`StateSpace::sync`], which uses the
//! [`StateChangeCache`] ring buffer (`CACHE_SIZE = 30`) for shallow rollbacks.
//! Continuity classification uses [`SnapshotPublisher::observe_head`].
//!
//! **Numeric gaps** (head number jumps ahead of the last published tip) are **not**
//! reorgs: nothing needs unwinding. Small gaps (`<= CACHE_SIZE`) apply the observed
//! tip only for state mutation, and widen the Moe tip-refresh dirty set via one
//! range `eth_getLogs` (WHI-893). Large gaps **re-baseline** at the observed tip
//! (publish a fresh snapshot so `previous` advances) — cold-start latency past
//! the cache window is the common case (WHI-792). Mid-run large gaps get louder
//! logging but the same re-baseline, so the loop never deadlocks on a stuck baseline.
//!
//! **True reorgs** (fork / height rollback / wrong parent / same-height
//! replacement) still Halt via the publisher. A reorg deeper than the cache
//! refuses `StateSpace::sync` and skips; full deep-reorg unwinding is owned by
//! WHI-533 and is deliberately out of scope here.
//!
//! ## Head assembly vs `StateSpaceManager::subscribe`
//!
//! Live `--watch` defaults to dual providers (WS heads + HTTP state) matching the
//! three example services, because Mantle WS endpoints often whitelist only
//! `eth_subscribe`. That means this module intentionally does **not** call
//! [`crate::state_space::StateSpaceManager::subscribe`] (single-provider
//! assemble/backfill). When HTTP lags the WS tip, the loop waits up to
//! [`WatchLoopConfig::http_tip_wait`] before skipping (WHI-792 / WHI-762).
//! Optional `--head-source http-poll` drives heads from the same HTTP transport
//! so a dry run never depends on WS/HTTP tip agreement.

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::execution::LatestWinsSlot;
use crate::service::block_summary::BlockSummary;
use crate::service::discovery::{
    walk_attempt_plan, AttemptIdentityContext, AttemptJobContext, DiscoveryConfig,
    DiscoveryPassStats, DiscoveredOpportunity,
};
use crate::service::eligibility::{classify_with_send_runtime, DEFAULT_ATTEMPT_BUDGET};
use crate::service::path_index::DiscoveryEngine;
use crate::service::gas::GasConfig;
use crate::service::startup::production_send_allowed;
use crate::service::protocol::{
    AgniV2Protocol, AgniV3Protocol, ExecutionAttempt, MoeProtocol, Protocol, TipRefreshScope,
};
use crate::service::select::SelectedProtocol;
use crate::state_space::{
    hash_pinned_logs_filter, AssembleKind, BlockHeaderContext, HeadObservation, MarketSnapshot,
    ObservedHead, ProtocolCoverage, SnapshotId, SnapshotPublisher, SnapshotStatus, StateSpace,
    CACHE_SIZE,
};
use alloy::consensus::BlockHeader;
use alloy::eips::BlockNumberOrTag;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::primitives::{Address, B256};
use alloy::providers::{DynProvider, Provider};
use alloy::rpc::types::{Filter, Log};
use eyre::{eyre, Context, Result};
use futures::Stream;
use futures::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Poll interval used by the v3/moe execution workers when the slot is empty.
pub const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Default HTTP tip catch-up wait (WHI-792). Well under Mantle ~2s block time.
pub const DEFAULT_HTTP_TIP_WAIT: Duration = Duration::from_millis(800);

/// Poll interval while waiting for HTTP to observe a WS tip.
const HTTP_TIP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Consecutive skips that abort the watch loop as unhealthy (WHI-792).
pub const DEFAULT_SKIP_FATAL_WINDOW: u64 = 16;

/// Rolling window size (heads) for the skip-ratio warning (WHI-762).
pub const DEFAULT_SKIP_RATIO_WINDOW: usize = 32;

/// Skip-ratio threshold that triggers a warn (WHI-762). `0.5` = half the window.
pub const DEFAULT_SKIP_RATIO_THRESHOLD: f64 = 0.5;

/// Default poll interval for [`poll_heads_http`] (signerless dry-run latency is irrelevant).
pub const DEFAULT_HTTP_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why a head was skipped without emitting candidates (WHI-762 / WHI-792).
///
/// Metric label via [`BlockSkipReason::as_metric_label`] — keep names stable for
/// WHI-532 export (`arbbot_watch_block_skips_total{reason=…}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSkipReason {
    /// Same hash already processed.
    Duplicate,
    /// Continuity classifier halted (fork / reorg surface).
    ContinuityHalt,
    /// Unwind would exceed [`CACHE_SIZE`].
    DeepReorg,
    /// HTTP cannot serve the announced block hash within the wait deadline.
    PinnedHeaderUnavailable,
    /// Hard RPC error while loading the hash-pinned header.
    PinnedHeaderRpcError,
    /// Hash-pinned `eth_getLogs` failed (no number-range fallback).
    PinnedLogsUnavailable,
    /// Per-protocol tip refresh failed for the pinned hash.
    TipRefreshFailed,
    /// Other processing error after the head was accepted for assembly.
    ProcessingFailed,
}

impl BlockSkipReason {
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::ContinuityHalt => "continuity_halt",
            Self::DeepReorg => "deep_reorg",
            Self::PinnedHeaderUnavailable => "pinned_header_unavailable",
            Self::PinnedHeaderRpcError => "pinned_header_rpc_error",
            Self::PinnedLogsUnavailable => "pinned_logs_unavailable",
            Self::TipRefreshFailed => "tip_refresh_failed",
            Self::ProcessingFailed => "processing_failed",
        }
    }

    /// True when the skip is caused by the HTTP node not serving the announced hash.
    pub const fn is_pin_failure(self) -> bool {
        matches!(
            self,
            Self::PinnedHeaderUnavailable
                | Self::PinnedHeaderRpcError
                | Self::PinnedLogsUnavailable
                | Self::TipRefreshFailed
        )
    }
}

/// Latest-wins job slot shared between the block loop and the execution worker.
pub type JobSlot<T> = Arc<LatestWinsSlot<T>>;

/// Create an empty latest-wins job slot.
pub fn new_job_slot<T>() -> JobSlot<T> {
    Arc::new(LatestWinsSlot::new())
}

/// Protocol-agnostic execution job envelope (v3/moe shape).
///
/// `C` is the protocol's candidate type (typically [`super::protocol::Candidate`]).
#[derive(Clone, Debug)]
pub struct ExecutionJob<C> {
    pub candidate: C,
    pub block_number: u64,
    pub header: crate::state_space::BlockHeaderContext,
    pub pool_universe_fingerprint: B256,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
    /// When the originating head was first observed (WHI-532 block-to-submit timer).
    pub observed_at: std::time::Instant,
}

/// Latency-stage names aligned with WHI-537's block-to-submit taxonomy.
///
/// Structured-log stage labels for the watch loop. Prometheus stage histograms
/// use the separate `metrics::stage::*` vocabulary (discovery/optimize/preflight/
/// balance_read).
pub mod stages {
    pub const BLOCK_OBSERVED: &str = "block_observed";
    pub const STATE_APPLIED: &str = "state_applied";
    pub const TIP_REFRESHED: &str = "tip_refreshed";
    /// Strategy-A executor WMNT balance pin (WHI-950 / G-3).
    pub const BALANCE_READ: &str = "balance_read";
    pub const DISCOVERY: &str = "discovery";
    pub const JOB_PUBLISHED: &str = "job_published";
    pub const EXECUTION_ATTEMPT: &str = "execution_attempt";
}

/// Why a large backfill gap triggered a re-baseline (WHI-792).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebaselineKind {
    /// First processed head of the run; startup latency past the cache window.
    ColdStart,
    /// After at least one successful process; possible stalled feed / discontinuity.
    MidRun,
}

impl RebaselineKind {
    pub const fn as_metric_label(self) -> &'static str {
        match self {
            Self::ColdStart => "cold_start",
            Self::MidRun => "mid_run",
        }
    }
}

/// Cumulative counters for a completed (or shut-down) watch run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchLoopStats {
    pub blocks_processed: u64,
    pub opportunities_found: u64,
    pub attempts: u64,
    /// Always `1` when started via [`run_multi_protocol_watch_loop`] — one shared
    /// subscription serves every selected protocol (WHI-741 AC).
    pub block_subscriptions: u64,
    pub halted_or_skipped: u64,
    /// Heads delivered by the subscription (including skipped ones).
    pub heads_observed: u64,
    /// Large-gap re-baselines at cold start (WHI-792).
    pub cold_start_rebaselines: u64,
    /// Large-gap re-baselines after at least one processed block (WHI-792).
    pub mid_run_rebaselines: u64,
    /// Times the loop waited for HTTP to catch a WS tip (success or timeout).
    pub http_tip_waits: u64,
    /// WS tips still unobserved by HTTP after the wait deadline.
    pub http_tip_timeouts: u64,
    /// Skips caused by the HTTP node not serving the announced hash (WHI-762).
    pub pin_skips: u64,
    /// Times the rolling skip-ratio threshold fired (WHI-762).
    pub skip_ratio_warnings: u64,
}

/// Result of applying one head through [`process_observed_head`].
#[derive(Debug)]
pub struct ProcessHeadResult {
    pub tick: Option<BlockTick>,
    pub rebaseline: Option<RebaselineKind>,
    /// Set when `tick` is `None` — why this head produced no candidates.
    pub skip_reason: Option<BlockSkipReason>,
}

impl ProcessHeadResult {
    fn skipped(reason: BlockSkipReason) -> Self {
        Self {
            tick: None,
            rebaseline: None,
            skip_reason: Some(reason),
        }
    }

    fn processed(tick: BlockTick) -> Self {
        Self {
            tick: Some(tick),
            rebaseline: None,
            skip_reason: None,
        }
    }

    fn rebaselined(kind: RebaselineKind, tick: BlockTick) -> Self {
        Self {
            tick: Some(tick),
            rebaseline: Some(kind),
            skip_reason: None,
        }
    }
}

/// One successfully processed block tick.
#[derive(Debug, Clone)]
pub struct BlockTick {
    pub block_number: u64,
    pub snapshot_id: SnapshotId,
    pub header: BlockHeaderContext,
    pub base_fee_per_gas: Option<u64>,
    pub affected_pools: usize,
    pub opportunities: Vec<DiscoveredOpportunity>,
    pub attempts: Vec<(DiscoveredOpportunity, ExecutionAttempt)>,
}

/// Shared state handles the multi-protocol watch loop mutates.
#[derive(Clone)]
pub struct WatchLoopState {
    pub state: Arc<RwLock<StateSpace>>,
    pub latest_block: Arc<AtomicU64>,
    pub snapshots: SnapshotPublisher,
    pub block_filter: Filter,
    pub chain_id: u64,
    /// Incremental path index + gross-quote cache (WHI-940). Built lazily on
    /// the first discovery pass; topology rebuilds only if the pool address set
    /// or discovery hop/settlement knobs change.
    pub discovery_engine: Arc<std::sync::Mutex<Option<DiscoveryEngine>>>,
}

impl WatchLoopState {
    /// Construct loop state with an empty discovery engine (built on first use).
    pub fn new(
        state: Arc<RwLock<StateSpace>>,
        latest_block: Arc<AtomicU64>,
        snapshots: SnapshotPublisher,
        block_filter: Filter,
        chain_id: u64,
    ) -> Self {
        Self {
            state,
            latest_block,
            snapshots,
            block_filter,
            chain_id,
            discovery_engine: Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

/// Knobs for continuous multi-protocol discovery across blocks.
#[derive(Clone)]
pub struct WatchLoopConfig {
    pub discovery: DiscoveryConfig,
    pub selected: Vec<SelectedProtocol>,
    /// When true, run the WHI-951 attempt plan each block (gate-closed: top-1;
    /// armed: eligible pure candidates under `attempt_budget`).
    pub attempt_execution: bool,
    /// When true (default for live `--watch`), dispatch
    /// [`Protocol::refresh_block_tip_state`] after log application. Offline/mock
    /// multi-block tests set this false so Moe tip-sync RPC is not required.
    pub refresh_tip_state: bool,
    /// Max time to wait for HTTP to serve a block the WS tip already announced
    /// (WHI-792). Default is well under one Mantle block time.
    pub http_tip_wait: Duration,
    /// Consecutive-skip threshold (WHI-792).
    ///
    /// - While `blocks_processed == 0`: reaching this count aborts with `Err`
    ///   (refuse silent success on a dead loop).
    /// - After any successful process: reaching this count emits a repeated
    ///   `error!` on each further skip (does not abort — mid-run recovery may
    ///   still re-baseline). Zero disables the threshold entirely; stream-end
    ///   with heads but zero processed still fails closed.
    pub skip_fatal_window: u64,
    /// Rolling window (heads) for the skip-ratio warning (WHI-762). Zero disables.
    pub skip_ratio_window: usize,
    /// Fire a warn when `skips / window > threshold` over the full window (WHI-762).
    pub skip_ratio_threshold: f64,
    /// Armed send runtime (WHI-860). `None` keeps the historical gate-blocked path.
    pub send_runtime: Option<std::sync::Arc<crate::service::send_path::SendRuntime>>,
    /// Measured gas profile for discovery ranking (WHI-949).
    ///
    /// When set (typically cloned from the send runtime or loaded at startup),
    /// discovery uses [`FeePolicy::build`] / `fee_plan_cost` instead of the
    /// offline hop table. `None` keeps offline-fixture GasConfig screening.
    pub discovery_gas_profile:
        Option<std::sync::Arc<crate::execution::RuntimeGasProfile>>,
    /// Priority fee used with [`Self::discovery_gas_profile`] (wei).
    pub discovery_priority_fee_wei: u128,
    /// Block gas reserve used with [`Self::discovery_gas_profile`].
    pub discovery_block_gas_reserve: u64,
    /// Pool-universe fingerprint stamped onto send identity (from frozen universe load).
    pub pool_universe_fingerprint: alloy::primitives::B256,
    /// Wall-clock budget for dynamic candidate attempts within one block (WHI-951).
    ///
    /// Static ineligibility is filtered before this budget is spent. On dynamic
    /// preflight failure the next eligible candidate may be tried until the
    /// budget expires; a successful broadcast stops immediately.
    pub attempt_budget: Duration,
    /// Optimal-input capital policy (WHI-950 / G-3).
    ///
    /// * `None` — historical path: use [`DiscoveryConfig::max_input`] unchanged
    ///   (offline fixtures / unrestricted observation).
    /// * `Some(Shadow)` — assumed capital; never reads chain balance.
    /// * `Some(Production|Canary)` — strategy A: one hash-pinned balance per
    ///   head via [`SendRuntime`], domain applied before discovery.
    pub capital_policy: Option<crate::service::capital_bound::CapitalPolicy>,
}

impl std::fmt::Debug for WatchLoopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchLoopConfig")
            .field("discovery", &self.discovery)
            .field("selected", &self.selected)
            .field("attempt_execution", &self.attempt_execution)
            .field("refresh_tip_state", &self.refresh_tip_state)
            .field("http_tip_wait", &self.http_tip_wait)
            .field("skip_fatal_window", &self.skip_fatal_window)
            .field("skip_ratio_window", &self.skip_ratio_window)
            .field("skip_ratio_threshold", &self.skip_ratio_threshold)
            .field("send_runtime", &self.send_runtime.as_ref().map(|_| "Some(..)"))
            .field(
                "discovery_gas_profile",
                &self.discovery_gas_profile.as_ref().map(|_| "Some(..)"),
            )
            .field("discovery_priority_fee_wei", &self.discovery_priority_fee_wei)
            .field(
                "discovery_block_gas_reserve",
                &self.discovery_block_gas_reserve,
            )
            .field("pool_universe_fingerprint", &self.pool_universe_fingerprint)
            .field("attempt_budget", &self.attempt_budget)
            .field(
                "capital_policy",
                &self.capital_policy.as_ref().map(|p| p.mode.as_str()),
            )
            .finish()
    }
}

impl WatchLoopConfig {
    /// Offline / test defaults: no tip refresh, short HTTP wait, fatal skip window on.
    pub fn offline(discovery: DiscoveryConfig, selected: Vec<SelectedProtocol>) -> Self {
        use crate::execution::ExecutorConfig;
        let exec_defaults = ExecutorConfig::default();
        Self {
            discovery,
            selected,
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: exec_defaults.default_priority_fee_wei,
            discovery_block_gas_reserve: exec_defaults.block_gas_limit_reserve,
            pool_universe_fingerprint: alloy::primitives::B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        }
    }
}

/// Rolling skip-ratio tracker (WHI-762).
///
/// Records process/skip outcomes; when the window is full and
/// `skips / window > threshold`, [`SkipRatioTracker::record`] returns `true`
/// so the loop can emit a loud warning.
#[derive(Debug, Clone)]
pub struct SkipRatioTracker {
    window: usize,
    threshold: f64,
    outcomes: VecDeque<bool>,
}

impl SkipRatioTracker {
    pub fn new(window: usize, threshold: f64) -> Self {
        Self {
            window,
            threshold,
            outcomes: VecDeque::with_capacity(window.max(1)),
        }
    }

    pub fn from_config(config: &WatchLoopConfig) -> Self {
        Self::new(config.skip_ratio_window, config.skip_ratio_threshold)
    }

    /// Record whether this head was skipped. Returns `true` when the ratio
    /// exceeds the threshold over a full window.
    pub fn record(&mut self, skipped: bool) -> bool {
        if self.window == 0 {
            return false;
        }
        self.outcomes.push_back(skipped);
        while self.outcomes.len() > self.window {
            self.outcomes.pop_front();
        }
        if self.outcomes.len() < self.window {
            return false;
        }
        let skips = self.outcomes.iter().filter(|s| **s).count();
        (skips as f64) / (self.window as f64) > self.threshold
    }

    /// Current skip count in the window (for tests).
    pub fn skip_count(&self) -> usize {
        self.outcomes.iter().filter(|s| **s).count()
    }

    pub fn len(&self) -> usize {
        self.outcomes.len()
    }
}

/// Hooks so the binary can record shadow-ledger rows without coupling this module
/// to `ShadowExecutionContext`.
pub trait WatchLoopHooks: Send {
    fn on_block_ready(&mut self, tick: &BlockTick) -> Result<()>;
    fn on_attempt(
        &mut self,
        opp: &DiscoveredOpportunity,
        attempt: &ExecutionAttempt,
    ) -> Result<()>;
}

/// No-op hooks for monitor-only / test runs.
#[derive(Debug, Default)]
pub struct NoopWatchHooks;

impl WatchLoopHooks for NoopWatchHooks {
    fn on_block_ready(&mut self, _tick: &BlockTick) -> Result<()> {
        Ok(())
    }
    fn on_attempt(
        &mut self,
        _opp: &DiscoveredOpportunity,
        _attempt: &ExecutionAttempt,
    ) -> Result<()> {
        Ok(())
    }
}

/// Reject queued work unless the live tip is still Ready at the candidate SnapshotId.
///
/// Relocated from `examples/protocols/intent_service_support.rs`.
pub fn require_matching_ready_tip(
    tip: Option<SnapshotStatus>,
    candidate_id: SnapshotId,
) -> Result<SnapshotStatus> {
    match tip {
        Some(SnapshotStatus::Ready(snapshot)) if snapshot.id == candidate_id => {
            Ok(SnapshotStatus::Ready(snapshot))
        }
        Some(SnapshotStatus::Ready(snapshot)) => {
            record_stale_tip_metric();
            Err(eyre!(
                "stale queued opportunity: candidate {:?} != live tip {:?}",
                candidate_id,
                snapshot.id
            ))
        }
        Some(_) | None => {
            record_stale_tip_metric();
            Err(eyre!("execution gate has no live Ready snapshot tip"))
        }
    }
}

fn record_stale_tip_metric() {
    crate::metrics::record_block_to_submit(
        "unknown",
        crate::metrics::block_outcome::STALE_TIP,
        std::time::Duration::ZERO,
    );
}

/// Merge screening gas across selected protocols via [`Protocol::refresh_gas_config`].
///
/// Prefer a live-fee tracker (Agni-V3 / Moe) when selected — their hooks bind the
/// block base fee. V2 freezes [`GasConfig::default`] and is only used when no
/// live-tracking protocol is in the selection (a raw max would pick the 25 gwei
/// default over a smaller live base fee).
pub fn merged_gas_config(selected: &[SelectedProtocol], base_fee_per_gas: Option<u64>) -> GasConfig {
    let mut live: Option<GasConfig> = None;
    let mut v2_fallback = GasConfig::default();
    for proto in selected {
        match proto {
            SelectedProtocol::AgniV2 => {
                v2_fallback =
                    AgniV2Protocol::new(Address::ZERO).refresh_gas_config(base_fee_per_gas);
            }
            SelectedProtocol::AgniV3 => {
                live = Some(
                    AgniV3Protocol::new(Address::ZERO).refresh_gas_config(base_fee_per_gas),
                );
            }
            SelectedProtocol::Moe => {
                // Moe and V3 share the live-fee shape; last live tracker wins.
                live = Some(MoeProtocol::new().refresh_gas_config(base_fee_per_gas));
            }
        }
    }
    live.unwrap_or(v2_fallback)
}

/// Per-protocol tip refresh dispatch ([`Protocol::refresh_block_tip_state`]).
///
/// Mutates `pools` in place. Moe re-syncs LB bin snapshots for pools selected
/// by `scope` (WHI-885); V2/V3 no-op by default.
pub async fn refresh_selected_tip_state(
    provider: &DynProvider,
    pools: &mut [AMM],
    selected: &[SelectedProtocol],
    block_hash: B256,
    header: &BlockHeaderContext,
    scope: &TipRefreshScope,
) -> Result<()> {
    for proto in selected {
        match proto {
            SelectedProtocol::AgniV2 => {
                AgniV2Protocol::new(Address::ZERO)
                    .refresh_block_tip_state(provider, pools, block_hash, header, scope)
                    .await
                    .map_err(|e| eyre!("agni-v2 tip refresh: {e}"))?;
            }
            SelectedProtocol::AgniV3 => {
                AgniV3Protocol::new(Address::ZERO)
                    .refresh_block_tip_state(provider, pools, block_hash, header, scope)
                    .await
                    .map_err(|e| eyre!("agni-v3 tip refresh: {e}"))?;
            }
            SelectedProtocol::Moe => {
                MoeProtocol::new()
                    .refresh_block_tip_state(provider, pools, block_hash, header, scope)
                    .await
                    .map_err(|e| eyre!("moe tip refresh: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Why a Full Moe tip refresh was selected (WHI-893).
///
/// Used for operator logs — `mode="full" affected=0` alone did not explain the
/// self-reinforcing gap loop diagnosed after WHI-885.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipRefreshFullReason {
    /// No block has been successfully processed yet this run.
    ColdStart,
    /// First tip after publisher bootstrap (no previous tip).
    Bootstrap,
    /// Numeric gap larger than [`CACHE_SIZE`] (re-baseline territory).
    LargeGap,
    /// Small-gap range `eth_getLogs` failed; fail safe to Full.
    RangeLogsFailed,
    /// Halted / duplicate paths that return before tip refresh (defensive).
    NotReached,
}

impl TipRefreshFullReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ColdStart => "cold_start",
            Self::Bootstrap => "bootstrap",
            Self::LargeGap => "large_gap",
            Self::RangeLogsFailed => "range_logs_failed",
            Self::NotReached => "not_reached",
        }
    }
}

/// Decide tip-refresh scope for this head (WHI-885 / WHI-893).
///
/// * **Full** when `force_full` is set (cold start, bootstrap, large gap, or
///   failed gap-range log fetch).
/// * **Touched** otherwise — dirty set is the union of head-applied addresses
///   and (on small gaps) addresses seen in the gap-range log query.
///
/// Range logs never enter this function as state mutations; callers only pass
/// addresses extracted from them.
pub fn tip_refresh_scope_for_head(
    force_full: bool,
    dirty_addresses: &[Address],
) -> TipRefreshScope {
    if force_full {
        TipRefreshScope::Full
    } else {
        TipRefreshScope::Touched(dirty_addresses.iter().copied().collect::<HashSet<_>>())
    }
}

/// Numeric gap for a Backfill observation (`head − previous`), or `None`.
pub fn backfill_gap(observation: &HeadObservation) -> Option<u64> {
    match observation {
        HeadObservation::Backfill {
            previous,
            observed,
        } => Some(
            observed
                .number
                .saturating_sub(previous.id.block_number),
        ),
        _ => None,
    }
}

/// Pure policy: whether this observation forces Full **before** any range-log
/// attempt (WHI-893).
///
/// Small gaps (`≤ CACHE_SIZE`) no longer force Full: the caller widens the
/// dirty set via a range `eth_getLogs` and uses [`TipRefreshScope::Touched`].
/// Full is reserved for cold start, bootstrap, large gaps, and (separately)
/// a failed range-log fetch.
pub fn tip_refresh_requires_full(
    observation: &HeadObservation,
    had_processed_block: bool,
) -> bool {
    tip_refresh_full_reason(observation, had_processed_block).is_some()
}

/// Reason Full is forced by observation alone, or `None` when Touched is
/// allowed (subject to a successful range-log fetch on small gaps).
pub fn tip_refresh_full_reason(
    observation: &HeadObservation,
    had_processed_block: bool,
) -> Option<TipRefreshFullReason> {
    if !had_processed_block {
        return Some(TipRefreshFullReason::ColdStart);
    }
    match observation {
        HeadObservation::Assemble(AssembleKind::Advance) => None,
        HeadObservation::Assemble(AssembleKind::Bootstrap) => {
            Some(TipRefreshFullReason::Bootstrap)
        }
        HeadObservation::Backfill { .. } => {
            let gap = backfill_gap(observation).unwrap_or(0);
            if gap > CACHE_SIZE as u64 {
                Some(TipRefreshFullReason::LargeGap)
            } else {
                // Small gap: dirty set will be widened from range logs.
                None
            }
        }
        // Callers return before tip refresh on these.
        HeadObservation::Duplicate | HeadObservation::Halted(_) => {
            Some(TipRefreshFullReason::NotReached)
        }
    }
}

/// Inclusive `(from, to)` block numbers for a gap-range dirty-set log query,
/// or `None` when range logs are not needed (advance, cold start, large gap).
///
/// Range is `previous+1 ..= head` and only for mid-run small gaps
/// (`gap ≤ CACHE_SIZE`).
pub fn tip_refresh_gap_log_range(
    observation: &HeadObservation,
    had_processed_block: bool,
) -> Option<(u64, u64)> {
    if !had_processed_block {
        return None;
    }
    match observation {
        HeadObservation::Backfill {
            previous,
            observed,
        } => {
            let gap = backfill_gap(observation).unwrap_or(0);
            if gap == 0 || gap > CACHE_SIZE as u64 {
                return None;
            }
            let from = previous.id.block_number.saturating_add(1);
            let to = observed.number;
            if from > to {
                return None;
            }
            Some((from, to))
        }
        _ => None,
    }
}

/// Numeric gap size for Backfill observations (for logging).
pub fn tip_refresh_gap_size(observation: &HeadObservation) -> Option<u64> {
    backfill_gap(observation)
}

/// Apply a gap-range log fetch result to tip-refresh policy (WHI-893 pure seam).
///
/// * `Ok(addrs)` → union into dirty set; keep Touched unless already forced Full.
/// * `Err(())` → force Full with [`TipRefreshFullReason::RangeLogsFailed`].
/// * `None` → no range fetch was needed (advance / already Full).
pub fn apply_gap_range_to_tip_refresh(
    force_full: bool,
    full_reason: Option<TipRefreshFullReason>,
    head_affected: &[Address],
    range_result: Option<Result<Vec<Address>, ()>>,
) -> (bool, Option<TipRefreshFullReason>, Vec<Address>) {
    match range_result {
        None => (force_full, full_reason, head_affected.to_vec()),
        Some(Ok(gap_addrs)) => {
            let dirty = union_tip_refresh_dirty(head_affected, &gap_addrs);
            (force_full, full_reason, dirty)
        }
        Some(Err(())) => (
            true,
            Some(TipRefreshFullReason::RangeLogsFailed),
            head_affected.to_vec(),
        ),
    }
}

/// Collect unique pool addresses from logs for the tip-refresh dirty set.
///
/// Used for both head-applied addresses and gap-range log addresses. Does not
/// mutate state — addresses only.
pub fn dirty_addresses_from_logs(logs: &[Log]) -> Vec<Address> {
    let mut out = Vec::with_capacity(logs.len());
    let mut seen = HashSet::new();
    for log in logs {
        let addr = log.address();
        if seen.insert(addr) {
            out.push(addr);
        }
    }
    out
}

/// Union head-affected addresses with gap-range log addresses (WHI-893).
///
/// Order is stable: head-affected first, then novel addresses from range logs.
pub fn union_tip_refresh_dirty(
    head_affected: &[Address],
    gap_range_addresses: &[Address],
) -> Vec<Address> {
    let mut out = Vec::with_capacity(head_affected.len() + gap_range_addresses.len());
    let mut seen = HashSet::new();
    for addr in head_affected.iter().chain(gap_range_addresses.iter()) {
        if seen.insert(*addr) {
            out.push(*addr);
        }
    }
    out
}

/// True when unwinding to `block_number` would fall outside the
/// [`CACHE_SIZE`] ring buffer (deep reorg / WHI-533 territory).
pub fn reorg_deeper_than_cache(latest_applied: u64, block_number: u64) -> bool {
    if block_number > latest_applied {
        return false;
    }
    // StateChangeCache panics when block_to_unwind < oldest_block. Oldest is at
    // most `latest - CACHE_SIZE + 1` once the ring is full; refuse earlier.
    let min_safe = latest_applied.saturating_sub(CACHE_SIZE as u64 - 1);
    block_number < min_safe
}

/// Apply one observed head to the shared multi-protocol state and run discovery.
///
/// Callers supply a **single** head stream (one subscription). This function never
/// opens a subscription itself — that invariant is what the multi-protocol AC tests.
///
/// `had_processed_block` distinguishes cold-start re-baselines (no successful
/// process yet) from mid-run discontinuities (WHI-792).
pub async fn process_observed_head(
    http: &DynProvider,
    loop_state: &WatchLoopState,
    config: &WatchLoopConfig,
    head: ObservedHead,
    base_fee_per_gas: Option<u64>,
    block_gas_limit: u64,
    had_processed_block: bool,
) -> Result<ProcessHeadResult> {
    let observed_at = std::time::Instant::now();
    if let Some(fee) = base_fee_per_gas {
        crate::metrics::record_gas_base_fee(u128::from(fee));
    }
    debug!(
        target: "service.block_loop",
        stage = stages::BLOCK_OBSERVED,
        block = head.number,
        hash = %head.hash,
        "observed multi-protocol head"
    );

    let mut rebaseline: Option<RebaselineKind> = None;
    let observation = loop_state.snapshots.observe_head(&head).await;
    // Capture policy before early-returns consume the observation (WHI-885 / WHI-893).
    let tip_full_reason =
        tip_refresh_full_reason(&observation, had_processed_block);
    let force_full_tip_refresh = tip_full_reason.is_some();
    let tip_gap = tip_refresh_gap_size(&observation);
    let gap_log_range = tip_refresh_gap_log_range(&observation, had_processed_block);
    match observation {
        HeadObservation::Duplicate => {
            debug!(
                target: "service.block_loop",
                block = head.number,
                reason = BlockSkipReason::Duplicate.as_metric_label(),
                "duplicate head; skipping"
            );
            return Ok(ProcessHeadResult::skipped(BlockSkipReason::Duplicate));
        }
        HeadObservation::Halted(reason) => {
            warn!(
                target: "service.block_loop",
                block = head.number,
                hash = %head.hash,
                reason = BlockSkipReason::ContinuityHalt.as_metric_label(),
                halt = %reason,
                "head continuity halted (fork); discovery skipped — deep recovery is WHI-533"
            );
            return Ok(ProcessHeadResult::skipped(BlockSkipReason::ContinuityHalt));
        }
        HeadObservation::Backfill { previous, .. } => {
            let gap = tip_gap.unwrap_or_else(|| {
                head.number.saturating_sub(previous.id.block_number)
            });
            if gap > CACHE_SIZE as u64 {
                // Large numeric gap is not a reorg: nothing to unwind. Skip used to
                // leave `previous` stuck forever (WHI-792 deadlock). Re-baseline by
                // applying the observed tip and publishing so continuity advances.
                let kind = if had_processed_block {
                    RebaselineKind::MidRun
                } else {
                    RebaselineKind::ColdStart
                };
                rebaseline = Some(kind);
                crate::metrics::record_watch_rebaseline(kind.as_metric_label());
                match kind {
                    RebaselineKind::ColdStart => {
                        warn!(
                            target: "service.block_loop",
                            block = head.number,
                            previous = previous.id.block_number,
                            gap,
                            cache_size = CACHE_SIZE,
                            "cold-start backfill gap exceeds StateChangeCache; \
                             re-baselining at observed tip (not a reorg — WHI-533 owns deep reorg)"
                        );
                    }
                    RebaselineKind::MidRun => {
                        error!(
                            target: "service.block_loop",
                            block = head.number,
                            previous = previous.id.block_number,
                            gap,
                            cache_size = CACHE_SIZE,
                            "mid-run backfill gap exceeds StateChangeCache; \
                             re-baselining at observed tip (possible stalled feed; \
                             not a reorg — WHI-533 owns deep reorg unwind)"
                        );
                    }
                }
                // Fall through: apply tip logs + publish (advances last_tip).
            } else {
                // Small gap: apply the observed tip only for state mutation
                // (hash-pinned head logs). WHI-893 widens the Moe dirty set via
                // a separate range getLogs — see gap_log_range below. Intermediate
                // blocks are not applied to StateSpace (StateChangeCache path).
                warn!(
                    target: "service.block_loop",
                    block = head.number,
                    previous = previous.id.block_number,
                    gap,
                    "small gap backfill: applying observed tip only; \
                     gap-range logs will widen tip-refresh dirty set (WHI-893)"
                );
            }
        }
        HeadObservation::Assemble(_) => {}
    }

    let latest = loop_state.latest_block.load(Ordering::Relaxed);
    if reorg_deeper_than_cache(latest, head.number) {
        warn!(
            target: "service.block_loop",
            latest,
            block = head.number,
            hash = %head.hash,
            reason = BlockSkipReason::DeepReorg.as_metric_label(),
            cache_size = CACHE_SIZE,
            "reorg deeper than StateChangeCache; refusing StateSpace::sync to avoid panic \
             (WHI-533 owns full deep-reorg unwinding)"
        );
        loop_state
            .snapshots
            .fail_read(format!(
                "reorg to #{} deeper than CACHE_SIZE={CACHE_SIZE} from #{latest}",
                head.number
            ))
            .await;
        return Ok(ProcessHeadResult::skipped(BlockSkipReason::DeepReorg));
    }

    let logs = match fetch_logs_for_head(http, &loop_state.block_filter, &head).await {
        Ok(logs) => logs,
        Err(e) => {
            warn!(
                target: "service.block_loop",
                block = head.number,
                hash = %head.hash,
                reason = BlockSkipReason::PinnedLogsUnavailable.as_metric_label(),
                error = %e,
                "hash-pinned get_logs failed; skipping block (no number-range fallback — WHI-762)"
            );
            loop_state
                .snapshots
                .fail_read(format!(
                    "pinned logs unavailable for #{} hash={}",
                    head.number, head.hash
                ))
                .await;
            return Ok(ProcessHeadResult::skipped(
                BlockSkipReason::PinnedLogsUnavailable,
            ));
        }
    };

    let header = head.to_header_context();
    let snapshot_id = head.to_snapshot_id();
    let affected = {
        let mut guard = loop_state.state.write().await;
        let affected = if logs.is_empty() {
            Vec::new()
        } else {
            guard
                .sync(&logs)
                .map_err(|e| eyre!("StateSpace::sync (StateChangeCache path): {e}"))?
        };
        guard
            .latest_block
            .store(head.number, Ordering::Relaxed);
        affected
    };
    loop_state
        .latest_block
        .store(head.number, Ordering::Relaxed);

    debug!(
        target: "service.block_loop",
        stage = stages::STATE_APPLIED,
        block = head.number,
        affected = affected.len(),
        logs = logs.len(),
        "applied multi-protocol logs via StateChangeCache path"
    );

    // Tip refresh + pool vector for discovery (shared StateSpace, not per-protocol).
    let mut pools: Vec<AMM> = {
        let guard = loop_state.state.read().await;
        guard.state.values().cloned().collect()
    };
    // Dirty set for tip refresh (WHI-885 / WHI-893):
    // * Head-applied `affected` from hash-pinned logs (state mutation path).
    // * On small gaps, union addresses from a number-range getLogs over the gap.
    //   Range logs are **dirty-set only** — never passed to StateSpace::sync.
    // * Full scope ignores the set (cold start / large gap / range-fetch fail).
    let mut gap_range_log_count = 0usize;
    // Range logs only matter when tip refresh is armed (WHI-893). Offline /
    // fixture runs with `refresh_tip_state: false` skip the extra RPC.
    let range_result: Option<Result<Vec<Address>, ()>> =
        if config.refresh_tip_state && !force_full_tip_refresh {
            if let Some((from, to)) = gap_log_range {
                match fetch_logs_for_range(http, &loop_state.block_filter, from, to).await {
                    Ok(range_logs) => {
                        gap_range_log_count = range_logs.len();
                        let gap_addrs = dirty_addresses_from_logs(&range_logs);
                        debug!(
                            target: "service.block_loop",
                            block = head.number,
                            gap = tip_gap,
                            from,
                            to,
                            gap_logs = gap_range_log_count,
                            gap_dirty = gap_addrs.len(),
                            "gap-range logs widened tip-refresh dirty set (not applied to state)"
                        );
                        Some(Ok(gap_addrs))
                    }
                    Err(e) => {
                        warn!(
                            target: "service.block_loop",
                            block = head.number,
                            gap = tip_gap,
                            from,
                            to,
                            error = %e,
                            full_reason = TipRefreshFullReason::RangeLogsFailed.as_str(),
                            "gap-range get_logs failed; falling back to full tip refresh (WHI-893)"
                        );
                        Some(Err(()))
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
    let (force_full_tip_refresh, tip_full_reason, dirty_for_tip) =
        apply_gap_range_to_tip_refresh(
            force_full_tip_refresh,
            tip_full_reason,
            &affected,
            range_result,
        );
    let tip_scope = tip_refresh_scope_for_head(force_full_tip_refresh, &dirty_for_tip);
    if config.refresh_tip_state {
        // Tip refresh is fail-closed for quoting (WHI-762): if the HTTP node cannot
        // serve pin-scoped tip state we skip discovery/candidates. Log application
        // above already used the hash-pinned filter and is left in place so
        // continuity/latest_block stay aligned with the announced height; we do
        // **not** merge the partially-refreshed `pools` vec on failure (write-back
        // is Ok-only below).
        match refresh_selected_tip_state(
            http,
            &mut pools,
            &config.selected,
            head.hash,
            &header,
            &tip_scope,
        )
        .await
        {
            Ok(()) => {
                let mut guard = loop_state.state.write().await;
                for amm in &pools {
                    guard.state.insert(amm.address(), amm.clone());
                }
                // Moe plan after write-back for operator visibility (WHI-893).
                let moe_plan =
                    crate::service::protocol::plan_moe_tip_refresh(&pools, &tip_scope);
                debug!(
                    target: "service.block_loop",
                    stage = stages::TIP_REFRESHED,
                    block = head.number,
                    hash = %head.hash,
                    protocols = ?config.selected,
                    tip_refresh_mode = tip_scope.as_metric_label(),
                    tip_refresh_full_reason = tip_full_reason.map(|r| r.as_str()),
                    gap = tip_gap,
                    affected = affected.len(),
                    dirty = dirty_for_tip.len(),
                    gap_logs = gap_range_log_count,
                    refreshed = moe_plan.to_refresh.len(),
                    held = moe_plan.held,
                    "per-protocol tip refresh complete"
                );
            }
            Err(e) => {
                warn!(
                    target: "service.block_loop",
                    stage = stages::TIP_REFRESHED,
                    block = head.number,
                    hash = %head.hash,
                    reason = BlockSkipReason::TipRefreshFailed.as_metric_label(),
                    tip_refresh_mode = tip_scope.as_metric_label(),
                    tip_refresh_full_reason = tip_full_reason.map(|r| r.as_str()),
                    gap = tip_gap,
                    error = %e,
                    "per-protocol tip refresh failed for pinned hash; skipping quotes (WHI-762)"
                );
                loop_state
                    .snapshots
                    .fail_read(format!(
                        "tip refresh failed for #{} hash={}: {e}",
                        head.number, head.hash
                    ))
                    .await;
                return Ok(ProcessHeadResult::skipped(BlockSkipReason::TipRefreshFailed));
            }
        }
    }

    let pool_map: HashMap<Address, AMM> = pools.iter().map(|a| (a.address(), a.clone())).collect();
    loop_state
        .snapshots
        .publish(MarketSnapshot::new(
            snapshot_id,
            header,
            pool_map,
            ProtocolCoverage::default(),
        ))
        .await;

    let mut discovery = config.discovery.clone();
    discovery.snapshot_id = snapshot_id;
    discovery.block_timestamp = header.block_timestamp;
    // WHI-949: prefer measured FeePolicy path when a profile is available
    // (send runtime or explicit discovery profile). Offline fixtures keep the
    // hop-table GasConfig via merged_gas_config. Live measured mode fails closed
    // on missing base fee / zero block gas limit — never silently ranks with
    // the hop table after loading a profile.
    let measured_profile = config
        .discovery_gas_profile
        .clone()
        .or_else(|| {
            config
                .send_runtime
                .as_ref()
                .map(|rt| std::sync::Arc::new(rt.gas_profile().clone()))
        });
    if let Some(profile) = measured_profile {
        let (priority, reserve) = config
            .send_runtime
            .as_ref()
            .map(|rt| rt.fee_policy_params())
            .unwrap_or((
                config.discovery_priority_fee_wei,
                config.discovery_block_gas_reserve,
            ));
        let Some(base_fee_u64) = base_fee_per_gas else {
            warn!(
                target: "service.block_loop",
                stage = stages::DISCOVERY,
                block = head.number,
                "measured gas profile configured but tip has no base_fee_per_gas; \
                 skipping discovery (fail closed, no hop-table fallback)"
            );
            return Ok(ProcessHeadResult::skipped(
                BlockSkipReason::PinnedHeaderUnavailable,
            ));
        };
        if block_gas_limit == 0 {
            warn!(
                target: "service.block_loop",
                stage = stages::DISCOVERY,
                block = head.number,
                "measured gas profile configured but tip block_gas_limit is 0; \
                 skipping discovery (fail closed, no hop-table fallback)"
            );
            return Ok(ProcessHeadResult::skipped(
                BlockSkipReason::PinnedHeaderUnavailable,
            ));
        }
        discovery.measured_fee = Some(crate::service::fee_scoring::MeasuredFeeScoring::new(
            profile,
            priority,
            reserve,
            crate::execution::BlockFeeContext {
                block_number: head.number,
                block_hash: head.hash,
                base_fee_per_gas: u128::from(base_fee_u64),
                block_gas_limit,
            },
        ));
        // Offline residual unused when measured_fee is set; leave default.
    } else {
        discovery.measured_fee = None;
        discovery.gas = merged_gas_config(&config.selected, base_fee_per_gas);
    }

    // WHI-950 / G-3: capital domain before discovery (strategy A balance pin).
    let mut pinned_balance: Option<crate::state_space::SnapshotBoundBalance> = None;
    let mut inventory_block_unsendable = false;
    if let Some(policy) = config.capital_policy.as_ref() {
        use crate::service::capital_bound::{resolve_capital_domain, CapitalDomain};
        use crate::metrics::{self, stage as metric_stage};

        let balance_for_domain = if policy.requires_balance_read() {
            let Some(rt) = config.send_runtime.as_ref() else {
                return Err(eyre!(
                    "capital policy {} requires strategy-A balance read but send_runtime is absent",
                    policy.mode.as_str()
                ));
            };
            let balance_start = std::time::Instant::now();
            let bound = rt
                .executor_wmnt_balance_bound(snapshot_id)
                .await
                .context("strategy-A executor WMNT balanceOf")?;
            metrics::record_pipeline_stage(
                metric_stage::BALANCE_READ,
                "merged",
                balance_start.elapsed(),
            );
            debug!(
                target: "service.block_loop",
                stage = stages::BALANCE_READ,
                block = head.number,
                amount = %bound.amount,
                strategy = crate::service::capital_bound::BALANCE_READ_STRATEGY,
                "pinned executor WMNT balance (WHI-950 strategy A)"
            );
            pinned_balance = Some(bound);
            Some(bound)
        } else {
            None
        };

        match resolve_capital_domain(policy, snapshot_id, balance_for_domain)
            .map_err(|e| eyre!("capital domain: {e}"))?
        {
            CapitalDomain::BlockUnsendable {
                executor_balance,
                max_total_inventory_wmnt_wei,
            } => {
                inventory_block_unsendable = true;
                warn!(
                    target: "service.block_loop",
                    stage = stages::DISCOVERY,
                    block = head.number,
                    executor_balance = %executor_balance,
                    max_total_inventory = %max_total_inventory_wmnt_wei,
                    "inventory precondition failed — block unsendable; emitting no candidates (WHI-950)"
                );
            }
            CapitalDomain::Feasible { max_input, .. } => {
                discovery.max_input = max_input;
                debug!(
                    target: "service.block_loop",
                    stage = stages::DISCOVERY,
                    block = head.number,
                    max_input = %max_input,
                    capital_mode = policy.mode.as_str(),
                    "applied capital-bound max_input (WHI-950)"
                );
            }
        }
    }

    // WHI-940: keep topology across blocks; optimize only dirty cycles on
    // Touched tip-refresh, full set on Full (or cold first pass).
    // Inventory precondition → empty candidate set (no "smaller input" search).
    let (opportunities, discovery_stats) = if inventory_block_unsendable {
        (
            Vec::new(),
            crate::service::path_index::DiscoveryStats::default(),
        )
    } else {
        let mut guard = loop_state
            .discovery_engine
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(
                DiscoveryEngine::build(&pools, discovery.settlement_asset, discovery.max_hops)
                    .context("DiscoveryEngine::build")?,
            );
        }
        let engine = guard.as_mut().expect("just inserted");
        engine
            .discover(&pools, &discovery, &tip_scope)
            .context("multi-protocol DiscoveryEngine::discover")?
    };
    let pass_stats = DiscoveryPassStats::from(discovery_stats);
    let optimize_ratio = if discovery_stats.cycles_total == 0 {
        0.0
    } else {
        discovery_stats.cycles_optimized as f64 / discovery_stats.cycles_total as f64
    };
    debug!(
        target: "service.block_loop",
        stage = stages::DISCOVERY,
        block = head.number,
        opportunities = opportunities.len(),
        cycles_total = discovery_stats.cycles_total,
        cycles_optimized = discovery_stats.cycles_optimized,
        cycles_evaluated = pass_stats.cycles_evaluated,
        amm_quotes = pass_stats.amm_quotes,
        dirty_pools = discovery_stats.dirty_pools,
        optimize_ratio,
        discovery_scope = discovery_stats.scope,
        tip_refresh_mode = tip_scope.as_metric_label(),
        "merged-graph discovery complete"
    );

    // WHI-951 / G-4: static eligibility first, then walk the attempt plan.
    // WHI-950: pass strategy-A pinned balance so inventory/balance caps apply statically.
    let armed = production_send_allowed();
    let eligibility = classify_with_send_runtime(
        &opportunities,
        config.send_runtime.as_deref(),
        pinned_balance.map(|b| b.amount),
    );
    // Inventory precondition already forced empty opportunities; surface the flag
    // even if send_runtime bounds were unrestricted (e.g. shadow has no balance).
    let mut eligibility = eligibility;
    if inventory_block_unsendable {
        eligibility.inventory_block_unsendable = true;
        eligibility.eligible_indices.clear();
        eligibility.eligible_count = 0;
    }

    let mut attempts = Vec::new();
    let mut attempt_outcome_override: Option<&'static str> = None;
    if config.attempt_execution {
        let walk = walk_attempt_plan(
            &opportunities,
            &eligibility,
            armed,
            config.attempt_budget,
            discovery.block_timestamp,
            AttemptJobContext {
                observed_at,
                base_fee_per_gas: base_fee_per_gas.map(u128::from).unwrap_or(0),
                block_gas_limit,
            },
            config.send_runtime.as_deref(),
            AttemptIdentityContext {
                header,
                pool_universe_fingerprint: config.pool_universe_fingerprint,
            },
            pinned_balance,
        )
        .await
        .context("walk_attempt_plan")?;
        attempts = walk.attempts;
        attempt_outcome_override = walk.outcome_override;
        if let Some((_, attempt)) = attempts.first() {
            debug!(
                target: "service.block_loop",
                stage = stages::EXECUTION_ATTEMPT,
                block = head.number,
                ?attempt,
                "execution attempt"
            );
        }
    }

    // WHI-952: one greppable info line per processed head.
    let mut summary = BlockSummary::from_eligibility(
        head.number,
        affected.len(),
        &pass_stats,
        &opportunities,
        &eligibility,
        &attempts,
    );
    if summary.attempt_outcome.is_none() {
        summary.attempt_outcome = attempt_outcome_override;
    }
    summary.emit();

    let tick = BlockTick {
        block_number: head.number,
        snapshot_id,
        header,
        base_fee_per_gas,
        affected_pools: affected.len(),
        opportunities,
        attempts,
    };
    Ok(match rebaseline {
        Some(kind) => ProcessHeadResult::rebaselined(kind, tick),
        None => ProcessHeadResult::processed(tick),
    })
}

/// Fetch logs for the announced head using a **hash filter only** (WHI-762).
///
/// Number-range fallback is forbidden for state mutation: a bare height does not
/// disambiguate forks and would re-introduce silent stale quotes under the
/// announced identity. Gap-range logs use [`fetch_logs_for_range`] and must not
/// be mixed into this path.
async fn fetch_logs_for_head(
    provider: &DynProvider,
    block_filter: &Filter,
    head: &ObservedHead,
) -> Result<Vec<Log>> {
    let hash_filter = hash_pinned_logs_filter(block_filter.clone(), head.hash);
    provider
        .get_logs(&hash_filter)
        .await
        .map_err(|e| eyre!("hash-pinned get_logs for #{} hash={}: {e}", head.number, head.hash))
}

/// Number-range `eth_getLogs` used **only** to widen the tip-refresh dirty set
/// on small gaps (WHI-893).
///
/// Callers must not pass the result to [`StateSpace::sync`]. State mutation for
/// the announced head remains hash-pinned via [`fetch_logs_for_head`].
async fn fetch_logs_for_range(
    provider: &DynProvider,
    block_filter: &Filter,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Log>> {
    // Intentionally a number range (not hash-pinned): intermediate block hashes
    // are not available on a gap. Safe only because the consumer uses addresses
    // for dirty-set selection, not for pool-state mutation.
    let range_filter = block_filter
        .clone()
        .from_block(from_block)
        .to_block(to_block);
    provider
        .get_logs(&range_filter)
        .await
        .map_err(|e| {
            eyre!(
                "gap-range get_logs for blocks {from_block}..={to_block}: {e}"
            )
        })
}

/// Continuous multi-protocol watch loop driven by a **single** head stream.
///
/// `heads` must be the only block subscription for this process (all selected
/// protocols share it). `block_subscriptions` is the number of subscriptions the
/// caller opened to produce `heads` — production always passes `1` from
/// [`subscribe_heads_once`].
///
/// Exit rules (WHI-792):
/// - Shutdown with zero heads observed → `Ok` (idle / SIGINT before first head).
/// - Stream end or shutdown after heads were observed but **zero** were processed
///   → `Err` (never report "exited cleanly" for a dead loop).
/// - Consecutive skips past [`WatchLoopConfig::skip_fatal_window`] while
///   `blocks_processed == 0` → `Err` (mid-loop abort of a dead start).
/// - Same window after prior success → repeated `error!` only; loop continues.
pub async fn run_multi_protocol_watch_loop<S, F, H>(
    http: DynProvider,
    loop_state: WatchLoopState,
    config: WatchLoopConfig,
    mut heads: S,
    mut shutdown: F,
    mut hooks: H,
    block_subscriptions: u64,
) -> Result<WatchLoopStats>
where
    S: Stream<Item = ObservedHead> + Unpin,
    F: Future<Output = ()> + Unpin,
    H: WatchLoopHooks,
{
    if block_subscriptions != 1 {
        return Err(eyre!(
            "multi-protocol watch requires exactly one block subscription \
             (got {block_subscriptions}); per-protocol subscriptions are forbidden"
        ));
    }
    let mut stats = WatchLoopStats {
        block_subscriptions,
        ..WatchLoopStats::default()
    };
    let mut consecutive_skips = 0u64;
    let mut skip_ratio = SkipRatioTracker::from_config(&config);
    let mut exit_reason = WatchExitReason::StreamEnded;

    info!(
        target: "service.block_loop",
        protocols = ?config.selected,
        block_subscriptions = stats.block_subscriptions,
        http_tip_wait_ms = config.http_tip_wait.as_millis() as u64,
        skip_fatal_window = config.skip_fatal_window,
        skip_ratio_window = config.skip_ratio_window,
        skip_ratio_threshold = config.skip_ratio_threshold,
        "starting multi-protocol watch loop (single shared subscription; announced hash is authoritative — WHI-762)"
    );

    // WHI-762 pin invariant (loop head): for each iteration the announced hash is
    // authoritative for every read; no call may widen to latest/number/neighbour.
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!(
                    target: "service.block_loop",
                    blocks = stats.blocks_processed,
                    heads = stats.heads_observed,
                    halted_or_skipped = stats.halted_or_skipped,
                    "shutdown signal; ending multi-protocol watch loop"
                );
                exit_reason = WatchExitReason::Shutdown;
                break;
            }
            next = heads.next() => {
                let Some(head) = next else {
                    info!(
                        target: "service.block_loop",
                        blocks = stats.blocks_processed,
                        heads = stats.heads_observed,
                        "head stream ended; watch loop complete"
                    );
                    // exit_reason already StreamEnded
                    break;
                };
                stats.heads_observed += 1;
                let head_number = head.number;
                let head_hash = head.hash;
                let mut head_skipped = false;

                // Hash-pinned HTTP header for base fee / gas limit (WS may omit).
                // Bounded wait when HTTP lags the announced tip (WHI-792 / WHI-762).
                let header_load = load_canonical_header_with_wait(
                    &http,
                    head_number,
                    head_hash,
                    config.http_tip_wait,
                )
                .await;
                match header_load {
                    CanonicalHeaderLoad::Ready { base_fee, gas_limit, waited } => {
                        if waited > Duration::ZERO {
                            stats.http_tip_waits += 1;
                            crate::metrics::record_http_tip_wait(waited);
                            debug!(
                                target: "service.block_loop",
                                block = head_number,
                                hash = %head_hash,
                                waited_ms = waited.as_millis() as u64,
                                "HTTP served announced hash after wait"
                            );
                        }
                        match process_observed_head(
                            &http,
                            &loop_state,
                            &config,
                            head,
                            base_fee,
                            gas_limit,
                            stats.blocks_processed > 0,
                        )
                        .await
                        {
                            Ok(result) => {
                                if let Some(kind) = result.rebaseline {
                                    match kind {
                                        RebaselineKind::ColdStart => {
                                            stats.cold_start_rebaselines += 1;
                                        }
                                        RebaselineKind::MidRun => {
                                            stats.mid_run_rebaselines += 1;
                                        }
                                    }
                                }
                                if let Some(tick) = result.tick {
                                    consecutive_skips = 0;
                                    stats.blocks_processed += 1;
                                    stats.opportunities_found += tick.opportunities.len() as u64;
                                    stats.attempts += tick.attempts.len() as u64;
                                    debug!(
                                        target: "service.block_loop",
                                        block = tick.block_number,
                                        opportunities = tick.opportunities.len(),
                                        attempts = tick.attempts.len(),
                                        blocks_processed = stats.blocks_processed,
                                        "multi-protocol block progress"
                                    );
                                    hooks.on_block_ready(&tick)?;
                                    for (opp, attempt) in &tick.attempts {
                                        hooks.on_attempt(opp, attempt)?;
                                    }
                                } else {
                                    head_skipped = true;
                                    let reason = result
                                        .skip_reason
                                        .unwrap_or(BlockSkipReason::ProcessingFailed);
                                    record_skip(&mut stats, reason);
                                    consecutive_skips += 1;
                                    // Summary for skipped heads (processed path emits inside
                                    // process_observed_head). WHI-952 greppable contract.
                                    BlockSummary::skipped(head_number, reason.as_metric_label())
                                        .emit();
                                }
                            }
                            Err(e) => {
                                warn!(
                                    target: "service.block_loop",
                                    block = head_number,
                                    hash = %head_hash,
                                    reason = BlockSkipReason::ProcessingFailed.as_metric_label(),
                                    error = %e,
                                    "block processing failed; skipping"
                                );
                                head_skipped = true;
                                record_skip(&mut stats, BlockSkipReason::ProcessingFailed);
                                consecutive_skips += 1;
                                BlockSummary::skipped(
                                    head_number,
                                    BlockSkipReason::ProcessingFailed.as_metric_label(),
                                )
                                .emit();
                            }
                        }
                    }
                    CanonicalHeaderLoad::TimedOut { waited } => {
                        stats.http_tip_waits += 1;
                        stats.http_tip_timeouts += 1;
                        head_skipped = true;
                        consecutive_skips += 1;
                        crate::metrics::record_http_tip_wait(waited);
                        crate::metrics::record_http_tip_timeout();
                        record_skip(&mut stats, BlockSkipReason::PinnedHeaderUnavailable);
                        warn!(
                            target: "service.block_loop",
                            block = head_number,
                            hash = %head_hash,
                            reason = BlockSkipReason::PinnedHeaderUnavailable.as_metric_label(),
                            waited_ms = waited.as_millis() as u64,
                            "HTTP has not served announced hash within deadline; skipping (WHI-762)"
                        );
                        BlockSummary::skipped(
                            head_number,
                            BlockSkipReason::PinnedHeaderUnavailable.as_metric_label(),
                        )
                        .emit();
                    }
                    CanonicalHeaderLoad::RpcError { error, waited } => {
                        if waited > Duration::ZERO {
                            stats.http_tip_waits += 1;
                            crate::metrics::record_http_tip_wait(waited);
                        }
                        head_skipped = true;
                        consecutive_skips += 1;
                        record_skip(&mut stats, BlockSkipReason::PinnedHeaderRpcError);
                        warn!(
                            target: "service.block_loop",
                            block = head_number,
                            hash = %head_hash,
                            reason = BlockSkipReason::PinnedHeaderRpcError.as_metric_label(),
                            error = %error,
                            "failed to load hash-pinned header; skipping (WHI-762)"
                        );
                        BlockSummary::skipped(
                            head_number,
                            BlockSkipReason::PinnedHeaderRpcError.as_metric_label(),
                        )
                        .emit();
                    }
                }

                if skip_ratio.record(head_skipped) {
                    stats.skip_ratio_warnings += 1;
                    crate::metrics::record_watch_skip_ratio_warning();
                    warn!(
                        target: "service.block_loop",
                        window = config.skip_ratio_window,
                        threshold = config.skip_ratio_threshold,
                        skip_count = skip_ratio.skip_count(),
                        pin_skips = stats.pin_skips,
                        halted_or_skipped = stats.halted_or_skipped,
                        heads_observed = stats.heads_observed,
                        blocks_processed = stats.blocks_processed,
                        "watch skip ratio exceeds threshold; HTTP/WS transports may be out of step (WHI-762)"
                    );
                }

                if config.skip_fatal_window > 0
                    && consecutive_skips >= config.skip_fatal_window
                {
                    if stats.blocks_processed == 0 {
                        error!(
                            target: "service.block_loop",
                            consecutive_skips,
                            heads_observed = stats.heads_observed,
                            halted_or_skipped = stats.halted_or_skipped,
                            pin_skips = stats.pin_skips,
                            "watch loop unhealthy: zero blocks processed over consecutive-skip window"
                        );
                        return Err(eyre!(
                            "watch loop unhealthy: {consecutive_skips} consecutive skips with \
                             blocks_processed=0 (heads_observed={}); refusing silent success (WHI-792)",
                            stats.heads_observed
                        ));
                    }
                    // Mid-run: prominent repeated error (spec allows escalate-or-exit).
                    // Do not abort — re-baseline / next head may recover.
                    error!(
                        target: "service.block_loop",
                        consecutive_skips,
                        blocks_processed = stats.blocks_processed,
                        heads_observed = stats.heads_observed,
                        halted_or_skipped = stats.halted_or_skipped,
                        pin_skips = stats.pin_skips,
                        "watch loop elevated skip rate after prior success; \
                         loop continues but needs operator attention (WHI-792)"
                    );
                }
            }
        }
    }

    finalize_watch_stats(stats, exit_reason)
}

#[derive(Debug, Clone, Copy)]
enum WatchExitReason {
    Shutdown,
    StreamEnded,
}

fn finalize_watch_stats(
    stats: WatchLoopStats,
    reason: WatchExitReason,
) -> Result<WatchLoopStats> {
    if stats.heads_observed > 0 && stats.blocks_processed == 0 {
        error!(
            target: "service.block_loop",
            heads_observed = stats.heads_observed,
            halted_or_skipped = stats.halted_or_skipped,
            cold_start_rebaselines = stats.cold_start_rebaselines,
            mid_run_rebaselines = stats.mid_run_rebaselines,
            http_tip_timeouts = stats.http_tip_timeouts,
            exit = ?reason,
            "watch loop ended with zero blocks processed — not a clean exit"
        );
        return Err(eyre!(
            "watch loop observed {} heads but processed zero blocks \
             (halted_or_skipped={}, http_tip_timeouts={}); refusing clean exit (WHI-792)",
            stats.heads_observed,
            stats.halted_or_skipped,
            stats.http_tip_timeouts
        ));
    }
    Ok(stats)
}

fn record_skip(stats: &mut WatchLoopStats, reason: BlockSkipReason) {
    stats.halted_or_skipped += 1;
    if reason.is_pin_failure() {
        stats.pin_skips += 1;
    }
    crate::metrics::record_watch_block_skip(reason.as_metric_label());
}

#[derive(Debug)]
pub enum CanonicalHeaderLoad {
    Ready {
        base_fee: Option<u64>,
        gas_limit: u64,
        waited: Duration,
    },
    TimedOut {
        waited: Duration,
    },
    RpcError {
        error: eyre::Report,
        waited: Duration,
    },
}

/// Wait up to `deadline` for HTTP to serve the **announced block hash** (WHI-762).
///
/// Uses `eth_getBlockByHash` — never a bare number — so a same-height fork on the
/// HTTP node cannot supply base-fee/gas for the wrong identity.
pub async fn load_canonical_header_with_wait(
    http: &DynProvider,
    block_number: u64,
    block_hash: B256,
    deadline: Duration,
) -> CanonicalHeaderLoad {
    let started = std::time::Instant::now();
    loop {
        match http.get_block_by_hash(block_hash).await {
            Ok(Some(block)) => {
                let h = block.header();
                // Defensive: reject a response whose embedded hash disagrees.
                let returned = block.hash();
                if returned != block_hash && returned != B256::ZERO {
                    return CanonicalHeaderLoad::RpcError {
                        error: eyre!(
                            "get_block_by_hash returned hash {returned} for announced {block_hash} (#{block_number})"
                        ),
                        waited: started.elapsed(),
                    };
                }
                return CanonicalHeaderLoad::Ready {
                    base_fee: h.base_fee_per_gas(),
                    gas_limit: h.gas_limit(),
                    waited: started.elapsed(),
                };
            }
            Ok(None) => {
                if started.elapsed() >= deadline {
                    return CanonicalHeaderLoad::TimedOut {
                        waited: started.elapsed(),
                    };
                }
                tokio::time::sleep(
                    HTTP_TIP_POLL_INTERVAL.min(deadline.saturating_sub(started.elapsed())),
                )
                .await;
            }
            Err(e) => {
                // Hard RPC failures are not "HTTP lag" — skip immediately.
                // Only `Ok(None)` (hash not yet available) waits (WHI-792).
                return CanonicalHeaderLoad::RpcError {
                    error: eyre!("get_block_by_hash #{block_number} {block_hash}: {e}"),
                    waited: started.elapsed(),
                };
            }
        }
    }
}

/// Result of opening the multi-protocol head subscription.
///
/// `subscription_count` is always `1` — the AC that one subscription serves all
/// selected protocols is carried in this type so callers cannot silently open
/// multiple streams without updating the count.
pub struct HeadSubscription<S> {
    pub stream: S,
    pub subscription_count: u64,
}

/// Build an [`ObservedHead`] stream from a WS provider's `subscribe_blocks`.
///
/// This is the **single** subscription used by the multi-protocol bot (default
/// `--head-source ws`).
pub async fn subscribe_heads_once<P>(
    ws: &P,
    chain_id: u64,
) -> Result<HeadSubscription<impl Stream<Item = ObservedHead> + Unpin>>
where
    P: Provider + Clone,
{
    let sub = ws
        .subscribe_blocks()
        .await
        .map_err(|e| eyre!("subscribe_blocks (single multi-protocol subscription): {e}"))?;
    let stream = sub.into_stream().filter_map(move |block| {
        let number = block.number();
        async move {
            if number == 0 {
                return None;
            }
            // `subscribe_blocks` yields a header-like BlockResponse; parent/timestamp
            // are on the header methods (same shape as StateSpaceManager::subscribe).
            Some(ObservedHead::new(
                chain_id,
                number,
                block.hash(),
                block.parent_hash(),
                block.timestamp(),
            ))
        }
    });
    Ok(HeadSubscription {
        stream: Box::pin(stream),
        subscription_count: 1,
    })
}

/// Poll new heads over HTTP (WHI-762 `--head-source http-poll`).
///
/// Emits an [`ObservedHead`] whenever `eth_blockNumber` advances, loading the full
/// header via `eth_getBlockByNumber`. Single-transport operation removes WS/HTTP
/// tip skew for signerless dry runs. `subscription_count` is still `1`.
pub fn poll_heads_http(
    http: DynProvider,
    chain_id: u64,
    interval: Duration,
) -> HeadSubscription<impl Stream<Item = ObservedHead> + Unpin> {
    struct PollState {
        http: DynProvider,
        chain_id: u64,
        interval: Duration,
        last: u64,
    }

    let stream = futures::stream::unfold(
        PollState {
            http,
            chain_id,
            interval,
            last: 0,
        },
        |mut state| async move {
            loop {
                match state.http.get_block_number().await {
                    Ok(tip) if tip > state.last && tip > 0 => {
                        match state
                            .http
                            .get_block_by_number(BlockNumberOrTag::Number(tip))
                            .await
                        {
                            Ok(Some(block)) => {
                                let number = block.header().number();
                                if number == 0 {
                                    state.last = tip;
                                    continue;
                                }
                                state.last = number;
                                let head = ObservedHead::new(
                                    state.chain_id,
                                    number,
                                    block.hash(),
                                    block.header().parent_hash(),
                                    block.header().timestamp(),
                                );
                                return Some((head, state));
                            }
                            Ok(None) => {
                                // Tip number raced ahead of full block availability.
                            }
                            Err(e) => {
                                warn!(
                                    target: "service.block_loop",
                                    tip,
                                    error = %e,
                                    "http-poll get_block_by_number failed; will retry"
                                );
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            target: "service.block_loop",
                            error = %e,
                            "http-poll eth_blockNumber failed; will retry"
                        );
                    }
                }
                tokio::time::sleep(state.interval).await;
            }
        },
    );
    HeadSubscription {
        stream: Box::pin(stream),
        subscription_count: 1,
    }
}

/// Source of new-head notifications for the multi-protocol watch loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadSource {
    /// `eth_subscribe("newHeads")` over WS (default).
    Ws,
    /// Poll `eth_blockNumber` over HTTP (WHI-762).
    HttpPoll,
}

impl HeadSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ws => "ws",
            Self::HttpPoll => "http-poll",
        }
    }

    /// Parse CLI / env value (`ws` | `http-poll`).
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ws" | "websocket" => Ok(Self::Ws),
            "http-poll" | "http_poll" | "http" | "poll" => Ok(Self::HttpPoll),
            other => Err(eyre!(
                "unknown head source '{other}'; expected 'ws' or 'http-poll'"
            )),
        }
    }
}

/// Await SIGINT or SIGTERM (Unix). Used for graceful watch-loop shutdown so the
/// shadow ledger is flushed via normal drop paths rather than mid-row truncation.
pub async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    target: "service.block_loop",
                    error = %e,
                    "failed to install SIGTERM handler; waiting for ctrl_c only"
                );
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(target: "service.block_loop", signal = "SIGINT", "shutdown signal received");
            }
            _ = sigterm.recv() => {
                info!(target: "service.block_loop", signal = "SIGTERM", "shutdown signal received");
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!(target: "service.block_loop", signal = "SIGINT", "shutdown signal received");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::discovery::DiscoveryConfig;
    use crate::service::fixture::{cross_protocol_fixture_pools, fixture_settlement_asset};
    use crate::service::select::SelectedProtocol;
    use crate::state_space::{BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId};
    use alloy::primitives::{b256, B256};
    use alloy::providers::{ProviderBuilder, Provider};
    use alloy::transports::mock::Asserter;
    use futures::stream;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    fn ready_tip(id: SnapshotId) -> SnapshotStatus {
        let header = BlockHeaderContext::new(B256::ZERO, 1);
        SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
            id,
            header,
            HashMap::new(),
            ProtocolCoverage::default(),
        )))
    }

    #[test]
    fn matching_ready_tip_accepts() {
        let id = SnapshotId::new(
            5000,
            10,
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        );
        let tip = require_matching_ready_tip(Some(ready_tip(id)), id).unwrap();
        match tip {
            SnapshotStatus::Ready(s) => assert_eq!(s.id, id),
            other => panic!("unexpected tip: {other:?}"),
        }
    }

    #[test]
    fn stale_ready_tip_rejects() {
        let candidate = SnapshotId::new(
            5000,
            10,
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        );
        let live = SnapshotId::new(
            5000,
            11,
            b256!("2222222222222222222222222222222222222222222222222222222222222222"),
        );
        let err = require_matching_ready_tip(Some(ready_tip(live)), candidate).unwrap_err();
        assert!(err.to_string().contains("stale queued opportunity"));
    }

    #[test]
    fn absent_tip_rejects() {
        let id = SnapshotId::new(5000, 1, B256::ZERO);
        let err = require_matching_ready_tip(None, id).unwrap_err();
        assert!(err.to_string().contains("no live Ready snapshot tip"));
    }

    #[test]
    fn job_slot_latest_wins() {
        let slot = new_job_slot::<u32>();
        slot.publish(1);
        slot.publish(2);
        assert_eq!(slot.take(), Some(2));
        assert_eq!(slot.take(), None);
    }

    #[test]
    fn reorg_depth_guard_matches_cache_size() {
        assert!(!reorg_deeper_than_cache(100, 101));
        assert!(!reorg_deeper_than_cache(100, 100));
        assert!(!reorg_deeper_than_cache(100, 71)); // within CACHE_SIZE=30
        assert!(reorg_deeper_than_cache(100, 70)); // 100-70=30 >= CACHE_SIZE-ish boundary
        assert!(reorg_deeper_than_cache(100, 50));
    }

    #[test]
    fn merged_gas_prefers_live_base_fee_for_v3_or_moe() {
        let gas = merged_gas_config(&[SelectedProtocol::AgniV3, SelectedProtocol::Moe], Some(42));
        assert_eq!(gas.gas_price_wei, 42);
        let gas_v2 = merged_gas_config(&[SelectedProtocol::AgniV2], Some(99));
        // V2 ignores base fee and keeps default.
        assert_eq!(gas_v2.gas_price_wei, GasConfig::default().gas_price_wei);
    }

    /// AC: a single head stream drives all selected protocols (no per-protocol
    /// subscriptions). Replay three synthetic heads offline against the fixture.
    #[tokio::test]
    async fn single_subscription_processes_multiple_blocks_for_all_protocols() {
        let pools = cross_protocol_fixture_pools();
        let mut space = StateSpace::default();
        for amm in &pools {
            space.state.insert(amm.address(), amm.clone());
        }
        space.latest_block.store(10, Ordering::Relaxed);
        let latest_block = Arc::clone(&space.latest_block);
        let state = Arc::new(RwLock::new(space));
        let snapshots = SnapshotPublisher::new();
        // Seed continuity so heads 11,12,13 Advance.
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, 10, B256::repeat_byte(0x10)),
                BlockHeaderContext::new(B256::repeat_byte(0x0f), 1_700_000_000),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;

        let loop_state = WatchLoopState::new(
            state,
            latest_block,
            snapshots,
            Filter::new(),
            5000,
        );

        let mut discovery = DiscoveryConfig::offline_default(fixture_settlement_asset());
        discovery.gas.gas_price_wei = 0;
        let config = WatchLoopConfig {
            discovery,
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: true,
            // Fixture pools are static; skip Moe tip-sync RPC on the mock provider.
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        };

        // Drive process_observed_head directly (no get_block) to prove multi-block +
        // single-subscription semantics without live RPC.
        let heads = [
            ObservedHead::new(5000, 11, B256::repeat_byte(0x11), B256::repeat_byte(0x10), 1_700_000_001),
            ObservedHead::new(5000, 12, B256::repeat_byte(0x12), B256::repeat_byte(0x11), 1_700_000_002),
            ObservedHead::new(5000, 13, B256::repeat_byte(0x13), B256::repeat_byte(0x12), 1_700_000_003),
        ];

        let asserter = Asserter::new();
        // Hash-pinned get_logs only (one call per head).
        for _ in 0..4 {
            asserter.push_success(&Vec::<Log>::new());
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let mut ticks = 0u64;
        let mut blocks = Vec::new();
        for head in heads {
            let tick = process_observed_head(
                &http,
                &loop_state,
                &config,
                head,
                Some(25),
                30_000_000,
                ticks > 0,
            )
            .await
            .expect("process head")
            .tick
            .expect("tick produced");
            ticks += 1;
            blocks.push(tick.block_number);
            // Fixture is static — discovery still runs on the shared pool set.
            assert!(
                !tick.opportunities.is_empty(),
                "block {} should still discover fixture cycle",
                tick.block_number
            );
            assert_eq!(tick.attempts.len(), 1);
            assert!(matches!(
                tick.attempts[0].1,
                ExecutionAttempt::ProductionGateBlocked { .. }
            ));
        }

        assert_eq!(ticks, 3, "must process 3 consecutive blocks");
        assert_eq!(blocks, vec![11, 12, 13]);
        // One shared WatchLoopState + one process_observed_head path for all
        // SelectedProtocol::all() — no per-protocol head streams were opened.
        assert_eq!(ticks, 3);
        assert_eq!(config.selected.len(), SelectedProtocol::all().len());

        // WHI-940: topology is built once for the watch session, not per block.
        let engine = loop_state
            .discovery_engine
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let eng = engine.as_ref().expect("discovery engine built");
        assert_eq!(
            eng.index().build_graph_calls(),
            1,
            "build_graph must run once per universe load"
        );
        assert_eq!(
            eng.index().find_cycles_calls(),
            1,
            "find_cycles must run once per universe load"
        );
    }

    /// WHI-940 AC: empty-log consecutive blocks optimize 0 cycles after prime;
    /// Full cold-start optimizes all.
    #[tokio::test]
    async fn multi_block_empty_logs_use_incremental_path_index() {
        let pools = cross_protocol_fixture_pools();
        let mut space = StateSpace::default();
        for amm in &pools {
            space.state.insert(amm.address(), amm.clone());
        }
        space.latest_block.store(10, Ordering::Relaxed);
        let latest_block = Arc::clone(&space.latest_block);
        let state = Arc::new(RwLock::new(space));
        let snapshots = SnapshotPublisher::new();
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, 10, B256::repeat_byte(0x10)),
                BlockHeaderContext::new(B256::repeat_byte(0x0f), 1_700_000_000),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;

        let loop_state = WatchLoopState::new(
            state,
            latest_block,
            snapshots,
            Filter::new(),
            5000,
        );

        let mut discovery = DiscoveryConfig::offline_default(fixture_settlement_asset());
        discovery.gas.gas_price_wei = 0;
        let config = WatchLoopConfig {
            discovery,
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        };

        let heads = [
            ObservedHead::new(5000, 11, B256::repeat_byte(0x11), B256::repeat_byte(0x10), 1_700_000_001),
            ObservedHead::new(5000, 12, B256::repeat_byte(0x12), B256::repeat_byte(0x11), 1_700_000_002),
        ];
        let asserter = Asserter::new();
        for _ in 0..4 {
            asserter.push_success(&Vec::<Log>::new());
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        // Block 1: cold start → Full re-optimize.
        let r1 = process_observed_head(
            &http,
            &loop_state,
            &config,
            heads[0],
            Some(25),
            30_000_000,
            false,
        )
        .await
        .expect("head 1");
        assert!(!r1.tick.as_ref().unwrap().opportunities.is_empty());

        // Block 2: empty logs, Touched(empty) → cycles_optimized == 0, same opps.
        let r2 = process_observed_head(
            &http,
            &loop_state,
            &config,
            heads[1],
            Some(25),
            30_000_000,
            true,
        )
        .await
        .expect("head 2");
        let tick2 = r2.tick.expect("tick2");
        assert!(!tick2.opportunities.is_empty());

        let guard = loop_state
            .discovery_engine
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let eng = guard.as_ref().expect("engine");
        assert_eq!(eng.index().build_graph_calls(), 1);
        assert_eq!(eng.index().find_cycles_calls(), 1);
        assert!(eng.is_primed());
        let stats = eng.last_stats().expect("last discover stats");
        assert_eq!(stats.scope, "touched");
        assert_eq!(
            stats.cycles_optimized, 0,
            "empty-log consecutive block must not re-optimize any cycle"
        );
        assert_eq!(stats.dirty_pools, 0);
        assert!(stats.cycles_total >= 1);
    }

    /// Drive the full select-loop with a synthetic head stream and immediate shutdown
    /// after the stream ends — proves the loop exit path and subscription count.
    #[tokio::test]
    async fn watch_loop_exits_on_stream_end_with_single_subscription() {
        let pools = cross_protocol_fixture_pools();
        let mut space = StateSpace::default();
        for amm in &pools {
            space.state.insert(amm.address(), amm.clone());
        }
        space.latest_block.store(1, Ordering::Relaxed);
        let latest_block = Arc::clone(&space.latest_block);
        let state = Arc::new(RwLock::new(space));
        let snapshots = SnapshotPublisher::new();
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, 1, B256::repeat_byte(0x01)),
                BlockHeaderContext::new(B256::ZERO, 1_700_000_000),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;

        let loop_state = WatchLoopState::new(
            state,
            latest_block,
            snapshots,
            Filter::new(),
            5000,
        );

        let mut discovery = DiscoveryConfig::offline_default(fixture_settlement_asset());
        discovery.gas.gas_price_wei = 0;
        let config = WatchLoopConfig {
            discovery,
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        };

        // Empty head stream → loop exits immediately with subscription count 1.
        let heads = stream::empty::<ObservedHead>();
        let shutdown = std::future::pending::<()>();
        let asserter = Asserter::new();
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            heads,
            Box::pin(shutdown),
            NoopWatchHooks,
            1, // caller-reported single subscription (matches subscribe_heads_once)
        )
        .await
        .expect("loop");

        assert_eq!(stats.block_subscriptions, 1);
        assert_eq!(stats.blocks_processed, 0);
    }

    #[tokio::test]
    async fn watch_loop_rejects_multi_subscription_count() {
        let loop_state = WatchLoopState::new(
            Arc::new(RwLock::new(StateSpace::default())),
            Arc::new(AtomicU64::new(0)),
            SnapshotPublisher::new(),
            Filter::new(),
            5000,
        );
        let config = WatchLoopConfig {
            discovery: DiscoveryConfig::offline_default(fixture_settlement_asset()),
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        };
        let http = ProviderBuilder::new()
            .connect_mocked_client(Asserter::new())
            .erased();
        let err = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::empty::<ObservedHead>(),
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            2, // forbidden
        )
        .await
        .expect_err("must reject >1 subscription");
        assert!(
            err.to_string().contains("exactly one block subscription"),
            "got: {err}"
        );
    }

    /// Shutdown future completion ends the loop with Ok (SIGINT/SIGTERM path).
    #[tokio::test]
    async fn watch_loop_exits_cleanly_on_shutdown_signal() {
        let loop_state = WatchLoopState::new(
            Arc::new(RwLock::new(StateSpace::default())),
            Arc::new(AtomicU64::new(0)),
            SnapshotPublisher::new(),
            Filter::new(),
            5000,
        );
        let config = WatchLoopConfig {
            discovery: DiscoveryConfig::offline_default(fixture_settlement_asset()),
            selected: vec![SelectedProtocol::AgniV2],
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        };
        let http = ProviderBuilder::new()
            .connect_mocked_client(Asserter::new())
            .erased();
        // Never-ending head stream + immediate shutdown → clean exit.
        let heads = stream::pending::<ObservedHead>();
        let shutdown = async {};
        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            heads,
            Box::pin(shutdown),
            NoopWatchHooks,
            1,
        )
        .await
        .expect("shutdown must yield Ok");
        assert_eq!(stats.block_subscriptions, 1);
        assert_eq!(stats.blocks_processed, 0);
    }

    fn fixture_loop_state_at(block: u64) -> WatchLoopState {
        let pools = cross_protocol_fixture_pools();
        let mut space = StateSpace::default();
        for amm in &pools {
            space.state.insert(amm.address(), amm.clone());
        }
        space.latest_block.store(block, Ordering::Relaxed);
        let latest_block = Arc::clone(&space.latest_block);
        WatchLoopState::new(
            Arc::new(RwLock::new(space)),
            latest_block,
            SnapshotPublisher::new(),
            Filter::new(),
            5000,
        )
    }

    async fn seed_tip(loop_state: &WatchLoopState, number: u64, hash: u8, parent: u8) {
        loop_state
            .snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, number, B256::repeat_byte(hash)),
                BlockHeaderContext::new(B256::repeat_byte(parent), 1_700_000_000 + number),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;
        loop_state.latest_block.store(number, Ordering::Relaxed);
    }

    fn offline_config(attempt: bool) -> WatchLoopConfig {
        let mut discovery = DiscoveryConfig::offline_default(fixture_settlement_asset());
        discovery.gas.gas_price_wei = 0;
        WatchLoopConfig {
            discovery,
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: attempt,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
            skip_ratio_window: DEFAULT_SKIP_RATIO_WINDOW,
            skip_ratio_threshold: DEFAULT_SKIP_RATIO_THRESHOLD,
            send_runtime: None,
            discovery_gas_profile: None,
            discovery_priority_fee_wei: 100_000,
            discovery_block_gas_reserve: 1,
            pool_universe_fingerprint: B256::ZERO,
            attempt_budget: DEFAULT_ATTEMPT_BUDGET,
            capital_policy: None,
        }
    }

    fn mock_block(number: u64, hash: B256) -> alloy::rpc::types::Block {
        mock_block_with_parent(number, hash, B256::repeat_byte(number.saturating_sub(1) as u8))
    }

    fn mock_block_with_parent(
        number: u64,
        hash: B256,
        parent: B256,
    ) -> alloy::rpc::types::Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
        inner.parent_hash = parent;
        inner.timestamp = 1_700_000_000 + number;
        inner.base_fee_per_gas = Some(25);
        inner.gas_limit = 30_000_000;
        let mut header = alloy::rpc::types::Header::new(inner);
        header.hash = hash;
        alloy::rpc::types::Block::empty(header)
    }

    /// WHI-792 AC: cold start whose first head is >CACHE_SIZE past the snapshot
    /// re-baselines and produces a processed block (blocks_processed > 0).
    #[tokio::test]
    async fn cold_start_large_gap_rebaselines_and_processes() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let config = offline_config(false);

        // Gap of 62 (> CACHE_SIZE=30): previously deadlocked forever.
        let head = ObservedHead::new(
            5000,
            72,
            B256::repeat_byte(0x48),
            B256::repeat_byte(0x47),
            1_700_000_072,
        );

        let asserter = Asserter::new();
        for _ in 0..4 {
            asserter.push_success(&Vec::<Log>::new());
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let result = process_observed_head(
            &http,
            &loop_state,
            &config,
            head,
            Some(25),
            30_000_000,
            false, // cold start
        )
        .await
        .expect("process");

        assert_eq!(result.rebaseline, Some(RebaselineKind::ColdStart));
        let tick = result.tick.expect("must process after re-baseline");
        assert_eq!(tick.block_number, 72);
        assert_eq!(
            loop_state.snapshots.last_tip().await.unwrap().block_number,
            72,
            "baseline must advance to the re-baselined tip"
        );
    }

    /// WHI-792 AC: previous baseline advances across successive large-gap heads;
    /// the gap does not grow monotonically because each publish moves previous.
    #[tokio::test]
    async fn large_gap_baseline_advances_so_gap_does_not_grow() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let config = offline_config(false);

        let asserter = Asserter::new();
        for _ in 0..8 {
            asserter.push_success(&Vec::<Log>::new());
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let head1 = ObservedHead::new(
            5000,
            72,
            B256::repeat_byte(0x48),
            B256::repeat_byte(0x47),
            1_700_000_072,
        );
        let r1 = process_observed_head(
            &http,
            &loop_state,
            &config,
            head1,
            Some(25),
            30_000_000,
            false,
        )
        .await
        .expect("first");
        assert_eq!(r1.rebaseline, Some(RebaselineKind::ColdStart));
        assert!(r1.tick.is_some());
        let tip_after_first = loop_state.snapshots.last_tip().await.unwrap().block_number;
        assert_eq!(tip_after_first, 72);

        // Second large gap from the re-baselined tip (another >CACHE_SIZE jump).
        // Old deadlock would keep previous=10 and grow the gap forever.
        let head2 = ObservedHead::new(
            5000,
            140,
            B256::repeat_byte(0x8c),
            B256::repeat_byte(0x8b),
            1_700_000_140,
        );
        let tip_before_second = tip_after_first;
        let gap_before = head2.number.saturating_sub(tip_before_second);
        assert!(
            gap_before > CACHE_SIZE as u64,
            "test setup: second head must also exceed CACHE_SIZE"
        );
        let r2 = process_observed_head(
            &http,
            &loop_state,
            &config,
            head2,
            Some(25),
            30_000_000,
            true,
        )
        .await
        .expect("second");
        assert_eq!(r2.rebaseline, Some(RebaselineKind::MidRun));
        assert!(r2.tick.is_some(), "second head must process");
        let tip_after_second = loop_state.snapshots.last_tip().await.unwrap().block_number;
        assert_eq!(tip_after_second, 140);
        assert!(
            tip_after_second > tip_before_second,
            "baseline must keep advancing ({tip_before_second} → {tip_after_second})"
        );
        // If previous were stuck at 10, gap to a hypothetical third head at 142
        // would be 132 and growing. After re-baseline, gap from tip 140 is 2.
        let stuck_gap_to_next = 142u64.saturating_sub(10);
        let advanced_gap_to_next = 142u64.saturating_sub(tip_after_second);
        assert!(
            advanced_gap_to_next < stuck_gap_to_next,
            "gap must not grow against a stuck previous (advanced={advanced_gap_to_next}, stuck={stuck_gap_to_next})"
        );
    }

    /// WHI-792: consecutive skips with zero processed abort mid-loop (not only at stream end).
    #[tokio::test]
    async fn consecutive_skips_with_zero_processed_abort_mid_loop() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.http_tip_wait = Duration::ZERO;
        config.skip_fatal_window = 3;

        let heads: Vec<ObservedHead> = (0..5)
            .map(|i| {
                ObservedHead::new(
                    5000,
                    11 + i,
                    B256::repeat_byte(0x11 + i as u8),
                    B256::repeat_byte(0x10 + i as u8),
                    1_700_000_011 + i,
                )
            })
            .collect();

        let asserter = Asserter::new();
        for _ in 0..5 {
            asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let err = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(heads),
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            1,
        )
        .await
        .expect_err("must abort on consecutive-skip window");

        let msg = err.to_string();
        assert!(
            msg.contains("consecutive skips") || msg.contains("WHI-792"),
            "got: {msg}"
        );
    }

    /// WHI-792 AC: HTTP lag shorter than the wait deadline yields a processed block.
    #[tokio::test]
    async fn http_tip_lag_within_deadline_processes_block() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.http_tip_wait = Duration::from_millis(500);

        let head = ObservedHead::new(
            5000,
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
            1_700_000_011,
        );
        let block = mock_block(11, B256::repeat_byte(0x11));

        let asserter = Asserter::new();
        // First get_block → None (HTTP lag), then Some, then get_logs empties.
        asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        asserter.push_success(&Some(block));
        for _ in 0..4 {
            asserter.push_success(&Vec::<Log>::new());
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(vec![head]),
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            1,
        )
        .await
        .expect("loop must succeed");

        assert_eq!(stats.blocks_processed, 1);
        assert!(stats.http_tip_waits >= 1);
        assert_eq!(stats.http_tip_timeouts, 0);
        assert_eq!(stats.halted_or_skipped, 0);
    }

    /// WHI-792 AC: zero blocks processed after observing heads is not a clean exit.
    #[tokio::test]
    async fn zero_processed_after_heads_is_not_clean_exit() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        // Immediate skip on missing tip — no wait.
        config.http_tip_wait = Duration::ZERO;
        config.skip_fatal_window = 0; // exercise stream-end fatality, not mid-window

        let heads: Vec<ObservedHead> = (0..3)
            .map(|i| {
                ObservedHead::new(
                    5000,
                    11 + i,
                    B256::repeat_byte(0x11 + i as u8),
                    B256::repeat_byte(0x10 + i as u8),
                    1_700_000_011 + i,
                )
            })
            .collect();

        let asserter = Asserter::new();
        // Every get_block_by_hash returns None → all heads timeout/skip.
        for _ in 0..3 {
            asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let err = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(heads),
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            1,
        )
        .await
        .expect_err("must refuse clean exit");

        let msg = err.to_string();
        assert!(
            msg.contains("processed zero") || msg.contains("WHI-792"),
            "got: {msg}"
        );
    }

    // -----------------------------------------------------------------------
    // WHI-762 — pin-by-hash, fail-closed skip, skip-ratio, http-poll
    // -----------------------------------------------------------------------

    #[test]
    fn skip_ratio_tracker_fires_when_threshold_exceeded() {
        let mut tracker = SkipRatioTracker::new(4, 0.5);
        assert!(!tracker.record(true)); // 1/1 — window not full
        assert!(!tracker.record(true));
        assert!(!tracker.record(true));
        // 4/4 skips → 1.0 > 0.5
        assert!(tracker.record(true));
        assert_eq!(tracker.skip_count(), 4);
    }

    #[test]
    fn skip_ratio_tracker_does_not_fire_when_under_threshold() {
        let mut tracker = SkipRatioTracker::new(4, 0.5);
        assert!(!tracker.record(false));
        assert!(!tracker.record(false));
        assert!(!tracker.record(true));
        // 1/4 = 0.25 ≤ 0.5
        assert!(!tracker.record(false));
    }

    #[test]
    fn head_source_parse_accepts_ws_and_http_poll() {
        assert_eq!(HeadSource::parse("ws").unwrap(), HeadSource::Ws);
        assert_eq!(HeadSource::parse("http-poll").unwrap(), HeadSource::HttpPoll);
        assert!(HeadSource::parse("garbage").is_err());
    }

    /// WHI-762 AC: unknown announced hash → skip, no candidates; pin_skips readable.
    ///
    /// One successful head then one unknown hash so the loop exits Ok and
    /// `WatchLoopStats.pin_skips` is returned.
    #[tokio::test]
    async fn unknown_announced_hash_skips_without_candidates() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(true);
        config.http_tip_wait = Duration::ZERO;
        config.skip_fatal_window = 0;

        let good = ObservedHead::new(
            5000,
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
            1_700_000_011,
        );
        let unknown = ObservedHead::new(
            5000,
            12,
            B256::repeat_byte(0x12),
            B256::repeat_byte(0x11),
            1_700_000_012,
        );

        let asserter = Asserter::new();
        // Good head: hash header + empty logs.
        asserter.push_success(&Some(mock_block_with_parent(
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
        )));
        asserter.push_success(&Vec::<Log>::new());
        // Unknown hash → None (pin skip).
        asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let ready = Arc::new(AtomicU64::new(0));
        let attempts = Arc::new(AtomicU64::new(0));
        struct CountingHooks {
            ready: Arc<AtomicU64>,
            attempts: Arc<AtomicU64>,
        }
        impl WatchLoopHooks for CountingHooks {
            fn on_block_ready(&mut self, _tick: &BlockTick) -> Result<()> {
                self.ready.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            fn on_attempt(
                &mut self,
                _opp: &crate::service::discovery::DiscoveredOpportunity,
                _attempt: &ExecutionAttempt,
            ) -> Result<()> {
                self.attempts.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        }

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(vec![good, unknown]),
            Box::pin(std::future::pending::<()>()),
            CountingHooks {
                ready: Arc::clone(&ready),
                attempts: Arc::clone(&attempts),
            },
            1,
        )
        .await
        .expect("one processed head allows clean exit");

        assert_eq!(stats.blocks_processed, 1);
        assert!(
            stats.pin_skips >= 1,
            "pin_skips must be readable, got {}",
            stats.pin_skips
        );
        assert_eq!(ready.load(Ordering::Relaxed), 1, "only the known head is ready");
        // Attempts only for the processed head (fixture may or may not find a path).
        assert!(
            attempts.load(Ordering::Relaxed) <= 1,
            "unknown head must not emit an extra attempt"
        );
    }

    /// WHI-762 AC: one-block-behind HTTP (hash unknown) → zero quotes, pin counter.
    #[tokio::test]
    async fn one_block_behind_http_produces_zero_quotes_and_counts_pin_skip() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(true);
        config.http_tip_wait = Duration::ZERO;
        config.skip_fatal_window = 0;

        // Process #11, then announce #12 while HTTP only knows #11 → pin skip.
        let heads = vec![
            ObservedHead::new(
                5000,
                11,
                B256::repeat_byte(0x11),
                B256::repeat_byte(0x10),
                1_700_000_011,
            ),
            ObservedHead::new(
                5000,
                12,
                B256::repeat_byte(0x12),
                B256::repeat_byte(0x11),
                1_700_000_012,
            ),
        ];

        let asserter = Asserter::new();
        asserter.push_success(&Some(mock_block_with_parent(
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
        )));
        asserter.push_success(&Vec::<Log>::new());
        // One block behind: announced 0x12 is unknown.
        asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let ready_blocks = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
        struct RecordReady {
            blocks: Arc<std::sync::Mutex<Vec<u64>>>,
        }
        impl WatchLoopHooks for RecordReady {
            fn on_block_ready(&mut self, tick: &BlockTick) -> Result<()> {
                self.blocks.lock().unwrap().push(tick.block_number);
                Ok(())
            }
            fn on_attempt(
                &mut self,
                _opp: &crate::service::discovery::DiscoveredOpportunity,
                _attempt: &ExecutionAttempt,
            ) -> Result<()> {
                Ok(())
            }
        }

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(heads),
            Box::pin(std::future::pending::<()>()),
            RecordReady {
                blocks: Arc::clone(&ready_blocks),
            },
            1,
        )
        .await
        .expect("one processed allows clean exit");

        let ready = ready_blocks.lock().unwrap().clone();
        assert_eq!(ready, vec![11], "must not quote the behind tip #12");
        assert_eq!(stats.blocks_processed, 1);
        assert!(
            stats.pin_skips >= 1,
            "pin_skips must count the behind head, got {}",
            stats.pin_skips
        );
    }

    /// WHI-762 AC: hash-pinned header load uses get_block_by_hash (not number/latest).
    ///
    /// Method-aware transport returns different base fees for hash vs latest/number.
    /// The load path must surface the pin fee.
    #[tokio::test]
    async fn header_load_uses_pinned_hash_not_latest() {
        use alloy::transports::{TransportError, TransportErrorKind, TransportFut};
        use alloy_json_rpc::{RequestPacket, Response, ResponsePacket};
        use std::task::{Context as TaskContext, Poll};
        use tower::Service;

        const PINNED_FEE: u64 = 111;
        const LATEST_FEE: u64 = 999;
        let pin_hash = B256::repeat_byte(0xAB);

        #[derive(Clone, Debug)]
        struct PinDispatchTransport {
            pin_hash: B256,
        }

        impl Service<RequestPacket> for PinDispatchTransport {
            type Response = ResponsePacket;
            type Error = TransportError;
            type Future = TransportFut<'static>;

            fn poll_ready(
                &mut self,
                _cx: &mut TaskContext<'_>,
            ) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, request: RequestPacket) -> Self::Future {
                let pin_hash = self.pin_hash;
                Box::pin(async move {
                    let req = match request {
                        RequestPacket::Single(r) => r,
                        RequestPacket::Batch(_) => {
                            return Err(TransportErrorKind::custom_str("batch not supported"));
                        }
                    };
                    let method = req.method().to_string();
                    let params = req.params().map(|p| p.get()).unwrap_or("[]");
                    let id = req.id().clone();

                    let fee = if method == "eth_getBlockByHash" {
                        // Only the announced hash is the pin path.
                        if params.contains(&format!("{pin_hash:#x}"))
                            || params.contains(&format!("{pin_hash:x}"))
                            || params.contains(&pin_hash.to_string())
                        {
                            PINNED_FEE
                        } else {
                            LATEST_FEE
                        }
                    } else if method == "eth_getBlockByNumber" {
                        // latest / bare number — deliberately different fee.
                        LATEST_FEE
                    } else {
                        return Err(TransportErrorKind::custom_str(&format!(
                            "unexpected method {method}"
                        )));
                    };

                    let mut inner = alloy::consensus::Header::default();
                    inner.number = 11;
                    inner.base_fee_per_gas = Some(fee);
                    inner.gas_limit = 30_000_000;
                    let mut header = alloy::rpc::types::Header::new(inner);
                    header.hash = if fee == PINNED_FEE {
                        pin_hash
                    } else {
                        B256::repeat_byte(0xFF)
                    };
                    let block: alloy::rpc::types::Block = alloy::rpc::types::Block::empty(header);
                    let body = serde_json::to_string(&Some(block))
                        .map_err(|e| TransportErrorKind::custom_str(&e.to_string()))?;
                    let payload = alloy_json_rpc::ResponsePayload::Success(
                        serde_json::value::RawValue::from_string(body)
                            .map_err(|e| TransportErrorKind::custom_str(&e.to_string()))?,
                    );
                    Ok(ResponsePacket::Single(Response { id, payload }))
                })
            }
        }

        let client = alloy::rpc::client::ClientBuilder::default()
            .transport(PinDispatchTransport { pin_hash }, true);
        let http = ProviderBuilder::new().connect_client(client).erased();

        let load = load_canonical_header_with_wait(&http, 11, pin_hash, Duration::ZERO).await;
        match load {
            CanonicalHeaderLoad::Ready { base_fee, .. } => {
                assert_eq!(
                    base_fee,
                    Some(PINNED_FEE),
                    "must use hash-pinned base fee, not latest ({LATEST_FEE})"
                );
            }
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    /// WHI-762 AC: hash-pinned get_logs failure skips (no number-range success).
    #[tokio::test]
    async fn hash_logs_failure_skips_without_emitting_tick() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let config = offline_config(true);

        let head = ObservedHead::new(
            5000,
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
            1_700_000_011,
        );

        let asserter = Asserter::new();
        // Hash-pinned get_logs fails. Under the old code a number-range empty
        // success would have produced a tick; now we must skip.
        asserter.push_failure_msg("unknown block hash");
        // Poison: if number-range fallback still exists it would consume this.
        asserter.push_success(&Vec::<Log>::new());
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let result = process_observed_head(
            &http,
            &loop_state,
            &config,
            head,
            Some(25),
            30_000_000,
            false,
        )
        .await
        .expect("pin skip is Ok(skipped), not Err");

        assert!(result.tick.is_none(), "must not emit a tick/candidates");
        assert_eq!(
            result.skip_reason,
            Some(BlockSkipReason::PinnedLogsUnavailable)
        );
    }

    /// WHI-762 AC: rolling skip-ratio warning fires inside the watch loop.
    #[tokio::test]
    async fn rolling_skip_ratio_warning_increments_stats() {
        // 1 process + 4 pin-skips so stream-end is Ok (blocks_processed > 0).
        // Window=4, threshold=0.5 → once the window is full of mostly skips, fires.
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.http_tip_wait = Duration::ZERO;
        config.skip_fatal_window = 0;
        config.skip_ratio_window = 4;
        config.skip_ratio_threshold = 0.5;

        let good = ObservedHead::new(
            5000,
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
            1_700_000_011,
        );
        let mut heads = vec![good];
        for i in 0..4u64 {
            heads.push(ObservedHead::new(
                5000,
                100 + i,
                B256::repeat_byte(0x50 + i as u8),
                B256::repeat_byte(0x4f + i as u8),
                1_700_000_100 + i,
            ));
        }

        let asserter = Asserter::new();
        // First head: get_block_by_hash success + get_logs empty.
        asserter.push_success(&Some(mock_block(11, B256::repeat_byte(0x11))));
        asserter.push_success(&Vec::<Log>::new());
        // Four unknown hashes.
        for _ in 0..4 {
            asserter.push_success(&Option::<alloy::rpc::types::Block>::None);
        }
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            stream::iter(heads),
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            1,
        )
        .await
        .expect("at least one processed → clean exit");

        assert_eq!(stats.blocks_processed, 1);
        assert!(stats.pin_skips >= 4);
        assert!(
            stats.skip_ratio_warnings >= 1,
            "rolling skip-ratio warning must fire, got {}",
            stats.skip_ratio_warnings
        );
    }

    /// WHI-762 AC: `--head-source http-poll` multi-block run with no WS.
    ///
    /// `poll_heads_http` itself emits two heads from scripted HTTP (no WS
    /// transport), then the watch loop processes them under hash pins.
    #[tokio::test]
    async fn http_poll_heads_multi_block_without_ws() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.http_tip_wait = Duration::from_millis(50);

        // Shared asserter: FIFO is interleaved (poll emit → process → poll emit → process).
        let asserter = Asserter::new();
        // poll emit #11: eth_blockNumber + get_block_by_number
        asserter.push_success(&11u64);
        asserter.push_success(&Some(mock_block_with_parent(
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
        )));
        // process #11: get_block_by_hash + get_logs
        asserter.push_success(&Some(mock_block_with_parent(
            11,
            B256::repeat_byte(0x11),
            B256::repeat_byte(0x10),
        )));
        asserter.push_success(&Vec::<Log>::new());
        // poll emit #12
        asserter.push_success(&12u64);
        asserter.push_success(&Some(mock_block_with_parent(
            12,
            B256::repeat_byte(0x12),
            B256::repeat_byte(0x11),
        )));
        // process #12
        asserter.push_success(&Some(mock_block_with_parent(
            12,
            B256::repeat_byte(0x12),
            B256::repeat_byte(0x11),
        )));
        asserter.push_success(&Vec::<Log>::new());

        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        // Take exactly two heads from the HTTP poller — no WS involved.
        let head_sub = poll_heads_http(http.clone(), 5000, Duration::from_millis(5));
        assert_eq!(head_sub.subscription_count, 1);
        let heads = head_sub.stream.take(2);

        let stats = run_multi_protocol_watch_loop(
            http,
            loop_state,
            config,
            heads,
            Box::pin(std::future::pending::<()>()),
            NoopWatchHooks,
            1,
        )
        .await
        .expect("http-poll multi-block must succeed without WS");

        assert_eq!(stats.blocks_processed, 2);
        assert_eq!(stats.block_subscriptions, 1);
        assert_eq!(stats.pin_skips, 0);
    }

    /// WHI-885: consecutive advance after a processed block uses the dirty set.
    #[test]
    fn tip_refresh_scope_advance_is_touched() {
        let obs = HeadObservation::Assemble(AssembleKind::Advance);
        assert!(!tip_refresh_requires_full(&obs, true));
        assert!(tip_refresh_full_reason(&obs, true).is_none());
        assert!(tip_refresh_gap_log_range(&obs, true).is_none());
        let dirty = vec![Address::repeat_byte(0xaa)];
        let scope = tip_refresh_scope_for_head(false, &dirty);
        match scope {
            TipRefreshScope::Touched(set) => {
                assert!(set.contains(&Address::repeat_byte(0xaa)));
                assert_eq!(set.len(), 1);
            }
            TipRefreshScope::Full => panic!("advance must use Touched scope"),
        }
    }

    /// WHI-885: first processed head always full-refreshes (covers pre-watch
    /// tip advance that skipped pool re-init).
    #[test]
    fn tip_refresh_scope_first_block_is_full() {
        let obs = HeadObservation::Assemble(AssembleKind::Advance);
        assert!(tip_refresh_requires_full(&obs, false));
        assert_eq!(
            tip_refresh_full_reason(&obs, false),
            Some(TipRefreshFullReason::ColdStart)
        );
        assert!(matches!(
            tip_refresh_scope_for_head(true, &[]),
            TipRefreshScope::Full
        ));
    }

    fn backfill_obs(previous: u64, head: u64) -> HeadObservation {
        HeadObservation::Backfill {
            previous: crate::state_space::SnapshotTip {
                id: SnapshotId::new(5000, previous, B256::repeat_byte(2)),
                header: BlockHeaderContext::new(B256::ZERO, 1),
            },
            observed: ObservedHead::new(
                5000,
                head,
                B256::repeat_byte(1),
                B256::repeat_byte(0),
                1,
            ),
        }
    }

    /// WHI-893: small gaps no longer force Full; range logs widen the dirty set.
    #[test]
    fn tip_refresh_scope_small_gap_is_touched_not_full() {
        // gap of 16 (matches the live deadlock shape) within CACHE_SIZE=30.
        let backfill = backfill_obs(98940680, 98940696);
        assert_eq!(tip_refresh_gap_size(&backfill), Some(16));
        assert!(!tip_refresh_requires_full(&backfill, true));
        assert!(tip_refresh_full_reason(&backfill, true).is_none());
        assert_eq!(
            tip_refresh_gap_log_range(&backfill, true),
            Some((98940681, 98940696))
        );
        let dirty = vec![Address::repeat_byte(0xaa)];
        match tip_refresh_scope_for_head(false, &dirty) {
            TipRefreshScope::Touched(set) => {
                assert!(set.contains(&Address::repeat_byte(0xaa)));
            }
            TipRefreshScope::Full => panic!("small gap must use Touched after range logs"),
        }
    }

    /// WHI-893: large gap / bootstrap still force Full.
    #[test]
    fn tip_refresh_scope_large_gap_and_bootstrap_are_full() {
        // gap of 50 > CACHE_SIZE=30
        let large = backfill_obs(10, 60);
        assert!(tip_refresh_requires_full(&large, true));
        assert_eq!(
            tip_refresh_full_reason(&large, true),
            Some(TipRefreshFullReason::LargeGap)
        );
        assert!(tip_refresh_gap_log_range(&large, true).is_none());

        assert!(tip_refresh_requires_full(
            &HeadObservation::Assemble(AssembleKind::Bootstrap),
            true
        ));
        assert_eq!(
            tip_refresh_full_reason(
                &HeadObservation::Assemble(AssembleKind::Bootstrap),
                true
            ),
            Some(TipRefreshFullReason::Bootstrap)
        );
        assert!(matches!(
            tip_refresh_scope_for_head(true, &[Address::repeat_byte(1)]),
            TipRefreshScope::Full
        ));
    }

    /// WHI-893 AC: large-gap Full plan includes every Moe pool.
    #[test]
    fn large_gap_full_plan_includes_every_moe_pool() {
        use crate::service::fixture::{
            cross_protocol_fixture_pools, fixture_moe_pool_address,
        };
        use crate::service::protocol::plan_moe_tip_refresh;

        let pools = cross_protocol_fixture_pools();
        // gap 40 > CACHE_SIZE
        let force_full = tip_refresh_requires_full(&backfill_obs(10, 50), true);
        assert!(force_full);
        let scope = tip_refresh_scope_for_head(force_full, &[]);
        let plan = plan_moe_tip_refresh(&pools, &scope);
        assert_eq!(plan.mode, "full");
        assert_eq!(plan.held, 0);
        assert!(
            plan.to_refresh.contains(&fixture_moe_pool_address()),
            "large-gap full refresh must re-sync the Moe fixture pool"
        );
        assert_eq!(plan.to_refresh.len(), 1);
    }

    /// WHI-893 AC: a gap of N refreshes exactly the pools touched in the range.
    #[test]
    fn small_gap_plan_refreshes_only_range_touched_pools() {
        use crate::service::fixture::cross_protocol_fixture_pools;
        use crate::service::protocol::plan_moe_tip_refresh;
        use alloy::primitives::address;

        let pools = cross_protocol_fixture_pools();
        let moe = match pools.iter().find(|a| matches!(a, AMM::MoeLbPair(_))) {
            Some(AMM::MoeLbPair(p)) => p.address,
            _ => panic!("fixture must include a Moe pool"),
        };
        let untouched = address!("2222222222222222222222222222222222222222");
        // Simulate gap-range logs that touched only the fixture Moe pool.
        let gap_addrs = vec![moe, untouched];
        let dirty = union_tip_refresh_dirty(&[], &gap_addrs);
        assert!(!tip_refresh_requires_full(&backfill_obs(10, 20), true));
        let scope = tip_refresh_scope_for_head(false, &dirty);
        let plan = plan_moe_tip_refresh(&pools, &scope);
        assert_eq!(plan.mode, "touched");
        assert_eq!(plan.to_refresh, vec![moe]);
        assert_eq!(plan.held, 0); // fixture has one Moe pool
    }

    /// WHI-893 AC: failed range-log fetch falls back to Full, not empty Touched.
    #[test]
    fn range_logs_failed_forces_full_not_empty_touched() {
        use crate::service::fixture::{
            cross_protocol_fixture_pools, fixture_moe_pool_address,
        };
        use crate::service::protocol::plan_moe_tip_refresh;

        // Pure seam used by process_observed_head: range Err → Full with reason.
        let (force_full, reason, dirty) = apply_gap_range_to_tip_refresh(
            false,
            None,
            &[],
            Some(Err(())),
        );
        assert!(force_full);
        assert_eq!(reason, Some(TipRefreshFullReason::RangeLogsFailed));
        assert_eq!(reason.unwrap().as_str(), "range_logs_failed");
        // Empty head_affected must not become an empty Touched dirty set.
        let scope = tip_refresh_scope_for_head(force_full, &dirty);
        assert!(matches!(scope, TipRefreshScope::Full));
        let plan = plan_moe_tip_refresh(&cross_protocol_fixture_pools(), &scope);
        assert_eq!(plan.mode, "full");
        assert!(plan.to_refresh.contains(&fixture_moe_pool_address()));
        assert_eq!(plan.held, 0);
    }

    /// WHI-893 AC: successful range fetch unions addresses into Touched dirty set.
    #[test]
    fn apply_gap_range_success_unions_dirty_for_touched_plan() {
        use crate::service::fixture::{
            cross_protocol_fixture_pools, fixture_moe_pool_address,
        };
        use crate::service::protocol::plan_moe_tip_refresh;

        let moe = fixture_moe_pool_address();
        let (force_full, reason, dirty) = apply_gap_range_to_tip_refresh(
            false,
            None,
            &[],
            Some(Ok(vec![moe, Address::repeat_byte(0xee)])),
        );
        assert!(!force_full);
        assert!(reason.is_none());
        assert!(dirty.contains(&moe));
        let scope = tip_refresh_scope_for_head(force_full, &dirty);
        let plan = plan_moe_tip_refresh(&cross_protocol_fixture_pools(), &scope);
        assert_eq!(plan.mode, "touched");
        assert_eq!(plan.to_refresh, vec![moe]);
    }

    /// WHI-893 AC: gap-range logs with no Moe addresses → zero Moe bin RPC.
    #[tokio::test]
    async fn small_gap_no_moe_touch_issues_zero_rpc() {
        use crate::amms::moe::{
            BinReserve, MoeBinRange, MoeLbPair, MoeSnapshot, MoeSnapshotContext,
        };
        use crate::amms::Token;
        use crate::service::protocol::{plan_moe_tip_refresh, MoeProtocol};
        use alloy::primitives::address;
        use alloy::providers::{DynProvider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        fn moe_pool(addr: Address, timestamp: u64) -> AMM {
            let mut pair = MoeLbPair::new(addr);
            pair.token_x = Token::new_with_decimals(Address::repeat_byte(0x10), 18);
            pair.token_y = Token::new_with_decimals(Address::repeat_byte(0x20), 18);
            pair.active_id = 2u32.pow(23);
            pair.bin_step = 25;
            pair.reserve_x = 1_000_000_000_000_000_000;
            pair.reserve_y = 1_000_000;
            pair.protocol_share_bps = 100;
            pair.max_volatility_acc = 250_000;
            pair.bins.insert(
                pair.active_id,
                BinReserve {
                    reserve_x: pair.reserve_x,
                    reserve_y: pair.reserve_y,
                },
            );
            let range = MoeBinRange::new(
                pair.active_id.saturating_sub(10),
                pair.active_id.saturating_add(10),
            );
            let snapshot = MoeSnapshot::new(
                pair.snapshot_slot0(),
                pair.bins.clone(),
                vec![range],
                MoeSnapshotContext::new(B256::repeat_byte(1), timestamp),
            )
            .expect("snapshot");
            pair.install_snapshot(snapshot).expect("install");
            AMM::MoeLbPair(pair)
        }

        let timestamp = 1_700_000_000u64;
        let moe_a = address!("1111111111111111111111111111111111111111");
        let moe_b = address!("2222222222222222222222222222222222222222");
        // Gap logs touch only a non-Moe address → dirty set has no Moe pools.
        let gap_only = vec![address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")];
        let dirty = union_tip_refresh_dirty(&[], &gap_only);
        let scope = tip_refresh_scope_for_head(false, &dirty);
        assert_eq!(scope.as_metric_label(), "touched");
        // mode="full" must not appear on a small-gap advance with a known dirty set.
        assert!(!tip_refresh_requires_full(&backfill_obs(10, 20), true));

        let mut pools = vec![moe_pool(moe_a, timestamp), moe_pool(moe_b, timestamp)];
        let plan = plan_moe_tip_refresh(&pools, &scope);
        assert!(plan.to_refresh.is_empty());
        assert_eq!(plan.held, 2);
        assert_eq!(plan.mode, "touched");

        let asserter = Asserter::new();
        // No responses — any bin RPC would fail the test.
        let provider = DynProvider::new(
            ProviderBuilder::new()
                .connect_mocked_client(asserter)
                .erased(),
        );
        let header = BlockHeaderContext::new(B256::ZERO, timestamp);
        MoeProtocol::new()
            .refresh_block_tip_state(
                &provider,
                &mut pools,
                B256::repeat_byte(0xab),
                &header,
                &scope,
            )
            .await
            .expect("no Moe dirty set must short-circuit without RPC");
    }

    /// WHI-893 AC: mid-run small gap with range-log failure arms Full.
    ///
    /// With a Moe pool in state and no bin-RPC mocks, Full must attempt a chain
    /// read and fail closed (`TipRefreshFailed`). Empty Touched would short-circuit
    /// with zero RPC and produce a tick — so a TipRefreshFailed skip proves Full.
    #[tokio::test]
    async fn small_gap_range_logs_failure_arms_full_tip_refresh() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.refresh_tip_state = true;
        // Fixture pools include one Moe pool → Full attempts bin RPC (no mocks).
        // Do not clear state.

        // Gap of 6 from tip 10 → head 16 (within CACHE_SIZE).
        let head = ObservedHead::new(
            5000,
            16,
            B256::repeat_byte(0x16),
            B256::repeat_byte(0x15),
            1_700_000_016,
        );

        let asserter = Asserter::new();
        // 1) hash-pinned head get_logs succeeds (empty).
        asserter.push_success(&Vec::<Log>::new());
        // 2) gap-range get_logs fails → Full fallback.
        asserter.push_failure_msg("range query timeout");
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let result = process_observed_head(
            &http,
            &loop_state,
            &config,
            head,
            Some(25),
            30_000_000,
            true, // mid-run
        )
        .await
        .expect("range-log failure must not abort with Err");

        assert!(
            result.tick.is_none(),
            "Full tip refresh without bin mocks must fail closed"
        );
        assert_eq!(
            result.skip_reason,
            Some(BlockSkipReason::TipRefreshFailed),
            "range-log fail → Full → bin RPC attempted (not empty Touched short-circuit)"
        );
        assert!(result.rebaseline.is_none(), "small gap is not a re-baseline");
    }

    /// WHI-893: mid-run small gap with successful range logs processes without Full.
    #[tokio::test]
    async fn small_gap_range_logs_success_processes() {
        let loop_state = fixture_loop_state_at(10);
        seed_tip(&loop_state, 10, 0x10, 0x0f).await;
        let mut config = offline_config(false);
        config.refresh_tip_state = true;
        {
            let mut guard = loop_state.state.write().await;
            guard.state.clear();
        }

        let head = ObservedHead::new(
            5000,
            16,
            B256::repeat_byte(0x16),
            B256::repeat_byte(0x15),
            1_700_000_016,
        );

        let touched = Address::repeat_byte(0xaa);
        let range_log = Log {
            inner: alloy::primitives::Log {
                address: touched,
                data: alloy::primitives::LogData::empty(),
            },
            block_hash: None,
            block_number: Some(12),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };

        let asserter = Asserter::new();
        // head get_logs (hash-pinned)
        asserter.push_success(&Vec::<Log>::new());
        // gap-range get_logs (dirty set only)
        asserter.push_success(&vec![range_log]);
        let http = ProviderBuilder::new()
            .connect_mocked_client(asserter)
            .erased();

        let result = process_observed_head(
            &http,
            &loop_state,
            &config,
            head,
            Some(25),
            30_000_000,
            true,
        )
        .await
        .expect("small gap with range logs must process");

        assert!(result.tick.is_some());
        assert_eq!(result.tick.unwrap().block_number, 16);
        // Range logs must not have mutated StateSpace (empty universe stayed empty).
        let guard = loop_state.state.read().await;
        assert!(
            guard.state.is_empty(),
            "range logs must not enter StateSpace::sync"
        );
    }

    /// WHI-893: dirty-set helpers union without duplicates; order is stable.
    #[test]
    fn dirty_set_union_and_log_addresses() {
        let a = Address::repeat_byte(0x01);
        let b = Address::repeat_byte(0x02);
        let c = Address::repeat_byte(0x03);
        let head = vec![a, b];
        let gap = vec![b, c];
        assert_eq!(union_tip_refresh_dirty(&head, &gap), vec![a, b, c]);

        let log_a = Log {
            inner: alloy::primitives::Log {
                address: a,
                data: alloy::primitives::LogData::empty(),
            },
            block_hash: None,
            block_number: Some(11),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        let log_a2 = Log {
            inner: alloy::primitives::Log {
                address: a,
                data: alloy::primitives::LogData::empty(),
            },
            block_hash: None,
            block_number: Some(12),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        assert_eq!(dirty_addresses_from_logs(&[log_a, log_a2]), vec![a]);
    }

    /// WHI-893 AC: range logs are never a state-mutation path — only dirty-set.
    ///
    /// Documents the boundary: `fetch_logs_for_range` is separate from
    /// `fetch_logs_for_head`; process_observed_head applies only head logs to
    /// StateSpace::sync. This pure test locks the helper surface.
    #[test]
    fn range_log_addresses_do_not_imply_state_sync_api() {
        // The public dirty helpers accept bare addresses, not Log → StateSpace.
        // If someone re-routes range logs into sync, they must change this seam.
        let gap_addrs = dirty_addresses_from_logs(&[]);
        let dirty = union_tip_refresh_dirty(&[], &gap_addrs);
        let scope = tip_refresh_scope_for_head(false, &dirty);
        assert!(matches!(scope, TipRefreshScope::Touched(set) if set.is_empty()));
    }
}
