use alloy::primitives::U256;

use super::bit_math;
use super::error::MoeLbtMathError;

const U256_ONE: U256 = U256::from_limbs([1, 0, 0, 0]);

pub fn mul_div_round_down(x: U256, y: U256, denominator: U256) -> Result<U256, MoeLbtMathError> {
    if denominator.is_zero() {
        return Err(MoeLbtMathError::DivisionByZero);
    }

    let (prod0, prod1) = get_mul_prods(x, y);
    get_end_of_div_round_down(x, y, denominator, prod0, prod1)
}

pub fn mul_div_round_up(x: U256, y: U256, denominator: U256) -> Result<U256, MoeLbtMathError> {
    let result = mul_div_round_down(x, y, denominator)?;
    if x.mul_mod(y, denominator).is_zero() {
        Ok(result)
    } else {
        Ok(result + U256_ONE)
    }
}

pub fn mul_shift_round_down(x: U256, y: U256, offset: u8) -> Result<U256, MoeLbtMathError> {
    // Note: offset is u8, so it's always < 256

    let (prod0, prod1) = get_mul_prods(x, y);

    let mut result = if !prod0.is_zero() {
        prod0 >> offset
    } else {
        U256::ZERO
    };

    if !prod1.is_zero() {
        if prod1 >= (U256_ONE << offset) {
            return Err(MoeLbtMathError::Overflow);
        }
        result += prod1 << (256 - offset as u32);
    }

    Ok(result)
}

pub fn mul_shift_round_up(x: U256, y: U256, offset: u8) -> Result<U256, MoeLbtMathError> {
    let result = mul_shift_round_down(x, y, offset)?;
    let modulus = U256_ONE << offset;
    if x.mul_mod(y, modulus).is_zero() {
        Ok(result)
    } else {
        Ok(result + U256_ONE)
    }
}

pub fn shift_div_round_down(x: U256, offset: u8, denominator: U256) -> Result<U256, MoeLbtMathError> {
    if denominator.is_zero() {
        return Err(MoeLbtMathError::DivisionByZero);
    }
    // Note: offset is u8, so it's always < 256

    let prod0 = x << offset;
    let prod1 = if offset == 0 {
        U256::ZERO
    } else {
        x >> (256 - offset as u32)
    };

    get_end_of_div_round_down(x, U256_ONE << offset, denominator, prod0, prod1)
}

pub fn shift_div_round_up(x: U256, offset: u8, denominator: U256) -> Result<U256, MoeLbtMathError> {
    let result = shift_div_round_down(x, offset, denominator)?;
    let modulus = U256_ONE << offset;
    if x.mul_mod(modulus, denominator).is_zero() {
        Ok(result)
    } else {
        Ok(result + U256_ONE)
    }
}

pub fn sqrt(x: U256) -> U256 {
    if x.is_zero() {
        return U256::ZERO;
    }

    let msb = bit_math::most_significant_bit(x).expect("non-zero");
    let mut guess = U256_ONE << (msb as u32 / 2 + 1);

    loop {
        let next = (guess + x / guess) >> 1;
        if next >= guess {
            return guess.min(x / guess);
        }
        guess = next;
    }
}

fn get_mul_prods(x: U256, y: U256) -> (U256, U256) {
    let mm = x.mul_mod(y, U256::MAX);
    let prod0 = x.overflowing_mul(y).0;
    let prod1 = mm
        .overflowing_sub(prod0)
        .0
        .overflowing_sub(U256::from((mm < prod0) as u8))
        .0;
    (prod0, prod1)
}

fn get_end_of_div_round_down(
    x: U256,
    y: U256,
    denominator: U256,
    mut prod0: U256,
    mut prod1: U256,
) -> Result<U256, MoeLbtMathError> {
    if denominator.is_zero() {
        return Err(MoeLbtMathError::DivisionByZero);
    }

    if prod1.is_zero() {
        return Ok(prod0 / denominator);
    }

    if denominator <= prod1 {
        return Err(MoeLbtMathError::Overflow);
    }

    let remainder = x.mul_mod(y, denominator);
    if remainder > prod0 {
        prod1 -= U256_ONE;
    }
    prod0 -= remainder;

    let mut twos = U256::ZERO
        .overflowing_sub(denominator)
        .0
        .bitand(denominator);

    let denominator = denominator / twos;
    prod0 = prod0 / twos;

    twos = U256::ZERO
        .overflowing_sub(twos)
        .0
        .wrapping_div(twos)
        + U256_ONE;

    prod0 |= prod1 * twos;

    let mut inverse = (U256::from(3) * denominator) ^ U256::from(2);
    inverse *= U256::from(2) - denominator * inverse;
    inverse *= U256::from(2) - denominator * inverse;
    inverse *= U256::from(2) - denominator * inverse;
    inverse *= U256::from(2) - denominator * inverse;
    inverse *= U256::from(2) - denominator * inverse;
    inverse *= U256::from(2) - denominator * inverse;

    Ok(prod0 * inverse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mul_div_round_down() {
        let x = U256::from(3_u64);
        let y = U256::from(7_u64);
        let d = U256::from(2_u64);
        assert_eq!(mul_div_round_down(x, y, d).unwrap(), U256::from(10_u64));
    }

    #[test]
    fn test_mul_div_round_up() {
        let x = U256::from(5_u64);
        let y = U256::from(7_u64);
        let d = U256::from(6_u64);
        assert_eq!(mul_div_round_up(x, y, d).unwrap(), U256::from(6_u64));
    }

    #[test]
    fn test_mul_shift_round_down() {
        let x = U256::from(1u64) << 200;
        let y = U256::from(1_u64 << 30);
        let res = mul_shift_round_down(x, y, 128).unwrap();
        assert!(!res.is_zero());
    }

    #[test]
    fn test_shift_div_round_down() {
        let x = U256::from(1000_u64);
        let res = shift_div_round_down(x, 8, U256::from(25_u64)).unwrap();
        assert_eq!(res, ((x << 8) / U256::from(25_u64)));
    }

    #[test]
    fn test_sqrt() {
        assert_eq!(sqrt(U256::from(0_u64)), U256::ZERO);
        assert_eq!(sqrt(U256::from(16_u64)), U256::from(4_u64));
        assert_eq!(sqrt(U256::from(24_u64)), U256::from(4_u64));
    }
}

