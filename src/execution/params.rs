use super::contract::IMoePair;
use super::executor::{
    detect_pool_meta, pool_type_byte, protocol_kind_for_pool_type_byte, ArbitrageOpportunity,
};
use super::gas_profile::RouteKey;
use super::types::{
    ExecutionContext, ExecutionParams, ExecutorConfig, PoolType, VerifiedCrossingBuckets,
};
use alloy::primitives::{aliases::U112, Address, U256};
use alloy::providers::Provider;
use eyre::Result;

pub(crate) struct ParamsBuilder<'a> {
    pub(crate) context: &'a ExecutionContext,
    pub(crate) config: &'a ExecutorConfig,
    pub(crate) crossing_buckets: Option<VerifiedCrossingBuckets>,
}

impl ParamsBuilder<'_> {
    pub(crate) async fn build<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<ExecutionParams> {
        let mut token_path: Vec<Address> = opportunity
            .path
            .tokens
            .iter()
            .map(|token| token.get_address())
            .collect();
        if let Some(first) = token_path.first_mut() {
            *first = self.context.wmnt_address;
        }
        if let Some(last) = token_path.last_mut() {
            *last = self.context.wmnt_address;
        }

        let pool_addresses: Vec<Address> = opportunity
            .path
            .pools
            .iter()
            .map(|pool| pool.get_address())
            .collect();
        let mut expected_reserves_u112 = Vec::with_capacity(pool_addresses.len() * 2);
        let mut step_amounts_out = Vec::with_capacity(pool_addresses.len());
        let mut pool_types = Vec::with_capacity(pool_addresses.len());
        let mut pool_tokens = Vec::with_capacity(pool_addresses.len());
        let mut current_amount = opportunity.optimal_input_amount;
        let mut input_known = true;

        for (index, pool_meta) in opportunity.path.pools.iter().enumerate() {
            let pool_address = pool_meta.get_address();
            let (pool_type, token0, token1) =
                detect_pool_meta(provider, pool_address, pool_meta.pool_type).await?;
            pool_types.push(pool_type_byte(pool_type));
            pool_tokens.push((token0, token1));

            let output = match pool_type {
                PoolType::UniV2 => {
                    if !input_known {
                        eyre::bail!(
                            "cannot size V2 hop {index}: input is unknown after a V3/Moe hop"
                        );
                    }
                    let pair = IMoePair::new(pool_address, provider);
                    let reserves = pair.getReserves().call().await?;
                    expected_reserves_u112.push(reserves._0);
                    expected_reserves_u112.push(reserves._1);
                    let (reserve_in, reserve_out) = if token_path[index] == token0 {
                        (U256::from(reserves._0), U256::from(reserves._1))
                    } else {
                        (U256::from(reserves._1), U256::from(reserves._0))
                    };
                    let numerator = current_amount * U256::from(997u64) * reserve_out;
                    let denominator =
                        reserve_in * U256::from(1000u64) + current_amount * U256::from(997u64);
                    if denominator.is_zero() {
                        U256::ZERO
                    } else {
                        numerator / denominator
                    }
                }
                PoolType::UniV3 | PoolType::MoeLB => {
                    expected_reserves_u112.push(U112::ZERO);
                    expected_reserves_u112.push(U112::ZERO);
                    input_known = false;
                    U256::ZERO
                }
            };
            step_amounts_out.push(output);
            if !output.is_zero() {
                current_amount = output;
            }
        }

        let expected_profit = match current_amount.checked_sub(opportunity.optimal_input_amount) {
            Some(value) => value,
            None => U256::ZERO,
        };
        let slippage_allowance = mul_fraction(expected_profit, self.config.slippage_tolerance);
        let mut min_amount_out = current_amount.saturating_sub(slippage_allowance);
        if (self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss)
            && min_amount_out < opportunity.optimal_input_amount
        {
            min_amount_out = opportunity.optimal_input_amount;
        }
        let mut route_key = RouteKey::new(
            pool_types
                .iter()
                .copied()
                .map(protocol_kind_for_pool_type_byte)
                .collect::<Result<Vec<_>>>()?,
        )?;
        let has_v3 = route_key
            .protocols.contains(&super::gas_profile::ProtocolKind::V3);
        let has_moe = route_key
            .protocols.contains(&super::gas_profile::ProtocolKind::Moe);
        if (has_v3 || has_moe) && self.crossing_buckets.is_none() {
            eyre::bail!(
                "V3/Moe execution requires verified crossing-bucket evidence during parameter building"
            );
        }
        if let Some(buckets) = &self.crossing_buckets {
            if has_v3 {
                route_key.v3_tick_crossings = buckets.v3_tick_crossings;
            }
            if has_moe {
                route_key.moe_bin_crossings = buckets.moe_bin_crossings;
            }
            route_key.validate_structure()?;
        }

        Ok(ExecutionParams {
            amount_in: opportunity.optimal_input_amount,
            route_key,
            crossing_buckets_verified: self.crossing_buckets.is_some(),
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
}

fn mul_fraction(value: U256, fraction: f64) -> U256 {
    if fraction <= 0.0 {
        return U256::ZERO;
    }
    let scale = 1_000_000u128;
    let fraction_scaled = ((fraction * scale as f64) as u128).min(scale);
    value * U256::from(fraction_scaled) / U256::from(scale)
}
