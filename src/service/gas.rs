//! Screening-time gas config (relocated from `examples/protocols/legacy_service_support.rs`).
//!
//! Used by candidate net-profit screening before the pipeline fee context is
//! minted. Distinct from EIP-1559 `BlockFeeContext` used at permit time.

use crate::arbitrage::gas::{
    net_profit_after_gas_cost, required_gross_for_gas_margin, DEFAULT_GAS_SAFETY_MARGIN,
};
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
        // Delegate to the shared helper (equality → Some(0)).
        net_profit_after_gas_cost(gross_profit, self.calculate_gas_cost(hops))
    }

    /// Match legacy `GasConfig::is_profitable_after_gas`.
    pub fn is_profitable_after_gas(
        &self,
        gross_profit: U256,
        hops: usize,
        safety_margin: f64,
    ) -> bool {
        gross_profit >= required_gross_for_gas_margin(self.calculate_gas_cost(hops), safety_margin)
    }

    /// Default safety margin used by the monitor services.
    pub const fn default_safety_margin() -> f64 {
        DEFAULT_GAS_SAFETY_MARGIN
    }
}

/// Free-function alias matching `legacy_service_support::default_gas_safety_margin`.
///
/// Prefer this (or [`GasConfig::default_safety_margin`]) over a hardcoded `1.2`
/// literal at gross-candidate pre-filter sites (WHI-729).
pub const fn default_gas_safety_margin() -> f64 {
    DEFAULT_GAS_SAFETY_MARGIN
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

    #[test]
    fn net_profit_zero_when_gross_equals_cost() {
        let gas = GasConfig {
            gas_price_wei: 1,
        };
        // hops=1 → gas_limit 300_000_000 * price 1
        let cost = gas.calculate_gas_cost(1);
        assert_eq!(gas.net_profit(cost, 1), Some(U256::ZERO));
        assert_eq!(gas.net_profit(cost - U256::from(1u64), 1), None);
    }

    #[test]
    fn is_profitable_after_gas_applies_default_margin() {
        let gas = GasConfig::default();
        let cost = gas.calculate_gas_cost(2);
        // Below 1.2x cost → not profitable after margin.
        assert!(!gas.is_profitable_after_gas(cost, 2, GasConfig::default_safety_margin()));
        let above = cost * U256::from(2u64);
        assert!(gas.is_profitable_after_gas(above, 2, GasConfig::default_safety_margin()));
    }
}
