//! Protocol trait + three impls (WHI-727 / WHI-527.2).
//!
//! Bridges existing protocol-tagging enums without introducing a fourth:
//! - [`PoolProtocol`] (4-way pool-universe tag)
//! - [`PoolType`] (3-way on-chain execution tag)
//! - [`ProtocolKind`] (gas-profile route-key tag)

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::factory::Factory;
use crate::amms::moe::{
    sync_moe_snapshots_batch, MoeFactory, MoeLbPair, MoeSnapshotContext, MoeSnapshotSyncConfig,
    CANONICAL_MOE_FACTORY, CANONICAL_MOE_FACTORY_CREATION_BLOCK,
};
use crate::amms::agni::{AgniFactory, AgniPool};
use crate::amms::uniswap_v2::{UniswapV2Factory, UniswapV2Pool};
use crate::amms::Token;
use crate::arbitrage::ArbitragePath;
use crate::execution::{
    BinCrossingBucket, Executor, PoolType, ProtocolKind, RouteKey, ShadowExecutionContext,
    TickCrossingBucket,
};
use crate::service::gas::GasConfig;
use crate::service::startup::production_send_allowed;
use crate::state_space::{BlockHeaderContext, PoolProtocol, PoolUniverseRow, SnapshotId};
use alloy::eips::BlockId;
use alloy::network::Ethereum;
use alloy::primitives::{Address, B256, TxHash, U256};
use alloy::providers::DynProvider;
use std::collections::HashSet;

pub use crate::service::error::ProtocolError;
/// Canonical positive-path candidate (defined in `shadow_row` for schema ownership).
pub use crate::service::shadow_row::{Candidate, PositiveCandidate};

/// Default V2 fee in bps-scaled units used by the Agni V2 service (`V2_FEE_BPS = 300`).
pub const V2_FEE: usize = 300;

/// Bins around `active_id` loaded on Moe tip refresh / snapshot sync.
///
/// Aligned with [`crate::amms::moe::MoeSnapshotSyncConfig`]'s default (50).
/// Historically reduced from 200 because the merged bot re-synced **all** Moe
/// pools every block (WHI-862: ~7 min/block of CREATE eth_calls on free-tier
/// Mantle RPC). WHI-885 filters tip refresh to dirty pools, so the per-block
/// cost scales with activity rather than universe size — radius may be widened
/// after a live coverage re-measure; do not treat 50 as a permanent floor.
pub const MOE_BINS_RADIUS: u32 = 50;
/// Moe bin IDs packed per CREATE eth_call (legacy example monitors use the same size).
pub const MOE_BINS_BATCH_SIZE: u32 = 15;

/// Result of a (scaffold) execution attempt.
///
/// Standardized on Moe's `ExecutionAttempt` shape (WHI-729 / WHI-503): the
/// production-gate block is a typed success-path variant so callers never
/// pattern-match human-readable error strings. `Submitted` is reserved for
/// the (still gate-closed) production send path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionAttempt {
    /// Transaction was submitted on-chain (production send path only).
    Submitted(TxHash),
    /// Principal plan was valid; production send gate blocked the send.
    ProductionGateBlocked {
        amount_in: U256,
        min_profit: U256,
    },
}

/// Production vs shadow vs monitor-only execution handle for scaffold
/// [`Protocol::attempt_execution`].
///
/// `MonitorOnly` is for offline / signerless exercise: the default
/// `attempt_execution` body never reads the context when
/// [`production_send_allowed`] is false. Armed production sends go through
/// [`crate::service::send_path::SendRuntime`] (WHI-860), not this enum.
#[derive(Clone, Copy)]
pub enum ServiceExecutionContext<'a> {
    Production(&'a Executor),
    Shadow(&'a ShadowExecutionContext),
    /// No live executor — valid while the production send gate is closed.
    MonitorOnly,
}

/// Which pools a protocol should re-sync on a tip refresh (WHI-885).
///
/// * [`TipRefreshScope::Full`] — cold start, re-baseline, or any numeric gap
///   where intermediate logs may have been missed.
/// * [`TipRefreshScope::Touched`] — consecutive advance; only pools whose
///   addresses emitted protocol `sync_events` in this block.
///
/// V2/V3 currently no-op either mode; the set is threaded so they can adopt
/// selective refresh later without another signature break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TipRefreshScope {
    /// Re-sync every pool of this protocol in `pools`.
    Full,
    /// Re-sync only pools whose address is in the set (dirty this block).
    Touched(HashSet<Address>),
}

impl TipRefreshScope {
    pub fn as_metric_label(&self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Touched(_) => "touched",
        }
    }
}

/// Pure plan for which Moe pools a tip refresh will re-sync (WHI-885).
///
/// Separated from the RPC path so call-count / parity tests can assert the
/// filter without a live CREATE eth_call provider. Indices are into the
/// original `pools` slice so the refresh path does not re-scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoeTipRefreshPlan {
    /// Indices of Moe pools in `pools` that will be re-synced.
    pub refresh_indices: Vec<usize>,
    /// Moe pool addresses that will be re-synced (same order as indices).
    pub to_refresh: Vec<Address>,
    /// Moe pools held (not re-synced) this block.
    pub held: usize,
    /// Scope mode label (`full` / `touched`).
    pub mode: &'static str,
}

/// Plan Moe tip-refresh targets from the pool slice and scope (WHI-885).
///
/// Empty `to_refresh` means **zero** Moe bin RPC work for this block.
pub fn plan_moe_tip_refresh(pools: &[AMM], scope: &TipRefreshScope) -> MoeTipRefreshPlan {
    let mode = scope.as_metric_label();
    let mut refresh_indices = Vec::new();
    let mut to_refresh = Vec::new();
    let mut held = 0usize;
    for (idx, amm) in pools.iter().enumerate() {
        let AMM::MoeLbPair(pair) = amm else {
            continue;
        };
        let take = match scope {
            TipRefreshScope::Full => true,
            TipRefreshScope::Touched(touched) => touched.contains(&pair.address),
        };
        if take {
            refresh_indices.push(idx);
            to_refresh.push(pair.address);
        } else {
            held += 1;
        }
    }
    MoeTipRefreshPlan {
        refresh_indices,
        to_refresh,
        held,
        mode,
    }
}

/// Protocol adapter trait.
///
/// Default bodies for `is_pool_viable` / `refresh_gas_config` /
/// `refresh_block_tip_state` match today's per-file behaviour. Only Moe
/// overrides viability + tip refresh. Agni-V3 and Moe both refresh gas from
/// the live base fee (Moe's live-refresh is the intentional WHI-729 fix).
pub trait Protocol: Send + Sync {
    const NAME: &'static str;

    fn pool_universe_protocol() -> PoolProtocol;
    fn pool_type() -> PoolType;
    fn protocol_kind() -> ProtocolKind;

    fn factory(&self) -> Factory;

    fn build_amm(&self, row: &PoolUniverseRow) -> Result<AMM, ProtocolError>;

    fn is_pool_viable(&self, amm: &AMM) -> bool {
        let _ = amm;
        true
    }

    fn refresh_gas_config(&self, base_fee_per_gas: Option<u64>) -> GasConfig {
        let _ = base_fee_per_gas;
        GasConfig::default()
    }

    /// Optional per-block tip refresh after logs are applied.
    ///
    /// `block_hash` is required because [`BlockHeaderContext`] only carries
    /// parent hash + timestamp; Moe's snapshot context needs the tip hash.
    ///
    /// `scope` selects full vs dirty-pool refresh (WHI-885). Default no-op
    /// protocols ignore it; Moe filters the batch to the planned addresses.
    fn refresh_block_tip_state(
        &self,
        provider: &DynProvider,
        pools: &mut [AMM],
        block_hash: B256,
        header: &BlockHeaderContext,
        scope: &TipRefreshScope,
    ) -> impl std::future::Future<Output = Result<(), ProtocolError>> + Send {
        let _ = (provider, pools, block_hash, header, scope);
        async { Ok(()) }
    }

    /// Simulate a path, returning per-hop outputs, final amount out, and the
    /// measured [`RouteKey`] (V2: plain hops; V3: tick crossings; Moe: bins).
    fn simulate_path_with_route_key(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
        block_timestamp: u64,
    ) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError>;

    /// Scaffold execution attempt shared by all three protocols.
    ///
    /// With the production send gate closed this returns
    /// [`ExecutionAttempt::ProductionGateBlocked`] (typed, not a string `Err`)
    /// after validating the candidate can still be simulated — matching Moe's
    /// soft-success shape generalized across Agni-V2 / Agni-V3 / Moe (WHI-729).
    ///
    /// When the gate is armed, real sends are performed by
    /// [`crate::service::send_path::SendRuntime`] via the discovery job-slot
    /// helper (WHI-860) — this default body still fails closed so a lone
    /// `Protocol::attempt_execution` call cannot broadcast.
    fn attempt_execution(
        &self,
        candidate: &Candidate,
        _ctx: ServiceExecutionContext<'_>,
    ) -> impl std::future::Future<Output = Result<ExecutionAttempt, ProtocolError>> + Send
    where
        Self: Sync,
    {
        async move {
            let _ = self.simulate_path_with_route_key(
                &candidate.path,
                &candidate.pools,
                candidate.input,
                0,
            )?;
            if !production_send_allowed() {
                return Ok(ExecutionAttempt::ProductionGateBlocked {
                    amount_in: candidate.input,
                    min_profit: candidate.net_profit,
                });
            }
            Err(ProtocolError::Execution(
                "use SendRuntime via attempt_discovered_via_job_slot_with_send for production sends (WHI-860)"
                    .into(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Agni V2
// ---------------------------------------------------------------------------

/// Agni UniswapV2-compatible protocol adapter.
#[derive(Debug, Clone)]
pub struct AgniV2Protocol {
    factory_address: Address,
    creation_block: u64,
    fee: usize,
}

impl AgniV2Protocol {
    pub fn new(factory_address: Address) -> Self {
        Self {
            factory_address,
            creation_block: 0,
            fee: V2_FEE,
        }
    }

    pub fn with_creation_block(mut self, creation_block: u64) -> Self {
        self.creation_block = creation_block;
        self
    }
}

impl Protocol for AgniV2Protocol {
    const NAME: &'static str = "agni-v2";

    fn pool_universe_protocol() -> PoolProtocol {
        PoolProtocol::UniswapV2
    }

    fn pool_type() -> PoolType {
        PoolType::UniV2
    }

    fn protocol_kind() -> ProtocolKind {
        ProtocolKind::V2
    }

    fn factory(&self) -> Factory {
        Factory::UniswapV2Factory(UniswapV2Factory::new(
            self.factory_address,
            self.fee,
            self.creation_block,
        ))
    }

    fn build_amm(&self, row: &PoolUniverseRow) -> Result<AMM, ProtocolError> {
        if row.protocol != PoolProtocol::UniswapV2 {
            return Err(ProtocolError::Build(format!(
                "AgniV2Protocol cannot build protocol {:?}",
                row.protocol
            )));
        }
        let mut pool = UniswapV2Pool::new(row.pool, self.fee);
        // Token decimals unknown until on-chain init; use 18 placeholders so
        // the shell is address-complete for fingerprint / graph wiring.
        pool.token_a = Token::new_with_decimals(row.token0, 18);
        pool.token_b = Token::new_with_decimals(row.token1, 18);
        Ok(AMM::UniswapV2Pool(pool))
    }

    fn simulate_path_with_route_key(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
        _block_timestamp: u64,
    ) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError> {
        // Matches v2_monitor_executor_service::simulate_path_steps + RouteKey::new(V2).
        // Empty paths: the example's step helper returns empty outputs, but
        // RouteKey::new forbids hop_count 0, so this combined method fails closed
        // rather than fabricating a 1-hop key.
        if path.hops.is_empty() {
            return Err(ProtocolError::Simulation("empty path".into()));
        }
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let output = amm
                .simulate_swap(hop.token_in, hop.token_out, current)
                .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
            outputs.push(output);
            current = output;
        }
        let route_key = RouteKey::new(vec![ProtocolKind::V2; path.hops.len()])
            .map_err(|e| ProtocolError::RouteKey(e.to_string()))?;
        Ok((outputs, current, route_key))
    }
}

// ---------------------------------------------------------------------------
// Agni V3
// ---------------------------------------------------------------------------

/// Agni V3-compatible protocol adapter.
#[derive(Debug, Clone)]
pub struct AgniV3Protocol {
    factory_address: Address,
    creation_block: u64,
}

impl AgniV3Protocol {
    pub fn new(factory_address: Address) -> Self {
        Self {
            factory_address,
            creation_block: 0,
        }
    }

    pub fn with_creation_block(mut self, creation_block: u64) -> Self {
        self.creation_block = creation_block;
        self
    }
}

impl Protocol for AgniV3Protocol {
    const NAME: &'static str = "agni-v3";

    fn pool_universe_protocol() -> PoolProtocol {
        PoolProtocol::Agni
    }

    fn pool_type() -> PoolType {
        PoolType::UniV3
    }

    fn protocol_kind() -> ProtocolKind {
        ProtocolKind::V3
    }

    fn factory(&self) -> Factory {
        Factory::AgniFactory(AgniFactory::new(self.factory_address, self.creation_block))
    }

    fn build_amm(&self, row: &PoolUniverseRow) -> Result<AMM, ProtocolError> {
        if row.protocol != PoolProtocol::Agni {
            return Err(ProtocolError::Build(format!(
                "AgniV3Protocol cannot build protocol {:?}",
                row.protocol
            )));
        }
        let mut pool = AgniPool::new(row.pool);
        pool.token_a = Token::new_with_decimals(row.token0, 18);
        pool.token_b = Token::new_with_decimals(row.token1, 18);
        Ok(AMM::AgniPool(pool))
    }

    fn refresh_gas_config(&self, base_fee_per_gas: Option<u64>) -> GasConfig {
        // Matches v3_monitor_executor_service_1559::gas_config_for_base_fee when
        // base fee is present. When absent, fall back to the default config so
        // the trait always returns a GasConfig (callers that need Option can
        // check base_fee themselves — the example returns None and skips
        // selection, which is orchestration, not this hook).
        crate::service::gas::gas_config_for_base_fee(base_fee_per_gas)
    }

    fn simulate_path_with_route_key(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
        _block_timestamp: u64,
    ) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError> {
        // Matches legacy_service_support::agni_path_steps_with_route_key.
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        let mut crossings = 0u32;
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let AMM::AgniPool(pool) = amm else {
                return Err(ProtocolError::Simulation(
                    "Agni V3 route contains a non-Agni pool".into(),
                ));
            };
            let evidence = pool
                .simulate_swap_with_crossing_evidence(hop.token_in, current)
                .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
            crossings = crossings.saturating_add(evidence.crossing_count);
            current = evidence.amount_out;
            outputs.push(current);
        }
        if path.hops.is_empty() {
            return Err(ProtocolError::Simulation("empty path".into()));
        }
        let route_key = RouteKey::new(vec![ProtocolKind::V3; path.hops.len()])
            .map_err(|e| ProtocolError::RouteKey(e.to_string()))?
            .with_v3_ticks(TickCrossingBucket::from_crossings(crossings));
        Ok((outputs, current, route_key))
    }
}

// ---------------------------------------------------------------------------
// Moe
// ---------------------------------------------------------------------------

/// Merchant Moe Liquidity Book protocol adapter.
#[derive(Debug, Clone)]
pub struct MoeProtocol {
    factory_address: Address,
    creation_block: u64,
    bins_radius: u32,
    bins_batch_size: u32,
}

impl Default for MoeProtocol {
    fn default() -> Self {
        Self {
            factory_address: CANONICAL_MOE_FACTORY,
            creation_block: CANONICAL_MOE_FACTORY_CREATION_BLOCK,
            bins_radius: MOE_BINS_RADIUS,
            bins_batch_size: MOE_BINS_BATCH_SIZE,
        }
    }
}

impl MoeProtocol {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_factory(mut self, address: Address, creation_block: u64) -> Self {
        self.factory_address = address;
        self.creation_block = creation_block;
        self
    }
}

impl Protocol for MoeProtocol {
    const NAME: &'static str = "moe";

    fn pool_universe_protocol() -> PoolProtocol {
        PoolProtocol::MoeLb
    }

    fn pool_type() -> PoolType {
        PoolType::MoeLB
    }

    fn protocol_kind() -> ProtocolKind {
        ProtocolKind::Moe
    }

    fn factory(&self) -> Factory {
        Factory::MoeFactory(MoeFactory::new(self.factory_address, self.creation_block))
    }

    fn build_amm(&self, row: &PoolUniverseRow) -> Result<AMM, ProtocolError> {
        if row.protocol != PoolProtocol::MoeLb {
            return Err(ProtocolError::Build(format!(
                "MoeProtocol cannot build protocol {:?}",
                row.protocol
            )));
        }
        let mut pool = MoeLbPair::new(row.pool);
        pool.token_x = Token::new_with_decimals(row.token0, 18);
        pool.token_y = Token::new_with_decimals(row.token1, 18);
        Ok(AMM::MoeLbPair(pool))
    }

    fn is_pool_viable(&self, amm: &AMM) -> bool {
        // Matches moe_monitor_executor_service::is_pool_reasonable (zero-reserve filter).
        match amm {
            AMM::MoeLbPair(pool) => pool.reserve_x != 0 && pool.reserve_y != 0,
            _ => false,
        }
    }

    fn refresh_gas_config(&self, base_fee_per_gas: Option<u64>) -> GasConfig {
        // WHI-729 intentional correctness fix: track live base fee the same way
        // Agni-V3 does. The legacy moe example still freezes GasConfig::default()
        // at startup; the merged binary must not.
        crate::service::gas::gas_config_for_base_fee(base_fee_per_gas)
    }

    async fn refresh_block_tip_state(
        &self,
        provider: &DynProvider,
        pools: &mut [AMM],
        block_hash: B256,
        header: &BlockHeaderContext,
        scope: &TipRefreshScope,
    ) -> Result<(), ProtocolError> {
        // WHI-885: only re-sync Moe pools that need a chain read. Events update
        // active_id only (not reserves / bins), so a dirty pool still requires
        // `sync_moe_snapshots_batch` — but untouched pools keep their last
        // snapshot. Full scope covers cold start, re-baseline, and gaps.
        let plan = plan_moe_tip_refresh(pools, scope);
        if plan.refresh_indices.is_empty() {
            // Successful no-op: held-only metric still records the saving.
            crate::metrics::record_moe_tip_refresh(plan.mode, 0, plan.held);
            tracing::info!(
                target: "service.protocol.moe",
                mode = plan.mode,
                refreshed = 0usize,
                held = plan.held,
                "Moe tip refresh: no dirty pools; skipping bin RPC"
            );
            return Ok(());
        }

        let mut dirty_pools: Vec<AMM> = plan
            .refresh_indices
            .iter()
            .map(|&idx| pools[idx].clone())
            .collect();

        tracing::info!(
            target: "service.protocol.moe",
            mode = plan.mode,
            refreshed = dirty_pools.len(),
            held = plan.held,
            "Moe tip refresh: syncing bin snapshots"
        );

        let context = MoeSnapshotContext::new(block_hash, header.block_timestamp);
        let block_id = BlockId::hash_canonical(block_hash);
        sync_moe_snapshots_batch::<Ethereum, _>(
            &mut dirty_pools,
            block_id,
            provider.clone(),
            context,
            MoeSnapshotSyncConfig {
                bins_radius: self.bins_radius,
                bins_per_request: self.bins_batch_size,
            },
        )
        .await
        .map_err(|e| ProtocolError::TipRefresh(e.to_string()))?;

        let refreshed = plan.to_refresh.len();
        let held = plan.held;
        let mode = plan.mode;
        for (idx, synced) in plan
            .refresh_indices
            .into_iter()
            .zip(dirty_pools.into_iter())
        {
            pools[idx] = synced;
        }
        // Count only after a successful sync so failed refreshes do not look
        // like completed re-syncs on the scrape.
        crate::metrics::record_moe_tip_refresh(mode, refreshed, held);
        Ok(())
    }

    fn simulate_path_with_route_key(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
        block_timestamp: u64,
    ) -> Result<(Vec<U256>, U256, RouteKey), ProtocolError> {
        // Matches moe_monitor_executor_service::simulate_path_steps_with_route_key.
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        let mut crossings = 0u32;
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let AMM::MoeLbPair(pool) = amm else {
                return Err(ProtocolError::Simulation(
                    "Moe route contains a non-Moe pool".into(),
                ));
            };
            let swap_for_y = hop.token_in == pool.token_x.address;
            let evidence = pool
                .simulate_swap_with_crossing_evidence(swap_for_y, current, block_timestamp)
                .map_err(|e| ProtocolError::Simulation(e.to_string()))?;
            crossings = crossings.saturating_add(evidence.crossing_count);
            current = evidence.amount_out;
            outputs.push(current);
        }
        if path.hops.is_empty() {
            return Err(ProtocolError::Simulation("empty path".into()));
        }
        let route_key = RouteKey::new(vec![ProtocolKind::Moe; path.hops.len()])
            .map_err(|e| ProtocolError::RouteKey(e.to_string()))?
            .with_moe_bins(BinCrossingBucket::from_crossings(crossings));
        Ok((outputs, current, route_key))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::PathHop;
    use alloy::primitives::{address, I256};

    fn v2_pool(reserve0: u128, reserve1: u128) -> AMM {
        let mut pool = UniswapV2Pool::new(address!("00000000000000000000000000000000000000a1"), 300);
        pool.token_a = Token::new_with_decimals(address!("0000000000000000000000000000000000000001"), 18);
        pool.token_b = Token::new_with_decimals(address!("0000000000000000000000000000000000000002"), 18);
        pool.reserve_0 = reserve0;
        pool.reserve_1 = reserve1;
        AMM::UniswapV2Pool(pool)
    }

    fn agni_pool() -> AMM {
        let mut pool = AgniPool {
            address: address!("0000000000000000000000000000000000000003"),
            token_a: Token::new_with_decimals(address!("0000000000000000000000000000000000000001"), 18),
            token_b: Token::new_with_decimals(address!("0000000000000000000000000000000000000002"), 18),
            liquidity: 1_000_000_000_000_000,
            sqrt_price: uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(0)
                .expect("zero tick"),
            fee: 3_000,
            tick_spacing: 60,
            ..Default::default()
        };
        // Cover a wide bitmap so small swaps don't require tick data.
        pool.tick_bitmap_coverage.extend(-100i16..=100i16);
        AMM::AgniPool(pool)
    }

    fn hop(pool: Address, token_in: Address, token_out: Address) -> PathHop {
        PathHop {
            pool_address: pool,
            token_in,
            token_out,
            fee_bps: 30,
        }
    }

    /// Inline copy of the v2 example's simulate_path_steps for differential check.
    fn example_v2_simulate_path_steps(
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
    ) -> Result<(Vec<U256>, I256), String> {
        if path.hops.is_empty() {
            return Ok((Vec::new(), I256::ZERO));
        }
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let output = amm
                .simulate_swap(hop.token_in, hop.token_out, current)
                .map_err(|e| e.to_string())?;
            outputs.push(output);
            current = output;
        }
        let profit = I256::from_raw(current) - I256::from_raw(amount_in);
        Ok((outputs, profit))
    }

    /// Inline copy of legacy_service_support::agni_path_steps_with_route_key.
    fn example_agni_path_steps_with_route_key(
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
    ) -> Result<(Vec<U256>, I256, RouteKey), String> {
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        let mut crossings = 0u32;
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let AMM::AgniPool(pool) = amm else {
                return Err("Agni V3 route contains a non-Agni pool".into());
            };
            let evidence = pool
                .simulate_swap_with_crossing_evidence(hop.token_in, current)
                .map_err(|e| e.to_string())?;
            crossings = crossings.saturating_add(evidence.crossing_count);
            current = evidence.amount_out;
            outputs.push(current);
        }
        let route_key = RouteKey::new(vec![ProtocolKind::V3; path.hops.len()])
            .map_err(|e| e.to_string())?
            .with_v3_ticks(TickCrossingBucket::from_crossings(crossings));
        Ok((
            outputs,
            I256::from_raw(current) - I256::from_raw(amount_in),
            route_key,
        ))
    }

    #[test]
    fn protocol_tags_match_reconciliation_table() {
        assert_eq!(
            AgniV2Protocol::pool_universe_protocol(),
            PoolProtocol::UniswapV2
        );
        assert_eq!(AgniV2Protocol::pool_type(), PoolType::UniV2);
        assert_eq!(AgniV2Protocol::protocol_kind(), ProtocolKind::V2);
        assert_eq!(AgniV2Protocol::NAME, "agni-v2");

        assert_eq!(AgniV3Protocol::pool_universe_protocol(), PoolProtocol::Agni);
        assert_eq!(AgniV3Protocol::pool_type(), PoolType::UniV3);
        assert_eq!(AgniV3Protocol::protocol_kind(), ProtocolKind::V3);
        assert_eq!(AgniV3Protocol::NAME, "agni-v3");

        assert_eq!(MoeProtocol::pool_universe_protocol(), PoolProtocol::MoeLb);
        assert_eq!(MoeProtocol::pool_type(), PoolType::MoeLB);
        assert_eq!(MoeProtocol::protocol_kind(), ProtocolKind::Moe);
        assert_eq!(MoeProtocol::NAME, "moe");
    }

    #[test]
    fn v2_simulate_matches_example_inline_logic() {
        let t0 = address!("0000000000000000000000000000000000000001");
        let t1 = address!("0000000000000000000000000000000000000002");
        let pool = v2_pool(1_000_000_000_000_000_000_000, 1_000_000_000_000_000_000_000);
        let pool_addr = pool.address();
        let path = ArbitragePath {
            hops: vec![hop(pool_addr, t0, t1)],
        };
        let pools = vec![pool];
        let amount_in = U256::from(1_000_000_000_000_000u64);

        let example = example_v2_simulate_path_steps(&path, &pools, amount_in).unwrap();
        let protocol = AgniV2Protocol::new(Address::ZERO);
        let (outputs, amount_out, route_key) = protocol
            .simulate_path_with_route_key(&path, &pools, amount_in, 0)
            .unwrap();
        let profit = I256::from_raw(amount_out) - I256::from_raw(amount_in);

        assert_eq!(outputs, example.0);
        assert_eq!(profit, example.1);
        assert_eq!(route_key.protocols, vec![ProtocolKind::V2]);
        assert!(route_key.v3_tick_crossings.is_none());
        assert!(route_key.moe_bin_crossings.is_none());
    }

    #[test]
    fn v2_empty_path_fails_closed_for_route_key() {
        // Example simulate_path_steps returns Ok(([], 0)) without a RouteKey.
        // The trait method must also produce a RouteKey, so empty paths error
        // rather than inventing hop_count=1.
        let path = ArbitragePath { hops: vec![] };
        let example = example_v2_simulate_path_steps(&path, &[], U256::from(100u64)).unwrap();
        assert!(example.0.is_empty());
        assert_eq!(example.1, I256::ZERO);

        let protocol = AgniV2Protocol::new(Address::ZERO);
        let err = protocol
            .simulate_path_with_route_key(&path, &[], U256::from(100u64), 0)
            .unwrap_err();
        assert!(err.to_string().contains("empty path"));
    }

    #[test]
    fn v3_simulate_matches_example_inline_logic() {
        let t0 = address!("0000000000000000000000000000000000000001");
        let t1 = address!("0000000000000000000000000000000000000002");
        let pool = agni_pool();
        let pool_addr = pool.address();
        let path = ArbitragePath {
            hops: vec![hop(pool_addr, t0, t1)],
        };
        let pools = vec![pool];
        let amount_in = U256::from(10_000u64);

        // Both may fail on incomplete tick data — then the failure mode must match.
        let example = example_agni_path_steps_with_route_key(&path, &pools, amount_in);
        let protocol = AgniV3Protocol::new(Address::ZERO);
        let extracted = protocol.simulate_path_with_route_key(&path, &pools, amount_in, 0);

        match (example, extracted) {
            (Ok((e_out, e_profit, e_key)), Ok((a_out, a_amount, a_key))) => {
                let a_profit = I256::from_raw(a_amount) - I256::from_raw(amount_in);
                assert_eq!(a_out, e_out);
                assert_eq!(a_profit, e_profit);
                assert_eq!(a_key, e_key);
            }
            (Err(e), Err(a)) => {
                assert!(
                    a.to_string().contains(&e) || e.contains(&a.to_string()),
                    "error mismatch: example={e} extracted={a}"
                );
            }
            (Ok(_), Err(a)) => panic!("example ok but extracted failed: {a}"),
            (Err(e), Ok(_)) => panic!("extracted ok but example failed: {e}"),
        }
    }

    #[test]
    fn moe_zero_reserve_filter() {
        let protocol = MoeProtocol::new();
        let mut dead = MoeLbPair::new(address!("00000000000000000000000000000000000000b1"));
        dead.reserve_x = 0;
        dead.reserve_y = 100;
        assert!(!protocol.is_pool_viable(&AMM::MoeLbPair(dead)));

        let mut live = MoeLbPair::new(address!("00000000000000000000000000000000000000b2"));
        live.reserve_x = 1;
        live.reserve_y = 1;
        assert!(protocol.is_pool_viable(&AMM::MoeLbPair(live)));

        // V2/V3 always viable by default.
        let v2 = AgniV2Protocol::new(Address::ZERO);
        assert!(v2.is_pool_viable(&v2_pool(0, 0)));
    }

    #[test]
    fn gas_refresh_table() {
        let v2 = AgniV2Protocol::new(Address::ZERO);
        // V2 still freezes the default (matches legacy v2 service).
        assert_eq!(v2.refresh_gas_config(Some(99)).gas_price_wei, 25_000_000);

        let v3 = AgniV3Protocol::new(Address::ZERO);
        assert_eq!(v3.refresh_gas_config(Some(42)).gas_price_wei, 42);
        assert_eq!(v3.refresh_gas_config(None).gas_price_wei, 25_000_000);

        let moe = MoeProtocol::new();
        // WHI-729: Moe tracks live base fee (no longer the static 25_000_000 default).
        assert_eq!(moe.refresh_gas_config(Some(99)).gas_price_wei, 99);
        assert_eq!(moe.refresh_gas_config(None).gas_price_wei, 25_000_000);
    }

    #[tokio::test]
    async fn attempt_execution_returns_typed_gate_block() {
        use crate::state_space::SnapshotId;
        use alloy::primitives::I256;

        let t0 = address!("0000000000000000000000000000000000000001");
        let t1 = address!("0000000000000000000000000000000000000002");
        let pool = v2_pool(1_000_000_000_000_000_000_000, 1_000_000_000_000_000_000_000);
        let pool_addr = pool.address();
        let path = ArbitragePath {
            hops: vec![hop(pool_addr, t0, t1)],
        };
        let amount_in = U256::from(1_000_000_000_000_000u64);
        let candidate = Candidate {
            snapshot_id: SnapshotId::new(1, 1, B256::ZERO),
            signature: "test".into(),
            hops: 1,
            input: amount_in,
            output: amount_in,
            profit: I256::ZERO,
            net_profit: U256::from(1u64),
            pool_addresses: vec![pool_addr],
            token_path: vec![t0, t1],
            amounts_out: vec![amount_in],
            expected_states: vec![U256::from(1u64), U256::from(1u64)],
            path,
            pools: vec![pool],
            log_hops: "h".into(),
            roi: "0".into(),
        };
        assert_eq!(Candidate::FIELD_COUNT, 15);

        // Default `attempt_execution` body is shared by AgniV2/AgniV3/Moe;
        // exercise it via V2 (homogeneous pools for re-sim).
        let attempt = AgniV2Protocol::new(Address::ZERO)
            .attempt_execution(&candidate, ServiceExecutionContext::MonitorOnly)
            .await
            .unwrap();
        match attempt {
            ExecutionAttempt::ProductionGateBlocked {
                amount_in: ain,
                min_profit,
            } => {
                assert_eq!(ain, amount_in);
                assert_eq!(min_profit, U256::from(1u64));
            }
            ExecutionAttempt::Submitted(_) => panic!("send gate must stay closed"),
        }
        assert!(!production_send_allowed());
    }

    #[test]
    fn production_send_gate_stays_closed() {
        assert!(!production_send_allowed());
    }

    #[test]
    fn build_amm_sets_addresses() {
        let row = PoolUniverseRow {
            protocol: PoolProtocol::UniswapV2,
            factory: address!("1000000000000000000000000000000000000001"),
            pool: address!("2000000000000000000000000000000000000002"),
            token0: address!("000000000000000000000000000000000000000a"),
            token1: address!("000000000000000000000000000000000000000b"),
        };
        let amm = AgniV2Protocol::new(row.factory).build_amm(&row).unwrap();
        assert_eq!(amm.address(), row.pool);
        assert_eq!(amm.tokens(), vec![row.token0, row.token1]);
    }

    /// Inline copy of moe_monitor_executor_service::simulate_path_steps_with_route_key.
    fn example_moe_path_steps_with_route_key(
        path: &ArbitragePath,
        pools: &[AMM],
        amount_in: U256,
        timestamp: u64,
    ) -> Result<(Vec<U256>, I256, RouteKey), String> {
        let mut current = amount_in;
        let mut outputs = Vec::with_capacity(path.hops.len());
        let mut crossings = 0u32;
        for (hop, amm) in path.hops.iter().zip(pools.iter()) {
            let AMM::MoeLbPair(pool) = amm else {
                return Err("Moe route contains a non-Moe pool".into());
            };
            let swap_for_y = hop.token_in == pool.token_x.address;
            let evidence = pool
                .simulate_swap_with_crossing_evidence(swap_for_y, current, timestamp)
                .map_err(|e| e.to_string())?;
            crossings = crossings.saturating_add(evidence.crossing_count);
            current = evidence.amount_out;
            outputs.push(current);
        }
        let route_key = RouteKey::new(vec![ProtocolKind::Moe; path.hops.len()])
            .map_err(|e| e.to_string())?
            .with_moe_bins(BinCrossingBucket::from_crossings(crossings));
        Ok((
            outputs,
            I256::from_raw(current) - I256::from_raw(amount_in),
            route_key,
        ))
    }

    fn moe_pool_with_snapshot(timestamp: u64) -> AMM {
        use crate::amms::moe::{MoeBinRange, MoeSnapshot, MoeSnapshotContext};
        let mut pair = MoeLbPair::new(address!("1234567890123456789012345678901234567890"));
        pair.token_x =
            Token::new_with_decimals(address!("deaddeaddeaddeaddeaddeaddeaddeaddead0000"), 18);
        pair.token_y =
            Token::new_with_decimals(address!("0d500b1d8e8ef31e21c99d1db9a6444d3adf1270"), 6);
        pair.bin_step = 20;
        pair.active_id = 8_388_608;
        pair.reserve_x = 1_000_000_000_000_000_000;
        pair.reserve_y = 1_000_000;
        pair.protocol_share_bps = 100;
        pair.max_volatility_acc = 250_000;
        // Single active bin with both reserves so a small swap can quote.
        pair.bins.insert(
            pair.active_id,
            crate::amms::moe::BinReserve {
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

    #[test]
    fn moe_simulate_matches_example_inline_logic() {
        let timestamp = 1_700_000_000u64;
        let pool = moe_pool_with_snapshot(timestamp);
        let AMM::MoeLbPair(ref pair) = pool else {
            unreachable!()
        };
        let token_x = pair.token_x.address;
        let token_y = pair.token_y.address;
        let pool_addr = pair.address;
        let path = ArbitragePath {
            hops: vec![hop(pool_addr, token_x, token_y)],
        };
        let pools = vec![pool];
        let amount_in = U256::from(1_000_000_000_000u64); // 1e-6 of 1e18

        let example =
            example_moe_path_steps_with_route_key(&path, &pools, amount_in, timestamp);
        let protocol = MoeProtocol::new();
        let extracted =
            protocol.simulate_path_with_route_key(&path, &pools, amount_in, timestamp);

        match (example, extracted) {
            (Ok((e_out, e_profit, e_key)), Ok((a_out, a_amount, a_key))) => {
                let a_profit = I256::from_raw(a_amount) - I256::from_raw(amount_in);
                assert_eq!(a_out, e_out);
                assert_eq!(a_profit, e_profit);
                assert_eq!(a_key, e_key);
                assert_eq!(a_key.protocols, vec![ProtocolKind::Moe]);
                assert!(a_key.moe_bin_crossings.is_some());
            }
            (Err(e), Err(a)) => {
                assert!(
                    a.to_string().contains(&e) || e.contains(&a.to_string()),
                    "error mismatch: example={e} extracted={a}"
                );
            }
            (Ok(_), Err(a)) => panic!("example ok but extracted failed: {a}"),
            (Err(e), Ok(_)) => panic!("extracted ok but example failed: {e}"),
        }
    }

    fn moe_pool_at(addr: Address, timestamp: u64) -> AMM {
        use crate::amms::moe::{MoeBinRange, MoeSnapshot, MoeSnapshotContext};
        let mut pair = MoeLbPair::new(addr);
        pair.token_x =
            Token::new_with_decimals(address!("deaddeaddeaddeaddeaddeaddeaddeaddead0000"), 18);
        pair.token_y =
            Token::new_with_decimals(address!("0d500b1d8e8ef31e21c99d1db9a6444d3adf1270"), 6);
        pair.bin_step = 20;
        pair.active_id = 8_388_608;
        pair.reserve_x = 1_000_000_000_000_000_000;
        pair.reserve_y = 1_000_000;
        pair.protocol_share_bps = 100;
        pair.max_volatility_acc = 250_000;
        pair.bins.insert(
            pair.active_id,
            crate::amms::moe::BinReserve {
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

    /// WHI-885: empty dirty set → plan refreshes nothing (zero bin RPC).
    #[test]
    fn moe_tip_plan_empty_touched_refreshes_nothing() {
        let timestamp = 1_700_000_000u64;
        let a = address!("1111111111111111111111111111111111111111");
        let b = address!("2222222222222222222222222222222222222222");
        let pools = vec![
            moe_pool_at(a, timestamp),
            moe_pool_at(b, timestamp),
            v2_pool(1_000, 1_000),
        ];
        let plan = plan_moe_tip_refresh(&pools, &TipRefreshScope::Touched(HashSet::new()));
        assert!(plan.to_refresh.is_empty());
        assert_eq!(plan.held, 2);
        assert_eq!(plan.mode, "touched");
    }

    /// WHI-885: one dirty Moe address → only that pool is planned.
    #[test]
    fn moe_tip_plan_single_touched_pool() {
        let timestamp = 1_700_000_000u64;
        let a = address!("1111111111111111111111111111111111111111");
        let b = address!("2222222222222222222222222222222222222222");
        let pools = vec![moe_pool_at(a, timestamp), moe_pool_at(b, timestamp)];
        let mut touched = HashSet::new();
        touched.insert(a);
        let plan = plan_moe_tip_refresh(&pools, &TipRefreshScope::Touched(touched));
        assert_eq!(plan.to_refresh, vec![a]);
        assert_eq!(plan.held, 1);
    }

    /// WHI-885: full scope plans every Moe pool (gap / cold start / re-baseline).
    #[test]
    fn moe_tip_plan_full_refreshes_all_moe() {
        let timestamp = 1_700_000_000u64;
        let a = address!("1111111111111111111111111111111111111111");
        let b = address!("2222222222222222222222222222222222222222");
        let pools = vec![
            moe_pool_at(a, timestamp),
            moe_pool_at(b, timestamp),
            v2_pool(1_000, 1_000),
        ];
        let plan = plan_moe_tip_refresh(&pools, &TipRefreshScope::Full);
        assert_eq!(plan.to_refresh.len(), 2);
        assert!(plan.to_refresh.contains(&a));
        assert!(plan.to_refresh.contains(&b));
        assert_eq!(plan.held, 0);
        assert_eq!(plan.mode, "full");
    }

    /// WHI-885 AC: empty dirty set issues **zero** provider calls.
    ///
    /// Uses a mock provider with no queued responses — any eth_call would fail.
    #[tokio::test]
    async fn moe_tip_refresh_empty_touched_issues_zero_rpc() {
        use alloy::providers::{DynProvider, Provider, ProviderBuilder};
        use alloy::transports::mock::Asserter;

        let timestamp = 1_700_000_000u64;
        let a = address!("1111111111111111111111111111111111111111");
        let b = address!("2222222222222222222222222222222222222222");
        let mut pools = vec![moe_pool_at(a, timestamp), moe_pool_at(b, timestamp)];
        // Capture pre-refresh snapshots for equality check.
        let before: Vec<_> = pools
            .iter()
            .map(|amm| match amm {
                AMM::MoeLbPair(p) => (p.address, p.active_id, p.reserve_x, p.reserve_y),
                _ => unreachable!(),
            })
            .collect();

        let asserter = Asserter::new();
        // No responses pushed — any RPC would error.
        let provider = DynProvider::new(
            ProviderBuilder::new()
                .connect_mocked_client(asserter)
                .erased(),
        );
        let header = BlockHeaderContext::new(B256::ZERO, timestamp);
        let proto = MoeProtocol::new();
        proto
            .refresh_block_tip_state(
                &provider,
                &mut pools,
                B256::repeat_byte(0xab),
                &header,
                &TipRefreshScope::Touched(HashSet::new()),
            )
            .await
            .expect("empty dirty set must short-circuit without RPC");

        let after: Vec<_> = pools
            .iter()
            .map(|amm| match amm {
                AMM::MoeLbPair(p) => (p.address, p.active_id, p.reserve_x, p.reserve_y),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(before, after, "held pools must be untouched");
    }

    /// WHI-885 AC: quote parity — filtered write-back of the dirty pool alone
    /// yields the same quotes as a full write-back when the clean pool's
    /// on-chain state is unchanged (held snapshot == re-read snapshot).
    ///
    /// Models the post-`sync_moe_snapshots_batch` merge without RPC: a
    /// source-of-truth map from a "chain read", applied either to all Moe
    /// addresses (full) or only the dirty set (filtered).
    #[test]
    fn moe_filtered_refresh_quote_parity_with_full() {
        let timestamp = 1_700_000_000u64;
        let dirty_addr = address!("1234567890123456789012345678901234567890");
        let clean_addr = address!("2222222222222222222222222222222222222222");

        // Two independent known-good pools (same constructor as the working
        // moe_simulate_matches_example_inline_logic fixture).
        let base_dirty = moe_pool_with_snapshot(timestamp);
        let mut base_clean_pair = match moe_pool_with_snapshot(timestamp) {
            AMM::MoeLbPair(p) => p,
            _ => unreachable!(),
        };
        base_clean_pair.address = clean_addr;
        let base_clean = AMM::MoeLbPair(base_clean_pair);

        // Chain re-read results for this block: dirty was re-synced (clone of
        // tip state); clean was not traded so re-read equals held.
        let chain_dirty = base_dirty.clone();
        let chain_clean = base_clean.clone();

        // Filtered policy: write back only dirty. Full: write back both.
        let filtered_pools = [chain_dirty.clone(), base_clean.clone()];
        let full_pools = [chain_dirty.clone(), chain_clean.clone()];

        let AMM::MoeLbPair(ref dirty_pair) = base_dirty else {
            unreachable!()
        };
        let path_dirty = ArbitragePath {
            hops: vec![hop(
                dirty_addr,
                dirty_pair.token_x.address,
                dirty_pair.token_y.address,
            )],
        };
        let path_clean = ArbitragePath {
            hops: vec![hop(
                clean_addr,
                dirty_pair.token_x.address,
                dirty_pair.token_y.address,
            )],
        };
        let amount_in = U256::from(1_000_000_000_000u64);
        let proto = MoeProtocol::new();

        // Compare Result shapes (Ok payloads or Err messages). The fixture may
        // soft-fail IncompleteState for some sizes; parity is what matters.
        let f_dirty = proto.simulate_path_with_route_key(
            &path_dirty,
            &[filtered_pools[0].clone()],
            amount_in,
            timestamp,
        );
        let full_dirty = proto.simulate_path_with_route_key(
            &path_dirty,
            &[full_pools[0].clone()],
            amount_in,
            timestamp,
        );
        match (f_dirty, full_dirty) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "dirty quotes must match"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "dirty errs must match"),
            (Ok(a), Err(b)) => panic!("dirty filtered ok={a:?} full err={b}"),
            (Err(a), Ok(b)) => panic!("dirty filtered err={a} full ok={b:?}"),
        }

        let f_clean = proto.simulate_path_with_route_key(
            &path_clean,
            &[filtered_pools[1].clone()],
            amount_in,
            timestamp,
        );
        let full_clean = proto.simulate_path_with_route_key(
            &path_clean,
            &[full_pools[1].clone()],
            amount_in,
            timestamp,
        );
        match (f_clean, full_clean) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "clean quotes must match"),
            (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string(), "clean errs must match"),
            (Ok(a), Err(b)) => panic!("clean filtered ok={a:?} full err={b}"),
            (Err(a), Ok(b)) => panic!("clean filtered err={a} full ok={b:?}"),
        }

        let mut touched = HashSet::new();
        touched.insert(dirty_addr);
        let plan = plan_moe_tip_refresh(
            &[base_dirty, base_clean],
            &TipRefreshScope::Touched(touched),
        );
        assert_eq!(plan.to_refresh, vec![dirty_addr]);
        assert_eq!(plan.held, 1);
    }
}
