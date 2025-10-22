//! Gas cost calculations for arbitrage operations
//!
//! This module provides utilities to estimate gas costs for arbitrage paths
//! and filter out unprofitable opportunities after accounting for gas fees.

use alloy::primitives::U256;

/// Gas cost configuration for arbitrage operations
#[derive(Debug, Clone, Copy)]
pub struct GasConfig {
    /// Gas price in wei (e.g., 0.025 Gwei = 25_000_000 wei)
    pub gas_price_wei: u128,
    /// Base gas cost per swap operation
    pub gas_per_hop: u64,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            // 0.025 Gwei = 25,000,000 wei (Mantle 的 gas price)
            gas_price_wei: 25_000_000,
            // Conservative estimate per hop (unused in favor of fixed gas limits)
            gas_per_hop: 150_000,
        }
    }
}

impl GasConfig {
    /// Create a new gas configuration with custom values
    pub fn new(gas_price_gwei: f64, gas_per_hop: u64) -> Self {
        Self {
            gas_price_wei: (gas_price_gwei * 1_000_000_000.0) as u128,
            gas_per_hop,
        }
    }

    /// Calculate total gas cost in wei for a given number of hops
    ///
    /// # Arguments
    /// * `num_hops` - Number of swaps in the arbitrage path (2-4)
    ///
    /// # Returns
    /// Total gas cost in wei (MNT on Mantle network)
    ///
    /// # Formula (Mantle Network with MOE hooks overhead)
    /// 基于实际链上执行数据调整：
    /// - 1 hop:  300M gas (300,000,000) 
    /// - 2 hops: 900M gas (900,000,000) - 考虑 hooks 和复杂交互
    /// - 3 hops: 1.5B gas (1,500,000,000) - 实际观察到 ~500K gas per hop with hooks
    /// - 4 hops: 2.8B gas (2,800,000,000)
    /// Gas cost = gas_limit * gas_price_wei
    ///
    /// 注意：MOE 池子的 beforeSwap hooks 会触发额外的 deposit/mint 操作，
    /// 显著增加 gas 消耗。实际测试显示 3-hop 路径消耗约 505,266 gas。
    pub fn calculate_gas_cost(&self, num_hops: usize) -> U256 {
        let gas_limit = match num_hops {
            1 => 300_000_000u64,
            2 => 900_000_000u64,
            3 => 1_500_000_000u64,
            4 => 2_800_000_000u64,
            // For other cases, use a linear approximation
            n if n > 4 => 2_800_000_000u64 + (n as u64 - 4) * 500_000_000u64,
            _ => 0u64, // 0 hops = no gas
        };

        U256::from(gas_limit) * U256::from(self.gas_price_wei)
    }

    /// Check if profit covers gas costs with a safety margin
    ///
    /// # Arguments
    /// * `profit_wei` - Expected profit in wei
    /// * `num_hops` - Number of swaps in the path
    /// * `safety_margin` - Minimum profit margin above gas costs (default 1.2 = 20% margin)
    ///
    /// # Returns
    /// `true` if profit is sufficient after accounting for gas
    pub fn is_profitable_after_gas(
        &self,
        profit_wei: U256,
        num_hops: usize,
        safety_margin: f64,
    ) -> bool {
        let gas_cost = self.calculate_gas_cost(num_hops);
        let min_required = if safety_margin > 1.0 {
            let margin_u128 = (safety_margin * 1000.0) as u128;
            gas_cost * U256::from(margin_u128) / U256::from(1000u64)
        } else {
            gas_cost
        };

        profit_wei >= min_required
    }

    /// Calculate net profit after deducting gas costs
    ///
    /// # Arguments
    /// * `gross_profit_wei` - Expected profit before gas costs
    /// * `num_hops` - Number of swaps in the path
    ///
    /// # Returns
    /// Net profit in wei (can be negative if gas exceeds profit)
    pub fn net_profit(&self, gross_profit_wei: U256, num_hops: usize) -> Option<U256> {
        let gas_cost = self.calculate_gas_cost(num_hops);
        gross_profit_wei.checked_sub(gas_cost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gas_cost_calculation() {
        let config = GasConfig::default(); // 0.025 Gwei = 25,000,000 wei

        // 1 hop: 300M gas * 25,000,000 wei/gas = 7,500,000,000,000,000 wei (0.0075 MNT)
        let cost_1_hop = config.calculate_gas_cost(1);
        assert_eq!(cost_1_hop, U256::from(7_500_000_000_000_000u64));

        // 2 hops: 900M gas * 25,000,000 wei/gas = 22,500,000,000,000,000 wei (0.0225 MNT)
        let cost_2_hops = config.calculate_gas_cost(2);
        assert_eq!(cost_2_hops, U256::from(22_500_000_000_000_000u64));

        // 3 hops: 1.5B gas * 25,000,000 wei/gas = 37,500,000,000,000,000 wei (0.0375 MNT)
        let cost_3_hops = config.calculate_gas_cost(3);
        assert_eq!(cost_3_hops, U256::from(37_500_000_000_000_000u64));

        // 4 hops: 2.8B gas * 25,000,000 wei/gas = 70,000,000,000,000,000 wei (0.07 MNT)
        let cost_4_hops = config.calculate_gas_cost(4);
        assert_eq!(cost_4_hops, U256::from(70_000_000_000_000_000u64));
    }

    #[test]
    fn test_is_profitable_after_gas() {
        let config = GasConfig::default();

        // 3 hops costs 37,500,000,000,000,000 wei (0.0375 MNT)
        // With 1.2x safety margin, need 45,000,000,000,000,000 wei (0.045 MNT)
        // Profit of 50,000,000,000,000,000 wei (0.05 MNT) should be profitable
        let profit = U256::from(50_000_000_000_000_000u64);
        assert!(config.is_profitable_after_gas(profit, 3, 1.2));

        // Profit of 40,000,000,000,000,000 wei (0.04 MNT) should NOT be profitable with 1.2x margin
        let small_profit = U256::from(40_000_000_000_000_000u64);
        assert!(!config.is_profitable_after_gas(small_profit, 3, 1.2));
    }

    #[test]
    fn test_net_profit() {
        let config = GasConfig::default();

        // 3 hops costs 37,500,000,000,000,000 wei (0.0375 MNT)
        let profit = U256::from(50_000_000_000_000_000u64);
        let net = config.net_profit(profit, 3).unwrap();
        assert_eq!(net, U256::from(12_500_000_000_000_000u64)); // 0.05 - 0.0375 = 0.0125 MNT

        // Insufficient profit
        let small_profit = U256::from(30_000_000_000_000_000u64);
        assert!(config.net_profit(small_profit, 3).is_none());
    }

    #[test]
    fn test_custom_gas_config() {
        // 0.05 Gwei gas price = 50,000,000 wei
        let config = GasConfig::new(0.05, 150_000);

        // 3 hops: 1.5B gas * 50,000,000 wei/gas = 75,000,000,000,000,000 wei (0.075 MNT)
        let cost_3_hops = config.calculate_gas_cost(3);
        assert_eq!(cost_3_hops, U256::from(75_000_000_000_000_000u64));
    }
}
