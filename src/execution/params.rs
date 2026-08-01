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

/// Require path endpoints already equal the settlement asset (WHI-529).
///
/// Does **not** rewrite endpoints — masking a mismatch would defeat
/// `validate_execute_path` downstream.
pub(crate) fn require_path_settlement_endpoints(
    token_path: &[Address],
    settlement_asset: Address,
) -> Result<()> {
    let Some(first) = token_path.first() else {
        eyre::bail!("empty token path: cannot validate settlement endpoints");
    };
    let Some(last) = token_path.last() else {
        eyre::bail!("empty token path: cannot validate settlement endpoints");
    };
    if *first != settlement_asset || *last != settlement_asset {
        eyre::bail!(
            "path endpoints must equal settlement asset {settlement_asset}; \
             got first={first} last={last} (WHI-529: no silent rewrite)"
        );
    }
    Ok(())
}

impl ParamsBuilder<'_> {
    pub(crate) async fn build<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<ExecutionParams> {
        let token_path: Vec<Address> = opportunity
            .path
            .tokens
            .iter()
            .map(|token| token.get_address())
            .collect();
        require_path_settlement_endpoints(&token_path, self.context.wmnt_address)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::executor::{ArbitrageOpportunity, Pool, SwapPath, Token};
    use crate::execution::{
        load_artifact, BlockFeeContextCache, RuntimeGasProfile, RuntimeProfileConfig,
    };
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn addr(b: u8) -> Address {
        Address::repeat_byte(b)
    }

    #[test]
    fn require_settlement_endpoints_rejects_mismatch() {
        let settlement = addr(0x78);
        let other = addr(0x11);
        let err = require_path_settlement_endpoints(&[other, settlement], settlement)
            .expect_err("mismatched first must fail");
        assert!(
            err.to_string().contains("settlement asset"),
            "unexpected: {err}"
        );
        let err = require_path_settlement_endpoints(&[settlement, other], settlement)
            .expect_err("mismatched last must fail");
        assert!(err.to_string().contains("settlement asset"));
        assert!(require_path_settlement_endpoints(&[settlement, other, settlement], settlement)
            .is_ok());
    }

    #[tokio::test]
    async fn params_builder_build_errors_on_non_settlement_endpoints() {
        // Full ParamsBuilder::build path: validation fails before any pool RPC.
        let settlement = addr(0x78);
        let other = addr(0x11);
        let artifact_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/gas_profiles/mantle_mainnet_v1.json");
        let gas_profile = RuntimeGasProfile::from_artifact(
            load_artifact(&artifact_path).unwrap(),
            RuntimeProfileConfig::mantle_mainnet(Vec::new()),
        )
        .unwrap();
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let context = ExecutionContext {
            provider: provider.clone().erased(),
            executor_contract: addr(0xE0),
            wmnt_address: settlement,
            gas_profile,
            block_fee_contexts: Arc::new(BlockFeeContextCache::default()),
        };
        let config = ExecutorConfig::default();
        let builder = ParamsBuilder {
            context: &context,
            config: &config,
            crossing_buckets: None,
        };
        let opportunity = ArbitrageOpportunity {
            optimal_input_amount: U256::from(1u64),
            gas_cost_mnt_wei: U256::ZERO,
            net_profit_mnt_wei: U256::ZERO,
            path: SwapPath {
                tokens: vec![Token::new(other), Token::new(settlement)],
                pools: vec![Pool::with_type(addr(0xAA), PoolType::UniV2)],
            },
        };
        let err = builder
            .build(&provider, &opportunity)
            .await
            .expect_err("non-settlement first token must not be rewritten");
        assert!(
            err.to_string().contains("settlement asset"),
            "unexpected error: {err}"
        );
    }
}
