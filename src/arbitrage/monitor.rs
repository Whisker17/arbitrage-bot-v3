use std::{fs::OpenOptions, path::PathBuf, sync::Arc};

use alloy::{network::Network, primitives::Address, providers::Provider, rpc::types::Block};
use futures::{Stream, StreamExt};
use tokio::sync::RwLock;

use crate::amms::amm::AMM;
use crate::amms::factory::Factory;
use crate::state_space::{
    error::StateSpaceError, StateSpace, StateSpaceBuilder, StateSpaceManager,
};

use csv::WriterBuilder;
use tracing::info;

use super::error::ArbitrageError;
use super::graph::{build_graph, PoolGraph};
use super::optimizer::{
    pools_for_path, simulate_path, OptimizationConfig, OptimizationResult, PathOptimizer,
};
use super::pathfinder::{ArbitragePath, PathConstraints, PathFinder};

#[derive(Clone)]
#[derive(Default)]
pub struct MonitorConfig {
    pub factories: Vec<Factory>,
    pub manual_pools: Vec<AMM>,
    pub constraints: PathConstraints,
    pub optimization: OptimizationConfig,
    pub opportunity_log_path: Option<PathBuf>,
    pub best_snapshot_log_path: Option<PathBuf>,
    pub pool_update_log_path: Option<PathBuf>,
}


#[derive(Debug, Clone)]
pub struct OpportunisticScanResult {
    pub block_number: u64,
    pub opportunities: Vec<OptimizationResult>,
}

pub struct ArbitrageMonitor<N, P>
where
    N: Network<BlockResponse = Block>,
    P: Provider<N> + Clone + 'static,
{
    provider: P,
    config: MonitorConfig,
    state_manager: StateSpaceManager<N, P>,
    state: Arc<RwLock<StateSpace>>,
    optimizer: PathOptimizer,
    graph: RwLock<Option<PoolGraph>>,
    phantom: std::marker::PhantomData<N>,
}

impl<N, P> ArbitrageMonitor<N, P>
where
    N: Network<BlockResponse = Block>,
    P: Provider<N> + Clone + 'static,
{
    pub async fn new(provider: P, config: MonitorConfig) -> Result<Self, ArbitrageError> {
        let state_manager = StateSpaceBuilder::new(provider.clone())
            .with_factories(config.factories.clone())
            .with_amms(config.manual_pools.clone())
            .sync()
            .await?;

        let state = state_manager.state.clone();

        Ok(Self {
            provider,
            config: config.clone(),
            state_manager,
            state,
            optimizer: PathOptimizer::new(config.optimization.clone()),
            graph: RwLock::new(None),
            phantom: std::marker::PhantomData,
        })
    }

    fn state(&self) -> Arc<RwLock<StateSpace>> {
        self.state.clone()
    }

    pub async fn opportunistic_scan(&self) -> Result<OpportunisticScanResult, ArbitrageError> {
        if !self.state_manager.allows_execution().await {
            return Err(StateSpaceError::SnapshotNotReady.into());
        }
        let block_number = self.provider.get_block_number().await?;
        let state = self.state();
        let state_guard = state.read().await;
        let graph = build_graph(&state_guard)?;
        drop(state_guard);

        let path_finder = PathFinder::new(&graph, self.config.constraints);
        let mut paths = path_finder.find_cycles();
        paths.extend(path_finder.find_two_pool_misprices());

        let mut opportunities = Vec::new();
        let state_guard = state.read().await;
        let pools_snapshot: Vec<AMM> = state_guard.state.values().cloned().collect();

        for path in &paths {
            let pools = pools_for_path(path, &pools_snapshot)?;
            if let Some(result) = self.optimizer.optimize(path, &pools)? {
                if !result.expected_profit.is_zero() {
                    opportunities.push(result);
                }
            }
        }

        self.log_opportunities(block_number, &opportunities)?;
        self.log_best_snapshot(block_number, &paths, &pools_snapshot)?;
        self.log_pool_presence(block_number, &state_guard)?;

        Ok(OpportunisticScanResult {
            block_number,
            opportunities,
        })
    }

    fn log_opportunities(
        &self,
        block_number: u64,
        opportunities: &[OptimizationResult],
    ) -> Result<(), ArbitrageError> {
        let Some(path) = &self.config.opportunity_log_path else {
            return Ok(());
        };

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let need_header = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = WriterBuilder::new().has_headers(false).from_writer(file);

        if need_header {
            writer.write_record([
                "block_number",
                "opportunity_index",
                "path_length",
                "optimal_input",
                "expected_profit",
                "hops",
            ])?;
        }

        if opportunities.is_empty() {
            writer.flush()?;
            info!(
                target: "arb-monitor",
                block = block_number,
                "No profitable opportunities detected; CSV unchanged"
            );
            return Ok(());
        }

        for (idx, opportunity) in opportunities.iter().enumerate() {
            let path_desc = opportunity
                .path
                .hops
                .iter()
                .map(|hop| {
                    format!(
                        "{:#x}->{:#x}@{:#x}(fee_bps={})",
                        hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ");

            writer.write_record([
                block_number.to_string(),
                idx.to_string(),
                opportunity.path.hops.len().to_string(),
                opportunity.optimal_input.to_string(),
                opportunity.expected_profit.to_string(),
                path_desc,
            ])?;
        }

        writer.flush()?;

        Ok(())
    }

    fn log_best_snapshot(
        &self,
        block_number: u64,
        paths: &[ArbitragePath],
        pools_snapshot: &[AMM],
    ) -> Result<(), ArbitrageError> {
        let Some(path) = &self.config.best_snapshot_log_path else {
            return Ok(());
        };

        if paths.is_empty() {
            return Ok(());
        }

        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let need_header = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = WriterBuilder::new().has_headers(false).from_writer(file);

        if need_header {
            writer.write_record([
                "block_number",
                "path_index",
                "path_length",
                "optimal_input",
                "expected_profit",
                "hops",
            ])?;
        }

        for (idx, path) in paths.iter().enumerate() {
            let pools = pools_for_path(path, pools_snapshot)?;
            if let Some(result) = simulate_path(path, &pools, self.config.optimization.max_input)? {
                let hop_desc = path
                    .hops
                    .iter()
                    .map(|hop| {
                        format!(
                            "{:#x}->{:#x}@{:#x}(fee_bps={})",
                            hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");

                writer.write_record([
                    block_number.to_string(),
                    idx.to_string(),
                    path.hops.len().to_string(),
                    result.optimal_input.to_string(),
                    result.expected_profit.to_string(),
                    hop_desc,
                ])?;
            }
        }

        writer.flush()?;

        Ok(())
    }

    fn csv_writer(
        &self,
        path: &PathBuf,
        header: &[&str],
    ) -> Result<csv::Writer<std::fs::File>, ArbitrageError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        let need_header = !path.exists() || std::fs::metadata(path)?.len() == 0;
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut writer = WriterBuilder::new().has_headers(false).from_writer(file);

        if need_header {
            writer.write_record(header)?;
        }

        Ok(writer)
    }

    fn log_pool_updates(
        &self,
        block_number: u64,
        pool_presence: &[(Address, bool)],
    ) -> Result<(), ArbitrageError> {
        let Some(path) = &self.config.pool_update_log_path else {
            return Ok(());
        };

        let mut writer = self.csv_writer(path, &["block_number", "pool_address", "present"])?;

        for (addr, present) in pool_presence {
            writer.write_record([
                block_number.to_string(),
                format!("{:#x}", addr),
                present.to_string(),
            ])?;
        }

        writer.flush()?;
        Ok(())
    }

    fn log_pool_presence(
        &self,
        block_number: u64,
        state_guard: &StateSpace,
    ) -> Result<(), ArbitrageError> {
        let presence: Vec<_> = state_guard
            .state
            .keys()
            .copied()
            .map(|addr| (addr, true))
            .collect();
        self.log_pool_updates(block_number, &presence)
    }

    pub async fn subscribe(
        &self,
    ) -> Result<impl Stream<Item = Result<Vec<Address>, ArbitrageError>>, ArbitrageError> {
        Ok(self
            .state_manager
            .subscribe()
            .await?
            .map(|res| res.map_err(ArbitrageError::from)))
    }

    pub async fn refresh_graph(&self) -> Result<(), ArbitrageError> {
        let state = self.state();
        let state_guard = state.read().await;
        let graph = build_graph(&state_guard)?;
        drop(state_guard);

        *self.graph.write().await = Some(graph);
        Ok(())
    }

    pub async fn handle_updates(&self, updated: Vec<Address>) -> Result<(), ArbitrageError> {
        let state = self.state();
        let state_guard = state.read().await;

        let block_number = self.provider.get_block_number().await?;
        let mut presence = Vec::with_capacity(updated.len());

        for address in updated {
            if state_guard.state.contains_key(&address) {
                tracing::debug!(target: "arb-monitor", ?address, "Pool updated");
                presence.push((address, true));
            } else {
                tracing::debug!(target: "arb-monitor", ?address, "Update for unknown pool");
                presence.push((address, false));
            }
        }

        self.log_pool_updates(block_number, &presence)?;

        drop(state_guard);
        self.refresh_graph().await
    }
}
