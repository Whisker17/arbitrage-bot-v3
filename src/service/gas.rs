//! Screening-time gas config (relocated from `examples/protocols/legacy_service_support.rs`).
//!
//! Used by candidate net-profit screening before the pipeline fee context is
//! minted. Distinct from EIP-1559 `BlockFeeContext` used at permit time.

use alloy::primitives::U256;

/// Default gas-limit schedule shared with the three monitor services.
pub const fn gas_limit_for_hops(hops: usize) -> u64 {
    match hops {
        0 | 1 => 300_000_000,
        2 => 900_000_000,
        3 => 1_500_000_000,
        4 => 2_800_000_000,
        _ => 2_800_000_000,
    }
}

/// Fixed-price gas model used for pre-pipeline profit screening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        let cost = self.calculate_gas_cost(hops);
        if gross_profit > cost {
            Some(gross_profit - cost)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_gas_price_matches_legacy_service_support() {
        assert_eq!(GasConfig::default().gas_price_wei, 25_000_000);
    }

    #[test]
    fn gas_limit_schedule_matches_legacy() {
        assert_eq!(gas_limit_for_hops(1), 300_000_000);
        assert_eq!(gas_limit_for_hops(2), 900_000_000);
        assert_eq!(gas_limit_for_hops(3), 1_500_000_000);
        assert_eq!(gas_limit_for_hops(4), 2_800_000_000);
    }
}
