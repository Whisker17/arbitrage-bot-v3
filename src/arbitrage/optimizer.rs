use alloy::primitives::U256;

use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    moe::MoeError,
};

use super::error::ArbitrageError;
use super::pathfinder::{ArbitragePath, PathHop};

/// True when a hop cannot be quoted because pool state is not fully loaded.
///
/// Moe surfaces this as [`AMMError::MoeError`]`(`[`MoeError::IncompleteState`]`)`
/// via `#[from]`; the top-level [`AMMError::IncompleteState`] is used by other
/// AMM variants. Both must soft-skip a path rather than abort discovery.
fn is_incomplete_amm_state(err: &AMMError) -> bool {
    matches!(
        err,
        AMMError::IncompleteState | AMMError::MoeError(MoeError::IncompleteState)
    )
}

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

    // Zero input shows up at the low end of the binary search; not operational.
    if amount_in.is_zero() {
        tracing::trace!(
            target: "simulate.path",
            "Skipping simulation because input amount is zero"
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
            Err(error) if is_incomplete_amm_state(&error) => {
                tracing::debug!(
                    target: "simulate.path",
                    hop_index = index,
                    pool = %hop.pool_address,
                    error = %error,
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

    // `final_output < input` is ordinary unprofitability — the common case at
    // every binary-search step on a dead path (WHI-937). Do not treat checked
    // subtraction as an arithmetic "underflow" error: it is a comparison.
    if current_amount < amount_in {
        tracing::trace!(
            target: "simulate.path",
            final_output = %current_amount,
            input_amount = %amount_in,
            "path unprofitable at step"
        );
        return Ok(None);
    }

    let profit = current_amount - amount_in;
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

    /// WHI-937: `final_output < input` is ordinary unprofitability (fee-only
    /// round-trip), not a simulation failure. Soft-skip with Ok(None).
    #[test]
    fn simulate_path_unprofitable_roundtrip_returns_none() {
        use crate::amms::uniswap_v2::UniswapV2Pool;
        use crate::service::protocol::V2_FEE;

        let token_a = addr(0x11);
        let token_b = addr(0x22);
        let pool_addr = addr(0xa1);

        let mut pool = UniswapV2Pool::new(pool_addr, V2_FEE);
        pool.token_a = Token::new_with_decimals(token_a, 18);
        pool.token_b = Token::new_with_decimals(token_b, 18);
        pool.reserve_0 = 1_000_000_000_000_000_000_000;
        pool.reserve_1 = 1_000_000_000_000_000_000_000;
        let pools = vec![AMM::UniswapV2Pool(pool.clone()), AMM::UniswapV2Pool(pool)];

        // Same-pool round-trip always loses the V2 fee → unprofitable.
        let path = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: pool_addr,
                    token_in: token_a,
                    token_out: token_b,
                    fee_bps: 30,
                },
                PathHop {
                    pool_address: pool_addr,
                    token_in: token_b,
                    token_out: token_a,
                    fee_bps: 30,
                },
            ],
        };

        let amount_in = U256::from(10u128.pow(18));
        let result = simulate_path(&path, &pools, amount_in).expect("soft skip");
        assert!(
            result.is_none(),
            "fee-only round-trip must soft-skip as unprofitable, not Err"
        );
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

    /// WHI-862: Moe wraps IncompleteState as `AMMError::MoeError(...)` (via
    /// `#[from]`). Discovery must soft-skip those paths, not abort the whole pass.
    #[test]
    fn simulate_path_skips_moe_incomplete_state_variant() {
        use crate::amms::moe::MoeLbPair;

        let mut pair = MoeLbPair::new(addr(1));
        pair.token_x = Token::new_with_decimals(addr(2), 18);
        pair.token_y = Token::new_with_decimals(addr(3), 18);
        pair.bin_step = 20;
        pair.active_id = 8_388_608;
        // No snapshot → simulate_swap returns MoeError::IncompleteState.
        assert!(pair.snapshot.is_none());

        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: addr(1),
                token_in: addr(2),
                token_out: addr(3),
                fee_bps: 20,
            }],
        };
        let result =
            simulate_path(&path, &[AMM::MoeLbPair(pair)], U256::from(10_000)).expect("soft skip");
        assert!(
            result.is_none(),
            "Moe IncompleteState must soft-skip the path, not Err"
        );
    }
}
