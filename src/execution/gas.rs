use alloy::primitives::U256;

#[derive(Clone, Debug)]
pub struct GasPricingInput {
    pub expected_profit: U256,
    pub base_fee_wei: u128,
}

#[derive(Clone, Debug)]
pub struct GasPricingDecision {
    pub gas_price_wei: u128,
}

/// Simple gas strategy for Mantle: max(gas_price_from_node, floor + k * expected_profit)
pub fn simple_competitive_gas_price(input: GasPricingInput) -> GasPricingDecision {
    // Convert expected profit to a rough scalar for tipping. This is heuristic.
    let profit_scalar = if input.expected_profit > U256::ZERO {
        1u128
    } else {
        0u128
    };
    let min_tip = 21_000_000u128; // 0.021 gwei
    let dynamic_tip = min_tip.saturating_add(1_000_000 * profit_scalar);

    let gas_price_wei = input.base_fee_wei.saturating_add(dynamic_tip);
    GasPricingDecision { gas_price_wei }
}
