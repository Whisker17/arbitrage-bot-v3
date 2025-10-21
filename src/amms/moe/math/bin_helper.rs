use alloy::primitives::U256;

use super::constants::{MAX_LIQUIDITY_PER_BIN, SCALE, SCALE_OFFSET};
use super::error::MoeLbtMathError;
use super::fee_helper;
use super::packed_uint128_math;
use super::pair_parameter_helper;
use super::price_helper;
use super::uint256x256_math;

const U256_ONE: U256 = U256::from_limbs([1, 0, 0, 0]);

fn safe128(val: U256) -> Result<u128, MoeLbtMathError> {
    val.try_into().map_err(|_| MoeLbtMathError::Overflow)
}

pub fn get_amount_out_of_bin(bin_reserves: U256, amount_to_burn: U256, total_supply: U256) -> Result<U256, MoeLbtMathError> {
    let (bin_reserve_x, bin_reserve_y) = packed_uint128_math::decode(bin_reserves);

    let mut amount_x_out_from_bin = 0_u128;
    let mut amount_y_out_from_bin = 0_u128;

    if bin_reserve_x > 0 {
        let val = uint256x256_math::mul_div_round_down(amount_to_burn, U256::from(bin_reserve_x), total_supply)?;
        amount_x_out_from_bin = safe128(val)?;
    }

    if bin_reserve_y > 0 {
        let val = uint256x256_math::mul_div_round_down(amount_to_burn, U256::from(bin_reserve_y), total_supply)?;
        amount_y_out_from_bin = safe128(val)?;
    }

    Ok(packed_uint128_math::encode(amount_x_out_from_bin, amount_y_out_from_bin))
}

pub fn get_shares_and_effective_amounts_in(
    bin_reserves: U256,
    amounts_in: U256,
    price: U256,
    total_supply: U256,
) -> Result<(U256, U256), MoeLbtMathError> {
    let (mut x, mut y) = packed_uint128_math::decode(amounts_in);
    let user_liquidity = get_liquidity_components(U256::from(x), U256::from(y), price)?;
    if user_liquidity.is_zero() {
        return Ok((U256::ZERO, U256::ZERO));
    }

    let bin_liquidity = get_liquidity(bin_reserves, price)?;
    if bin_liquidity.is_zero() || total_supply.is_zero() {
        return Ok((uint256x256_math::sqrt(user_liquidity), amounts_in));
    }

    let shares = uint256x256_math::mul_div_round_down(user_liquidity, total_supply, bin_liquidity)?;
    let effective_liquidity = uint256x256_math::mul_div_round_up(shares, bin_liquidity, total_supply)?;

    let mut amounts_in = amounts_in;
    if user_liquidity > effective_liquidity {
        let mut delta_liquidity = user_liquidity - effective_liquidity;
        if delta_liquidity >= SCALE {
            let mut delta_y = delta_liquidity >> SCALE_OFFSET;
            let y_u256 = U256::from(y);
            if delta_y > y_u256 {
                delta_y = y_u256;
            }
            y -= delta_y.as_limbs()[0] as u128;
            delta_liquidity -= delta_y << SCALE_OFFSET;
        }

        if delta_liquidity >= price {
            let mut delta_x = delta_liquidity / price;
            let x_u256 = U256::from(x);
            if delta_x > x_u256 {
                delta_x = x_u256;
            }
            x -= delta_x.as_limbs()[0] as u128;
        }

        amounts_in = packed_uint128_math::encode(x, y);
    }

    let new_liquidity = get_liquidity(bin_reserves + amounts_in, price)?;
    if new_liquidity > MAX_LIQUIDITY_PER_BIN {
        return Err(MoeLbtMathError::MaxLiquidityPerBinExceeded);
    }

    Ok((shares, amounts_in))
}

pub fn get_liquidity(amounts: U256, price: U256) -> Result<U256, MoeLbtMathError> {
    let (x, y) = packed_uint128_math::decode(amounts);
    get_liquidity_components(U256::from(x), U256::from(y), price)
}

pub fn get_liquidity_components(x: U256, y: U256, price: U256) -> Result<U256, MoeLbtMathError> {
    let mut liquidity = U256::ZERO;
    if x > U256::ZERO {
        liquidity = price * x;
        if liquidity / x != price {
            return Err(MoeLbtMathError::LiquidityOverflow);
        }
    }
    if y > U256::ZERO {
        let scaled_y = y << SCALE_OFFSET;
        liquidity += scaled_y;
        if liquidity < scaled_y {
            return Err(MoeLbtMathError::LiquidityOverflow);
        }
    }
    Ok(liquidity)
}

pub fn verify_amounts(amounts: U256, active_id: u32, id: u32) -> Result<(), MoeLbtMathError> {
    let upper = amounts >> 128;
    let lower = amounts & U256::from(u128::MAX);

    if (id < active_id && upper > U256::ZERO) || (id > active_id && lower > U256::from(u128::MAX)) {
        return Err(MoeLbtMathError::InvalidConfig);
    }
    Ok(())
}

pub fn get_composition_fees(
    bin_reserves: U256,
    parameters: U256,
    bin_step: u16,
    amounts_in: U256,
    total_supply: U256,
    shares: U256,
) -> Result<U256, MoeLbtMathError> {
    if shares.is_zero() {
        return Ok(U256::ZERO);
    }

    let (amount_x, amount_y) = packed_uint128_math::decode(amounts_in);
    let encoded_out = get_amount_out_of_bin(bin_reserves + amounts_in, shares, total_supply + shares)?;
    let (received_x, received_y) = packed_uint128_math::decode(encoded_out);
    let total_fee = pair_parameter_helper::get_total_fee(parameters, bin_step)? as u128;

    if received_x > amount_x {
        let delta_y = amount_y - received_y;
        let fee_y = fee_helper::get_composition_fee(delta_y, total_fee)?;
        Ok(packed_uint128_math::encode(0, fee_y))
    } else if received_y > amount_y {
        let delta_x = amount_x - received_x;
        let fee_x = fee_helper::get_composition_fee(delta_x, total_fee)?;
        Ok(packed_uint128_math::encode(fee_x, 0))
    } else {
        Ok(U256::ZERO)
    }
}

pub fn is_empty(bin_reserves: U256, is_x: bool) -> bool {
    if is_x {
        packed_uint128_math::decode_x(bin_reserves) == 0
    } else {
        packed_uint128_math::decode_y(bin_reserves) == 0
    }
}

pub fn get_amounts(
    bin_reserves: U256,
    parameters: U256,
    bin_step: u16,
    swap_for_y: bool,
    active_id: u32,
    amounts_in_left: U256,
) -> Result<(U256, U256, U256), MoeLbtMathError> {
    let price = price_helper::get_price_from_id(active_id, bin_step);
    
    let bin_reserve_out = if swap_for_y {
        packed_uint128_math::decode_y(bin_reserves)
    } else {
        packed_uint128_math::decode_x(bin_reserves)
    };

    let max_amount_in = if swap_for_y {
        safe128(uint256x256_math::shift_div_round_up(U256::from(bin_reserve_out), SCALE_OFFSET as u8, price)?)?
    } else {
        safe128(uint256x256_math::mul_shift_round_up(U256::from(bin_reserve_out), price, SCALE_OFFSET as u8)?)?
    };

    let total_fee = pair_parameter_helper::get_total_fee(parameters, bin_step)? as u128;
    let max_fee = fee_helper::get_fee_amount(max_amount_in, total_fee)?;
    let max_amount_in_total = max_amount_in + max_fee;

    let amount_in = packed_uint128_math::decode_first(amounts_in_left, swap_for_y);
    let (amount_in_with_fee, mut amount_out, fee);

    if amount_in >= max_amount_in_total {
        fee = max_fee;
        amount_in_with_fee = max_amount_in_total;
        amount_out = bin_reserve_out;
    } else {
        fee = fee_helper::get_fee_amount_from(amount_in, total_fee)?;
        let amount_in_net = amount_in - fee;
        
        amount_out = if swap_for_y {
            let raw_out = uint256x256_math::mul_shift_round_down(U256::from(amount_in_net), price, SCALE_OFFSET as u8)?;
            safe128(raw_out)?
        } else {
            let raw_out = uint256x256_math::shift_div_round_down(U256::from(amount_in_net), SCALE_OFFSET as u8, price)?;
            safe128(raw_out)?
        };
        
        if amount_out > bin_reserve_out {
            amount_out = bin_reserve_out;
        }
        amount_in_with_fee = amount_in;
    }

    let amounts_in_with_fees = if swap_for_y {
        packed_uint128_math::encode(amount_in_with_fee, 0)
    } else {
        packed_uint128_math::encode(0, amount_in_with_fee)
    };

    let amounts_out_of_bin = if swap_for_y {
        packed_uint128_math::encode(0, amount_out)
    } else {
        packed_uint128_math::encode(amount_out, 0)
    };

    let total_fees = if swap_for_y {
        packed_uint128_math::encode(fee, 0)
    } else {
        packed_uint128_math::encode(0, fee)
    };

    let new_liquidity = get_liquidity(bin_reserves + amounts_in_with_fees - amounts_out_of_bin, price)?;
    if new_liquidity > MAX_LIQUIDITY_PER_BIN {
        return Err(MoeLbtMathError::MaxLiquidityPerBinExceeded);
    }

    Ok((amounts_in_with_fees, amounts_out_of_bin, total_fees))
}

pub fn received(reserves: U256, balance_x: U256, balance_y: U256) -> Result<U256, MoeLbtMathError> {
    let (reserve_x, reserve_y) = packed_uint128_math::decode(reserves);
    let amount_x = balance_x - U256::from(reserve_x);
    let amount_y = balance_y - U256::from(reserve_y);
    Ok(packed_uint128_math::encode(safe128(amount_x)?, safe128(amount_y)?))
}

pub fn received_x(reserves: U256, balance_x: U256) -> Result<U256, MoeLbtMathError> {
    let reserve_x = packed_uint128_math::decode_x(reserves);
    let amount_x = balance_x - U256::from(reserve_x);
    Ok(packed_uint128_math::encode(safe128(amount_x)?, 0))
}

pub fn received_y(reserves: U256, balance_y: U256) -> Result<U256, MoeLbtMathError> {
    let reserve_y = packed_uint128_math::decode_y(reserves);
    let amount_y = balance_y - U256::from(reserve_y);
    Ok(packed_uint128_math::encode(0, safe128(amount_y)?))
}

pub fn combine(x: U256, y: U256) -> Result<U256, MoeLbtMathError> {
    let a = packed_uint128_math::decode_x(x);
    let b = packed_uint128_math::decode_y(y);
    Ok(packed_uint128_math::encode(a, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_liquidity() {
        let amounts = packed_uint128_math::encode(10, 20);
        let liq = get_liquidity(amounts, U256::from(5)).unwrap();
        assert!(liq > U256::ZERO);
    }
}

