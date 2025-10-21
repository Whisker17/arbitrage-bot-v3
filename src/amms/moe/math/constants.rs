use alloy::primitives::U256;

pub const REAL_ID_SHIFT: i32 = 1 << 23;

pub const SCALE_OFFSET: u32 = 128;
// SCALE = 1 << 128 = U256::from_limbs([0, 0, 1, 0])
// U256::from_limbs takes [u64; 4] in little-endian order: [bits 0-63, 64-127, 128-191, 192-255]
pub const SCALE: U256 = U256::from_limbs([0, 0, 1, 0]);
pub const PRECISION_U128: u128 = 1_000_000_000_000_000_000;
// PRECISION_U128 fits in u64, so we can use (low, high, 0, 0)
pub const PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const PRECISION_U256: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
pub const SQUARED_PRECISION: U256 = U256::from_limbs([
    12_919_594_847_110_692_864,
    54_210_108_624_275_221,
    0,
    0,
]);
pub const MAX_FEE_U128: u128 = 100_000_000_000_000_000;
pub const MAX_FEE: U256 = U256::from_limbs([100_000_000_000_000_000, 0, 0, 0]);
pub const MAX_PROTOCOL_SHARE: u16 = 2_500;
pub const MAX_PROTOCOL_SHARE_U128: u128 = MAX_PROTOCOL_SHARE as u128;
pub const BASIS_POINT_MAX_U128: u128 = 10_000;
pub const BASIS_POINT_MAX: U256 = U256::from_limbs([10_000, 0, 0, 0]);
pub const MAX_CONFIG_VALUE: U256 = U256::from_limbs([0, 0, 0, 0xffff]);
pub const MAX_LIQUIDITY_PER_BIN: U256 = U256::from_limbs([
    7_868_752_419_796_399_633,
    14_574_905_401_537_909_530,
    16_199_717_119_757_969_306,
    10_395_202_414_653,
]);
pub const CALLBACK_SUCCESS: [u8; 32] = [
    0x4f, 0x96, 0xcb, 0x5a, 0xce, 0xf5, 0x37, 0xc4, 0xe1, 0x7d, 0x42, 0xc2, 0x7a, 0x59, 0x05, 0xf2,
    0x6e, 0xd1, 0x65, 0x1b, 0x05, 0xc9, 0x05, 0xc9, 0x1f, 0x31, 0xc4, 0xe0, 0x69, 0x32, 0xd5, 0xc3,
];

