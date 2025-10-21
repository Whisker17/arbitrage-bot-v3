use alloy::primitives::U256;

pub const MASK_UINT1: U256 = U256::from_limbs([0x1, 0, 0, 0]);
pub const MASK_UINT8: U256 = U256::from_limbs([0xff, 0, 0, 0]);
pub const MASK_UINT12: U256 = U256::from_limbs([0xfff, 0, 0, 0]);
pub const MASK_UINT14: U256 = U256::from_limbs([0x3fff, 0, 0, 0]);
pub const MASK_UINT16: U256 = U256::from_limbs([0xffff, 0, 0, 0]);
pub const MASK_UINT20: U256 = U256::from_limbs([0xfffff, 0, 0, 0]);
pub const MASK_UINT24: U256 = U256::from_limbs([0xffffff, 0, 0, 0]);
pub const MASK_UINT40: U256 = U256::from_limbs([0xffffffffff, 0, 0, 0]);
pub const MASK_UINT64: U256 = U256::from_limbs([0xffffffffffffffff, 0, 0, 0]);
pub const MASK_UINT128: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

#[inline(always)]
pub fn set(encoded: U256, value: U256, mask: U256, offset: u32) -> U256 {
    let cleared = encoded & !(mask << offset);
    cleared | ((value & mask) << offset)
}

#[inline(always)]
pub fn decode(encoded: U256, mask: U256, offset: u32) -> U256 {
    (encoded >> offset) & mask
}

#[inline(always)]
pub fn decode_bool(encoded: U256, offset: u32) -> bool {
    !decode(encoded, MASK_UINT1, offset).is_zero()
}

#[inline(always)]
pub fn decode_u8(encoded: U256, offset: u32) -> u8 {
    decode(encoded, MASK_UINT8, offset).as_limbs()[0] as u8
}

#[inline(always)]
pub fn decode_u12(encoded: U256, offset: u32) -> u16 {
    (decode(encoded, MASK_UINT12, offset).as_limbs()[0] & 0xfff) as u16
}

#[inline(always)]
pub fn decode_u14(encoded: U256, offset: u32) -> u16 {
    (decode(encoded, MASK_UINT14, offset).as_limbs()[0] & 0x3fff) as u16
}

#[inline(always)]
pub fn decode_u16(encoded: U256, offset: u32) -> u16 {
    decode(encoded, MASK_UINT16, offset).as_limbs()[0] as u16
}

#[inline(always)]
pub fn decode_u20(encoded: U256, offset: u32) -> u32 {
    (decode(encoded, MASK_UINT20, offset).as_limbs()[0] & 0xfffff) as u32
}

#[inline(always)]
pub fn decode_u24(encoded: U256, offset: u32) -> u32 {
    (decode(encoded, MASK_UINT24, offset).as_limbs()[0] & 0xffffff) as u32
}

#[inline(always)]
pub fn decode_u40(encoded: U256, offset: u32) -> u64 {
    (decode(encoded, MASK_UINT40, offset).as_limbs()[0] & 0xffffffffff) as u64
}

#[inline(always)]
pub fn decode_u64(encoded: U256, offset: u32) -> u64 {
    decode(encoded, MASK_UINT64, offset).as_limbs()[0]
}

#[inline(always)]
pub fn decode_u128(encoded: U256, offset: u32) -> u128 {
    let result = decode(encoded, MASK_UINT128, offset);
    let limbs = result.as_limbs();
    // Combine two u64 limbs into a u128
    (limbs[0] as u128) | ((limbs[1] as u128) << 64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_decode() {
        let mut value = U256::ZERO;
        value = set(value, U256::from(0b1010_u8), MASK_UINT8, 0);
        assert_eq!(decode_u8(value, 0), 0b1010);

        value = set(value, U256::from(0b11_u8), MASK_UINT14, 64);
        assert_eq!(decode_u14(value, 64), 0b11);

        value = set(value, U256::from(1_u8), MASK_UINT1, 10);
        assert!(decode_bool(value, 10));
        assert!(!decode_bool(value, 11));
    }
}

