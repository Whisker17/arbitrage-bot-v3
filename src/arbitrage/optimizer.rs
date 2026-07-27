use alloy::primitives::U256;

use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
};

use super::error::ArbitrageError;
use super::pathfinder::{ArbitragePath, PathHop};

#[derive(Debug, Clone)]
pub struct OptimizationConfig {
    pub max_iterations: usize,
    pub tolerance_bps: u32,
    pub min_profit: U256,
    pub max_input: U256,
}

impl Default for OptimizationConfig {
    fn default() -> Self {
        Self {
            max_iterations: 16,
            tolerance_bps: 5,
            min_profit: U256::from(1_000u64),
            max_input: U256::from(10_u128.pow(24)),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptimizationResult {
    pub path: ArbitragePath,
    pub optimal_input: U256,
    pub expected_profit: U256,
    pub output_amount: U256,
}

#[derive(Clone)]
pub struct PathOptimizer {
    config: OptimizationConfig,
}

impl PathOptimizer {
    pub fn new(config: OptimizationConfig) -> Self {
        Self { config }
    }

    pub fn optimize(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
    ) -> Result<Option<OptimizationResult>, ArbitrageError> {
        if pools.len() != path.hops.len() {
            return Err(ArbitrageError::Optimization(
                "Mismatch between path hops and pools".into(),
            ));
        }

        let initial_guess = U256::from(10_u128.pow(18));
        let mut low = U256::ZERO;
        let mut high = initial_guess.min(self.config.max_input);
        let mut best_result: Option<OptimizationResult> = None;

        for _ in 0..self.config.max_iterations {
            let mid = (low + high) >> 1;
            let simulation = simulate_path(path, pools, mid)?;

            if let Some(sim) = simulation {
                if sim.expected_profit > self.config.min_profit {
                    best_result = Some(sim.clone());
                    low = mid + U256::from(1);
                } else {
                    high = mid.saturating_sub(U256::from(1));
                }
            } else {
                high = mid.saturating_sub(U256::from(1));
            }
        }

        Ok(best_result)
    }
}

pub fn simulate_path(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> Result<Option<OptimizationResult>, ArbitrageError> {
    if path.hops.is_empty() {
        tracing::warn!(target: "simulate.path", "No hops found in arbitrage path");
        return Ok(None);
    }

    if amount_in.is_zero() {
        tracing::warn!(
            target: "simulate.path",
            message = "Skipping simulation because input amount is zero"
        );
        return Ok(None);
    }

    let mut current_amount = amount_in;

    for (index, (hop, amm)) in path.hops.iter().zip(pools.iter()).enumerate() {
        tracing::debug!(
            target: "simulate.path",
            hop_index = index,
            pool = %hop.pool_address,
            token_in = %hop.token_in,
            token_out = %hop.token_out,
            input_amount = %current_amount,
            "Simulating hop"
        );

        let output = match simulate_hop(amm, hop, current_amount) {
            Ok(output) => output,
            Err(AMMError::IncompleteState) => {
                tracing::debug!(
                    target: "simulate.path",
                    hop_index = index,
                    pool = %hop.pool_address,
                    "Skipping path because AMM state is incomplete"
                );
                return Ok(None);
            }
            Err(error) => return Err(ArbitrageError::Simulation(error.to_string())),
        };
        if output.is_zero() {
            tracing::warn!(
                target: "simulate.path",
                hop_index = index,
                pool = %hop.pool_address,
                "Simulation produced zero output; aborting path"
            );
            return Ok(None);
        }
        tracing::debug!(
            target: "simulate.path",
            hop_index = index,
            pool = %hop.pool_address,
            output_amount = %output,
            "Hop simulation succeeded"
        );
        current_amount = output;
    }

    let expected_profit = current_amount.checked_sub(amount_in);

    match expected_profit {
        Some(profit) => {
            tracing::debug!(
                target: "simulate.path",
                final_output = %current_amount,
                expected_profit = %profit,
                "Simulation completed"
            );
            Ok(Some(OptimizationResult {
                path: path.clone(),
                optimal_input: amount_in,
                expected_profit: profit,
                output_amount: current_amount,
            }))
        }
        None => {
            tracing::warn!(
                target: "simulate.path",
                final_output = %current_amount,
                input_amount = %amount_in,
                "Simulation failed to compute profit (underflow)"
            );
            Ok(None)
        }
    }
}

fn simulate_hop(amm: &AMM, hop: &PathHop, amount_in: U256) -> Result<U256, AMMError> {
    amm.simulate_swap(hop.token_in, hop.token_out, amount_in)
}

pub fn pools_for_path(
    path: &ArbitragePath,
    state_pools: &[AMM],
) -> Result<Vec<AMM>, ArbitrageError> {
    let mut pools = Vec::with_capacity(path.hops.len());

    for hop in &path.hops {
        let pool = state_pools
            .iter()
            .find(|pool| pool.address() == hop.pool_address)
            .ok_or_else(|| {
                ArbitrageError::Simulation(format!(
                    "Pool {:?} not found in state",
                    hop.pool_address
                ))
            })?;

        pools.push(pool.clone());
    }

    Ok(pools)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::{amm::AMM, uniswap_v3::UniswapV3Pool, Token};
    use alloy::primitives::Address;

    fn addr(b: u8) -> Address {
        let mut raw = [0u8; 20];
        raw[19] = b;
        Address::from(raw)
    }

    fn dummy_pool() -> AMM {
        let mut pool = UniswapV3Pool::default();
        pool.address = addr(1);
        pool.token_a = Token::new_with_decimals(addr(2), 6);
        pool.token_b = Token::new_with_decimals(addr(3), 6);
        pool.liquidity = 1_000_000;
        pool.sqrt_price = U256::from(1) << 96;
        pool.fee = 3000;
        AMM::from(pool)
    }

    #[test]
    fn simulate_path_zero_amount_returns_none() {
        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 3000,
            }],
        };

        let result = simulate_path(&path, &[dummy_pool()], U256::ZERO).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn simulate_path_skips_incomplete_amm_state() {
        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 3000,
            }],
        };

        let result = simulate_path(&path, &[dummy_pool()], U256::from(10_000)).unwrap();
        assert!(result.is_none());
    }
}
