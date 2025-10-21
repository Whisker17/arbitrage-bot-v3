use alloy::primitives::U256;

use super::constants::{MAX_CONFIG_VALUE, PRECISION, PRECISION_U128};
use super::encoded::{self, MASK_UINT24, MASK_UINT64};
use super::error::MoeLbtMathError;
use super::packed_uint128_math;

const OFFSET_ID: u32 = 0;
const OFFSET_DISTRIBUTION_Y: u32 = 24;
const OFFSET_DISTRIBUTION_X: u32 = 88;

pub type Configuration = U256;

pub fn encode_params(distribution_x: u64, distribution_y: u64, id: u32) -> Result<Configuration, MoeLbtMathError> {
    if distribution_x as u128 > PRECISION_U128 || distribution_y as u128 > PRECISION_U128 {
        return Err(MoeLbtMathError::InvalidParameter);
    }
    let mut config = U256::ZERO;
    config = encoded::set(config, U256::from(distribution_x), MASK_UINT64, OFFSET_DISTRIBUTION_X);
    config = encoded::set(config, U256::from(distribution_y), MASK_UINT64, OFFSET_DISTRIBUTION_Y);
    Ok(encoded::set(config, U256::from(id), MASK_UINT24, OFFSET_ID))
}

pub fn decode_params(config: Configuration) -> Result<(u64, u64, u32), MoeLbtMathError> {
    let dist_x = encoded::decode_u64(config, OFFSET_DISTRIBUTION_X);
    let dist_y = encoded::decode_u64(config, OFFSET_DISTRIBUTION_Y);
    let id = encoded::decode_u24(config, OFFSET_ID);

    if config > MAX_CONFIG_VALUE
        || dist_x as u128 > PRECISION_U128
        || dist_y as u128 > PRECISION_U128
    {
        return Err(MoeLbtMathError::InvalidConfig);
    }

    Ok((dist_x, dist_y, id))
}

pub fn get_amounts_and_id(config: Configuration, amounts_in: U256) -> Result<(U256, u32), MoeLbtMathError> {
    let (distribution_x, distribution_y, id) = decode_params(config)?;
    let (x1, x2) = packed_uint128_math::decode(amounts_in);

    let x1 = U256::from(x1) * U256::from(distribution_x) / PRECISION;
    let x2 = U256::from(x2) * U256::from(distribution_y) / PRECISION;

    let x1_u128: u128 = x1.try_into().map_err(|_| MoeLbtMathError::Overflow)?;
    let x2_u128: u128 = x2.try_into().map_err(|_| MoeLbtMathError::Overflow)?;

    Ok((packed_uint128_math::encode(x1_u128, x2_u128), id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let config = encode_params(500_000_000_000_000_000, 500_000_000_000_000_000, 123).unwrap();
        let (dx, dy, id) = decode_params(config).unwrap();
        assert_eq!(dx, 500_000_000_000_000_000);
        assert_eq!(dy, 500_000_000_000_000_000);
        assert_eq!(id, 123);
    }

    #[test]
    fn test_get_amounts_and_id() {
        let config = encode_params(500_000_000_000_000_000, 500_000_000_000_000_000, 123).unwrap();
        let amounts = packed_uint128_math::encode(1_000_000_000_000_000_000, 2_000_000_000_000_000_000);
        let (result, id) = get_amounts_and_id(config, amounts).unwrap();
        let (x, y) = packed_uint128_math::decode(result);
        assert_eq!(x, 500_000_000_000_000_000);
        assert_eq!(y, 1_000_000_000_000_000_000);
        assert_eq!(id, 123);
    }
}
