// Note: This executor is designed for ArbitrageOpportunity from logic module
// For direct swap execution, use SwapExecutor instead

// use crate::logic::types::ArbitrageOpportunity;
use alloy::primitives::{aliases::U112, Address, TxHash, U256};
use alloy::providers::Provider;
use eyre::Result;

use super::contract::{IAgniPool, IArbitrageExecutor, IMoeLBPair, IMoePair};
// Removed competitive gas pricing helper imports; using simplified Mantle logic
use super::types::{ExecutionContext, ExecutionParams, ExecutorConfig, FeeMode, PoolType};

// Placeholder for ArbitrageOpportunity when logic module is not available
#[derive(Clone, Debug)]
pub struct ArbitrageOpportunity {
    pub optimal_input_amount: U256,
    pub gas_cost_mnt_wei: U256,
    pub net_profit_mnt_wei: U256,
    pub path: SwapPath,
}

#[derive(Clone, Debug)]
pub struct SwapPath {
    pub tokens: Vec<Token>,
    pub pools: Vec<Pool>,
}

#[derive(Clone, Debug)]
pub struct Token {
    address: Address,
}

impl Token {
    pub fn get_address(&self) -> Address {
        self.address
    }
}

#[derive(Clone, Debug)]
pub struct Pool {
    address: Address,
    /// When set, used directly; otherwise `detect_pool_meta` probes on-chain.
    pub pool_type: Option<PoolType>,
}

impl Pool {
    pub fn get_address(&self) -> Address {
        self.address
    }

    pub fn with_type(address: Address, pool_type: PoolType) -> Self {
        Self {
            address,
            pool_type: Some(pool_type),
        }
    }
}

/// On-chain poolType byte matching `ArbitrageExecutor` constants.
pub fn pool_type_byte(pool_type: PoolType) -> u8 {
    match pool_type {
        PoolType::UniV2 => 0,
        PoolType::UniV3 => 1,
        PoolType::MoeLB => 2,
    }
}

#[cfg(test)]
mod pool_type_tests {
    use super::*;

    #[test]
    fn pool_type_bytes_match_solidity_constants() {
        assert_eq!(pool_type_byte(PoolType::UniV2), 0);
        assert_eq!(pool_type_byte(PoolType::UniV3), 1);
        assert_eq!(pool_type_byte(PoolType::MoeLB), 2);
    }
}

/// Resolve venue + token ends for one pool.
/// Prefer explicit `hint`; otherwise probe Moe → V3 → V2 (fail closed if none work).
pub async fn detect_pool_meta<P: Provider>(
    provider: &P,
    pool: Address,
    hint: Option<PoolType>,
) -> Result<(PoolType, Address, Address)> {
    if let Some(pt) = hint {
        return match pt {
            PoolType::UniV2 => {
                let pair = IMoePair::new(pool, provider);
                Ok((pt, pair.token0().call().await?, pair.token1().call().await?))
            }
            PoolType::UniV3 => {
                let p = IAgniPool::new(pool, provider);
                Ok((pt, p.token0().call().await?, p.token1().call().await?))
            }
            PoolType::MoeLB => {
                let p = IMoeLBPair::new(pool, provider);
                Ok((pt, p.getTokenX().call().await?, p.getTokenY().call().await?))
            }
        };
    }

    // Moe LB first: getTokenX is unique to LB pairs.
    let moe = IMoeLBPair::new(pool, provider);
    if let (Ok(x), Ok(y)) = (moe.getTokenX().call().await, moe.getTokenY().call().await) {
        if x != Address::ZERO && y != Address::ZERO && x != y {
            return Ok((PoolType::MoeLB, x, y));
        }
    }

    // V3: liquidity() + fee() present.
    let v3 = IAgniPool::new(pool, provider);
    if v3.liquidity().call().await.is_ok() {
        let t0 = v3.token0().call().await?;
        let t1 = v3.token1().call().await?;
        return Ok((PoolType::UniV3, t0, t1));
    }

    // V2 fallback.
    let v2 = IMoePair::new(pool, provider);
    let t0 = v2.token0().call().await?;
    let t1 = v2.token1().call().await?;
    Ok((PoolType::UniV2, t0, t1))
}

#[derive(Clone, Copy, Debug)]
pub struct FeePlan {
    pub gas_limit: u64,
    pub base_fee_wei: u128,
    pub default_priority_fee_wei: u128,
    pub total_cap_from_profit: u128,
    pub cap_priority_fee_from_profit: u128,
    pub initial_priority_fee: u128,
    pub max_priority_fee_per_gas_wei: u128,
    pub max_fee_per_gas_wei: u128,
    pub effective_global_cap_wei: u128,
    pub is_small_profit: bool,
}

pub const MANTLE_BASE_FEE_WEI: u128 = 20_000_000u128;

pub fn compute_fee_plan(config: &ExecutorConfig, hops: usize, net_expected: U256) -> FeePlan {
    let gas_limit = match hops {
        4 => 750_000_000,
        2 => 450_000_000,
        _ => config.gas_limit,
    };

    let priority_fee_wei: u128 = config.default_priority_fee_wei;
    let base_fee_wei: u128 = MANTLE_BASE_FEE_WEI;

    let net_expected_u128: u128 = net_expected.to_string().parse::<u128>().unwrap_or(0);

    let one_wmnt = U256::from(1_000_000_000_000_000_000u128);
    let five_wmnt = U256::from(5_000_000_000_000_000_000u128);

    let effective_global_cap_wei: u128 = if net_expected < one_wmnt {
        500_000_000u128
    } else if net_expected < five_wmnt {
        3_000_000_000u128
    } else {
        u128::MAX
    };

    let is_very_small_profit = net_expected < U256::from(50_000_000_000_000_000u128);
    let is_small_profit = net_expected < U256::from(100_000_000_000_000_000u128);

    let mut total_cap_from_profit = if gas_limit > 0 {
        net_expected_u128.saturating_div(gas_limit as u128)
    } else {
        0
    };

    if total_cap_from_profit > 0 {
        if is_very_small_profit {
            total_cap_from_profit = total_cap_from_profit.saturating_mul(3).saturating_div(2);
        } else if is_small_profit {
            total_cap_from_profit = total_cap_from_profit.saturating_mul(3).saturating_div(2);
        }
    }

    let cap_priority_fee_from_profit = total_cap_from_profit.saturating_sub(base_fee_wei);

    let initial_priority_fee = priority_fee_wei.max(cap_priority_fee_from_profit);

    let mut max_fee_per_gas_wei: u128 = base_fee_wei.saturating_add(initial_priority_fee);
    if max_fee_per_gas_wei > effective_global_cap_wei {
        max_fee_per_gas_wei = effective_global_cap_wei;
    }

    let max_priority_fee_per_gas_wei =
        initial_priority_fee.min(max_fee_per_gas_wei.saturating_sub(base_fee_wei));

    FeePlan {
        gas_limit,
        base_fee_wei,
        default_priority_fee_wei: priority_fee_wei,
        total_cap_from_profit,
        cap_priority_fee_from_profit,
        initial_priority_fee,
        max_priority_fee_per_gas_wei,
        max_fee_per_gas_wei,
        effective_global_cap_wei,
        is_small_profit,
    }
}

pub struct Executor {
    pub config: ExecutorConfig,
    pub context: ExecutionContext,
}

impl Executor {
    pub fn new(context: ExecutionContext, config: ExecutorConfig) -> Self {
        Self { config, context }
    }

    /// Convert an ArbitrageOpportunity into concrete execution params
    pub async fn build_params<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<ExecutionParams> {
        // Build token and pool address arrays from the SwapPath
        let mut token_path: Vec<Address> = opportunity
            .path
            .tokens
            .iter()
            .map(|t| t.get_address())
            .collect();
        let len = token_path.len();
        if len >= 1 {
            if token_path[0] != self.context.wmnt_address {
                token_path[0] = self.context.wmnt_address;
            }
            if token_path[len - 1] != self.context.wmnt_address {
                token_path[len - 1] = self.context.wmnt_address;
            }
        }

        let pool_addresses: Vec<Address> = opportunity
            .path
            .pools
            .iter()
            .map(|p| p.get_address())
            .collect();

        // Per-pool venue + token ends (hint from path, else on-chain detect).
        let mut expected_reserves_u112: Vec<U112> = Vec::with_capacity(pool_addresses.len() * 2);
        let mut step_amounts_out: Vec<U256> = Vec::with_capacity(pool_addresses.len());
        let mut pool_types: Vec<u8> = Vec::with_capacity(pool_addresses.len());
        let mut pool_tokens: Vec<(Address, Address)> = Vec::with_capacity(pool_addresses.len());

        let mut current_amount = opportunity.optimal_input_amount;
        // Tracks whether `current_amount` still reflects this trade's true input to the next
        // hop. V3/Moe outputs are not computed off-chain here (the contract sizes those hops
        // from on-chain balance deltas), so once a V3/Moe hop runs, the input to any later hop
        // is unknown at build time.
        let mut input_known = true;
        for (i, pool_meta) in opportunity.path.pools.iter().enumerate() {
            let pool_addr = pool_meta.get_address();
            let (ptype, token0, token1) =
                detect_pool_meta(provider, pool_addr, pool_meta.pool_type).await?;
            let ptype_byte = pool_type_byte(ptype);
            pool_types.push(ptype_byte);
            pool_tokens.push((token0, token1));

            let token_in = token_path[i];
            let out = match ptype {
                PoolType::UniV2 => {
                    // `_swapV2` uses amountsOut[i] as the pair's EXACT output, so it must be
                    // sized from this hop's real input. If a preceding V3/Moe hop left the input
                    // unknown, fail closed rather than emit reverting/lossy calldata.
                    if !input_known {
                        eyre::bail!(
                            "cannot size V2 hop {i}: input is unknown after a V3/Moe hop; \
                             mixed-venue routes with a V2 hop downstream of a V3/Moe hop are \
                             not supported by build_params"
                        );
                    }
                    let pair = IMoePair::new(pool_addr, provider);
                    let reserves = pair.getReserves().call().await?;
                    expected_reserves_u112.push(reserves._0);
                    expected_reserves_u112.push(reserves._1);
                    let (reserve_in, reserve_out) = if token_in == token0 {
                        (U256::from(reserves._0), U256::from(reserves._1))
                    } else {
                        (U256::from(reserves._1), U256::from(reserves._0))
                    };
                    // Uniswap V2: out = (in*997*Rout)/(Rin*1000 + in*997)
                    let numerator = current_amount * U256::from(997u64) * reserve_out;
                    let denominator =
                        reserve_in * U256::from(1000u64) + current_amount * U256::from(997u64);
                    if denominator.is_zero() {
                        U256::ZERO
                    } else {
                        numerator / denominator
                    }
                }
                // V3/Moe: contract sizes hops from balance deltas; amountsOut only required for V2.
                // Their output is not computable off-chain here, so downstream input is unknown.
                PoolType::UniV3 | PoolType::MoeLB => {
                    expected_reserves_u112.push(U112::ZERO);
                    expected_reserves_u112.push(U112::ZERO);
                    input_known = false;
                    U256::ZERO
                }
            };
            step_amounts_out.push(out);
            if !out.is_zero() {
                current_amount = out;
            }
        }

        // Compute minAmountOut using slippage policy on expected profit
        let expected_profit = if current_amount > opportunity.optimal_input_amount {
            current_amount - opportunity.optimal_input_amount
        } else {
            U256::ZERO
        };
        let slippage_allowance = mul_fraction(expected_profit, self.config.slippage_tolerance);
        let mut min_amount_out = current_amount.saturating_sub(slippage_allowance);

        // Enforce non-loss and include gas cost into min_out if configured
        if self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss {
            let gas_cost = opportunity.gas_cost_mnt_wei;
            let required_out = opportunity.optimal_input_amount.saturating_add(gas_cost);
            if min_amount_out < required_out {
                min_amount_out = required_out;
            }
        }

        Ok(ExecutionParams {
            amount_in: opportunity.optimal_input_amount,
            token_path,
            pool_addresses,
            pool_types,
            pool_tokens,
            expected_reserves_u112,
            step_amounts_out,
            min_amount_out,
            expected_net_profit_mnt_wei: opportunity.net_profit_mnt_wei,
        })
    }

    /// Execute the arbitrage via contract call with pre-flight checks
    pub async fn execute<P: Provider>(
        &self,
        provider: &P,
        params: &ExecutionParams,
    ) -> Result<TxHash> {
        // Determine hop count for fee planning
        let hops = params.token_path.len().saturating_sub(1);
        // Use expected net profit from opportunity for pricing; fallback to min_out - in
        let net_expected = if !params.expected_net_profit_mnt_wei.is_zero() {
            params.expected_net_profit_mnt_wei
        } else {
            params.min_amount_out.saturating_sub(params.amount_in)
        };
        // If enforcing non-loss, do not allow negative net expected
        if self.config.enforce_non_loss && net_expected.is_zero() {
            eyre::bail!("Abort execution: non-loss requirement not satisfied");
        }
        let fee_plan = compute_fee_plan(&self.config, hops, net_expected);
        let gas_limit_to_use = fee_plan.gas_limit;
        let max_fee_per_gas_wei = fee_plan.max_fee_per_gas_wei;
        let max_priority_fee_per_gas_wei = fee_plan.max_priority_fee_per_gas_wei;
        if max_priority_fee_per_gas_wei > max_fee_per_gas_wei {
            eyre::bail!(
                "Invalid EIP-1559 fees: max_priority_fee_per_gas ({}) > max_fee_per_gas ({})",
                max_priority_fee_per_gas_wei,
                max_fee_per_gas_wei
            );
        }

        // Runtime trace for chosen gas caps
        tracing::info!(
            base_fee_wei = fee_plan.base_fee_wei,
            default_priority_fee_wei = fee_plan.default_priority_fee_wei,
            total_cap_from_profit = fee_plan.total_cap_from_profit,
            cap_priority_fee_from_profit = fee_plan.cap_priority_fee_from_profit,
            initial_priority_fee = fee_plan.initial_priority_fee,
            max_priority_fee_per_gas_wei = max_priority_fee_per_gas_wei,
            dynamic_cap_wei = fee_plan.effective_global_cap_wei,
            max_fee_per_gas_wei = max_fee_per_gas_wei,
            priority_fee_adjusted = fee_plan.initial_priority_fee != max_priority_fee_per_gas_wei,
            "Gas caps computed"
        );

        // Venue-native per-hop outs (V2 amountOut args). Final principal uses minProfit.
        let mut outs = params.step_amounts_out.clone();
        let gas_cost_at_cap: u128 =
            (gas_limit_to_use as u128).saturating_mul(max_fee_per_gas_wei);
        let required_out = if (self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss)
            && !fee_plan.is_small_profit
        {
            params
                .amount_in
                .saturating_add(U256::from(gas_cost_at_cap))
        } else {
            params.min_amount_out
        };
        let computed_last = *outs.last().unwrap_or(&U256::ZERO);
        if computed_last < required_out {
            eyre::bail!(
                "Skip execution: expected last-hop out {} < required {}",
                computed_last,
                required_out
            );
        }
        let haircut = U256::from(1u64);
        let mut target_last = computed_last.saturating_sub(haircut);
        if target_last < required_out {
            target_last = required_out;
        }
        if let Some(last) = outs.last_mut() {
            *last = target_last;
        }

        if let Err(e) = super::contract::validate_execute_path(
            self.context.wmnt_address,
            &params.token_path,
            &params.pool_addresses,
            &params.pool_types,
            &params.pool_tokens,
            Some(&outs),
        ) {
            eyre::bail!("Invalid execute path: {e}");
        }

        // minProfit is net WMNT balance increase: required_out - amount_in (floored at 0).
        let min_profit = required_out.saturating_sub(params.amount_in);
        // Inclusive deadline; far-future until M2 wires block.timestamp-aware deadlines.
        let deadline = U256::from(u64::MAX);

        let contract = IArbitrageExecutor::new(self.context.executor_contract, provider);
        let call = contract
            .executeArbitrage(
                params.amount_in,
                params.token_path.clone(),
                params.pool_addresses.clone(),
                params.pool_types.clone(),
                outs,
                min_profit,
                deadline,
            )
            .gas(gas_limit_to_use);

        // Apply fees based on configured mode
        let pending = match self.config.fee_mode {
            FeeMode::Legacy => {
                // Legacy: single gas_price, clamp to global cap and at least base+tip
                let gas_price_wei = max_fee_per_gas_wei;
                call.gas_price(gas_price_wei).send().await?
            }
            FeeMode::Eip1559 => {
                call.max_fee_per_gas(max_fee_per_gas_wei)
                    .max_priority_fee_per_gas(max_priority_fee_per_gas_wei)
                    .send()
                    .await?
            }
        };

        Ok(*pending.tx_hash())
    }

    /// Convenience method: build params then execute
    pub async fn execute_opportunity<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<TxHash> {
        // Gate by configured minimum net profit after gas
        if opportunity.net_profit_mnt_wei < self.config.min_net_profit_mnt_wei {
            eyre::bail!(
                "Skip execution: net profit {} < min required {}",
                opportunity.net_profit_mnt_wei,
                self.config.min_net_profit_mnt_wei
            );
        }
        let params = self.build_params(provider, opportunity).await?;
        self.execute(provider, &params).await
    }
}

fn mul_fraction(value: U256, fraction: f64) -> U256 {
    if fraction <= 0.0 {
        return U256::ZERO;
    }
    // Convert f64 to fixed point 1e6 to avoid precision issues
    let scale = 1_000_000u128;
    let frac_scaled = ((fraction * scale as f64) as u128).min(scale);
    let value_u256 = value;
    let num = value_u256 * U256::from(frac_scaled);
    (num / U256::from(scale)).into()
}
