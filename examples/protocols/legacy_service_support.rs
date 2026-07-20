use alloy::primitives::U256;
use amms::arbitrage::gas::{
    net_profit_after_gas_cost, required_gross_for_gas_margin, DEFAULT_GAS_SAFETY_MARGIN,
};

pub use amms::execution::plan_resized_execution_default_margin;

#[derive(Debug, Clone, Copy)]
pub struct GasConfig {
    pub gas_price_wei: u128,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            gas_price_wei: 25_000_000,
        }
    }
}

impl GasConfig {
    pub fn calculate_gas_cost(&self, hops: usize) -> U256 {
        U256::from(gas_limit_for_hops(hops)) * U256::from(self.gas_price_wei)
    }

    pub fn net_profit(&self, gross_profit: U256, hops: usize) -> Option<U256> {
        net_profit_after_gas_cost(gross_profit, self.calculate_gas_cost(hops))
    }

    pub fn is_profitable_after_gas(
        &self,
        gross_profit: U256,
        hops: usize,
        safety_margin: f64,
    ) -> bool {
        gross_profit >= required_gross_for_gas_margin(self.calculate_gas_cost(hops), safety_margin)
    }
}

pub const fn gas_limit_for_hops(hops: usize) -> u64 {
    match hops {
        0 | 1 => 300_000_000,
        2 => 900_000_000,
        3 => 1_500_000_000,
        4 => 2_800_000_000,
        _ => 2_800_000_000,
    }
}

pub fn max_fee_per_gas_with_headroom(
    base_fee_per_gas: u64,
    priority_fee_per_gas: u128,
) -> Option<u128> {
    u128::from(base_fee_per_gas)
        .checked_mul(2)
        .and_then(|base_fee| base_fee.checked_add(priority_fee_per_gas))
}

pub const fn default_gas_safety_margin() -> f64 {
    DEFAULT_GAS_SAFETY_MARGIN
}

#[cfg(test)]
mod tests {
    use super::max_fee_per_gas_with_headroom;

    #[test]
    fn includes_base_fee_headroom_before_priority_fee() {
        assert_eq!(max_fee_per_gas_with_headroom(100, 3), Some(203));
    }

    #[test]
    fn rejects_base_fee_headroom_overflow() {
        assert_eq!(max_fee_per_gas_with_headroom(u64::MAX, u128::MAX), None);
    }
}
