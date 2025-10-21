use alloy::primitives::U256;

use super::bit_math;
use super::constants::SCALE;
use super::error::MoeLbtMathError;

const LOG_SCALE_OFFSET: u32 = 127;
// LOG_SCALE = 1 << 127 = U256::from_limbs([0, 0x8000_0000_0000_0000, 0, 0])
const LOG_SCALE: U256 = U256::from_limbs([0, 0x8000_0000_0000_0000, 0, 0]);
// LOG_SCALE_SQUARED = (1 << 127) * (1 << 127) = 1 << 254
const LOG_SCALE_SQUARED: U256 = U256::from_limbs([0, 0, 0, 0x4000_0000_0000_0000]);

pub fn log2(x: U256) -> Result<i128, MoeLbtMathError> {
    if x.is_zero() {
        return Err(MoeLbtMathError::LogUnderflow);
    }

    if x == U256::from(1_u8) {
        return Ok(-128);
    }

    let mut x = x >> 1;
    let sign = if x >= LOG_SCALE { 1 } else { -1 };
    if sign < 0 {
        x = LOG_SCALE_SQUARED / x;
    }

    let n = bit_math::most_significant_bit(x >> LOG_SCALE_OFFSET)?;
    let mut result: i128 = (n as i128) << LOG_SCALE_OFFSET;
    let mut y = x >> n;

    if y != LOG_SCALE {
        let mut delta = 1_i128 << (LOG_SCALE_OFFSET - 1);
        while delta > 0 {
            y = (y * y) >> LOG_SCALE_OFFSET;
            // 1 << (LOG_SCALE_OFFSET + 1) = 1 << 128
            if y >= U256::from(1_u128) << (LOG_SCALE_OFFSET + 1) {
                result += delta;
                y >>= 1;
            }
            delta >>= 1;
        }
    }

    Ok(result * sign * 2)
}

pub fn pow(x: U256, y: i128) -> Result<U256, MoeLbtMathError> {
    if y == 0 {
        return Ok(SCALE);
    }

    let mut invert = false;
    let mut abs_y = y;
    if abs_y < 0 {
        abs_y = -abs_y;
        invert = true;
    }

    if abs_y >= 0x100000 {
        return Err(MoeLbtMathError::PowUnderflow);
    }

    let mut result = SCALE;
    let mut squared = x;
    if x > U256::from_limbs([u64::MAX, 0, 0, 0]) {
        squared = U256::MAX / squared;
        invert = !invert;
    }

    let mut power = abs_y as u128;
    while power > 0 {
        if power & 1 == 1 {
            result = (result * squared) >> 128;
            if result.is_zero() {
                return Err(MoeLbtMathError::PowUnderflow);
            }
        }
        squared = (squared * squared) >> 128;
        power >>= 1;
    }

    if invert {
        Ok(U256::MAX / result)
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log2() {
        assert_eq!(log2(U256::from(1_u8)).unwrap(), -128);
        assert!(matches!(log2(U256::ZERO), Err(MoeLbtMathError::LogUnderflow)));
    }

    #[test]
    fn test_pow() {
        let base = SCALE + U256::from(1000_u64);
        let result = pow(base, 2).unwrap();
        assert!(!result.is_zero());
        assert!(pow(base, 0).unwrap() == SCALE);
    }
}
