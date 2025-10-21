use alloy::primitives::U256;

use super::error::MoeLbtMathError;

#[inline(always)]
pub fn to_u8(value: U256) -> Result<u8, MoeLbtMathError> {
    if value > U256::from(u8::MAX) {
        return Err(MoeLbtMathError::ValueExceedsBits(8));
    }
    Ok(value.as_limbs()[0] as u8)
}

#[inline(always)]
pub fn to_u16(value: U256) -> Result<u16, MoeLbtMathError> {
    if value > U256::from(u16::MAX) {
        return Err(MoeLbtMathError::ValueExceedsBits(16));
    }
    Ok(value.as_limbs()[0] as u16)
}

#[inline(always)]
pub fn to_u24(value: U256) -> Result<u32, MoeLbtMathError> {
    if value > U256::from(0xFF_FFFF_u32) {
        return Err(MoeLbtMathError::ValueExceedsBits(24));
    }
    Ok(value.as_limbs()[0] as u32)
}

#[inline(always)]
pub fn to_u32(value: U256) -> Result<u32, MoeLbtMathError> {
    if value > U256::from(u32::MAX) {
        return Err(MoeLbtMathError::ValueExceedsBits(32));
    }
    Ok(value.as_limbs()[0] as u32)
}

#[inline(always)]
pub fn to_u40(value: U256) -> Result<u64, MoeLbtMathError> {
    if value > U256::from(0xFF_FFFF_FFFF_u64) {
        return Err(MoeLbtMathError::ValueExceedsBits(40));
    }
    Ok(value.as_limbs()[0] as u64)
}

#[inline(always)]
pub fn to_u64(value: U256) -> Result<u64, MoeLbtMathError> {
    if value > U256::from(u64::MAX) {
        return Err(MoeLbtMathError::ValueExceedsBits(64));
    }
    Ok(value.as_limbs()[0] as u64)
}

#[inline(always)]
pub fn to_u88(value: U256) -> Result<u128, MoeLbtMathError> {
    if value > U256::from(0xFF_FFFF_FFFF_FFFF_FFFF_u128) {
        return Err(MoeLbtMathError::ValueExceedsBits(88));
    }
    Ok(value.as_limbs()[0] as u128)
}

#[inline(always)]
pub fn to_u128(value: U256) -> Result<u128, MoeLbtMathError> {
    if value > U256::from(u128::MAX) {
        return Err(MoeLbtMathError::ValueExceedsBits(128));
    }
    Ok(value.as_limbs()[0] as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_casts() {
        assert_eq!(to_u8(U256::from(255_u16)).unwrap(), 255);
        assert!(to_u8(U256::from(256_u16)).is_err());

        assert_eq!(to_u24(U256::from(0xAB_CDEF_u32)).unwrap(), 0xAB_CDEF);
        assert!(to_u24(U256::from(0x1_00_0000_u32)).is_err());

        assert_eq!(to_u40(U256::from(0xFF_FFFF_FFFF_u64)).unwrap(), 0xFF_FFFF_FFFF);
        assert!(to_u40(U256::from(0x1_00_0000_0000_u64)).is_err());

        assert_eq!(to_u128(U256::from(u128::MAX)).unwrap(), u128::MAX);
        assert!(to_u128(U256::from_limbs([0, 1, 0, 0])).is_err());
    }
}

