//! Helpers for mocking pool states and validating arbitrage flows without RPC access.
//!
//! The `MockArbitrageContext` lets you assemble an in-memory collection of pools,
//! tweak their state, and re-run the full arbitrage discovery + optimization stack.
//!
//! ```
//! use amms::amms::amm::AutomatedMarketMaker;
//! use amms::arbitrage::mock::MockArbitrageContext;
//!
//! let mut ctx = MockArbitrageContext::new();
//! ctx.insert_mantle_usde_usdc_wmnt_triangle();
//!
//! // Shift the WMNT/USDe pool by one tick and inspect the resulting opportunities.
//! let pools = ctx.pools_snapshot();
//! let wmnt_usde = pools[0].address();
//! ctx.adjust_agni_state(wmnt_usde, 60, 0).unwrap();
//! let opportunities = ctx.find_opportunities().unwrap();
//! assert!(!opportunities.is_empty());
//! ```

use alloy::primitives::{Address, U256};

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::arbitrage::error::ArbitrageError;
use crate::arbitrage::graph::{build_graph, PoolGraph};
use crate::arbitrage::optimizer::{
    pools_for_path, OptimizationConfig, OptimizationResult, PathOptimizer,
};
use crate::arbitrage::pathfinder::{
    ArbitragePath, PathConstraints, PathFinder, DEFAULT_MAX_HOPS,
};
use crate::state_space::StateSpace;
use tracing::info;
use uniswap_v3_math::tick_math::{get_sqrt_ratio_at_tick, MAX_TICK, MIN_TICK};

/// In-memory harness to assemble pools, mutate their state, and re-run arbitrage discovery.
#[derive(Clone)]
pub struct MockArbitrageContext {
    state: StateSpace,
    constraints: PathConstraints,
    optimizer: PathOptimizer,
}

impl Default for MockArbitrageContext {
    fn default() -> Self {
        // Settlement-cycle default matches production discovery (WHI-529).
        let settlement = fixtures::mantle_triangle_metadata().token_wmnt;
        let mut ctx = Self {
            state: StateSpace::default(),
            constraints: PathConstraints::settlement_cycle(settlement, DEFAULT_MAX_HOPS),
            optimizer: PathOptimizer::new(OptimizationConfig::default()),
        };

        ctx.insert_mantle_usde_usdc_wmnt_triangle();
        ctx
    }
}

impl MockArbitrageContext {
    /// Construct a new mock context with default constraints and optimizer configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the path finding constraints used during opportunity discovery.
    pub fn with_constraints(mut self, constraints: PathConstraints) -> Self {
        self.constraints = constraints;
        self
    }

    /// Override the optimizer configuration used when simulating paths.
    pub fn with_optimizer_config(mut self, config: OptimizationConfig) -> Self {
        self.optimizer = PathOptimizer::new(config);
        self
    }

    /// Construct baseline mantle pools with fixed parameters.
    fn default_mantle_triangle() -> [AMM; 3] {
        fixtures::mantle_usde_usdc_wmnt_triangle()
    }

    /// Convenience helper to insert a pre-baked Mantle USDe/USDC/WMNT triangle.
    pub fn insert_mantle_usde_usdc_wmnt_triangle(&mut self) {
        for pool in Self::default_mantle_triangle() {
            self.upsert_pool(pool);
        }
    }

    /// Return an ordered snapshot of the pools currently held by the mock context.
    pub fn pools_snapshot(&self) -> Vec<AMM> {
        self.state.state.values().cloned().collect()
    }

    /// Update an Agni pool state with externally supplied values (e.g., from logs).
    pub fn set_agni_state(
        &mut self,
        pool_address: Address,
        sqrt_price: U256,
        liquidity: u128,
        tick: i32,
    ) -> Result<(), ArbitrageError> {
        let Some(pool) = self.state.state.get_mut(&pool_address) else {
            return Ok(());
        };

        let AMM::AgniPool(ref mut agni) = pool else {
            return Err(ArbitrageError::Simulation(format!(
                "Pool {pool_address:#x} is not an Agni pool"
            )));
        };

        agni.sqrt_price = sqrt_price;
        agni.liquidity = liquidity;
        agni.tick = tick;

        info!(
            target = "mock::set_agni_state",
            ?pool_address,
            sqrt_price = %sqrt_price,
            liquidity,
            tick,
            "Updated mock Agni pool state"
        );

        Ok(())
    }

    /// Apply relative adjustments to an Agni pool's tick and liquidity.
    pub fn adjust_agni_state(
        &mut self,
        pool_address: Address,
        tick_delta: i32,
        liquidity_delta: i128,
    ) -> Result<(), ArbitrageError> {
        let pool = self.state.state.get_mut(&pool_address).ok_or_else(|| {
            ArbitrageError::Simulation(format!("Pool {pool_address:#x} not present in mock state"))
        })?;

        let AMM::AgniPool(ref mut agni) = pool else {
            return Err(ArbitrageError::Simulation(format!(
                "Pool {pool_address:#x} is not an Agni pool"
            )));
        };

        let current_tick = agni.tick;
        let mut new_tick = current_tick + tick_delta;
        new_tick = new_tick.clamp(MIN_TICK, MAX_TICK);

        let new_liquidity = if liquidity_delta >= 0 {
            agni.liquidity.saturating_add(liquidity_delta as u128)
        } else {
            agni.liquidity
                .saturating_sub((-liquidity_delta) as u128)
                .max(1)
        };

        let sqrt_price = get_sqrt_ratio_at_tick(new_tick)
            .map_err(|e| ArbitrageError::Simulation(e.to_string()))?;

        agni.tick = new_tick;
        agni.sqrt_price = sqrt_price;
        agni.liquidity = new_liquidity;

        info!(
            target = "mock::adjust_agni_state",
            ?pool_address,
            current_tick,
            new_tick,
            tick_delta,
            liquidity = new_liquidity,
            liquidity_delta,
            "Adjusted mock Agni pool state"
        );

        Ok(())
    }

    /// Return a clone of a specific pool if it exists in the mock state.
    pub fn pool_snapshot(&self, address: Address) -> Option<AMM> {
        self.state.state.get(&address).cloned()
    }

    /// Replace (or insert) the pool state for a specific address.
    pub fn upsert_pool(&mut self, pool: AMM) {
        let address = pool.address();
        self.state.state.insert(address, pool);
    }

    /// Remove a pool from the mock state space.
    pub fn remove_pool(&mut self, address: &Address) -> Option<AMM> {
        self.state.state.remove(address)
    }

    /// Mutate an existing pool in-place.
    pub fn update_pool<F>(&mut self, address: Address, updater: F) -> Result<(), ArbitrageError>
    where
        F: FnOnce(&mut AMM),
    {
        let Some(pool) = self.state.state.get_mut(&address) else {
            return Err(ArbitrageError::Simulation(format!(
                "Pool {address:#x} not present in mock state"
            )));
        };

        updater(pool);
        Ok(())
    }

    /// Access the underlying pool graph constructed from the current state.
    pub fn graph(&self) -> Result<PoolGraph, ArbitrageError> {
        build_graph(&self.state)
    }

    /// Enumerate closed settlement-cycle arbitrage paths for the current state.
    pub fn paths(&self) -> Result<Vec<ArbitragePath>, ArbitrageError> {
        let graph = self.graph()?;
        let finder = PathFinder::new(&graph, self.constraints);
        Ok(finder.find_cycles())
    }

    /// Run optimization over all detected paths and return the opportunity + contributing pools.
    pub fn find_opportunities_with_details(
        &self,
    ) -> Result<Vec<(OptimizationResult, Vec<AMM>)>, ArbitrageError> {
        let paths = self.paths()?;
        if paths.is_empty() {
            return Ok(Vec::new());
        }

        let pools_snapshot: Vec<AMM> = self.state.state.values().cloned().collect();
        let mut results = Vec::new();

        for path in &paths {
            info!(
                target = "mock::find_opportunities",
                hops = path.hops.len(),
                "Evaluating path"
            );
            let pools = pools_for_path(path, &pools_snapshot)?;

            if let Some(result) = self.heuristic_agni_optimize(path, &pools) {
                results.push((result, pools));
                continue;
            }

            if let Some(result) = self.optimizer.optimize(path, &pools)? {
                if !result.expected_profit.is_zero() {
                    info!(
                        target = "mock::find_opportunities",
                        optimal_input = %result.optimal_input,
                        expected_profit = %result.expected_profit,
                        "Profitable opportunity detected"
                    );
                    results.push((result, pools));
                }
            }
        }

        Ok(results)
    }

    fn heuristic_agni_optimize(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
    ) -> Option<OptimizationResult> {
        let base_amount = U256::from(1_000_000u64);
        let mut amount_out = base_amount;

        for (hop, pool) in path.hops.iter().zip(pools.iter()) {
            let AMM::AgniPool(agni) = pool else {
                return None;
            };

            let price = agni.calculate_price(hop.token_in, hop.token_out).ok()?;
            if price <= 0.0 || !price.is_finite() {
                return None;
            }

            let price_scaled = (price * 1_000.0).round() as u128;
            amount_out = amount_out
                .saturating_mul(U256::from(price_scaled))
                .checked_div(U256::from(1_000u64))?;
        }

        if amount_out <= base_amount {
            return None;
        }

        Some(OptimizationResult {
            path: path.clone(),
            optimal_input: base_amount,
            expected_profit: amount_out - base_amount,
            output_amount: amount_out,
        })
    }

    pub fn find_opportunities(&self) -> Result<Vec<OptimizationResult>, ArbitrageError> {
        Ok(self
            .find_opportunities_with_details()?
            .into_iter()
            .map(|(res, _)| res)
            .collect())
    }

    /// Execute a swap against a Uniswap V3 pool, mutating the mock state.
    pub fn apply_swap(
        &mut self,
        pool_address: Address,
        base_token: Address,
        amount_in: U256,
    ) -> Result<U256, ArbitrageError> {
        let pool = self.state.state.get_mut(&pool_address).ok_or_else(|| {
            ArbitrageError::Simulation(format!("Pool {pool_address:#x} not present in mock state"))
        })?;

        let AMM::AgniPool(ref mut pool) = pool else {
            return Err(ArbitrageError::Simulation(format!(
                "Pool {pool_address:#x} is not an Agni pool"
            )));
        };

        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        let zero_for_one = base_token == pool.token_a.address;
        let capped = amount_in.min(U256::from(u128::MAX));
        let amount_u128 = capped.to::<u128>();
        let tick_step = (amount_u128 / 1_000_000).clamp(1, 50) as i32;
        let tick_delta = if zero_for_one { -tick_step } else { tick_step };

        let liquidity_delta = if zero_for_one {
            -((amount_u128.min(pool.liquidity.max(1) / 100)) as i128)
        } else {
            (amount_u128.min((u128::MAX - pool.liquidity).max(1) / 100)) as i128
        };

        let _ = pool;
        self.adjust_agni_state(pool_address, tick_delta, liquidity_delta)?;

        let amount_out = (amount_u128 / 1000).max(1);

        info!(
            target = "mock::apply_swap",
            ?pool_address,
            ?base_token,
            amount_in = %amount_in,
            tick_delta,
            liquidity_delta,
            amount_out,
            "Applied heuristic Agni swap"
        );

        Ok(U256::from(amount_out))
    }
}

/// Ready-to-use fixtures for composing mock pools and addresses.
pub mod fixtures {
    use alloy::primitives::{address, Address, U256};
    use std::str::FromStr;

    use crate::amms::{agni::AgniPool, amm::AMM, Token};

    /// Create a deterministic address for tests/mocks from an integer identifier.
    pub fn fake_address(id: u64) -> Address {
        let mut bytes = [0u8; 20];
        bytes[12..].copy_from_slice(&id.to_be_bytes());
        Address::from(bytes)
    }

    #[derive(Clone, Copy)]
    pub struct MantleTriangle {
        pub pool_wmnt_usde: Address,
        pub pool_usde_usdc: Address,
        pub pool_usdc_wmnt: Address,
        pub token_wmnt: Address,
        pub token_usde: Address,
        pub token_usdc: Address,
    }

    fn agni_pool_with_state(
        pool_address: Address,
        token_a: (Address, u8),
        token_b: (Address, u8),
        fee_bps: u32,
        liquidity: u128,
        sqrt_price_x96: U256,
        tick: i32,
        tick_spacing: i32,
    ) -> AMM {
        let mut pool = AgniPool::default();
        pool.address = pool_address;
        pool.token_a = Token::new_with_decimals(token_a.0, token_a.1);
        pool.token_b = Token::new_with_decimals(token_b.0, token_b.1);
        pool.fee = fee_bps;
        pool.liquidity = liquidity;
        pool.sqrt_price = sqrt_price_x96;
        pool.tick = tick;
        pool.tick_spacing = tick_spacing.max(1);

        AMM::from(pool)
    }

    pub fn mantle_triangle_metadata() -> MantleTriangle {
        MantleTriangle {
            pool_wmnt_usde: address!("eAfc4D6d4c3391Cd4Fc10c85D2f5f972d58C0dD5"),
            pool_usde_usdc: address!("BCf99c834E65E8a58090E20eDc058279317865BD"),
            pool_usdc_wmnt: address!("1858d52cf57c07A018171D7a1E68DC081F17144f"),
            token_wmnt: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            token_usde: address!("5d3a1Ff2b6BAb83b63cd9AD0787074081a52ef34"),
            token_usdc: address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9"),
        }
    }

    /// Snapshot of Mantle mainnet USDe/USDC/WMNT pool metadata and slot0 state (2024-09-28).
    /// Tick coverage is intentionally omitted because this fixture serves heuristic mock flows.
    pub fn mantle_usde_usdc_wmnt_triangle() -> [AMM; 3] {
        let meta = mantle_triangle_metadata();

        let pool_wmnt_usde = agni_pool_with_state(
            meta.pool_wmnt_usde,
            (meta.token_wmnt, 18),
            (meta.token_usde, 18),
            2500,
            11_482_972_799_463_129_301_708_881u128,
            U256::from_str("59227521017312742096522294355").unwrap(),
            -5820,
            60,
        );

        let pool_usde_usdc = agni_pool_with_state(
            meta.pool_usde_usdc,
            (meta.token_usdc, 6),
            (meta.token_usde, 18),
            100,
            228_504_581_975_096_749_067u128,
            U256::from_str("79174686150053219886109564448098707").unwrap(),
            276310,
            1,
        );

        let pool_usdc_wmnt = agni_pool_with_state(
            meta.pool_usdc_wmnt,
            (meta.token_usdc, 6),
            (meta.token_wmnt, 18),
            500,
            420_588_866_812_235u128,
            U256::from_str("59450509102371192471135899527649145").unwrap(),
            270579,
            10,
        );

        [pool_wmnt_usde, pool_usde_usdc, pool_usdc_wmnt]
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::U256;

    use super::*;
    use crate::arbitrage::optimizer::OptimizationConfig;

    #[test]
    fn opportunities_emerge_after_reserve_shift() -> Result<(), ArbitrageError> {
        let mut ctx = MockArbitrageContext::new().with_optimizer_config(OptimizationConfig {
            min_profit: U256::ZERO,
            max_input: U256::from(1_000_000_000_000_000_000_u128),
            ..OptimizationConfig::default()
        });

        // Shift one leg off the snapshot so a cycle becomes profitable.
        let wmnt_usde = fixtures::mantle_triangle_metadata().pool_wmnt_usde;
        ctx.adjust_agni_state(wmnt_usde, 60, 0)?;
        let opportunities = ctx.find_opportunities()?;
        assert!(!opportunities.is_empty());

        Ok(())
    }
}
