use alloy::primitives::U256;

use super::constants::{BASIS_POINT_MAX_U128, REAL_ID_SHIFT, SCALE, SCALE_OFFSET, PRECISION};
use super::uint128x128_math;
use super::uint256x256_math;

pub fn get_base(bin_step: u16) -> U256 {
    let term1 = SCALE;
    let term2 = (U256::from(bin_step as u128) << SCALE_OFFSET) / U256::from(BASIS_POINT_MAX_U128);
    term1 + term2
}

pub fn get_price_from_id(id: u32, bin_step: u16) -> U256 {
    let base = get_base(bin_step);
    let exponent = get_exponent(id);
    uint128x128_math::pow(base, exponent as i128).expect("pow computation")
}

pub fn get_id_from_price(price: U256, bin_step: u16) -> u32 {
    let base = get_base(bin_step);
    let real_id = uint128x128_math::log2(price).unwrap() / uint128x128_math::log2(base).unwrap();
    (REAL_ID_SHIFT + real_id as i32) as u32
}

pub fn get_exponent(id: u32) -> i32 {
    id as i32 - REAL_ID_SHIFT
}

pub fn convert_decimal_price_to_128x128(price: U256) -> U256 {
    uint256x256_math::shift_div_round_down(price, SCALE_OFFSET as u8, PRECISION).unwrap()
}

pub fn convert_128x128_price_to_decimal(price128x128: U256) -> U256 {
    uint256x256_math::mul_shift_round_down(price128x128, PRECISION, SCALE_OFFSET as u8).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_base() {
        let base = get_base(25);
        assert!(base > SCALE);
    }

    #[test]
    fn test_price_roundtrip() {
        let id = 1_000_000;
        let bin_step = 25;
        let price = get_price_from_id(id, bin_step);
        let recovered = get_id_from_price(price, bin_step);
        assert_eq!(recovered, id);
    }
}
