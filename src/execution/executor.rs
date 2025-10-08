// Note: This executor is designed for ArbitrageOpportunity from logic module
// For direct swap execution, use SwapExecutor instead

// use crate::logic::types::ArbitrageOpportunity;
use alloy::primitives::{aliases::U112, Address, TxHash, U256};
use alloy::providers::Provider;
use eyre::Result;

use super::contract::{IArbitrageExecutor, IMoePair};
// Removed competitive gas pricing helper imports; using simplified Mantle logic
use super::types::{ExecutionContext, ExecutionParams, ExecutorConfig, FeeMode};

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
}

impl Pool {
    pub fn get_address(&self) -> Address {
        self.address
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

        // Collect expected reserves and compute step outputs using on-chain reserves
        let mut expected_reserves_u112: Vec<U112> = Vec::with_capacity(pool_addresses.len() * 2);
        let mut step_amounts_out: Vec<U256> = Vec::with_capacity(pool_addresses.len());

        let mut current_amount = opportunity.optimal_input_amount;
        for (i, pool_addr) in pool_addresses.iter().copied().enumerate() {
            let pair = IMoePair::new(pool_addr, provider);
            let reserves = pair.getReserves().call().await?;
            expected_reserves_u112.push(reserves._0);
            expected_reserves_u112.push(reserves._1);

            // Determine in/out reserves by token ordering
            let token_in = token_path[i];
            let _token_out = token_path[i + 1];
            let token0: Address = pair.token0().call().await?;
            let (reserve_in, reserve_out) = if token_in == token0 {
                (U256::from(reserves._0), U256::from(reserves._1))
            } else {
                (U256::from(reserves._1), U256::from(reserves._0))
            };

            // Uniswap V2 formula: out = (in*997*Rout)/(Rin*1000 + in*997)
            let numerator = current_amount * U256::from(997u64) * reserve_out;
            let denominator =
                reserve_in * U256::from(1000u64) + current_amount * U256::from(997u64);
            let out = if denominator.is_zero() {
                U256::ZERO
            } else {
                numerator / denominator
            };
            step_amounts_out.push(out);
            current_amount = out;
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
        // EIP-1559 fees on Mantle: fixed base fee and fixed tip, dynamic max fee cap from profit
        // Fixed tip = 0.0001 gwei by default (see ExecutorConfig), base fee fixed at 0.02 gwei
        let priority_fee_wei: u128 = self.config.default_priority_fee_wei; // 0.0001 gwei
        let base_fee_wei: u128 = 20_000_000u128; // 0.02 gwei in Mantle wei units
                                                 // Determine gas limit to use for this hop count (needed for cap computation)
        let hops = params.token_path.len().saturating_sub(1);
        let gas_limit_to_use = if hops == 4 {
            750_000_000
        } else if hops == 2 {
            450_000_000
        } else {
            self.config.gas_limit
        };
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
        // Compute dynamic max_priority_fee_per_gas based on profit constraint
        // Use estimated gas = gas_limit_to_use for a conservative cap; spend up to 70% of expected profit
        // net_expected is U256; convert to u128 saturating for per-gas computation
        let net_expected_u128: u128 = net_expected.to_string().parse::<u128>().unwrap_or(0);
        // Dynamic hard cap policy based on expected profit:
        //   < 1 WMNT  -> 0.5 gwei
        //   1-5 WMNT  -> 2.7 gwei
        //   >= 5 WMNT -> no hard cap (u128::MAX)
        let one_wmnt = U256::from(1_000_000_000_000_000_000u128);
        let five_wmnt = U256::from(5_000_000_000_000_000_000u128);
        let effective_global_cap_wei: u128 = if net_expected < one_wmnt {
            500_000_000u128 // 0.5 gwei
        } else if net_expected < five_wmnt {
            3_000_000_000u128 // 2.7 gwei
        } else {
            u128::MAX // unlimited
        };
        // derive total cap price per gas = profit / gas
        // If expected profit is small, amplify cap:
        //   - < 0.05 WMNT: x2
        //   - < 0.1 WMNT: x1.5
        let is_very_small_profit = net_expected < U256::from(50_000_000_000_000_000u128); // 0.05 WMNT in wei
        let is_small_profit = net_expected < U256::from(100_000_000_000_000_000u128); // 0.1 WMNT in wei
        let mut total_cap_from_profit = if gas_limit_to_use > 0 {
            // multiply first then divide to avoid precision loss
            net_expected_u128.saturating_div(gas_limit_to_use as u128)
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
        // priority fee cap = total_cap - base_fee (cannot be negative)
        let cap_priority_fee_from_profit = total_cap_from_profit.saturating_sub(base_fee_wei);
        // initial priority fee: max of default and profit-derived cap
        let initial_priority_fee = priority_fee_wei.max(cap_priority_fee_from_profit);
        // max_fee_per_gas = base_fee + max_priority_fee_per_gas, clamp to dynamic cap
        let mut max_fee_per_gas_wei: u128 = base_fee_wei.saturating_add(initial_priority_fee);
        if max_fee_per_gas_wei > effective_global_cap_wei {
            max_fee_per_gas_wei = effective_global_cap_wei;
        }
        // Ensure max_priority_fee_per_gas <= max_fee_per_gas by adjusting if needed
        let max_priority_fee_per_gas_wei =
            initial_priority_fee.min(max_fee_per_gas_wei.saturating_sub(base_fee_wei));
        // Validate EIP-1559 constraints
        if max_priority_fee_per_gas_wei > max_fee_per_gas_wei {
            eyre::bail!(
                "Invalid EIP-1559 fees: max_priority_fee_per_gas ({}) > max_fee_per_gas ({})",
                max_priority_fee_per_gas_wei,
                max_fee_per_gas_wei
            );
        }

        // Runtime trace for chosen gas caps
        tracing::info!(
            base_fee_wei = base_fee_wei,
            default_priority_fee_wei = priority_fee_wei,
            total_cap_from_profit = total_cap_from_profit,
            cap_priority_fee_from_profit = cap_priority_fee_from_profit,
            initial_priority_fee = initial_priority_fee,
            max_priority_fee_per_gas_wei = max_priority_fee_per_gas_wei,
            dynamic_cap_wei = effective_global_cap_wei,
            max_fee_per_gas_wei = max_fee_per_gas_wei,
            priority_fee_adjusted = initial_priority_fee != max_priority_fee_per_gas_wei,
            "Gas caps computed"
        );

        let contract = IArbitrageExecutor::new(self.context.executor_contract, provider);
        let mut call = contract
            .executeArbitrage(
                params.amount_in,
                params.token_path.clone(),
                params.pool_addresses.clone(),
                vec![1u8; params.pool_addresses.len()],
                params
                    .expected_reserves_u112
                    .iter()
                    .map(|v| U256::from(*v))
                    .collect::<Vec<U256>>(),
                {
                    // Never inflate the last hop amountOut beyond what AMM math allows.
                    // Gate the trade if we cannot cover required min-out; otherwise optionally apply a tiny haircut
                    // to avoid rounding issues while keeping last-hop amountOut <= computed maximum.
                    let mut outs = params.step_amounts_out.clone();

                    // Compute the effective required final out (amount_in + gas at cap) if configured
                    let gas_cost_at_cap: u128 = (gas_limit_to_use as u128).saturating_mul(max_fee_per_gas_wei);
                    let required_out = if (self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss)
                        && !is_small_profit
                    {
                        params.amount_in.saturating_add(U256::from(gas_cost_at_cap))
                    } else {
                        params.min_amount_out
                    };

                    // Current computed last-hop out from AMM math
                    let computed_last = *outs.last().unwrap_or(&U256::ZERO);

                    // If we cannot meet the required minimum without inflating, abort before sending
                    if computed_last < required_out {
                        eyre::bail!(
                            "Skip execution: expected last-hop out {} < required {} (would violate invariant)",
                            computed_last,
                            required_out
                        );
                    }

                    // Apply a tiny haircut to stay strictly within invariant bounds and avoid rounding edge cases
                    let haircut = U256::from(1u64);
                    let mut target_last = computed_last.saturating_sub(haircut);
                    if target_last < required_out {
                        target_last = required_out;
                    }

                    if let Some(last) = outs.last_mut() {
                        *last = target_last;
                    }

                    outs
                },
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
