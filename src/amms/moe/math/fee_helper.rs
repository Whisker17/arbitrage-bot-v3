use alloy::primitives::U256;

use super::constants::{BASIS_POINT_MAX_U128, MAX_FEE_U128, PRECISION, PRECISION_U128};
use super::error::MoeLbtMathError;

const U256_ONE: U256 = U256::from_limbs([1, 0, 0, 0]);

fn ensure_fee(total_fee: u128) -> Result<(), MoeLbtMathError> {
    if total_fee > MAX_FEE_U128 {
        Err(MoeLbtMathError::FeeTooLarge)
    } else {
        Ok(())
    }
}

fn ensure_protocol_share(protocol_share: u128) -> Result<(), MoeLbtMathError> {
    if protocol_share > BASIS_POINT_MAX_U128 as u128 {
        Err(MoeLbtMathError::ProtocolShareTooLarge)
    } else {
        Ok(())
    }
}

pub fn get_fee_amount_from(amount_with_fees: u128, total_fee: u128) -> Result<u128, MoeLbtMathError> {
    ensure_fee(total_fee)?;
    let mut numerator = U256::from(amount_with_fees) * U256::from(total_fee);
    numerator += PRECISION - U256_ONE;
    let fee = numerator / PRECISION;
    fee.try_into().map_err(|_| MoeLbtMathError::Overflow)
}

pub fn get_fee_amount(amount: u128, total_fee: u128) -> Result<u128, MoeLbtMathError> {
    ensure_fee(total_fee)?;
    let denominator_scalar = PRECISION_U128 - total_fee;
    let denominator = U256::from(denominator_scalar);
    let mut numerator = U256::from(amount) * U256::from(total_fee);
    numerator += denominator - U256_ONE;
    let fee = numerator / denominator;
    fee.try_into().map_err(|_| MoeLbtMathError::Overflow)
}

pub fn get_composition_fee(amount_with_fees: u128, total_fee: u128) -> Result<u128, MoeLbtMathError> {
    ensure_fee(total_fee)?;
    let total_fee_u256 = U256::from(total_fee);
    let precision_squared = PRECISION * PRECISION;
    let result = U256::from(amount_with_fees) * total_fee_u256 * (total_fee_u256 + PRECISION) / precision_squared;
    result.try_into().map_err(|_| MoeLbtMathError::Overflow)
}

pub fn get_protocol_fee_amount(fee_amount: u128, protocol_share: u128) -> Result<u128, MoeLbtMathError> {
    ensure_protocol_share(protocol_share)?;
    let numerator = U256::from(fee_amount) * U256::from(protocol_share);
    let result = numerator / U256::from(BASIS_POINT_MAX_U128);
    result.try_into().map_err(|_| MoeLbtMathError::Overflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fee_amount_from() {
        let fee = get_fee_amount_from(1_000_000_000_000_000_000, 100_000_000_000_000).unwrap();
        assert_eq!(fee, 100_000_000_000_000);
    }

    #[test]
    fn test_fee_amount() {
        let fee = get_fee_amount(1_000_000_000_000_000_000, 100_000_000_000_000).unwrap();
        assert_eq!(fee, 111_111_111_111_112);
    }

    #[test]
    fn test_protocol_fee() {
        let fee = get_protocol_fee_amount(1_000_000_000_000_000_000, 2_500).unwrap();
        assert_eq!(fee, 250_000_000_000_000);
    }
}

