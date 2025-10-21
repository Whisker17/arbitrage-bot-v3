use alloy::primitives::U256;

use super::error::MoeLbtMathError;

pub fn closest_bit_right(x: U256, bit: u8) -> Result<u32, MoeLbtMathError> {
    let shift = 255_u32.saturating_sub(bit as u32);
    let shifted = x << shift;

    if shifted.is_zero() {
        Ok(u32::MAX)
    } else {
        Ok(most_significant_bit(shifted)? as u32 - shift)
    }
}

pub fn closest_bit_left(x: U256, bit: u8) -> Result<u32, MoeLbtMathError> {
    let shifted = x >> bit;

    if shifted.is_zero() {
        Ok(u32::MAX)
    } else {
        Ok(least_significant_bit(shifted)? as u32 + bit as u32)
    }
}

pub fn most_significant_bit(x: U256) -> Result<u8, MoeLbtMathError> {
    if x.is_zero() {
        return Err(MoeLbtMathError::ZeroValue);
    }
    Ok(255 - x.leading_zeros() as u8)
}

pub fn least_significant_bit(x: U256) -> Result<u8, MoeLbtMathError> {
    if x.is_zero() {
        return Err(MoeLbtMathError::ZeroValue);
    }
    Ok(x.trailing_zeros() as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_msb_lsb() {
        assert_eq!(most_significant_bit(U256::from(1_u8)).unwrap(), 0);
        assert_eq!(least_significant_bit(U256::from(1_u8)).unwrap(), 0);
        assert_eq!(most_significant_bit(U256::from(1_u16 << 10)).unwrap(), 10);
        assert_eq!(least_significant_bit(U256::from(1_u16 << 10)).unwrap(), 10);
    }

    #[test]
    fn test_closest_bit_right() {
        let val = U256::from(0b1011000_u64);
        assert_eq!(closest_bit_right(val, 6).unwrap(), 3);
        assert_eq!(closest_bit_right(val, 4).unwrap(), 3);
        assert_eq!(closest_bit_right(val, 3).unwrap(), 0);
        assert_eq!(closest_bit_right(U256::ZERO, 5).unwrap(), u32::MAX);
    }

    #[test]
    fn test_closest_bit_left() {
        let val = U256::from(0b1011000_u64);
        assert_eq!(closest_bit_left(val, 1).unwrap(), 3);
        assert_eq!(closest_bit_left(val, 4).unwrap(), 6);
        assert_eq!(closest_bit_left(val, 6).unwrap(), u32::MAX);
    }
}

