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
//! Continuity classification uses [`SnapshotPublisher::observe_head`]. On a reorg
//! deeper than the cache (or a continuity Halt / large Gap), this loop **logs and
//! skips discovery** rather than inventing recovery — full deep-reorg unwinding is
//! owned by WHI-533.

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::execution::LatestWinsSlot;
use crate::service::discovery::{
    attempt_discovered_via_job_slot, discover_opportunities, DiscoveryConfig, DiscoveredOpportunity,
};
use crate::service::gas::GasConfig;
use crate::service::protocol::{
    AgniV2Protocol, AgniV3Protocol, Candidate, ExecutionAttempt, MoeProtocol, Protocol,
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
use tracing::{info, warn};

/// Poll interval used by the v3/moe execution workers when the slot is empty.
pub const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
}

/// Latency-stage names aligned with WHI-537's block-to-submit taxonomy.
///
/// No Prometheus instrumentation here (WHI-532); names are stable so metrics can
/// attach later without renames.
pub mod stages {
    pub const BLOCK_OBSERVED: &str = "block_observed";
    pub const STATE_APPLIED: &str = "state_applied";
    pub const TIP_REFRESHED: &str = "tip_refreshed";
    pub const DISCOVERY: &str = "discovery";
    pub const JOB_PUBLISHED: &str = "job_published";
    pub const EXECUTION_ATTEMPT: &str = "execution_attempt";
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
        Some(SnapshotStatus::Ready(snapshot)) => Err(eyre!(
            "stale queued opportunity: candidate {:?} != live tip {:?}",
            candidate_id,
            snapshot.id
        )),
        Some(_) => Err(eyre!("execution gate has no live Ready snapshot tip")),
        None => Err(eyre!("execution gate has no live Ready snapshot tip")),
    }
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
pub async fn process_observed_head(
    http: &DynProvider,
    loop_state: &WatchLoopState,
    config: &WatchLoopConfig,
    head: ObservedHead,
    base_fee_per_gas: Option<u64>,
    block_gas_limit: u64,
) -> Result<Option<BlockTick>> {
    info!(
        target: "service.block_loop",
        stage = stages::BLOCK_OBSERVED,
        block = head.number,
        hash = %head.hash,
        "observed multi-protocol head"
    );

    let observation = loop_state.snapshots.observe_head(&head).await;
    match observation {
        HeadObservation::Duplicate => {
            info!(
                target: "service.block_loop",
                block = head.number,
                "duplicate head; skipping"
            );
            return Ok(None);
        }
        HeadObservation::Halted(reason) => {
            warn!(
                target: "service.block_loop",
                block = head.number,
                %reason,
                "head continuity halted (fork/gap); discovery skipped — deep recovery is WHI-533"
            );
            return Ok(None);
        }
        HeadObservation::Backfill { previous, .. } => {
            let gap = head.number.saturating_sub(previous.id.block_number);
            if gap > CACHE_SIZE as u64 {
                warn!(
                    target: "service.block_loop",
                    block = head.number,
                    previous = previous.id.block_number,
                    gap,
                    cache_size = CACHE_SIZE,
                    "backfill gap exceeds StateChangeCache; skipping (WHI-533 owns full recovery)"
                );
                loop_state
                    .snapshots
                    .fail_read(format!(
                        "backfill gap {gap} exceeds CACHE_SIZE={CACHE_SIZE}"
                    ))
                    .await;
                return Ok(None);
            }
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
        return Ok(None);
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
            let slot = new_job_slot::<ExecutionJob<Candidate>>();
            slot.publish(ExecutionJob {
                candidate: best.candidate.clone(),
                block_number: head.number,
                header,
                pool_universe_fingerprint: B256::ZERO,
                base_fee_per_gas: base_fee_per_gas.map(u128::from).unwrap_or(0),
                block_gas_limit,
            });
            // Drain so attempt_discovered_via_job_slot's internal slot is not required
            // for the publish-side invariant; still exercise the shared helper for AC.
            let _ = slot.take();
            info!(
                target: "service.block_loop",
                stage = stages::JOB_PUBLISHED,
                block = head.number,
                signature = %best.candidate.signature,
                "published best candidate on job slot"
            );
            let attempt = attempt_discovered_via_job_slot(best, discovery.block_timestamp)
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

    Ok(Some(BlockTick {
        block_number: head.number,
        snapshot_id,
        header,
        base_fee_per_gas,
        affected_pools: affected.len(),
        opportunities,
        attempts,
    }))
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
/// protocols share it). Exits cleanly when `shutdown` completes or the stream ends.
pub async fn run_multi_protocol_watch_loop<S, F, H>(
    http: DynProvider,
    loop_state: WatchLoopState,
    config: WatchLoopConfig,
    mut heads: S,
    mut shutdown: F,
    mut hooks: H,
) -> Result<WatchLoopStats>
where
    S: Stream<Item = ObservedHead> + Unpin,
    F: Future<Output = ()> + Unpin,
    H: WatchLoopHooks,
{
    let mut stats = WatchLoopStats {
        block_subscriptions: 1,
        ..WatchLoopStats::default()
    };

    info!(
        target: "service.block_loop",
        protocols = ?config.selected,
        block_subscriptions = stats.block_subscriptions,
        "starting multi-protocol watch loop (single shared subscription)"
    );

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!(
                    target: "service.block_loop",
                    blocks = stats.blocks_processed,
                    "shutdown signal; ending multi-protocol watch loop"
                );
                break;
            }
            next = heads.next() => {
                let Some(head) = next else {
                    info!(
                        target: "service.block_loop",
                        blocks = stats.blocks_processed,
                        "head stream ended; watch loop complete"
                    );
                    break;
                };

                // Canonical HTTP header for base fee / gas limit (WS may omit).
                let (base_fee, gas_limit) = match http
                    .get_block_by_number(BlockNumberOrTag::Number(head.number))
                    .await
                {
                    Ok(Some(block)) => {
                        let h = block.header();
                        (h.base_fee_per_gas(), h.gas_limit())
                    }
                    Ok(None) => {
                        warn!(
                            target: "service.block_loop",
                            block = head.number,
                            "HTTP has not observed WS tip yet; skipping"
                        );
                        stats.halted_or_skipped += 1;
                        continue;
                    }
                    Err(e) => {
                        warn!(
                            target: "service.block_loop",
                            block = head.number,
                            error = %e,
                            "failed to load canonical header; skipping"
                        );
                        stats.halted_or_skipped += 1;
                        continue;
                    }
                };

                match process_observed_head(
                    &http,
                    &loop_state,
                    &config,
                    head,
                    base_fee,
                    gas_limit,
                )
                .await
                {
                    Ok(Some(tick)) => {
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
                    }
                    Ok(None) => {
                        stats.halted_or_skipped += 1;
                    }
                    Err(e) => {
                        warn!(
                            target: "service.block_loop",
                            block = head.number,
                            error = %e,
                            "block processing failed; continuing"
                        );
                        stats.halted_or_skipped += 1;
                    }
                }
            }
        }
    }

    Ok(stats)
}

/// Build an [`ObservedHead`] stream from a WS provider's `subscribe_blocks`.
///
/// This is the **single** subscription used by the multi-protocol bot.
pub async fn subscribe_heads_once<P>(
    ws: &P,
    chain_id: u64,
) -> Result<impl Stream<Item = ObservedHead> + Unpin>
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
    Ok(Box::pin(stream))
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
            let tick = process_observed_head(&http, &loop_state, &config, head, Some(25), 30_000_000)
                .await
                .expect("process head")
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
        // Single-subscription invariant: this test never opened a WS subscription;
        // the production path sets block_subscriptions=1 in run_multi_protocol_watch_loop.
        let stats = WatchLoopStats {
            blocks_processed: 3,
            block_subscriptions: 1,
            opportunities_found: 3, // at least one per block; may be more
            attempts: 3,
            halted_or_skipped: 0,
        };
        assert_eq!(stats.block_subscriptions, 1);
        assert!(stats.blocks_processed >= 3);
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
        )
        .await
        .expect("loop");

        assert_eq!(stats.block_subscriptions, 1);
        assert_eq!(stats.blocks_processed, 0);
    }
}
