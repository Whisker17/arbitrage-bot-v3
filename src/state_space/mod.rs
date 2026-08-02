pub mod cache;
pub mod error;
pub mod filters;
pub mod snapshot;

pub use snapshot::{
    classify_head, hash_pinned_logs_filter, hash_pinned_state_block_id,
    max_input_bound_for_snapshot, pool_universe_fingerprint, snapshot_state_block_id, AssembleKind,
    AssemblyHashGuard, BlockHeaderContext, ForkKind, HaltReason, HeadDecision, HeadObservation,
    IdentityBarrier, IdentityReadLease, MarketSnapshot, NumberPinnedSession, ObservedHead,
    PinError, PoolProtocol, PoolUniverseError, PoolUniverseRow, ProtocolCoverage,
    SnapshotBalanceError, SnapshotBoundBalance, SnapshotId, SnapshotPublisher, SnapshotStatus,
    SnapshotTip, EFFECTIVE_MAX_HOPS,
};

use crate::amms::amm::AutomatedMarketMaker;
use crate::amms::amm::AMM;
use crate::amms::error::AMMError;
use crate::amms::factory::Factory;
use crate::amms::logs::{fetch_logs_in_ranges, LogRangeConfig};
use crate::amms::moe::{sync_moe_snapshots_batch, MoeSnapshotContext, MoeSnapshotSyncConfig};

use alloy::consensus::BlockHeader;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::rpc::types::{Block, Filter, FilterSet, Log};
use alloy::{
    network::Network,
    primitives::{Address, FixedBytes},
    providers::Provider,
};
use async_stream::stream;
use cache::StateChange;
use cache::StateChangeCache;

use error::StateSpaceError;
use filters::{AMMFilter, FilterStage, PoolFilter};
use futures::stream::FuturesUnordered;
use futures::Stream;
use futures::StreamExt;
use std::collections::HashSet;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::{collections::HashMap, marker::PhantomData, sync::Arc};
use tokio::sync::RwLock;
use tracing::debug;
use tracing::info;
use tracing::warn;

pub const CACHE_SIZE: usize = 30;

#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,
    pub latest_block: Arc<AtomicU64>,
    /// Chain id bound into every [`SnapshotId`] published by this manager.
    pub chain_id: u64,
    /// Atomic readiness surface for quote / candidate / send gates (WHI-510).
    pub snapshots: SnapshotPublisher,
    pub block_filter: Filter,
    pub factories: Arc<Vec<Factory>>,
    pub filters: Arc<Vec<PoolFilter>>,
    pub provider: P,
    phantom: PhantomData<N>,
    // TODO: add support for caching
}

impl<N, P> StateSpaceManager<N, P> {
    /// Current readiness token. Only [`SnapshotStatus::Ready`] may be quoted.
    pub async fn snapshot_status(&self) -> SnapshotStatus {
        self.snapshots.status().await
    }

    /// Executable market snapshot, if and only if status is Ready.
    pub async fn ready_snapshot(&self) -> Option<Arc<MarketSnapshot>> {
        self.snapshots.ready_snapshot().await
    }

    pub async fn allows_execution(&self) -> bool {
        self.snapshots.allows_execution().await
    }

    pub async fn subscribe(
        &self,
    ) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    >
    where
        P: Provider<N> + Clone + 'static,
        N: Network<BlockResponse = Block>,
    {
        let provider = self.provider.clone();
        let state = self.state.clone();
        let block_filter = self.block_filter.clone();
        let snapshots = self.snapshots.clone();
        let chain_id = self.chain_id;
        let latest_block = self.latest_block.clone();
        let factories = self.factories.clone();
        let filters = self.filters.clone();

        let initial_stream = provider.subscribe_blocks().await?.into_stream();

        Ok(Box::pin(stream! {
            let mut current_stream = Some(initial_stream);
            loop {
                let block_stream = match current_stream.take() {
                    Some(block_stream) => block_stream,
                    None => loop {
                        match provider.subscribe_blocks().await {
                            Ok(subscription) => break subscription.into_stream(),
                            Err(err) => {
                                snapshots.fail_read(format!("block subscription failed: {err}")).await;
                                yield Err(StateSpaceError::from(err));
                                tokio::task::yield_now().await;
                            }
                        }
                    },
                };
                tokio::pin!(block_stream);

                while let Some(block) = block_stream.next().await {
                    let observed = ObservedHead::new(
                        chain_id,
                        block.number(),
                        block.hash(),
                        block.parent_hash(),
                        block.timestamp(),
                    );
                    let observation = snapshots.observe_head(&observed).await;
                    let previous = match &observation {
                        HeadObservation::Backfill { previous, .. } => Some(*previous),
                        HeadObservation::Assemble(_) => snapshots.last_tip_with_header().await,
                        HeadObservation::Duplicate | HeadObservation::Halted(_) => None,
                    };

                    match observation {
                        HeadObservation::Duplicate => {
                            debug!(
                                target: "state_space::sync",
                                block_number = observed.number,
                                block_hash = ?observed.hash,
                                "Ignoring duplicate head notification"
                            );
                        }
                        HeadObservation::Halted(reason) => {
                            warn!(
                                target: "state_space::sync",
                                block_number = observed.number,
                                block_hash = ?observed.hash,
                                %reason,
                                "Head discontinuity; halted quoting"
                            );
                            yield Err(StateSpaceError::SnapshotHalted(reason));
                        }
                        HeadObservation::Assemble(_) | HeadObservation::Backfill { .. } => {
                            match assemble_head(
                                provider.clone(),
                                state.clone(),
                                latest_block.clone(),
                                snapshots.clone(),
                                block_filter.clone(),
                                chain_id,
                                observed,
                                previous,
                                factories.clone(),
                                filters.clone(),
                            )
                            .await
                            {
                                Ok(affected_amms) => yield Ok(affected_amms),
                                Err(err) => {
                                    if let StateSpaceError::IdentityMismatch(message) = &err {
                                        snapshots
                                            .halt(HaltReason::IdentityMismatch(message.clone()))
                                            .await;
                                    } else if let StateSpaceError::Pin(pin_error) = &err {
                                        snapshots
                                            .halt(HaltReason::IdentityMismatch(pin_error.to_string()))
                                            .await;
                                    } else {
                                        snapshots.fail_read(err.to_string()).await;
                                    }
                                    yield Err(err);
                                }
                            }
                        }
                    }
                }

                snapshots.fail_read("block subscription dropped").await;
                warn!(target: "state_space::sync", "Block subscription dropped; reconnecting");
            }
        }))
    }
}

async fn assemble_head<N, P>(
    provider: P,
    state: Arc<RwLock<StateSpace>>,
    latest_block: Arc<AtomicU64>,
    snapshots: SnapshotPublisher,
    block_filter: Filter,
    chain_id: u64,
    observed: ObservedHead,
    previous: Option<SnapshotTip>,
    factories: Arc<Vec<Factory>>,
    filters: Arc<Vec<PoolFilter>>,
) -> Result<Vec<Address>, StateSpaceError>
where
    P: Provider<N> + Clone,
    N: Network<BlockResponse = Block>,
{
    let target = canonical_header(&provider, chain_id, observed.number).await?;
    if target != observed {
        return Err(StateSpaceError::IdentityMismatch(format!(
            "WS head #{} {:?} differs from canonical header #{} {:?}",
            observed.number, observed.hash, target.number, target.hash
        )));
    }

    let headers = if let Some(previous) = previous {
        if observed.number <= previous.id.block_number {
            return Err(StateSpaceError::IdentityMismatch(format!(
                "head #{} does not advance previous tip #{}",
                observed.number, previous.id.block_number
            )));
        }
        let first_number = previous
            .id
            .block_number
            .checked_add(1)
            .ok_or_else(|| StateSpaceError::IdentityMismatch("block number overflow".into()))?;
        let mut headers = Vec::new();

        for number in first_number..=observed.number {
            let header = if number == target.number {
                target
            } else {
                canonical_header(&provider, chain_id, number).await?
            };
            headers.push(header);
        }
        validate_header_chain(previous, &headers)?;
        headers
    } else {
        vec![target]
    };

    let mut working_state = {
        let state_guard = state.read().await;
        let latest = state_guard.latest_block.load(Ordering::Relaxed);
        let mut working_state = state_guard.clone();
        working_state.latest_block = Arc::new(AtomicU64::new(latest));
        working_state
    };
    let mut affected_amms = HashSet::new();
    let mut replayed_logs = Vec::new();

    for header in &headers {
        let logs = fetch_logs_for_header(&provider, &block_filter, header).await?;
        validate_logs_for_header(header, &logs)?;
        replayed_logs.extend(logs.iter().cloned());
        let (affected, _) = apply_logs_atomically(&mut working_state, &logs)?;
        affected_amms.extend(affected);
        working_state
            .latest_block
            .store(header.number, Ordering::Relaxed);
    }

    for header in &headers {
        let canonical = canonical_header(&provider, chain_id, header.number).await?;
        if canonical != *header {
            return Err(StateSpaceError::IdentityMismatch(format!(
                "canonical header #{} changed from {:?} to {:?} during backfill",
                header.number, header.hash, canonical.hash
            )));
        }
    }

    affected_amms.extend(
        initialize_new_pools(
            &mut working_state,
            &replayed_logs,
            factories.as_slice(),
            hash_pinned_state_block_id(target.hash),
            provider.clone(),
            filters.as_slice(),
        )
        .await?,
    );

    let mut snapshot_amms: Vec<AMM> = working_state.state.values().cloned().collect();
    sync_moe_snapshots_batch(
        &mut snapshot_amms,
        hash_pinned_state_block_id(target.hash),
        provider.clone(),
        MoeSnapshotContext::new(target.hash, target.timestamp),
        MoeSnapshotSyncConfig::default(),
    )
    .await?;
    let canonical_after_sync = canonical_header(&provider, chain_id, target.number).await?;
    if canonical_after_sync != target {
        return Err(StateSpaceError::IdentityMismatch(format!(
            "canonical target #{} changed from {:?} to {:?} before publish",
            target.number, target.hash, canonical_after_sync.hash
        )));
    }
    let pools: HashMap<Address, AMM> = snapshot_amms
        .into_iter()
        .map(|amm| (amm.address(), amm))
        .collect();
    working_state.state = pools.clone();
    working_state.latest_block = latest_block.clone();

    {
        let mut state_guard = state.write().await;
        *state_guard = working_state;
    }
    latest_block.store(target.number, Ordering::Relaxed);

    snapshots
        .publish(MarketSnapshot::new(
            target.to_snapshot_id(),
            target.to_header_context(),
            pools,
            ProtocolCoverage::default(),
        ))
        .await;
    Ok(affected_amms.into_iter().collect())
}

fn validate_header_chain(
    previous: SnapshotTip,
    headers: &[ObservedHead],
) -> Result<(), StateSpaceError> {
    let mut expected_number = previous
        .id
        .block_number
        .checked_add(1)
        .ok_or_else(|| StateSpaceError::IdentityMismatch("block number overflow".into()))?;
    let mut expected_parent = previous.id.block_hash;

    for header in headers {
        if header.number != expected_number {
            return Err(StateSpaceError::IdentityMismatch(format!(
                "canonical header #{} is not the expected #{}",
                header.number, expected_number
            )));
        }
        if header.parent_hash != expected_parent {
            return Err(StateSpaceError::IdentityMismatch(format!(
                "canonical header #{} parent {:?} does not extend {:?}",
                header.number, header.parent_hash, expected_parent
            )));
        }
        expected_parent = header.hash;
        expected_number = expected_number
            .checked_add(1)
            .ok_or_else(|| StateSpaceError::IdentityMismatch("block number overflow".into()))?;
    }
    Ok(())
}

async fn canonical_header<N, P>(
    provider: &P,
    chain_id: u64,
    number: u64,
) -> Result<ObservedHead, StateSpaceError>
where
    P: Provider<N>,
    N: Network<BlockResponse = Block>,
{
    let block = provider
        .get_block_by_number(BlockNumberOrTag::Number(number))
        .await?
        .ok_or(StateSpaceError::MissingBlock(number))?;
    let header = ObservedHead::new(
        chain_id,
        block.header().number(),
        block.header().hash(),
        block.header().parent_hash(),
        block.header().timestamp(),
    );
    if header.number != number {
        return Err(StateSpaceError::IdentityMismatch(format!(
            "provider returned header #{} for requested #{}",
            header.number, number
        )));
    }
    Ok(header)
}

async fn fetch_logs_for_header<N, P>(
    provider: &P,
    block_filter: &Filter,
    header: &ObservedHead,
) -> Result<Vec<Log>, StateSpaceError>
where
    P: Provider<N> + Clone,
    N: Network<BlockResponse = Block>,
{
    let hash_filter = hash_pinned_logs_filter(block_filter.clone(), header.hash);
    match provider.get_logs(&hash_filter).await {
        Ok(logs) => Ok(logs),
        Err(hash_error) => {
            warn!(
                target: "state_space::sync",
                block_number = header.number,
                block_hash = ?header.hash,
                %hash_error,
                "Hash-pinned log query failed; retrying with canonical number fallback"
            );
            let result = fetch_logs_in_ranges::<N, _>(
                provider.clone(),
                block_filter.clone(),
                header.number,
                header.number,
                LogRangeConfig {
                    initial_window: 1,
                    minimum_window: 1,
                },
            )
            .await
            .map_err(AMMError::from)?;
            let canonical = canonical_header(provider, header.chain_id, header.number).await?;
            if canonical != *header {
                return Err(StateSpaceError::IdentityMismatch(format!(
                    "canonical fallback header #{} changed from {:?} to {:?}",
                    header.number, header.hash, canonical.hash
                )));
            }
            Ok(result.logs)
        }
    }
}

fn validate_logs_for_header(header: &ObservedHead, logs: &[Log]) -> Result<(), StateSpaceError> {
    for log in logs {
        match log.block_hash {
            Some(hash) if hash == header.hash => {}
            Some(hash) => {
                return Err(StateSpaceError::Pin(PinError::MixedBlockHash {
                    got: hash,
                    expected: header.hash,
                }))
            }
            None => return Err(StateSpaceError::MissingBlockHash),
        }
        match log.block_number {
            Some(number) if number == header.number => {}
            Some(number) => {
                return Err(StateSpaceError::Pin(PinError::MixedBlockNumber {
                    got: number,
                    expected: header.number,
                }))
            }
            None => return Err(StateSpaceError::MissingBlockNumber),
        }
    }
    Ok(())
}

async fn initialize_new_pools<N, P>(
    state: &mut StateSpace,
    logs: &[Log],
    factories: &[Factory],
    block_id: BlockId,
    provider: P,
    filters: &[PoolFilter],
) -> Result<Vec<Address>, StateSpaceError>
where
    P: Provider<N> + Clone,
    N: Network<BlockResponse = Block>,
{
    let mut known_addresses = state.state.keys().copied().collect::<HashSet<_>>();
    let mut candidates = Vec::new();
    for log in logs {
        let Some(event) = log.topics().first().copied() else {
            continue;
        };
        let Some(factory) = factories.iter().find(|factory| {
            factory.address() == log.address() && factory.discovery_event() == event
        }) else {
            continue;
        };
        let pool = factory.create_pool(log.clone())?;
        if known_addresses.insert(pool.address()) {
            candidates.push(pool);
        }
    }

    for filter in filters
        .iter()
        .filter(|filter| filter.stage() == FilterStage::Discovery)
    {
        candidates = filter.filter(candidates).await?;
    }

    let mut initialized = Vec::with_capacity(candidates.len());
    for pool in candidates {
        initialized.push(pool.init(block_id, provider.clone()).await?);
    }

    for filter in filters
        .iter()
        .filter(|filter| filter.stage() == FilterStage::Sync)
    {
        initialized = filter.filter(initialized).await?;
    }

    let mut addresses = Vec::with_capacity(initialized.len());
    for pool in initialized {
        let address = pool.address();
        addresses.push(address);
        state.state.insert(address, pool);
    }
    Ok(addresses)
}

/// Apply logs with pool-map rollback on failure (subscribe assembly atomicity).
///
/// On error the pool map is restored to the pre-apply backup so consumers that
/// still hold a recovery baseline are not paired with a partial working state.
fn apply_logs_atomically(
    state: &mut StateSpace,
    logs: &[Log],
) -> Result<(Vec<Address>, HashMap<Address, AMM>), StateSpaceError> {
    let pools_backup = state.state.clone();
    match state.sync(logs) {
        Ok(affected) => {
            let pools = state.state.clone();
            Ok((affected, pools))
        }
        Err(err) => {
            state.state = pools_backup;
            Err(err)
        }
    }
}

// TODO: Drop impl, create a checkpoint
#[derive(Debug)]
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub latest_block: u64,
    /// Optional chain id override; when `None`, resolved via `eth_chainId` at sync time.
    pub chain_id: Option<u64>,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    phantom: PhantomData<N>,
    // TODO: add support for caching
}

impl<N, P> StateSpaceBuilder<N, P>
where
    N: Network,
    P: Provider<N> + Clone + 'static,
{
    pub fn new(provider: P) -> StateSpaceBuilder<N, P> {
        Self {
            provider,
            latest_block: 0,
            chain_id: None,
            factories: vec![],
            amms: vec![],
            filters: vec![],
            // discovery: false,
            phantom: PhantomData,
        }
    }

    pub fn block(self, latest_block: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            latest_block,
            ..self
        }
    }

    pub fn chain_id(self, chain_id: u64) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder {
            chain_id: Some(chain_id),
            ..self
        }
    }

    pub fn with_factories(self, factories: Vec<Factory>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { factories, ..self }
    }

    pub fn with_amms(self, amms: Vec<AMM>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { amms, ..self }
    }

    pub fn with_filters(self, filters: Vec<PoolFilter>) -> StateSpaceBuilder<N, P> {
        StateSpaceBuilder { filters, ..self }
    }

    pub async fn sync(self) -> Result<StateSpaceManager<N, P>, StateSpaceError>
    where
        N: Network<BlockResponse = Block>,
    {
        let chain_id = match self.chain_id {
            Some(id) => id,
            None => self.provider.get_chain_id().await?,
        };

        // Resolve a single canonical tip identity, then pin every discovery/state
        // call to that hash (EIP-1898 requireCanonical). Never leave middle reads
        // on `latest`. After all reads, re-resolve the number and reject drift.
        let tip_number = self.provider.get_block_number().await?;
        let tip_before = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(tip_number))
            .await?
            .ok_or(StateSpaceError::MissingTipBlock(tip_number))?;
        let tip_hash = tip_before.header().hash();
        let tip_parent = tip_before.header().parent_hash();
        let tip_timestamp = tip_before.header().timestamp();

        // Preferred path: hash-canonical BlockId for all AMM state reads.
        // The before/after number→hash recheck below is an identity guard only
        // (not the number-pin fallback session — middle calls never use Latest
        // or a bare number BlockId here).
        let chain_tip = hash_pinned_state_block_id(tip_hash);

        let manager_factories = Arc::new(self.factories.clone());
        let manager_filters = Arc::new(self.filters.clone());
        let factories = self.factories.clone();
        let mut futures = FuturesUnordered::new();

        let mut filter_set = HashSet::new();
        for factory in &self.factories {
            filter_set.insert(factory.discovery_event());
            for event in factory.pool_events() {
                filter_set.insert(event);
            }
        }

        for amm in self.amms.iter() {
            for event in amm.sync_events() {
                filter_set.insert(event);
            }
        }

        let block_filter = Filter::new().event_signature(FilterSet::from(
            filter_set.into_iter().collect::<Vec<FixedBytes<32>>>(),
        ));
        let mut amm_variants = HashMap::new();
        for amm in self.amms.into_iter() {
            amm_variants
                .entry(amm.variant())
                .or_insert_with(Vec::new)
                .push(amm);
        }

        for factory in factories {
            let provider = self.provider.clone();
            let filters = self.filters.clone();

            let extension = amm_variants.remove(&factory.variant());
            futures.push(tokio::spawn(async move {
                let mut discovered_amms = factory.discover(chain_tip, provider.clone()).await?;

                if let Some(amms) = extension {
                    discovered_amms.extend(amms);
                }

                // Apply discovery filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Discovery {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Discovery filter"
                        );
                    }
                }

                discovered_amms = factory.sync(discovered_amms, chain_tip, provider).await?;

                // Apply sync filters
                for filter in filters.iter() {
                    if filter.stage() == filters::FilterStage::Sync {
                        let pre_filter_len = discovered_amms.len();
                        discovered_amms = filter.filter(discovered_amms).await?;

                        info!(
                            target: "state_space::sync",
                            factory = %factory.address(),
                            pre_filter_len,
                            post_filter_len = discovered_amms.len(),
                            filter = ?filter,
                            "Sync filter"
                        );
                    }
                }

                Ok::<Vec<AMM>, AMMError>(discovered_amms)
            }));
        }

        // Share one tip counter between manager and StateSpace so reorg detection
        // (spec 01-D1) can observe advances made by subscribe / publish paths.
        // Seed from the hash-pinned tip we actually read (not the builder's default 0).
        let latest_block = Arc::new(AtomicU64::new(tip_number));
        let mut state_space = StateSpace {
            state: HashMap::new(),
            latest_block: Arc::clone(&latest_block),
            cache: StateChangeCache::default(),
        };
        while let Some(res) = futures.next().await {
            let synced_amms = res??;

            for amm in synced_amms {
                state_space.state.insert(amm.address(), amm);
            }
        }

        // Sync remaining AMM variants
        for (_, remaining_amms) in amm_variants.drain() {
            for mut amm in remaining_amms {
                let address = amm.address();
                amm = amm.init(chain_tip, self.provider.clone()).await?;
                state_space.state.insert(address, amm);
            }
        }

        let mut snapshot_amms: Vec<AMM> = state_space.state.values().cloned().collect();
        sync_moe_snapshots_batch(
            &mut snapshot_amms,
            chain_tip,
            self.provider.clone(),
            MoeSnapshotContext::new(tip_hash, tip_timestamp),
            MoeSnapshotSyncConfig::default(),
        )
        .await?;
        state_space.state = snapshot_amms
            .into_iter()
            .map(|amm| (amm.address(), amm))
            .collect();

        // Post-read identity guard: the tip number must still map to the same
        // hash we pinned for discovery. Prefer-hash path already pins middle
        // calls; this rejects a same-height replacement during the bulk sync.
        let tip_after = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(tip_number))
            .await?
            .ok_or(StateSpaceError::MissingTipBlock(tip_number))?;
        let hash_after = tip_after.header().hash();
        if hash_after != tip_hash {
            return Err(StateSpaceError::Pin(PinError::HashChangedDuringReads {
                before: tip_hash,
                after: hash_after,
            }));
        }

        let snapshots = SnapshotPublisher::new();
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(chain_id, tip_number, tip_hash),
                BlockHeaderContext::new(tip_parent, tip_timestamp),
                state_space.state.clone(),
                ProtocolCoverage::default(),
            ))
            .await;

        Ok(StateSpaceManager {
            latest_block,
            chain_id,
            snapshots,
            state: Arc::new(RwLock::new(state_space)),
            block_filter,
            factories: manager_factories,
            filters: manager_filters,
            provider: self.provider,
            phantom: PhantomData,
        })
    }
}

#[derive(Debug, Default)]
pub struct StateSpace {
    pub state: HashMap<Address, AMM>,
    pub latest_block: Arc<AtomicU64>,
    cache: StateChangeCache<CACHE_SIZE>,
}

impl Clone for StateSpace {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            latest_block: Arc::new(AtomicU64::new(self.latest_block.load(Ordering::Relaxed))),
            cache: self.cache.clone(),
        }
    }
}

impl StateSpace {
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address)
    }

    pub fn sync(&mut self, logs: &[Log]) -> Result<Vec<Address>, StateSpaceError> {
        let latest = self.latest_block.load(Ordering::Relaxed);
        let Some(mut block_number) = logs
            .first()
            .map(|log| log.block_number.ok_or(StateSpaceError::MissingBlockNumber))
            .transpose()?
        else {
            return Ok(vec![]);
        };

        // Check if there is a reorg and unwind to state before block_number
        if latest >= block_number {
            info!(
                target: "state_space::sync",
                from = %latest,
                to = %block_number - 1,
                "Unwinding state changes"
            );

            let cached_state = self.cache.unwind_state_changes(block_number);
            for amm in cached_state {
                debug!(target: "state_space::sync", ?amm, "Reverting AMM state");
                self.state.insert(amm.address(), amm);
            }
        }

        let mut cached_amms = HashSet::new();
        let mut affected_amms = HashSet::new();
        for log in logs {
            // If the block number is updated, cache the current block state changes
            let log_block_number = log
                .block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;
            if log_block_number != block_number {
                let amms = cached_amms.drain().collect::<Vec<AMM>>();
                affected_amms.extend(amms.iter().map(|amm| amm.address()));
                let state_change = StateChange::new(amms, block_number);

                debug!(
                    target: "state_space::sync",
                    state_change = ?state_change,
                    "Caching state change"
                );

                self.cache.push(state_change);
                block_number = log_block_number;
            }

            // If the AMM is in the state space add the current state to cache and sync from log
            let address = log.address();
            if let Some(amm) = self.state.get_mut(&address) {
                cached_amms.insert(amm.clone());
                amm.sync(log)?;

                info!(
                    target: "state_space::sync",
                    ?amm,
                    "Synced AMM"
                );
            }
        }

        if !cached_amms.is_empty() {
            let amms = cached_amms.drain().collect::<Vec<AMM>>();
            affected_amms.extend(amms.iter().map(|amm| amm.address()));
            let state_change = StateChange::new(amms, block_number);

            debug!(
                target: "state_space::sync",
                state_change = ?state_change,
                "Caching state change"
            );

            self.cache.push(state_change);
        }

        Ok(affected_amms.into_iter().collect())
    }
}

#[macro_export]
macro_rules! sync {
    // Sync factories with provider
    ($factories:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .sync()
            .await?
    }};

    // Sync factories with filters
    ($factories:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_filters($filters)
            .sync()
            .await?
    }};

    ($factories:expr, $amms:expr, $filters:expr, $provider:expr) => {{
        StateSpaceBuilder::new($provider.clone())
            .with_factories($factories)
            .with_amms($amms)
            .with_filters($filters)
            .sync()
            .await?
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::uniswap_v2::{IUniswapV2Pair, UniswapV2Pool};
    use alloy::primitives::B256;
    use alloy::sol_types::SolEvent;
    use alloy::transports::ws::WsConnect;
    use alloy::{
        network::Ethereum,
        providers::ProviderBuilder,
        rpc::{client::ClientBuilder, types::Log},
        transports::mock::Asserter,
    };
    use futures::StreamExt;
    use std::{collections::HashMap, time::Duration};
    use tokio::time::timeout;
    use tracing_subscriber;

    #[test]
    fn apply_logs_atomically_restores_pools_on_sync_error() {
        // Empty logs succeed with no mutation.
        let mut state = StateSpace::default();
        let (affected, pools) = apply_logs_atomically(&mut state, &[]).unwrap();
        assert!(affected.is_empty());
        assert!(pools.is_empty());

        // A log without block_number must fail closed without altering the pool map.
        let mut state = StateSpace::default();
        let bad_log = Log {
            inner: Default::default(),
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };
        let err = apply_logs_atomically(&mut state, std::slice::from_ref(&bad_log)).unwrap_err();
        assert!(matches!(err, StateSpaceError::MissingBlockNumber));
        assert!(state.state.is_empty());
    }

    fn test_hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn canonical_header_chain_accepts_skipped_blocks_in_order() {
        let previous = SnapshotTip::new(
            SnapshotId::new(5000, 10, test_hash(1)),
            BlockHeaderContext::new(test_hash(0), 10),
        );
        let headers = [
            ObservedHead::new(5000, 11, test_hash(2), test_hash(1), 11),
            ObservedHead::new(5000, 12, test_hash(3), test_hash(2), 12),
            ObservedHead::new(5000, 13, test_hash(4), test_hash(3), 13),
        ];

        assert!(validate_header_chain(previous, &headers).is_ok());
    }

    #[test]
    fn canonical_header_chain_rejects_reorged_middle_block() {
        let previous = SnapshotTip::new(
            SnapshotId::new(5000, 10, test_hash(1)),
            BlockHeaderContext::new(test_hash(0), 10),
        );
        let headers = [
            ObservedHead::new(5000, 11, test_hash(2), test_hash(1), 11),
            ObservedHead::new(5000, 12, test_hash(3), test_hash(9), 12),
        ];

        assert!(matches!(
            validate_header_chain(previous, &headers),
            Err(StateSpaceError::IdentityMismatch(_))
        ));
    }

    fn mock_block(number: u64, hash: B256, parent_hash: B256) -> Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
        inner.parent_hash = parent_hash;
        inner.timestamp = number;
        let mut header = alloy::rpc::types::Header::new(inner);
        header.hash = hash;
        Block::empty(header)
    }

    fn sync_log(address: Address, block_hash: B256, block_number: u64) -> Log {
        Log {
            inner: alloy::primitives::Log {
                address,
                data: IUniswapV2Pair::Sync {
                    reserve0: alloy::primitives::Uint::<112, 2>::from_limbs([111, 0]),
                    reserve1: alloy::primitives::Uint::<112, 2>::from_limbs([222, 0]),
                }
                .encode_log_data(),
            },
            block_hash: Some(block_hash),
            block_number: Some(block_number),
            block_timestamp: None,
            transaction_hash: Some(test_hash(6)),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    async fn ready_test_snapshot(snapshots: &SnapshotPublisher) {
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, 10, test_hash(1)),
                BlockHeaderContext::new(test_hash(0), 10),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;
    }

    #[tokio::test]
    async fn assemble_head_backfills_every_canonical_block_before_publish() {
        let pool_address = Address::repeat_byte(7);
        let asserter = Asserter::new();
        asserter.push_success(&Some(mock_block(13, test_hash(4), test_hash(3))));
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        asserter.push_success(&Some(mock_block(12, test_hash(3), test_hash(2))));
        asserter.push_success(&vec![sync_log(pool_address, test_hash(2), 11)]);
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        asserter.push_success(&Some(mock_block(12, test_hash(3), test_hash(2))));
        asserter.push_success(&Some(mock_block(13, test_hash(4), test_hash(3))));
        asserter.push_success(&Some(mock_block(13, test_hash(4), test_hash(3))));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let latest_block = Arc::new(AtomicU64::new(10));
        let state = Arc::new(RwLock::new(StateSpace {
            state: HashMap::from([(
                pool_address,
                AMM::UniswapV2Pool(UniswapV2Pool {
                    address: pool_address,
                    reserve_0: 100,
                    reserve_1: 200,
                    ..Default::default()
                }),
            )]),
            latest_block: latest_block.clone(),
            cache: StateChangeCache::default(),
        }));
        let snapshots = SnapshotPublisher::new();
        snapshots
            .publish(MarketSnapshot::new(
                SnapshotId::new(5000, 10, test_hash(1)),
                BlockHeaderContext::new(test_hash(0), 10),
                HashMap::new(),
                ProtocolCoverage::default(),
            ))
            .await;

        let affected = assemble_head(
            provider,
            state.clone(),
            latest_block.clone(),
            snapshots.clone(),
            Filter::new(),
            5000,
            ObservedHead::new(5000, 13, test_hash(4), test_hash(3), 13),
            Some(SnapshotTip::new(
                SnapshotId::new(5000, 10, test_hash(1)),
                BlockHeaderContext::new(test_hash(0), 10),
            )),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
        )
        .await
        .unwrap();

        assert_eq!(affected, vec![pool_address]);
        assert_eq!(latest_block.load(Ordering::Relaxed), 13);
        assert_eq!(snapshots.last_tip().await.unwrap().block_number, 13);
        let state_guard = state.read().await;
        let AMM::UniswapV2Pool(pool) = state_guard.state.get(&pool_address).unwrap() else {
            panic!("expected a Uniswap V2 pool");
        };
        assert_eq!(pool.reserve_0, 111);
        assert_eq!(pool.reserve_1, 222);
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn assemble_head_rejects_reorg_after_log_reads_without_publishing() {
        let asserter = Asserter::new();
        asserter.push_success(&Some(mock_block(13, test_hash(4), test_hash(3))));
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        asserter.push_success(&Some(mock_block(12, test_hash(3), test_hash(2))));
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Some(mock_block(11, test_hash(9), test_hash(1))));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let latest_block = Arc::new(AtomicU64::new(10));
        let state = Arc::new(RwLock::new(StateSpace {
            state: HashMap::new(),
            latest_block: latest_block.clone(),
            cache: StateChangeCache::default(),
        }));
        let snapshots = SnapshotPublisher::new();
        ready_test_snapshot(&snapshots).await;

        let error = assemble_head(
            provider,
            state,
            latest_block.clone(),
            snapshots.clone(),
            Filter::new(),
            5000,
            ObservedHead::new(5000, 13, test_hash(4), test_hash(3), 13),
            Some(SnapshotTip::new(
                SnapshotId::new(5000, 10, test_hash(1)),
                BlockHeaderContext::new(test_hash(0), 10),
            )),
            Arc::new(Vec::new()),
            Arc::new(Vec::new()),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, StateSpaceError::IdentityMismatch(_)));
        assert_eq!(latest_block.load(Ordering::Relaxed), 10);
        assert_eq!(snapshots.last_tip().await.unwrap().block_number, 10);
        assert!(snapshots.ready_snapshot().await.is_some());
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test]
    async fn hash_pinned_log_failure_falls_back_to_canonical_number_query() {
        let asserter = Asserter::new();
        asserter.push_failure_msg("blockHash filters are unsupported");
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        asserter.push_success(&Vec::<Log>::new());
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        asserter.push_success(&Some(mock_block(11, test_hash(2), test_hash(1))));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());

        let logs = fetch_logs_for_header(
            &provider,
            &Filter::new(),
            &ObservedHead::new(5000, 11, test_hash(2), test_hash(1), 11),
        )
        .await
        .unwrap();

        assert!(logs.is_empty());
        assert!(asserter.read_q().is_empty());
    }

    /// RPC endpoint table for optional live subscribe tests only.
    ///
    /// WHI-744: keep a **single** endpoint list in-tree (no parallel table in
    /// `rpc_probe`). Qualification of production pairs is `cargo run --bin
    /// rpc_probe`; this list is only for ad-hoc subscribe smoke tests.
    /// WS capability is inferred from the URL scheme (`wss://` / `ws://`).
    #[derive(Debug, Clone)]
    struct RpcEndpoint {
        name: &'static str,
        url: &'static str,
    }

    impl RpcEndpoint {
        fn supports_ws(&self) -> bool {
            self.url.starts_with("wss://") || self.url.starts_with("ws://")
        }
    }

    /// Test-only public Mantle endpoints (not used by `rpc_probe`).
    fn get_test_rpc_endpoints() -> Vec<RpcEndpoint> {
        vec![
            RpcEndpoint {
                name: "Mantle Mainnet Ws",
                url: "wss://rpc.mantle.xyz",
            },
            RpcEndpoint {
                name: "Mantle Mainnet Https",
                url: "https://rpc.mantle.xyz",
            },
            RpcEndpoint {
                name: "Mantle Mainnet PubicNode Ws",
                url: "wss://mantle.publicnode.com",
            },
            RpcEndpoint {
                name: "Mantle Mainnet DRPC Ws",
                url: "wss://mantle.drpc.org",
            },
            RpcEndpoint {
                name: "Mantle Sepolia Https",
                url: "https://rpc.sepolia.mantle.xyz",
            },
            RpcEndpoint {
                name: "Mantle Sepolia DRPC Ws",
                url: "wss://mantle-sepolia.drpc.org",
            },
            RpcEndpoint {
                name: "Mantle Sepolia DRPC Https",
                url: "https://mantle-sepolia.drpc.org",
            },
        ]
    }

    // Test All Subscribe Support
    // cargo test subscribe -- --nocapture

    /// 测试单个 RPC 端点的 subscribe 功能
    // cargo test subscribe_single -- --nocapture
    async fn test_rpc_subscribe_support(endpoint: &RpcEndpoint) -> (String, bool, Option<String>) {
        if !endpoint.supports_ws() {
            return (
                endpoint.name.to_string(),
                false,
                Some("HTTP-only endpoint (no ws/wss scheme)".to_string()),
            );
        }
        let result = timeout(Duration::from_secs(10), async {
            // 创建 WebSocket 客户端
            let ws = WsConnect::new(endpoint.url);
            let client_result = ClientBuilder::default().ws(ws).await;

            let client = match client_result {
                Ok(client) => client,
                Err(e) => return (false, Some(format!("Failed to connect: {}", e))),
            };

            // 创建 Provider
            let provider = ProviderBuilder::new().connect_client(client);

            // 尝试订阅区块
            let subscribe_result = provider.subscribe_blocks().await;

            match subscribe_result {
                Ok(mut stream) => {
                    // 尝试接收一个区块以验证订阅确实工作
                    let stream = stream.into_stream();
                    tokio::pin!(stream);

                    match timeout(Duration::from_secs(5), stream.next()).await {
                        Ok(Some(_block)) => (true, None),
                        Ok(None) => (
                            false,
                            Some("Stream ended without receiving blocks".to_string()),
                        ),
                        Err(_) => (false, Some("Timeout waiting for blocks".to_string())),
                    }
                }
                Err(e) => (false, Some(format!("Subscribe failed: {}", e))),
            }
        })
        .await;

        match result {
            Ok((success, error)) => (endpoint.name.to_string(), success, error),
            Err(_) => (
                endpoint.name.to_string(),
                false,
                Some("Test timeout".to_string()),
            ),
        }
    }

    /// 测试 StateSpaceManager 的 subscribe 方法
    // cargo test state_space_subscribe_functionality -- --nocapture
    #[tokio::test]
    async fn test_state_space_subscribe_functionality() {
        // 初始化日志（测试时可选）
        let _ = tracing_subscriber::fmt().try_init();

        let endpoints = get_test_rpc_endpoints();
        let mut results: HashMap<String, (bool, Option<String>)> = HashMap::new();

        println!("\n🚀 开始测试 RPC 提供商的 subscribe 支持...\n");

        // 并发测试所有端点
        let futures: Vec<_> = endpoints
            .iter()
            .map(|endpoint| test_rpc_subscribe_support(endpoint))
            .collect();

        let results_vec = futures::future::join_all(futures).await;

        // 收集结果
        for (name, success, error) in results_vec {
            results.insert(name.clone(), (success, error.clone()));

            if success {
                println!("✅ {}: Subscribe 支持", name);
            } else {
                println!(
                    "❌ {}: Subscribe 不支持 - {}",
                    name,
                    error.unwrap_or("未知错误".to_string())
                );
            }
        }

        // 统计结果
        let total = results.len();
        let supported = results.values().filter(|(success, _)| *success).count();
        let unsupported = total - supported;

        println!("\n📊 测试结果统计:");
        println!("   总计: {} 个 RPC 端点", total);
        println!("   支持: {} 个", supported);
        println!("   不支持: {} 个", unsupported);
        println!(
            "   成功率: {:.1}%",
            (supported as f64 / total as f64) * 100.0
        );

        // 输出详细的支持列表
        println!("\n✅ 支持 Subscribe 的 RPC:");
        for (name, (success, _)) in &results {
            if *success {
                println!("   - {}", name);
            }
        }

        println!("\n❌ 不支持 Subscribe 的 RPC:");
        for (name, (success, error)) in &results {
            if !*success {
                println!(
                    "   - {}: {}",
                    name,
                    error.as_ref().unwrap_or(&"未知原因".to_string())
                );
            }
        }

        // 这个测试不会失败，只是用来验证和展示结果
        assert!(total > 0, "应该至少测试了一个 RPC 端点");
    }

    /// 使用环境变量测试自定义 RPC 端点
    // TEST_RPC_WS_URL=wss://your-rpc-url.com cargo test custom_rpc_subscribe -- --nocapture
    #[tokio::test]
    async fn test_custom_rpc_subscribe() {
        // 从环境变量读取自定义 RPC URL
        if let Ok(custom_rpc_url) = std::env::var("TEST_RPC_WS_URL") {
            println!("\n🧪 测试自定义 RPC 端点: {}", custom_rpc_url);

            let endpoint = RpcEndpoint {
                name: "Custom RPC",
                url: Box::leak(custom_rpc_url.into_boxed_str()),
            };

            let (_name, success, error) = test_rpc_subscribe_support(&endpoint).await;

            if success {
                println!("✅ 自定义 RPC 支持 Subscribe!");
            } else {
                println!(
                    "❌ 自定义 RPC 不支持 Subscribe: {}",
                    error.unwrap_or("未知错误".to_string())
                );
            }
        } else {
            println!("💡 提示: 设置 TEST_RPC_WS_URL 环境变量来测试自定义 RPC 端点");
        }
    }

    fn pair_created_log(
        factory: Address,
        token0: Address,
        token1: Address,
        pair: Address,
        block_hash: B256,
        block_number: u64,
    ) -> Log {
        use alloy::primitives::U256;
        use crate::amms::uniswap_v2::IUniswapV2Factory;
        let event = IUniswapV2Factory::PairCreated {
            token0,
            token1,
            pair,
            // anonymous trailing uint in the event ABI
            _3: U256::from(1u64),
        };
        Log {
            inner: alloy::primitives::Log {
                address: factory,
                data: event.encode_log_data(),
            },
            block_hash: Some(block_hash),
            block_number: Some(block_number),
            block_timestamp: None,
            transaction_hash: Some(test_hash(6)),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    /// WHI-784: empty factory list freezes the universe — creation logs must not
    /// introduce pools that were not in the loaded AMM set.
    #[tokio::test]
    async fn empty_factories_ignore_pool_creation_logs() {
        use crate::amms::uniswap_v2::UniswapV2Factory;

        let known_pool = Address::repeat_byte(0x11);
        let factory_addr = Address::repeat_byte(0xF1);
        let new_pair = Address::repeat_byte(0x22);
        let mut state = StateSpace {
            state: HashMap::from([(
                known_pool,
                AMM::UniswapV2Pool(UniswapV2Pool {
                    address: known_pool,
                    reserve_0: 100,
                    reserve_1: 200,
                    ..Default::default()
                }),
            )]),
            latest_block: Arc::new(AtomicU64::new(10)),
            cache: StateChangeCache::default(),
        };
        let size_before = state.state.len();

        // Even with a well-formed PairCreated log, empty factories ⇒ no add.
        let creation = pair_created_log(
            factory_addr,
            Address::repeat_byte(0xA1),
            Address::repeat_byte(0xA2),
            new_pair,
            test_hash(4),
            11,
        );
        let asserter = Asserter::new();
        // initialize_new_pools would eth_call init if a factory matched; none do.
        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let added = initialize_new_pools(
            &mut state,
            &[creation.clone()],
            &[],
            hash_pinned_state_block_id(test_hash(4)),
            provider.clone(),
            &[],
        )
        .await
        .unwrap();
        assert!(added.is_empty());
        assert_eq!(state.state.len(), size_before);
        assert!(!state.state.contains_key(&new_pair));

        // Contrast: with a matching factory the creation log is recognized (init
        // will fail for lack of mock eth_call — we only assert the candidate is
        // selected by checking the error path is not "no match").
        let factory = Factory::UniswapV2Factory(UniswapV2Factory {
            address: factory_addr,
            creation_block: 1,
            fee: 30,
        });
        // Re-seed state.
        state.state.clear();
        state.state.insert(
            known_pool,
            AMM::UniswapV2Pool(UniswapV2Pool {
                address: known_pool,
                reserve_0: 100,
                reserve_1: 200,
                ..Default::default()
            }),
        );
        // Without eth_call responses, init fails — proving create_pool matched.
        let err = initialize_new_pools(
            &mut state,
            &[creation],
            &[factory],
            hash_pinned_state_block_id(test_hash(4)),
            provider,
            &[],
        )
        .await
        .unwrap_err();
        // Pool was not inserted because init failed closed.
        assert_eq!(state.state.len(), 1);
        assert!(!state.state.contains_key(&new_pair));
        let _ = err; // any RPC/AMM error is fine; key is we attempted init
    }

    /// WHI-784: syncing with only pre-loaded AMMs (no factories) never issues
    /// `eth_getLogs`. Mock provider has no log responses; any historical log
    /// query fails the asserter.
    #[tokio::test]
    async fn sync_without_factories_issues_no_eth_get_logs() {
        let tip = 42u64;
        let tip_hash = test_hash(0x42);
        let parent = test_hash(0x41);
        let asserter = Asserter::new();
        // get_block_number
        asserter.push_success(&tip);
        // tip_before header
        asserter.push_success(&Some(mock_block(tip, tip_hash, parent)));
        // tip_after identity recheck (no factory discover, no remaining AMM init)
        asserter.push_success(&Some(mock_block(tip, tip_hash, parent)));
        // Deliberately no eth_getLogs responses — discover would fail closed.

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let manager: StateSpaceManager<Ethereum, _> = StateSpaceBuilder::new(provider)
            .chain_id(5000)
            .with_amms(vec![])
            // no .with_factories — frozen-universe path
            .sync()
            .await
            .expect("sync without factories must not need eth_getLogs");

        assert_eq!(manager.latest_block.load(Ordering::Relaxed), tip);
        assert!(manager.state.read().await.state.is_empty());
        assert!(asserter.read_q().is_empty());
    }

    /// 测试 StateSpaceManager 的完整订阅流程（模拟）
    // TEST_RPC_WS_URL=wss://your-rpc-url.com cargo test test_state_space_manager_mock_subscribe -- --nocapture
    #[tokio::test]
    async fn test_state_space_manager_mock_subscribe() {
        // 这是一个简化的测试，验证 StateSpaceManager 的基本结构
        use alloy::providers::ProviderBuilder;
        use alloy::rpc::client::ClientBuilder;

        // 如果有可用的测试 RPC，创建一个 StateSpaceManager
        if let Ok(test_url) = std::env::var("TEST_RPC_WS_URL") {
            let ws = WsConnect::new(&test_url);
            match ClientBuilder::default().ws(ws).await {
                Ok(client) => {
                    let provider = ProviderBuilder::new().connect_client(client);

                    let manager: StateSpaceManager<Ethereum, _> = StateSpaceBuilder::new(provider)
                        .sync()
                        .await
                        .expect("Failed to create StateSpaceManager");

                    // Discovery now hash-pins the canonical tip and publishes Ready.
                    assert!(manager.latest_block.load(Ordering::Relaxed) > 0);
                    assert!(manager.allows_execution().await);
                    let ready = manager
                        .ready_snapshot()
                        .await
                        .expect("Ready after discovery");
                    assert_eq!(
                        ready.id.block_number,
                        manager.latest_block.load(Ordering::Relaxed)
                    );

                    println!("✅ StateSpaceManager 创建成功，可以进行 subscribe 测试");

                    // 注意: 实际的 subscribe 测试需要真实的区块链连接
                    // 这里只是验证结构体可以正确创建
                }
                Err(e) => {
                    println!("⚠️  无法连接到测试 RPC: {}", e);
                }
            }
        } else {
            println!("💡 提示: 设置 TEST_RPC_WS_URL 环境变量来进行完整的 StateSpaceManager 测试");
        }
    }
}
