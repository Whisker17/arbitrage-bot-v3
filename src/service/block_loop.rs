//! Shared block-subscription job-slot / multi-protocol watch loop (WHI-727 / WHI-741).
//!
//! Job-slot primitives (latest-wins) remain the execution handoff. WHI-741 adds the
//! continuous multi-protocol subscribe/worker path: **one** block subscription drives
//! all selected protocols against a single shared [`StateSpace`].
//!
//! ## Reorg policy
//!
//! Log application goes through [`StateSpace::sync`], which uses the
//! [`StateChangeCache`] ring buffer (`CACHE_SIZE = 30`) for shallow rollbacks.
//! Continuity classification uses [`SnapshotPublisher::observe_head`].
//!
//! **Numeric gaps** (head number jumps ahead of the last published tip) are **not**
//! reorgs: nothing needs unwinding. Small gaps (`<= CACHE_SIZE`) apply the observed
//! tip only. Large gaps **re-baseline** at the observed tip (publish a fresh
//! snapshot so `previous` advances) — cold-start latency past the cache window is
//! the common case (WHI-792). Mid-run large gaps get louder logging but the same
//! re-baseline, so the loop never deadlocks on a stuck baseline.
//!
//! **True reorgs** (fork / height rollback / wrong parent / same-height
//! replacement) still Halt via the publisher. A reorg deeper than the cache
//! refuses `StateSpace::sync` and skips; full deep-reorg unwinding is owned by
//! WHI-533 and is deliberately out of scope here.
//!
//! ## Head assembly vs `StateSpaceManager::subscribe`
//!
//! Live `--watch` uses dual providers (WS heads + HTTP logs) matching the three
//! example services, because Mantle WS endpoints often whitelist only
//! `eth_subscribe`. That means this module intentionally does **not** call
//! [`crate::state_space::StateSpaceManager::subscribe`] (single-provider
//! assemble/backfill). When HTTP lags the WS tip, the loop waits up to
//! [`WatchLoopConfig::http_tip_wait`] before skipping (WHI-792 / WHI-762).

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::execution::LatestWinsSlot;
use crate::service::discovery::{
    attempt_discovered_via_job_slot, discover_opportunities, AttemptJobContext, DiscoveryConfig,
    DiscoveredOpportunity,
};
use crate::service::gas::GasConfig;
use crate::service::protocol::{
    AgniV2Protocol, AgniV3Protocol, ExecutionAttempt, MoeProtocol, Protocol,
};
use crate::service::select::SelectedProtocol;
use crate::state_space::{
    hash_pinned_logs_filter, BlockHeaderContext, HeadObservation, MarketSnapshot, ObservedHead,
    ProtocolCoverage, SnapshotId, SnapshotPublisher, SnapshotStatus, StateSpace, CACHE_SIZE,
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
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

/// Poll interval used by the v3/moe execution workers when the slot is empty.
pub const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Default HTTP tip catch-up wait (WHI-792). Well under Mantle ~2s block time.
pub const DEFAULT_HTTP_TIP_WAIT: Duration = Duration::from_millis(800);

/// Poll interval while waiting for HTTP to observe a WS tip.
const HTTP_TIP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Consecutive skips that abort the watch loop as unhealthy (WHI-792).
pub const DEFAULT_SKIP_FATAL_WINDOW: u64 = 16;

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
/// use the separate `metrics::stage::*` vocabulary (discovery/optimize/preflight).
pub mod stages {
    pub const BLOCK_OBSERVED: &str = "block_observed";
    pub const STATE_APPLIED: &str = "state_applied";
    pub const TIP_REFRESHED: &str = "tip_refreshed";
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
}

/// Result of applying one head through [`process_observed_head`].
#[derive(Debug)]
pub struct ProcessHeadResult {
    pub tick: Option<BlockTick>,
    pub rebaseline: Option<RebaselineKind>,
}

impl ProcessHeadResult {
    fn skipped() -> Self {
        Self {
            tick: None,
            rebaseline: None,
        }
    }

    fn processed(tick: BlockTick) -> Self {
        Self {
            tick: Some(tick),
            rebaseline: None,
        }
    }

    fn rebaselined(kind: RebaselineKind, tick: BlockTick) -> Self {
        Self {
            tick: Some(tick),
            rebaseline: Some(kind),
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
}

/// Knobs for continuous multi-protocol discovery across blocks.
#[derive(Debug, Clone)]
pub struct WatchLoopConfig {
    pub discovery: DiscoveryConfig,
    pub selected: Vec<SelectedProtocol>,
    /// When true, run `attempt_discovered_via_job_slot` for the best candidate each
    /// block (signerless → `ProductionGateBlocked`).
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
}

impl WatchLoopConfig {
    /// Offline / test defaults: no tip refresh, short HTTP wait, fatal skip window on.
    pub fn offline(discovery: DiscoveryConfig, selected: Vec<SelectedProtocol>) -> Self {
        Self {
            discovery,
            selected,
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
        }
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
/// Mutates `pools` in place. Moe re-syncs LB snapshots; V2/V3 no-op by default.
pub async fn refresh_selected_tip_state(
    provider: &DynProvider,
    pools: &mut [AMM],
    selected: &[SelectedProtocol],
    block_hash: B256,
    header: &BlockHeaderContext,
) -> Result<()> {
    for proto in selected {
        match proto {
            SelectedProtocol::AgniV2 => {
                AgniV2Protocol::new(Address::ZERO)
                    .refresh_block_tip_state(provider, pools, block_hash, header)
                    .await
                    .map_err(|e| eyre!("agni-v2 tip refresh: {e}"))?;
            }
            SelectedProtocol::AgniV3 => {
                AgniV3Protocol::new(Address::ZERO)
                    .refresh_block_tip_state(provider, pools, block_hash, header)
                    .await
                    .map_err(|e| eyre!("agni-v3 tip refresh: {e}"))?;
            }
            SelectedProtocol::Moe => {
                MoeProtocol::new()
                    .refresh_block_tip_state(provider, pools, block_hash, header)
                    .await
                    .map_err(|e| eyre!("moe tip refresh: {e}"))?;
            }
        }
    }
    Ok(())
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
    info!(
        target: "service.block_loop",
        stage = stages::BLOCK_OBSERVED,
        block = head.number,
        hash = %head.hash,
        "observed multi-protocol head"
    );

    let mut rebaseline: Option<RebaselineKind> = None;
    let observation = loop_state.snapshots.observe_head(&head).await;
    match observation {
        HeadObservation::Duplicate => {
            info!(
                target: "service.block_loop",
                block = head.number,
                "duplicate head; skipping"
            );
            return Ok(ProcessHeadResult::skipped());
        }
        HeadObservation::Halted(reason) => {
            warn!(
                target: "service.block_loop",
                block = head.number,
                %reason,
                "head continuity halted (fork); discovery skipped — deep recovery is WHI-533"
            );
            return Ok(ProcessHeadResult::skipped());
        }
        HeadObservation::Backfill { previous, .. } => {
            let gap = head.number.saturating_sub(previous.id.block_number);
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
                // Small gap: fall through and apply the observed tip only. Intermediate
                // blocks are best-effort missing; StateSpace::sync handles shallow reorg.
                warn!(
                    target: "service.block_loop",
                    block = head.number,
                    previous = previous.id.block_number,
                    gap,
                    "small gap backfill: applying observed tip only"
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
        return Ok(ProcessHeadResult::skipped());
    }

    let logs = fetch_logs_for_head(http, &loop_state.block_filter, &head)
        .await
        .context("fetch logs for multi-protocol head")?;

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

    info!(
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
    if config.refresh_tip_state {
        // Tip refresh is best-effort per block: a single protocol RPC failure must
        // not kill continuous multi-protocol operation. Last applied pool state is
        // retained on failure.
        match refresh_selected_tip_state(http, &mut pools, &config.selected, head.hash, &header)
            .await
        {
            Ok(()) => {
                let mut guard = loop_state.state.write().await;
                for amm in &pools {
                    guard.state.insert(amm.address(), amm.clone());
                }
                info!(
                    target: "service.block_loop",
                    stage = stages::TIP_REFRESHED,
                    block = head.number,
                    protocols = ?config.selected,
                    "per-protocol tip refresh complete"
                );
            }
            Err(e) => {
                warn!(
                    target: "service.block_loop",
                    stage = stages::TIP_REFRESHED,
                    block = head.number,
                    error = %e,
                    "per-protocol tip refresh failed; continuing with last applied state"
                );
                pools = {
                    let guard = loop_state.state.read().await;
                    guard.state.values().cloned().collect()
                };
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
    discovery.gas = merged_gas_config(&config.selected, base_fee_per_gas);

    let opportunities = discover_opportunities(&pools, &discovery)
        .context("multi-protocol discover_opportunities")?;
    info!(
        target: "service.block_loop",
        stage = stages::DISCOVERY,
        block = head.number,
        opportunities = opportunities.len(),
        "merged-graph discovery complete"
    );

    let mut attempts = Vec::new();
    if config.attempt_execution {
        if let Some(best) = opportunities.first() {
            // One handoff path: `attempt_discovered_via_job_slot` owns the
            // latest-wins slot publish/take + Protocol::attempt_execution (or
            // mixed gate-closed outcome). No outer throwaway slot — that would
            // discard a populated ExecutionJob and open a second slot.
            info!(
                target: "service.block_loop",
                stage = stages::JOB_PUBLISHED,
                block = head.number,
                signature = %best.candidate.signature,
                "dispatching best candidate through shared job-slot helper"
            );
            let attempt = attempt_discovered_via_job_slot(
                best,
                discovery.block_timestamp,
                AttemptJobContext {
                    observed_at,
                    base_fee_per_gas: base_fee_per_gas.map(u128::from).unwrap_or(0),
                    block_gas_limit,
                },
            )
            .await
            .context("attempt_discovered_via_job_slot")?;
            info!(
                target: "service.block_loop",
                stage = stages::EXECUTION_ATTEMPT,
                block = head.number,
                ?attempt,
                "signerless execution attempt"
            );
            attempts.push((best.clone(), attempt));
        }
    }

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

async fn fetch_logs_for_head(
    provider: &DynProvider,
    block_filter: &Filter,
    head: &ObservedHead,
) -> Result<Vec<Log>> {
    let hash_filter = hash_pinned_logs_filter(block_filter.clone(), head.hash);
    match provider.get_logs(&hash_filter).await {
        Ok(logs) => Ok(logs),
        Err(hash_err) => {
            warn!(
                target: "service.block_loop",
                block = head.number,
                error = %hash_err,
                "hash-pinned get_logs failed; falling back to number-range filter"
            );
            let number_filter = block_filter
                .clone()
                .from_block(head.number)
                .to_block(head.number);
            provider
                .get_logs(&number_filter)
                .await
                .map_err(|e| eyre!("get_logs fallback for #{}: {e}", head.number))
        }
    }
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
    let mut exit_reason = WatchExitReason::StreamEnded;

    info!(
        target: "service.block_loop",
        protocols = ?config.selected,
        block_subscriptions = stats.block_subscriptions,
        http_tip_wait_ms = config.http_tip_wait.as_millis() as u64,
        skip_fatal_window = config.skip_fatal_window,
        "starting multi-protocol watch loop (single shared subscription)"
    );

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

                // Canonical HTTP header for base fee / gas limit (WS may omit).
                // Bounded wait when HTTP lags the WS tip (WHI-792).
                let header_load = load_canonical_header_with_wait(
                    &http,
                    head.number,
                    config.http_tip_wait,
                )
                .await;
                match header_load {
                    CanonicalHeaderLoad::Ready { base_fee, gas_limit, waited } => {
                        if waited > Duration::ZERO {
                            stats.http_tip_waits += 1;
                            crate::metrics::record_http_tip_wait(waited);
                            info!(
                                target: "service.block_loop",
                                block = head_number,
                                waited_ms = waited.as_millis() as u64,
                                "HTTP caught WS tip after wait"
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
                                    info!(
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
                                    stats.halted_or_skipped += 1;
                                    consecutive_skips += 1;
                                }
                            }
                            Err(e) => {
                                warn!(
                                    target: "service.block_loop",
                                    block = head_number,
                                    error = %e,
                                    "block processing failed; continuing"
                                );
                                stats.halted_or_skipped += 1;
                                consecutive_skips += 1;
                            }
                        }
                    }
                    CanonicalHeaderLoad::TimedOut { waited } => {
                        stats.http_tip_waits += 1;
                        stats.http_tip_timeouts += 1;
                        stats.halted_or_skipped += 1;
                        consecutive_skips += 1;
                        crate::metrics::record_http_tip_wait(waited);
                        crate::metrics::record_http_tip_timeout();
                        warn!(
                            target: "service.block_loop",
                            block = head_number,
                            waited_ms = waited.as_millis() as u64,
                            "HTTP has not observed WS tip within deadline; skipping"
                        );
                    }
                    CanonicalHeaderLoad::RpcError { error, waited } => {
                        if waited > Duration::ZERO {
                            stats.http_tip_waits += 1;
                            crate::metrics::record_http_tip_wait(waited);
                        }
                        stats.halted_or_skipped += 1;
                        consecutive_skips += 1;
                        warn!(
                            target: "service.block_loop",
                            block = head_number,
                            error = %error,
                            "failed to load canonical header; skipping"
                        );
                    }
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

enum CanonicalHeaderLoad {
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

/// Wait up to `deadline` for HTTP to serve the WS-announced block number.
async fn load_canonical_header_with_wait(
    http: &DynProvider,
    block_number: u64,
    deadline: Duration,
) -> CanonicalHeaderLoad {
    let started = std::time::Instant::now();
    loop {
        match http
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await
        {
            Ok(Some(block)) => {
                let h = block.header();
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
                tokio::time::sleep(HTTP_TIP_POLL_INTERVAL.min(deadline.saturating_sub(started.elapsed())))
                    .await;
            }
            Err(e) => {
                // Hard RPC failures are not "HTTP lag" — skip immediately.
                // Only `Ok(None)` (tip not yet available) waits (WHI-792).
                return CanonicalHeaderLoad::RpcError {
                    error: eyre!("get_block_by_number #{block_number}: {e}"),
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
/// This is the **single** subscription used by the multi-protocol bot.
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

        let loop_state = WatchLoopState {
            state,
            latest_block,
            snapshots,
            block_filter: Filter::new(),
            chain_id: 5000,
        };

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
        };

        // Drive process_observed_head directly (no get_block) to prove multi-block +
        // single-subscription semantics without live RPC.
        let heads = [
            ObservedHead::new(5000, 11, B256::repeat_byte(0x11), B256::repeat_byte(0x10), 1_700_000_001),
            ObservedHead::new(5000, 12, B256::repeat_byte(0x12), B256::repeat_byte(0x11), 1_700_000_002),
            ObservedHead::new(5000, 13, B256::repeat_byte(0x13), B256::repeat_byte(0x12), 1_700_000_003),
        ];

        let asserter = Asserter::new();
        // get_logs may try hash-pin then number fallback → provision spare empties.
        for _ in 0..8 {
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

        let loop_state = WatchLoopState {
            state,
            latest_block,
            snapshots,
            block_filter: Filter::new(),
            chain_id: 5000,
        };

        let mut discovery = DiscoveryConfig::offline_default(fixture_settlement_asset());
        discovery.gas.gas_price_wei = 0;
        let config = WatchLoopConfig {
            discovery,
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
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
        let loop_state = WatchLoopState {
            state: Arc::new(RwLock::new(StateSpace::default())),
            latest_block: Arc::new(AtomicU64::new(0)),
            snapshots: SnapshotPublisher::new(),
            block_filter: Filter::new(),
            chain_id: 5000,
        };
        let config = WatchLoopConfig {
            discovery: DiscoveryConfig::offline_default(fixture_settlement_asset()),
            selected: SelectedProtocol::all().to_vec(),
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
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
        let loop_state = WatchLoopState {
            state: Arc::new(RwLock::new(StateSpace::default())),
            latest_block: Arc::new(AtomicU64::new(0)),
            snapshots: SnapshotPublisher::new(),
            block_filter: Filter::new(),
            chain_id: 5000,
        };
        let config = WatchLoopConfig {
            discovery: DiscoveryConfig::offline_default(fixture_settlement_asset()),
            selected: vec![SelectedProtocol::AgniV2],
            attempt_execution: false,
            refresh_tip_state: false,
            http_tip_wait: DEFAULT_HTTP_TIP_WAIT,
            skip_fatal_window: DEFAULT_SKIP_FATAL_WINDOW,
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
        WatchLoopState {
            state: Arc::new(RwLock::new(space)),
            latest_block,
            snapshots: SnapshotPublisher::new(),
            block_filter: Filter::new(),
            chain_id: 5000,
        }
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
        }
    }

    fn mock_block(number: u64, hash: B256) -> alloy::rpc::types::Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
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
        // Every get_block returns None → all heads timeout/skip.
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
}
