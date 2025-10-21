use alloy::primitives::U256;

use super::constants::BASIS_POINT_MAX_U128;
use super::error::MoeLbtMathError;

// MASK_128 = u128::MAX = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF (lower 128 bits all set)
const MASK_128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);
const OFFSET: u32 = 128;

pub fn encode(x1: u128, x2: u128) -> U256 {
    let lower = U256::from(x1);
    let upper = U256::from(x2) << OFFSET;
    lower | upper
}

pub fn encode_first(x1: u128) -> U256 {
    U256::from(x1)
}

pub fn encode_second(x2: u128) -> U256 {
    U256::from(x2) << OFFSET
}

pub fn decode(z: U256) -> (u128, u128) {
    let x1 = (z & MASK_128).as_limbs()[0] as u128;
    let x2 = (z >> OFFSET).as_limbs()[0] as u128;
    (x1, x2)
}

pub fn decode_x(z: U256) -> u128 {
    (z & MASK_128).as_limbs()[0] as u128
}

pub fn decode_y(z: U256) -> u128 {
    (z >> OFFSET).as_limbs()[0] as u128
}

pub fn decode_first(z: U256, is_first: bool) -> u128 {
    if is_first {
        decode_x(z)
    } else {
        decode_y(z)
    }
}

pub fn add(x: U256, y: U256) -> Result<U256, MoeLbtMathError> {
    let z = x + y;
    if ((z & MASK_128) < (x & MASK_128)) || (((z >> OFFSET) & MASK_128) < ((x >> OFFSET) & MASK_128)) {
        return Err(MoeLbtMathError::Overflow);
    }
    Ok(z)
}

pub fn add_with_components(x: U256, y1: u128, y2: u128) -> Result<U256, MoeLbtMathError> {
    add(x, encode(y1, y2))
}

pub fn sub(x: U256, y: U256) -> Result<U256, MoeLbtMathError> {
    if (x & MASK_128) < (y & MASK_128) || ((x >> OFFSET) & MASK_128) < ((y >> OFFSET) & MASK_128) {
        return Err(MoeLbtMathError::Underflow);
    }
    Ok(x - y)
}

pub fn sub_with_components(x: U256, y1: u128, y2: u128) -> Result<U256, MoeLbtMathError> {
    sub(x, encode(y1, y2))
}

pub fn scalar_mul_div_basis_point_round_down(x: U256, multiplier: u128) -> Result<U256, MoeLbtMathError> {
    if multiplier == 0 {
        return Ok(U256::ZERO);
    }
    if multiplier > BASIS_POINT_MAX_U128 {
        return Err(MoeLbtMathError::MultiplierTooLarge);
    }

    let (x1, x2) = decode(x);
    let basis_point = U256::from(BASIS_POINT_MAX_U128);
    let x1_res = U256::from(x1) * U256::from(multiplier) / basis_point;
    let x2_res = U256::from(x2) * U256::from(multiplier) / basis_point;
    let x1_res: u128 = x1_res.try_into().map_err(|_| MoeLbtMathError::Overflow)?;
    let x2_res: u128 = x2_res.try_into().map_err(|_| MoeLbtMathError::Overflow)?;
    Ok(encode(x1_res, x2_res))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let packed = encode(10, 20);
        assert_eq!(decode(packed), (10, 20));
    }

    #[test]
    fn test_decode_x_y() {
        let packed = encode(123, 456);
        assert_eq!(decode_x(packed), 123);
        assert_eq!(decode_y(packed), 456);
    }

    #[test]
    fn test_add_sub() {
        let a = encode(5, 7);
        let b = encode(10, 3);
        let added = add(a, b).unwrap();
        assert_eq!(decode(added), (15, 10));
        let subtracted = sub(added, b).unwrap();
        assert_eq!(decode(subtracted), (5, 7));
    }

    #[test]
    fn test_scalar_mul_div() {
        let value = encode(1000, 2000);
        let result = scalar_mul_div_basis_point_round_down(value, 5000).unwrap();
        assert_eq!(decode(result), (500, 1000));
    }
}

