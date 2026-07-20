use alloy::primitives::U256;

pub const DEFAULT_GAS_SAFETY_MARGIN: f64 = 1.2;

pub fn required_gross_for_gas_margin(gas_cost: U256, safety_margin: f64) -> U256 {
    if safety_margin > 1.0 && !gas_cost.is_zero() {
        let margin_millis = (safety_margin * 1000.0) as u128;
        gas_cost * U256::from(margin_millis) / U256::from(1000u64)
    } else {
        gas_cost
    }
}

pub fn net_profit_after_gas_cost(gross_profit_wei: U256, gas_cost_wei: U256) -> Option<U256> {
    gross_profit_wei.checked_sub(gas_cost_wei)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_the_shared_gas_margin() {
        let gas = U256::from(100u64);

        assert_eq!(
            required_gross_for_gas_margin(gas, DEFAULT_GAS_SAFETY_MARGIN),
            U256::from(120u64)
        );
        assert_eq!(required_gross_for_gas_margin(gas, 1.0), gas);
        assert_eq!(
            net_profit_after_gas_cost(U256::from(150u64), gas),
            Some(U256::from(50u64))
        );
        assert!(net_profit_after_gas_cost(U256::from(50u64), gas).is_none());
    }
}
