use alloy::primitives::U256;

use super::constants::{BASIS_POINT_MAX_U128, MAX_PROTOCOL_SHARE};
use super::encoded::{self, MASK_UINT12, MASK_UINT14, MASK_UINT16, MASK_UINT20, MASK_UINT24, MASK_UINT40};
use super::error::MoeLbtMathError;

const OFFSET_BASE_FACTOR: u32 = 0;
const OFFSET_FILTER_PERIOD: u32 = 16;
const OFFSET_DECAY_PERIOD: u32 = 28;
const OFFSET_REDUCTION_FACTOR: u32 = 40;
const OFFSET_VAR_FEE_CONTROL: u32 = 54;
const OFFSET_PROTOCOL_SHARE: u32 = 78;
const OFFSET_MAX_VOL_ACC: u32 = 92;
const OFFSET_VOL_ACC: u32 = 112;
const OFFSET_VOL_REF: u32 = 132;
const OFFSET_ID_REF: u32 = 152;
const OFFSET_TIME_LAST_UPDATE: u32 = 176;
const OFFSET_ORACLE_ID: u32 = 216;
const OFFSET_ACTIVE_ID: u32 = 232;

const BASE_FEE_SCALAR: u128 = 10_000_000_000; // 1e10

pub type Parameters = U256;

#[inline(always)]
pub fn get_base_factor(params: Parameters) -> u16 {
    encoded::decode_u16(params, OFFSET_BASE_FACTOR)
}

#[inline(always)]
pub fn get_filter_period(params: Parameters) -> u16 {
    encoded::decode_u12(params, OFFSET_FILTER_PERIOD)
}

#[inline(always)]
pub fn get_decay_period(params: Parameters) -> u16 {
    encoded::decode_u12(params, OFFSET_DECAY_PERIOD)
}

#[inline(always)]
pub fn get_reduction_factor(params: Parameters) -> u16 {
    encoded::decode_u14(params, OFFSET_REDUCTION_FACTOR)
}

#[inline(always)]
pub fn get_variable_fee_control(params: Parameters) -> u32 {
    encoded::decode_u24(params, OFFSET_VAR_FEE_CONTROL)
}

#[inline(always)]
pub fn get_protocol_share(params: Parameters) -> u16 {
    encoded::decode_u14(params, OFFSET_PROTOCOL_SHARE)
}

#[inline(always)]
pub fn get_max_volatility_accumulator(params: Parameters) -> u32 {
    encoded::decode_u20(params, OFFSET_MAX_VOL_ACC)
}

#[inline(always)]
pub fn get_volatility_accumulator(params: Parameters) -> u32 {
    encoded::decode_u20(params, OFFSET_VOL_ACC)
}

#[inline(always)]
pub fn get_volatility_reference(params: Parameters) -> u32 {
    encoded::decode_u20(params, OFFSET_VOL_REF)
}

#[inline(always)]
pub fn get_id_reference(params: Parameters) -> u32 {
    encoded::decode_u24(params, OFFSET_ID_REF)
}

#[inline(always)]
pub fn get_time_of_last_update(params: Parameters) -> u64 {
    encoded::decode_u40(params, OFFSET_TIME_LAST_UPDATE)
}

#[inline(always)]
pub fn get_oracle_id(params: Parameters) -> u16 {
    encoded::decode_u16(params, OFFSET_ORACLE_ID)
}

#[inline(always)]
pub fn get_active_id(params: Parameters) -> u32 {
    encoded::decode_u24(params, OFFSET_ACTIVE_ID)
}

#[inline(always)]
pub fn get_delta_id(params: Parameters, active_id: u32) -> u32 {
    let cached = get_active_id(params);
    if active_id > cached {
        active_id - cached
    } else {
        cached - active_id
    }
}

pub fn get_base_fee(params: Parameters, bin_step: u16) -> u128 {
    let base_factor = get_base_factor(params) as u128;
    base_factor * bin_step as u128 * BASE_FEE_SCALAR
}

pub fn get_variable_fee(params: Parameters, bin_step: u16) -> u128 {
    let variable_fee_control = get_variable_fee_control(params) as u128;
    if variable_fee_control == 0 {
        return 0;
    }
    let vol_acc = get_volatility_accumulator(params) as u128;
    let prod = vol_acc * bin_step as u128;
    let numerator = U256::from(prod) * U256::from(prod) * U256::from(variable_fee_control);
    let result = (numerator + U256::from(99_u8)) / U256::from(100_u8);
    result.try_into().unwrap_or(u128::MAX)
}

pub fn get_total_fee(params: Parameters, bin_step: u16) -> Result<u128, MoeLbtMathError> {
    let base = get_base_fee(params, bin_step);
    let variable = get_variable_fee(params, bin_step);
    base.checked_add(variable).ok_or(MoeLbtMathError::Overflow)
}

#[inline(always)]
pub fn get_protocol_share_as_u128(params: Parameters) -> u128 {
    get_protocol_share(params) as u128
}

pub fn set_active_id(params: Parameters, active_id: u32) -> Parameters {
    encoded::set(params, U256::from(active_id), MASK_UINT24, OFFSET_ACTIVE_ID)
}

pub fn set_id_reference(params: Parameters, id_reference: u32) -> Parameters {
    encoded::set(params, U256::from(id_reference), MASK_UINT24, OFFSET_ID_REF)
}

pub fn set_oracle_id(params: Parameters, oracle_id: u16) -> Parameters {
    encoded::set(params, U256::from(oracle_id), MASK_UINT16, OFFSET_ORACLE_ID)
}

pub fn set_volatility_reference(params: Parameters, vol_ref: u32) -> Result<Parameters, MoeLbtMathError> {
    if vol_ref as u64 > MASK_UINT20.as_limbs()[0] as u64 {
        return Err(MoeLbtMathError::InvalidParameter);
    }
    Ok(encoded::set(params, U256::from(vol_ref), MASK_UINT20, OFFSET_VOL_REF))
}

pub fn set_volatility_accumulator(params: Parameters, vol_acc: u32) -> Result<Parameters, MoeLbtMathError> {
    if vol_acc as u64 > MASK_UINT20.as_limbs()[0] as u64 {
        return Err(MoeLbtMathError::InvalidParameter);
    }
    Ok(encoded::set(params, U256::from(vol_acc), MASK_UINT20, OFFSET_VOL_ACC))
}

pub fn update_id_reference(params: Parameters) -> Parameters {
    let active = get_active_id(params);
    encoded::set(params, U256::from(active), MASK_UINT24, OFFSET_ID_REF)
}

pub fn update_time_of_last_update(params: Parameters, timestamp: u64) -> Parameters {
    let value = timestamp.min(MASK_UINT40.as_limbs()[0] as u64);
    encoded::set(params, U256::from(value), MASK_UINT40, OFFSET_TIME_LAST_UPDATE)
}

pub fn update_volatility_reference(params: Parameters) -> Result<Parameters, MoeLbtMathError> {
    let vol_acc = get_volatility_accumulator(params) as u64;
    let reduction_factor = get_reduction_factor(params) as u64;
    let value = (vol_acc * reduction_factor) / BASIS_POINT_MAX_U128 as u64;
    set_volatility_reference(params, value as u32)
}

pub fn update_volatility_accumulator(params: Parameters, active_id: u32) -> Result<Parameters, MoeLbtMathError> {
    let id_reference = get_id_reference(params) as i64;
    let active_id = active_id as i64;
    let delta = if active_id > id_reference {
        (active_id - id_reference) as u64
    } else {
        (id_reference - active_id) as u64
    };
    let mut vol_acc = get_volatility_reference(params) as u64 + delta * BASIS_POINT_MAX_U128 as u64;
    let max_vol_acc = get_max_volatility_accumulator(params) as u64;
    if vol_acc > max_vol_acc {
        vol_acc = max_vol_acc;
    }
    set_volatility_accumulator(params, vol_acc as u32)
}

pub fn update_references(params: Parameters, timestamp: u64) -> Result<Parameters, MoeLbtMathError> {
    let last_update = get_time_of_last_update(params);
    let dt = timestamp.saturating_sub(last_update);
    let filter_period = get_filter_period(params) as u64;
    let decay_period = get_decay_period(params) as u64;

    let mut params = params;
    if dt >= filter_period {
        params = update_id_reference(params);
        params = if dt < decay_period {
            update_volatility_reference(params)?
        } else {
            set_volatility_reference(params, 0)?
        };
    }
    Ok(update_time_of_last_update(params, timestamp))
}

pub fn update_volatility_parameters(
    params: Parameters,
    active_id: u32,
    timestamp: u64,
) -> Result<Parameters, MoeLbtMathError> {
    let params = update_references(params, timestamp)?;
    update_volatility_accumulator(params, active_id)
}

pub fn set_static_fee_parameters(
    params: Parameters,
    base_factor: u16,
    filter_period: u16,
    decay_period: u16,
    reduction_factor: u16,
    variable_fee_control: u32,
    protocol_share: u16,
    max_volatility_accumulator: u32,
) -> Result<Parameters, MoeLbtMathError> {
    if filter_period > decay_period
        || decay_period as u64 > MASK_UINT12.as_limbs()[0] as u64
        || reduction_factor as u64 > BASIS_POINT_MAX_U128 as u64
        || protocol_share as u32 > MAX_PROTOCOL_SHARE as u32
        || max_volatility_accumulator as u64 > MASK_UINT20.as_limbs()[0] as u64
    {
        return Err(MoeLbtMathError::InvalidParameter);
    }

    let mut new_params = U256::ZERO;
    new_params = encoded::set(new_params, U256::from(base_factor), MASK_UINT16, OFFSET_BASE_FACTOR);
    new_params = encoded::set(new_params, U256::from(filter_period), MASK_UINT12, OFFSET_FILTER_PERIOD);
    new_params = encoded::set(new_params, U256::from(decay_period), MASK_UINT12, OFFSET_DECAY_PERIOD);
    new_params = encoded::set(new_params, U256::from(reduction_factor), MASK_UINT14, OFFSET_REDUCTION_FACTOR);
    new_params = encoded::set(new_params, U256::from(variable_fee_control), MASK_UINT24, OFFSET_VAR_FEE_CONTROL);
    new_params = encoded::set(new_params, U256::from(protocol_share), MASK_UINT14, OFFSET_PROTOCOL_SHARE);
    new_params = encoded::set(new_params, U256::from(max_volatility_accumulator), MASK_UINT20, OFFSET_MAX_VOL_ACC);

    Ok((params & !U256::from(0xffff_ffff_ffff_ffff_ffff_ffff_u128)) | new_params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_getters_setters() {
        let mut params = U256::ZERO;
        params = set_active_id(params, 123456);
        assert_eq!(get_active_id(params), 123456);
        params = set_oracle_id(params, 42);
        assert_eq!(get_oracle_id(params), 42);
    }

    #[test]
    fn test_update_references() {
        let mut params = U256::ZERO;
        params = set_active_id(params, 100);
        let timestamp = 1000_u64;
        let updated = update_references(params, timestamp).unwrap();
        assert_eq!(get_time_of_last_update(updated), timestamp.min(MASK_UINT40.as_limbs()[0] as u64));
    }
}
