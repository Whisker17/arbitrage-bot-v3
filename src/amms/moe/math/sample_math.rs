use alloy::primitives::U256;

use super::encoded::{self, MASK_UINT16, MASK_UINT40, MASK_UINT64, MASK_UINT8};

pub type Sample = U256;

const OFFSET_ORACLE_LENGTH: u32 = 0;
const OFFSET_CUMULATIVE_ID: u32 = 16;
const OFFSET_CUMULATIVE_VOLATILITY: u32 = 80;
const OFFSET_CUMULATIVE_BIN_CROSSED: u32 = 144;
const OFFSET_SAMPLE_LIFETIME: u32 = 208;
const OFFSET_SAMPLE_CREATION: u32 = 216;

pub fn encode(
    oracle_length: u16,
    cumulative_id: u64,
    cumulative_volatility: u64,
    cumulative_bin_crossed: u64,
    sample_lifetime: u8,
    created_at: u64,
) -> Sample {
    let mut sample = U256::ZERO;
    sample = encoded::set(sample, U256::from(oracle_length), MASK_UINT16, OFFSET_ORACLE_LENGTH);
    sample = encoded::set(sample, U256::from(cumulative_id), MASK_UINT64, OFFSET_CUMULATIVE_ID);
    sample = encoded::set(sample, U256::from(cumulative_volatility), MASK_UINT64, OFFSET_CUMULATIVE_VOLATILITY);
    sample = encoded::set(sample, U256::from(cumulative_bin_crossed), MASK_UINT64, OFFSET_CUMULATIVE_BIN_CROSSED);
    sample = encoded::set(sample, U256::from(sample_lifetime), MASK_UINT8, OFFSET_SAMPLE_LIFETIME);
    encoded::set(sample, U256::from(created_at), MASK_UINT40, OFFSET_SAMPLE_CREATION)
}

pub fn get_oracle_length(sample: Sample) -> u16 {
    encoded::decode_u16(sample, OFFSET_ORACLE_LENGTH)
}

pub fn get_cumulative_id(sample: Sample) -> u64 {
    encoded::decode_u64(sample, OFFSET_CUMULATIVE_ID)
}

pub fn get_cumulative_volatility(sample: Sample) -> u64 {
    encoded::decode_u64(sample, OFFSET_CUMULATIVE_VOLATILITY)
}

pub fn get_cumulative_bin_crossed(sample: Sample) -> u64 {
    encoded::decode_u64(sample, OFFSET_CUMULATIVE_BIN_CROSSED)
}

pub fn get_sample_lifetime(sample: Sample) -> u8 {
    encoded::decode_u8(sample, OFFSET_SAMPLE_LIFETIME)
}

pub fn get_sample_creation(sample: Sample) -> u64 {
    encoded::decode_u40(sample, OFFSET_SAMPLE_CREATION)
}

pub fn get_sample_last_update(sample: Sample) -> u64 {
    get_sample_creation(sample) + get_sample_lifetime(sample) as u64
}

pub fn get_weighted_average(
    sample1: Sample,
    sample2: Sample,
    weight1: u64,
    weight2: u64,
) -> (u64, u64, u64) {
    let c_id1 = get_cumulative_id(sample1);
    let c_vol1 = get_cumulative_volatility(sample1);
    let c_bin1 = get_cumulative_bin_crossed(sample1);

    if weight2 == 0 {
        return (c_id1, c_vol1, c_bin1);
    }

    let c_id2 = get_cumulative_id(sample2);
    let c_vol2 = get_cumulative_volatility(sample2);
    let c_bin2 = get_cumulative_bin_crossed(sample2);

    if weight1 == 0 {
        return (c_id2, c_vol2, c_bin2);
    }

    let total_weight = weight1 + weight2;
    let weighted_id = (c_id1 * weight1 + c_id2 * weight2) / total_weight;
    let weighted_vol = (c_vol1 * weight1 + c_vol2 * weight2) / total_weight;
    let weighted_bin = (c_bin1 * weight1 + c_bin2 * weight2) / total_weight;
    (weighted_id, weighted_vol, weighted_bin)
}

pub fn update(
    sample: Sample,
    delta_time: u64,
    active_id: u32,
    volatility_accumulator: u32,
    bin_crossed: u32,
) -> (u64, u64, u64) {
    let cumulative_id = get_cumulative_id(sample) + active_id as u64 * delta_time;
    let cumulative_vol = get_cumulative_volatility(sample) + volatility_accumulator as u64 * delta_time;
    let cumulative_bin = get_cumulative_bin_crossed(sample) + bin_crossed as u64 * delta_time;
    (cumulative_id, cumulative_vol, cumulative_bin)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode() {
        let sample = encode(10, 20, 30, 40, 50, 60);
        assert_eq!(get_oracle_length(sample), 10);
        assert_eq!(get_cumulative_id(sample), 20);
        assert_eq!(get_cumulative_volatility(sample), 30);
        assert_eq!(get_cumulative_bin_crossed(sample), 40);
        assert_eq!(get_sample_lifetime(sample), 50);
        assert_eq!(get_sample_creation(sample), 60);
    }

    #[test]
    fn test_weighted_average() {
        let s1 = encode(0, 10, 20, 30, 0, 0);
        let s2 = encode(0, 20, 40, 60, 0, 0);
        let (id, vol, bin) = get_weighted_average(s1, s2, 1, 1);
        assert_eq!(id, 15);
        assert_eq!(vol, 30);
        assert_eq!(bin, 45);
    }

    #[test]
    fn test_update() {
        let sample = encode(0, 10, 20, 30, 0, 0);
        let (id, vol, bin) = update(sample, 2, 5, 3, 1);
        assert_eq!(id, 10 + 10);
        assert_eq!(vol, 20 + 6);
        assert_eq!(bin, 30 + 2);
    }
}

