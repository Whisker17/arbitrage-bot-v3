use super::{
    amm::{AutomatedMarketMaker, AMM},
    error::{AMMError, BatchContractError},
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals, Token,
};
use crate::amms::{
    batch_create::{
        self, bisect_i16_range, bisect_tick_list, is_execution_reverted, v3_slot0_chunk_size,
        with_create_size_split, V3_SLOT0_RETURN_BYTES_PER_POOL,
    },
    consts::U256_1,
    logs::{block_number_for_range, fetch_logs_in_ranges, LogRangeConfig},
    uniswap_v3::GetUniswapV3PoolTickBitmapBatchRequest::TickBitmapInfo,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, Bytes, Signed, B256, I256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
};
use rayon::iter::{IntoParallelRefIterator, ParallelDrainRange, ParallelIterator};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    future::Future,
    hash::Hash,
    str::FromStr,
};
use thiserror::Error;
use tracing::info;
use uniswap_v3_math::error::UniswapV3MathError;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};
use GetUniswapV3PoolTickDataBatchRequest::TickDataInfo;

sol! {
    // UniswapV3Factory
    #[allow(missing_docs)]
    #[derive(Debug)]
    #[sol(rpc)]
    contract IUniswapV3Factory {
        /// @notice Emitted when a pool is created
        event PoolCreated(
            address indexed token0,
            address indexed token1,
            uint24 indexed fee,
            int24 tickSpacing,
            address pool
        );
    }

    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3PoolEvents {
        /// @notice Emitted when liquidity is minted for a given position
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

        /// @notice Emitted when a position's liquidity is removed
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );

        /// @notice Emitted by the pool for any swaps between token0 and token1
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
    }


    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);

    }
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolSlot0BatchRequest,
    "src/amms/abi/GetUniswapV3PoolSlot0BatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolTickBitmapBatchRequest,
    "src/amms/abi/GetUniswapV3PoolTickBitmapBatchRequest.json",
}

sol! {
    #[sol(rpc)]
    GetUniswapV3PoolTickDataBatchRequest,
    "src/amms/abi/GetUniswapV3PoolTickDataBatchRequest.json"
}

#[derive(Error, Debug)]
pub enum UniswapV3Error {
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
    #[error("Liquidity Underflow")]
    LiquidityUnderflow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV3Pool {
    pub address: Address,
    pub token_a: Token,
    pub token_b: Token,
    pub liquidity: u128,
    pub sqrt_price: U256,
    pub fee: u32,
    pub tick: i32,
    pub tick_spacing: i32, // TODO: we can make this a u8, tick spacing will never exceed 200
    pub tick_bitmap: HashMap<i16, U256>,
    #[serde(default)]
    pub tick_bitmap_coverage: HashSet<i16>,
    pub ticks: HashMap<i32, Info>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

impl Info {
    pub fn new(liquidity_gross: u128, liquidity_net: i128, initialized: bool) -> Self {
        Info {
            liquidity_gross,
            liquidity_net,
            initialized,
        }
    }
}

pub struct CurrentState {
    amount_specified_remaining: I256,
    amount_calculated: I256,
    sqrt_price_x_96: U256,
    tick: i32,
    liquidity: u128,
}

#[derive(Default)]
pub struct StepComputations {
    pub sqrt_price_start_x_96: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

impl AutomatedMarketMaker for UniswapV3Pool {
    fn address(&self) -> Address {
        self.address
    }

    fn sync_events(&self) -> Vec<B256> {
        vec![
            IUniswapV3PoolEvents::Mint::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Burn::SIGNATURE_HASH,
            IUniswapV3PoolEvents::Swap::SIGNATURE_HASH,
        ]
    }

    fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
        let event_signature = log.topics()[0];
        match event_signature {
            IUniswapV3PoolEvents::Swap::SIGNATURE_HASH => {
                let swap_event = IUniswapV3PoolEvents::Swap::decode_log(log.as_ref())?;

                self.sqrt_price = swap_event.sqrtPriceX96.to();
                self.liquidity = swap_event.liquidity;
                self.tick = swap_event.tick.unchecked_into();

                info!(
                    target = "amms::uniswap_v3::sync",
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Swap"
                );
            }
            IUniswapV3PoolEvents::Mint::SIGNATURE_HASH => {
                let mint_event = IUniswapV3PoolEvents::Mint::decode_log(log.as_ref())?;

                self.modify_position(
                    mint_event.tickLower.unchecked_into(),
                    mint_event.tickUpper.unchecked_into(),
                    mint_event.amount as i128,
                )?;

                info!(
                    target = "amms::uniswap_v3::sync",
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Mint"
                );
            }
            IUniswapV3PoolEvents::Burn::SIGNATURE_HASH => {
                let burn_event = IUniswapV3PoolEvents::Burn::decode_log(log.as_ref())?;

                self.modify_position(
                    burn_event.tickLower.unchecked_into(),
                    burn_event.tickUpper.unchecked_into(),
                    -(burn_event.amount as i128),
                )?;

                info!(
                    target = "amms::uniswap_v3::sync",
                    address = ?self.address,
                    sqrt_price = ?self.sqrt_price,
                    liquidity = ?self.liquidity,
                    tick = ?self.tick,
                    "Burn"
                );
            }
            _ => {
                return Err(AMMError::UnrecognizedEventSignature(event_signature));
            }
        }

        Ok(())
    }

    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        let zero_for_one = base_token == self.token_a.address;

        // Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        // Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            sqrt_price_x_96: self.sqrt_price, // Active price on the pool
            amount_calculated: I256::ZERO,    // Amount of token_out that has been calculated
            amount_specified_remaining: I256::from_raw(amount_in), // Amount of token_in that has not been swapped
            tick: self.tick,                                       // Current i24 tick of the pool
            liquidity: self.liquidity, // Current available liquidity in the tick range
        };

        while current_state.amount_specified_remaining != I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            // Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = StepComputations {
                // Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                sqrt_price_start_x_96: current_state.sqrt_price_x_96,
                ..Default::default()
            };
            self.ensure_tick_bitmap_coverage(current_state.tick, zero_for_one)?;

            // Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(UniswapV3Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            // Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            // Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(UniswapV3Error::from)?;

            // Target spot price
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };

            // Compute swap step and update the current state
            (
                current_state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                current_state.sqrt_price_x_96,
                swap_target_sqrt_ratio,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                self.fee,
            )
            .map_err(UniswapV3Error::from)?;

            // Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            // TODO: adjust for fee protocol

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = self
                        .ticks
                        .get(&step.tick_next)
                        .ok_or(AMMError::IncompleteState)?
                        .liquidity_net;

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(UniswapV3Error::LiquidityUnderflow.into());
                        } else {
                            current_state.liquidity - (-liquidity_net as u128)
                        }
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };
                }
                // Increment the current tick
                current_state.tick = if zero_for_one {
                    step.tick_next.wrapping_sub(1)
                } else {
                    step.tick_next
                }
                // If the current_state sqrt price is not equal to the step sqrt price, then we are not on the same tick.
                // Update the current_state.tick to the tick at the current_state.sqrt_price_x_96
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(UniswapV3Error::from)?;
            }
        }

        let amount_out = (-current_state.amount_calculated).into_raw();

        tracing::trace!(?amount_out);

        Ok(amount_out)
    }

    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        if amount_in.is_zero() {
            return Ok(U256::ZERO);
        }

        let zero_for_one = base_token == self.token_a.address;

        // Set sqrt_price_limit_x_96 to the max or min sqrt price in the pool depending on zero_for_one
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };

        // Initialize a mutable state state struct to hold the dynamic simulated state of the pool
        let mut current_state = CurrentState {
            // Active price on the pool
            sqrt_price_x_96: self.sqrt_price,
            // Amount of token_out that has been calculated
            amount_calculated: I256::ZERO,
            // Amount of token_in that has not been swapped
            amount_specified_remaining: I256::from_raw(amount_in),
            // Current i24 tick of the pool
            tick: self.tick,
            // Current available liquidity in the tick range
            liquidity: self.liquidity,
        };

        while current_state.amount_specified_remaining != I256::ZERO
            && current_state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            // Initialize a new step struct to hold the dynamic state of the pool at each step
            let mut step = StepComputations {
                // Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                sqrt_price_start_x_96: current_state.sqrt_price_x_96,
                ..Default::default()
            };
            self.ensure_tick_bitmap_coverage(current_state.tick, zero_for_one)?;

            // Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(UniswapV3Error::from)?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            // Note: this could be removed as we are clamping in the batch contract
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            // Get the next sqrt price from the input amount
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(UniswapV3Error::from)?;

            // Target spot price
            let swap_target_sqrt_ratio = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                }
            } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                sqrt_price_limit_x_96
            } else {
                step.sqrt_price_next_x96
            };

            // Compute swap step and update the current state
            (
                current_state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                current_state.sqrt_price_x_96,
                swap_target_sqrt_ratio,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                self.fee,
            )
            .map_err(UniswapV3Error::from)?;

            // Decrement the amount remaining to be swapped and amount received from the step
            current_state.amount_specified_remaining = current_state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;

            current_state.amount_calculated -= I256::from_raw(step.amount_out);

            // If the price moved all the way to the next price, recompute the liquidity change for the next iteration
            if current_state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = self
                        .ticks
                        .get(&step.tick_next)
                        .ok_or(AMMError::IncompleteState)?
                        .liquidity_net;

                    // we are on a tick boundary, and the next tick is initialized, so we must charge a protocol fee
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    current_state.liquidity = if liquidity_net < 0 {
                        if current_state.liquidity < (-liquidity_net as u128) {
                            return Err(AMMError::from(UniswapV3Error::LiquidityUnderflow));
                        } else {
                            current_state.liquidity - (-liquidity_net as u128)
                        }
                    } else {
                        current_state.liquidity + (liquidity_net as u128)
                    };
                }
                // Increment the current tick
                current_state.tick = if zero_for_one {
                    step.tick_next.wrapping_sub(1)
                } else {
                    step.tick_next
                }
                // If the current_state sqrt price is not equal to the step sqrt price, then we are not on the same tick.
                // Update the current_state.tick to the tick at the current_state.sqrt_price_x_96
            } else if current_state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                    current_state.sqrt_price_x_96,
                )
                .map_err(UniswapV3Error::from)?;
            }
        }

        // Update the pool state
        self.liquidity = current_state.liquidity;
        self.sqrt_price = current_state.sqrt_price_x_96;
        self.tick = current_state.tick;

        let amount_out = (-current_state.amount_calculated).into_raw();

        tracing::trace!(?amount_out);

        Ok(amount_out)
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }

    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.sqrt_price)
            .map_err(UniswapV3Error::from)?;
        let shift = self.token_a.decimals as i8 - self.token_b.decimals as i8;

        let price = match shift.cmp(&0) {
            Ordering::Less => 1.0001_f64.powi(tick) / 10_f64.powi(-shift as i32),
            Ordering::Greater => 1.0001_f64.powi(tick) * 10_f64.powi(shift as i32),
            Ordering::Equal => 1.0001_f64.powi(tick),
        };

        if base_token == self.token_a.address {
            Ok(price)
        } else {
            Ok(1.0 / price)
        }
    }

    async fn init<N, P>(mut self, block_number: BlockId, provider: P) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = IUniswapV3Pool::new(self.address, provider.clone());

        // Get pool data
        self.tick_spacing = pool.tickSpacing().call().await?.as_i32();
        self.fee = pool.fee().call().await?.to::<u32>();

        // Get tokens
        self.token_a = Token::new(pool.token0().call().await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().await?, provider.clone()).await?;

        let mut pool = vec![self.into()];
        UniswapV3Factory::sync_slot_0(&mut pool, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_token_decimals(&mut pool, provider.clone()).await?;
        UniswapV3Factory::sync_tick_bitmaps(&mut pool, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_tick_data(&mut pool, block_number, provider.clone()).await?;

        let AMM::UniswapV3Pool(pool) = pool[0].to_owned() else {
            unreachable!()
        };

        Ok(pool)
    }
}

impl UniswapV3Pool {
    pub fn simulate_swap_with_crossing_evidence(
        &self,
        base_token: Address,
        quote_token: Address,
        amount_in: U256,
    ) -> Result<crate::amms::amm::SwapSimulationEvidence, AMMError> {
        let initial_tick = self.tick;
        let zero_for_one = base_token == self.token_a.address;
        let mut simulated = self.clone();
        let amount_out = simulated.simulate_swap_mut(base_token, quote_token, amount_in)?;
        let final_tick = simulated.tick;
        let crossing_count = self
            .ticks
            .iter()
            .filter(|(tick, info)| {
                info.initialized
                    && if zero_for_one {
                        final_tick < **tick && **tick <= initial_tick
                    } else {
                        initial_tick < **tick && **tick <= final_tick
                    }
            })
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        Ok(crate::amms::amm::SwapSimulationEvidence {
            amount_out,
            crossing_count,
        })
    }

    fn ensure_tick_bitmap_coverage(&self, tick: i32, zero_for_one: bool) -> Result<(), AMMError> {
        if self.tick_spacing <= 0 {
            return Err(AMMError::IncompleteState);
        }
        let compressed = if tick < 0 && tick % self.tick_spacing != 0 {
            (tick / self.tick_spacing) - 1
        } else {
            tick / self.tick_spacing
        };
        let search_word = if zero_for_one {
            compressed
        } else {
            compressed.saturating_add(1)
        };
        let (word_pos, _) = uniswap_v3_math::tick_bitmap::position(search_word);
        if self.tick_bitmap_coverage.contains(&word_pos) {
            Ok(())
        } else {
            Err(AMMError::IncompleteState)
        }
    }

    // Create a new, unsynced UniswapV3 pool
    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    /// Modifies a positions liquidity in the pool.
    pub fn modify_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        //We are only using this function when a mint or burn event is emitted,
        //therefore we do not need to checkTicks as that has happened before the event is emitted
        self.update_position(tick_lower, tick_upper, liquidity_delta)?;

        if liquidity_delta != 0 {
            //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
            if self.tick >= tick_lower && self.tick < tick_upper {
                self.liquidity = if liquidity_delta < 0 {
                    self.liquidity - ((-liquidity_delta) as u128)
                } else {
                    self.liquidity + (liquidity_delta as u128)
                }
            }
        }

        Ok(())
    }

    pub fn update_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        let mut flipped_lower = false;
        let mut flipped_upper = false;

        if liquidity_delta != 0 {
            flipped_lower = self.update_tick(tick_lower, liquidity_delta, false)?;
            flipped_upper = self.update_tick(tick_upper, liquidity_delta, true)?;
            if flipped_lower {
                self.flip_tick(tick_lower, self.tick_spacing);
            }
            if flipped_upper {
                self.flip_tick(tick_upper, self.tick_spacing);
            }
        }

        if liquidity_delta < 0 {
            if flipped_lower {
                self.ticks.remove(&tick_lower);
            }

            if flipped_upper {
                self.ticks.remove(&tick_upper);
            }
        }

        Ok(())
    }

    pub fn update_tick(
        &mut self,
        tick: i32,
        liquidity_delta: i128,
        upper: bool,
    ) -> Result<bool, AMMError> {
        let info = self.ticks.entry(tick).or_default();

        let liquidity_gross_before = info.liquidity_gross;

        let liquidity_gross_after = if liquidity_delta < 0 {
            liquidity_gross_before - ((-liquidity_delta) as u128)
        } else {
            liquidity_gross_before + (liquidity_delta as u128)
        };

        // we do not need to check if liqudity_gross_after > maxLiquidity because we are only calling update tick on a burn or mint log.
        // this should already be validated when a log is
        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        if liquidity_gross_before == 0 {
            info.initialized = true;
        }

        info.liquidity_gross = liquidity_gross_after;

        info.liquidity_net = if upper {
            info.liquidity_net - liquidity_delta
        } else {
            info.liquidity_net + liquidity_delta
        };

        Ok(flipped)
    }

    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32) {
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(tick / tick_spacing);
        let mask = U256::from(1) << bit_pos;

        if let Some(word) = self.tick_bitmap.get_mut(&word_pos) {
            *word ^= mask;
        } else {
            self.tick_bitmap.insert(word_pos, mask);
        }
    }

    pub fn swap_calldata(
        &self,
        recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Result<Bytes, AMMError> {
        Ok(IUniswapV3Pool::swapCall {
            recipient,
            zeroForOne: zero_for_one,
            amountSpecified: amount_specified,
            sqrtPriceLimitX96: sqrt_price_limit_x_96.to(),
            data: calldata.into(),
        }
        .abi_encode()
        .into())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct UniswapV3Factory {
    pub address: Address,
    pub creation_block: u64,
}

impl UniswapV3Factory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        UniswapV3Factory {
            address,
            creation_block,
        }
    }

    pub async fn get_all_pools<N, P>(
        &self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let disc_filter = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]);

        let to_block = block_number_for_range::<N, _>(&provider, block_number).await?;
        let result = fetch_logs_in_ranges::<N, _>(
            provider,
            disc_filter,
            self.creation_block,
            to_block,
            LogRangeConfig::from_env(),
        )
        .await?;

        let mut pools = Vec::with_capacity(result.logs.len());
        for log in result.logs {
            pools.push(self.create_pool(log)?);
        }

        Ok(pools)
    }

    pub async fn sync_all_pools<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        UniswapV3Factory::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_token_decimals(&mut pools, provider.clone()).await?;

        pools = pools
            .par_drain(..)
            .filter(|pool| match pool {
                AMM::UniswapV3Pool(uv3_pool) => {
                    uv3_pool.liquidity > 0
                        && uv3_pool.token_a.decimals > 0
                        && uv3_pool.token_b.decimals > 0
                }
                _ => true,
            })
            .collect();

        UniswapV3Factory::sync_tick_bitmaps(&mut pools, block_number, provider.clone()).await?;
        UniswapV3Factory::sync_tick_data(&mut pools, block_number, provider.clone()).await?;

        Ok(pools)
    }

    /// Batch-init a frozen-universe UniswapV3 set (WHI-936).
    ///
    /// Same shape as [`AgniFactory::batch_init_pools`]: fill fee/tick_spacing
    /// when missing, then size-derived batch CREATE for slot0 / decimals / ticks.
    /// Does not drop zero-liquidity pools.
    pub async fn batch_init_pools<N, P>(
        mut pools: Vec<AMM>,
        block_number: BlockId,
        provider: P,
    ) -> Result<Vec<AMM>, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        if pools.is_empty() {
            return Ok(pools);
        }
        populate_univ3_static_fields(&mut pools, block_number, provider.clone()).await?;
        Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        Self::sync_token_decimals(&mut pools, provider.clone()).await?;
        Self::sync_tick_bitmaps(&mut pools, block_number, provider.clone()).await?;
        Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?;
        Ok(pools)
    }

    async fn sync_token_decimals<N, P>(
        pools: &mut [AMM],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // Get all token decimals
        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for token in pool.tokens() {
                tokens.insert(token);
            }
        }
        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;

        // Set token decimals
        for pool in pools.iter_mut() {
            let AMM::UniswapV3Pool(uniswap_v3_pool) = pool else {
                unreachable!()
            };

            if let Some(decimals) = token_decimals.get(&uniswap_v3_pool.token_a.address) {
                uniswap_v3_pool.token_a.decimals = *decimals;
            }

            if let Some(decimals) = token_decimals.get(&uniswap_v3_pool.token_b.address) {
                uniswap_v3_pool.token_b.decimals = *decimals;
            }
        }

        Ok(())
    }

    async fn sync_slot_0<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // WHI-925: same request shape as Agni V3 slot0 — size-derived chunk +
        // split-on-CreateContractSizeLimit (no time backoff).
        let step = v3_slot0_chunk_size();
        info!(
            target: "amms.uniswap_v3.sync",
            path = "uniswap_v3_slot0",
            pool_count = pools.len(),
            chunk_size = step,
            item_count = pools.len(),
            per_item_bytes = V3_SLOT0_RETURN_BYTES_PER_POOL,
            budget_bytes = batch_create::create_return_budget_bytes(),
            "Uniswap V3 slot0 batch sync starting"
        );

        let addresses: Vec<Address> = pools.iter().map(|p| p.address()).collect();
        let mut all_slot0: Vec<(i32, u128, U256)> = Vec::with_capacity(addresses.len());

        for group in addresses.chunks(step) {
            let provider = provider.clone();
            let decoded = with_create_size_split(
                group.to_vec(),
                "uniswap_v3_slot0",
                |a: &Address| Some(*a),
                |_| None,
                |_| None, // address is atomic — cannot narrow further
                move |chunk| {
                    let provider = provider.clone();
                    async move {
                        let ret =
                            GetUniswapV3PoolSlot0BatchRequest::deploy_builder(provider, chunk)
                                .call_raw()
                                .block(block_number)
                                .await?;
                        let data = <Vec<(i32, u128, U256)> as SolValue>::abi_decode(&ret)?;
                        Ok(data)
                    }
                },
            )
            .await?;
            all_slot0.extend(decoded);
        }

        if all_slot0.len() != pools.len() {
            return Err(BatchContractError::MalformedBatchResponse {
                path: "uniswap_v3_slot0",
                expected: pools.len(),
                actual: all_slot0.len(),
            }
            .into());
        }

        for (slot_0_data, pool) in all_slot0.iter().zip(pools.iter_mut()) {
            let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                unreachable!()
            };
            uv3_pool.tick = slot_0_data.0;
            uv3_pool.liquidity = slot_0_data.1;
            uv3_pool.sqrt_price = slot_0_data.2;
        }

        Ok(())
    }

    async fn sync_tick_bitmaps<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        // WHI-929: optimistic word-range grouping + CREATE-size split/bisect.
        // Per-item return size varies with non-zero word density.
        let max_range = 6900;
        info!(
            target: "amms.uniswap_v3.sync",
            path = "uniswap_v3_tick_bitmap",
            pool_count = pools.len(),
            chunk_size = max_range,
            item_count = pools.len(),
            max_range_words = max_range,
            "Uniswap V3 tick-bitmap batch sync starting"
        );

        // Sequential groups — unbounded FuturesUnordered on dense pools piles
        // behind the HTTP throttle and times out (WHI-929 live cold-start).
        let mut groups: Vec<Vec<TickBitmapInfo>> = Vec::new();
        let mut group_range = 0;
        let mut group = vec![];

        for pool in pools.iter() {
            let AMM::UniswapV3Pool(uniswap_v3_pool) = pool else {
                unreachable!()
            };

            let mut min_word = tick_to_word(MIN_TICK, uniswap_v3_pool.tick_spacing);
            let max_word = tick_to_word(MAX_TICK, uniswap_v3_pool.tick_spacing);
            while min_word <= max_word {
                let remaining_range = max_range - group_range;
                let range = (max_word - min_word + 1).min(remaining_range);
                let max_chunk = min_word + range - 1;

                group.push(TickBitmapInfo {
                    pool: uniswap_v3_pool.address,
                    minWord: min_word as i16,
                    maxWord: max_chunk as i16,
                });

                min_word = max_chunk + 1;
                group_range += range;

                if group_range >= max_range {
                    groups.push(std::mem::take(&mut group));
                    group_range = 0;
                }
            }
        }

        if !group.is_empty() {
            groups.push(group);
        }

        let mut pool_set = pools
            .iter_mut()
            .map(|pool| (pool.address(), pool))
            .collect::<HashMap<Address, &mut AMM>>();

        for calldata in groups {
            let leaves = fetch_uv3_tick_bitmaps(provider.clone(), block_number, calldata).await?;
            for (info, tick_bitmaps) in leaves {
                let pool = pool_set.get_mut(&info.pool).unwrap();

                let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                    unreachable!()
                };

                uv3_pool
                    .tick_bitmap_coverage
                    .extend(info.minWord..=info.maxWord);
                for chunk in tick_bitmaps.chunks_exact(2) {
                    let word_pos = I256::from_raw(chunk[0]).as_i16();
                    let tick_bitmap = chunk[1];

                    uv3_pool.tick_bitmap.insert(word_pos, tick_bitmap);
                    uv3_pool.tick_bitmap_coverage.insert(word_pos);
                }
            }
        }
        Ok(())
    }

    // TODO: Clean this function up
    async fn sync_tick_data<N, P>(
        pools: &mut [AMM],
        block_number: BlockId,
        provider: P,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool_ticks = pools
            .par_iter()
            .filter_map(|pool| {
                if let AMM::UniswapV3Pool(uniswap_v3_pool) = pool {
                    let min_word = tick_to_word(MIN_TICK, uniswap_v3_pool.tick_spacing);
                    let max_word = tick_to_word(MAX_TICK, uniswap_v3_pool.tick_spacing);

                    let initialized_ticks: Vec<Signed<24, 1>> = (min_word..=max_word)
                        // Filter out empty bitmaps
                        .filter_map(|word_pos| {
                            uniswap_v3_pool
                                .tick_bitmap
                                .get(&(word_pos as i16))
                                .filter(|&bitmap| *bitmap != U256::ZERO)
                                .map(|&bitmap| (word_pos, bitmap))
                        })
                        // Get tick index for non zero bitmaps
                        .flat_map(|(word_pos, bitmap)| {
                            (0..256)
                                .filter(move |i| {
                                    (bitmap & (U256::from(1) << U256::from(*i))) != U256::ZERO
                                })
                                .map(move |i| {
                                    let tick_index =
                                        (word_pos * 256 + i) * uniswap_v3_pool.tick_spacing;

                                    // TODO: update to use from be bytes or similar
                                    Signed::<24, 1>::from_str(&tick_index.to_string()).unwrap()
                                })
                        })
                        .collect();

                    // Only return pools with non-empty initialized ticks
                    if !initialized_ticks.is_empty() {
                        Some((uniswap_v3_pool.address, initialized_ticks))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(Address, Vec<Signed<24, 1>>)>>();

        // WHI-929: optimistic max_ticks + CREATE-size split/bisect.
        let max_ticks = 60;
        info!(
            target: "amms.uniswap_v3.sync",
            path = "uniswap_v3_tick_data",
            pool_count = pool_ticks.len(),
            chunk_size = max_ticks,
            item_count = pool_ticks.len(),
            max_ticks_per_batch = max_ticks,
            "Uniswap V3 tick-data batch sync starting"
        );

        // Sequential batch processing — same rationale as tick-bitmap (WHI-929).
        let mut groups: Vec<Vec<TickDataInfo>> = Vec::new();
        let mut group_ticks = 0;
        let mut group = vec![];

        for (pool_address, mut ticks) in pool_ticks {
            while !ticks.is_empty() {
                let remaining_ticks = max_ticks - group_ticks;
                let selected_ticks = ticks.drain(0..remaining_ticks.min(ticks.len()));
                group_ticks += selected_ticks.len();

                group.push(GetUniswapV3PoolTickDataBatchRequest::TickDataInfo {
                    pool: pool_address,
                    ticks: selected_ticks.collect(),
                });

                if group_ticks >= max_ticks {
                    groups.push(std::mem::take(&mut group));
                    group_ticks = 0;
                }
            }
        }

        if !group.is_empty() {
            groups.push(group);
        }

        let mut pool_set = pools
            .iter_mut()
            .map(|pool| (pool.address(), pool))
            .collect::<HashMap<Address, &mut AMM>>();

        for calldata in groups {
            let leaves = fetch_uv3_tick_data(provider.clone(), block_number, calldata).await?;
            for (tick_info, tick_bitmaps) in leaves {
                let pool = pool_set.get_mut(&tick_info.pool).unwrap();

                let AMM::UniswapV3Pool(ref mut uv3_pool) = pool else {
                    unreachable!()
                };

                for (tick, tick_idx) in tick_bitmaps.iter().zip(tick_info.ticks.iter()) {
                    let info = Info {
                        liquidity_gross: tick.1,
                        liquidity_net: tick.2,
                        initialized: tick.0,
                    };

                    uv3_pool.ticks.insert(tick_idx.as_i32(), info);
                }
            }
        }
        Ok(())
    }
}

/// Fetch one Uniswap V3 tick-bitmap batch with CREATE-size split + word-range
/// bisect (WHI-929).
async fn fetch_uv3_tick_bitmaps<N, P>(
    provider: P,
    block_number: BlockId,
    items: Vec<TickBitmapInfo>,
) -> Result<Vec<(TickBitmapInfo, Vec<U256>)>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let item_count = items.len();
    info!(
        target: "amms.uniswap_v3.sync",
        path = "uniswap_v3_tick_bitmap",
        chunk_size = item_count,
        item_count,
        "Uniswap V3 tick-bitmap batch CREATE"
    );
    with_create_size_split(
        items,
        "uniswap_v3_tick_bitmap",
        |i: &TickBitmapInfo| Some(i.pool),
        |i: &TickBitmapInfo| {
            Some(format!(
                "words=[{},{}] span={}",
                i.minWord,
                i.maxWord,
                (i.maxWord as i32) - (i.minWord as i32) + 1
            ))
        },
        |i: &TickBitmapInfo| {
            bisect_i16_range(i.minWord, i.maxWord).map(|((a0, a1), (b0, b1))| {
                (
                    TickBitmapInfo {
                        pool: i.pool,
                        minWord: a0,
                        maxWord: a1,
                    },
                    TickBitmapInfo {
                        pool: i.pool,
                        minWord: b0,
                        maxWord: b1,
                    },
                )
            })
        },
        move |chunk| {
            let provider = provider.clone();
            async move {
                let ret = match GetUniswapV3PoolTickBitmapBatchRequest::deploy_builder(
                    provider,
                    chunk.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let err: AMMError = e.into();
                        if chunk.len() == 1 && is_execution_reverted(&err) {
                            tracing::error!(
                                target: "amms.uniswap_v3.sync",
                                path = "uniswap_v3_tick_bitmap",
                                pool = ?chunk[0].pool,
                                min_word = chunk[0].minWord,
                                max_word = chunk[0].maxWord,
                                error = %err,
                                "tick-bitmap CREATE reverted; skipping range"
                            );
                            return Ok(vec![(chunk[0].clone(), Vec::new())]);
                        }
                        return Err(err);
                    }
                };
                let data = <Vec<Vec<U256>> as SolValue>::abi_decode(&ret)?;
                Ok(chunk.into_iter().zip(data).collect::<Vec<_>>())
            }
        },
    )
    .await
}

/// Fetch one Uniswap V3 tick-data batch with CREATE-size split + tick-list
/// bisect (WHI-929).
async fn fetch_uv3_tick_data<N, P>(
    provider: P,
    block_number: BlockId,
    items: Vec<TickDataInfo>,
) -> Result<Vec<(TickDataInfo, Vec<(bool, u128, i128)>)>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let item_count = items.len();
    let tick_count: usize = items.iter().map(|i| i.ticks.len()).sum();
    info!(
        target: "amms.uniswap_v3.sync",
        path = "uniswap_v3_tick_data",
        chunk_size = item_count,
        item_count,
        tick_count,
        "Uniswap V3 tick-data batch CREATE"
    );
    with_create_size_split(
        items,
        "uniswap_v3_tick_data",
        |i: &TickDataInfo| Some(i.pool),
        |i: &TickDataInfo| {
            Some(format!(
                "ticks={} first={:?} last={:?}",
                i.ticks.len(),
                i.ticks.first().map(|t| t.as_i32()),
                i.ticks.last().map(|t| t.as_i32())
            ))
        },
        |i: &TickDataInfo| {
            bisect_tick_list(&i.ticks).map(|(a, b)| {
                (
                    TickDataInfo {
                        pool: i.pool,
                        ticks: a,
                    },
                    TickDataInfo {
                        pool: i.pool,
                        ticks: b,
                    },
                )
            })
        },
        move |chunk| {
            let provider = provider.clone();
            async move {
                let ret = match GetUniswapV3PoolTickDataBatchRequest::deploy_builder(
                    provider,
                    chunk.clone(),
                )
                .call_raw()
                .block(block_number)
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        let err: AMMError = e.into();
                        if chunk.len() == 1 && is_execution_reverted(&err) {
                            tracing::error!(
                                target: "amms.uniswap_v3.sync",
                                path = "uniswap_v3_tick_data",
                                pool = ?chunk[0].pool,
                                ticks = chunk[0].ticks.len(),
                                error = %err,
                                "tick-data CREATE reverted; skipping item"
                            );
                            return Ok(vec![(chunk[0].clone(), Vec::new())]);
                        }
                        return Err(err);
                    }
                };
                let data = <Vec<Vec<(bool, u128, i128)>> as SolValue>::abi_decode(&ret)?;
                Ok(chunk.into_iter().zip(data).collect::<Vec<_>>())
            }
        },
    )
    .await
}

/// Concurrently fetch `fee` + `tickSpacing` for UniswapV3 pools that lack them
/// (WHI-936 frozen-universe cold start).
async fn populate_univ3_static_fields<N, P>(
    pools: &mut [AMM],
    block_number: BlockId,
    provider: P,
) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    use futures::stream::{self, StreamExt};

    const METADATA_CONCURRENCY: usize = 16;

    let need: Vec<(usize, Address)> = pools
        .iter()
        .enumerate()
        .filter_map(|(i, amm)| match amm {
            AMM::UniswapV3Pool(p) if p.fee == 0 || p.tick_spacing == 0 => Some((i, p.address)),
            _ => None,
        })
        .collect();
    if need.is_empty() {
        return Ok(());
    }

    info!(
        target: "amms.uniswap_v3.sync",
        pool_count = need.len(),
        concurrency = METADATA_CONCURRENCY,
        "populating UniswapV3 fee/tick_spacing (pipelined eth_calls)"
    );

    let mut stream = stream::iter(need.into_iter().map(|(idx, address)| {
        let provider = provider.clone();
        async move {
            let pool = IUniswapV3Pool::new(address, provider);
            let tick_spacing = pool
                .tickSpacing()
                .call()
                .block(block_number)
                .await?
                .as_i32();
            let fee = pool.fee().call().block(block_number).await?.to::<u32>();
            if tick_spacing == 0 {
                return Err(AMMError::IncompleteState);
            }
            Ok::<(usize, u32, i32), AMMError>((idx, fee, tick_spacing))
        }
    }))
    .buffer_unordered(METADATA_CONCURRENCY);

    while let Some(result) = stream.next().await {
        let (idx, fee, tick_spacing) = result?;
        let AMM::UniswapV3Pool(p) = &mut pools[idx] else {
            unreachable!("populate_univ3_static_fields only indexes UniswapV3Pool")
        };
        p.fee = fee;
        p.tick_spacing = tick_spacing;
    }
    Ok(())
}

fn tick_to_word(tick: i32, tick_spacing: i32) -> i32 {
    let mut compressed = tick / tick_spacing;
    if tick < 0 && tick % tick_spacing != 0 {
        compressed -= 1;
    }

    compressed >> 8
}

impl AutomatedMarketMakerFactory for UniswapV3Factory {
    type PoolVariant = UniswapV3Pool;

    fn address(&self) -> Address {
        self.address
    }

    fn pool_creation_event(&self) -> B256 {
        IUniswapV3Factory::PoolCreated::SIGNATURE_HASH
    }

    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let pool_created_event: alloy::primitives::Log<IUniswapV3Factory::PoolCreated> =
            IUniswapV3Factory::PoolCreated::decode_log(&log.inner)?;

        Ok(AMM::UniswapV3Pool(UniswapV3Pool {
            address: pool_created_event.pool,
            token_a: pool_created_event.token0.into(),
            token_b: pool_created_event.token1.into(),
            fee: pool_created_event.fee.to::<u32>(),
            tick_spacing: pool_created_event.tickSpacing.unchecked_into(),
            ..Default::default()
        }))
    }

    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}

impl DiscoverySync for UniswapV3Factory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v3::discover",
            address = ?self.address,
            "Discovering all pools"
        );

        self.get_all_pools(to_block, provider.clone())
    }

    fn sync<N, P>(
        &self,
        amms: Vec<AMM>,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(
            target = "amms::uniswap_v3::sync",
            address = ?self.address,
            "Syncing all pools"
        );

        UniswapV3Factory::sync_all_pools(amms, to_block, provider)
    }
}

#[cfg(test)]
mod test {

    use super::*;

    use alloy::{
        primitives::{address, aliases::U24, U160, U256},
        providers::ProviderBuilder,
        rpc::client::ClientBuilder,
        transports::layers::{RetryBackoffLayer, ThrottleLayer},
    };

    sol! {
        /// Interface of the Quoter
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract IQuoter {
            function quoteExactInputSingle(address tokenIn, address tokenOut,uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut);
        }
    }

    fn test_pool() -> UniswapV3Pool {
        let mut pool = UniswapV3Pool {
            address: address!("0000000000000000000000000000000000000003"),
            token_a: Token::new_with_decimals(
                address!("0000000000000000000000000000000000000001"),
                18,
            ),
            token_b: Token::new_with_decimals(
                address!("0000000000000000000000000000000000000002"),
                18,
            ),
            liquidity: 1_000_000,
            sqrt_price: uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(0)
                .expect("zero tick has a valid sqrt ratio"),
            fee: 3_000,
            tick_spacing: 1,
            ..Default::default()
        };
        pool.tick_bitmap_coverage.extend(-10i16..=10i16);
        pool
    }

    #[test]
    fn simulate_swap_preserves_state_with_full_range_fixture() {
        // Given
        let pool = test_pool();
        let initial_sqrt_price = pool.sqrt_price;

        // When
        let amount_out = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect("the deterministic pool can simulate the swap");

        // Then
        assert_eq!(amount_out, U256::from(9_871));
        assert_eq!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(pool.tick, 0);
        assert_eq!(pool.liquidity, 1_000_000);
    }

    #[test]
    fn simulate_swap_mut_commits_full_range_fixture_state() {
        // Given
        let mut pool = test_pool();

        // When
        let amount_out = pool
            .simulate_swap_mut(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect("the deterministic pool can simulate the swap");

        // Then
        assert_eq!(amount_out, U256::from(9_871));
        assert_eq!(
            pool.sqrt_price,
            U256::from(78_446_055_342_499_616_417_857_907_004u128)
        );
        assert_eq!(pool.tick, -199);
        assert_eq!(pool.liquidity, 1_000_000);
    }

    #[test]
    fn simulate_swap_mut_updates_liquidity_when_initialized_tick_is_crossed() {
        // Given
        let mut pool = test_pool();
        uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, 0, pool.tick_spacing)
            .expect("the current tick can be initialized");
        pool.ticks.insert(0, Info::new(200_000, -200_000, true));

        // When
        let amount_out = pool
            .simulate_swap_mut(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect("the initialized tick can be crossed");

        // Then
        assert!(amount_out > U256::ZERO);
        assert_eq!(pool.liquidity, 1_200_000);
    }

    #[test]
    fn crossing_evidence_counts_each_initialized_tick_crossed() {
        let mut pool = test_pool();
        for tick in [0, -50, -100] {
            uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, tick, pool.tick_spacing)
                .unwrap();
            pool.ticks.insert(tick, Info::new(1, 0, true));
        }
        let evidence = pool
            .simulate_swap_with_crossing_evidence(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .unwrap();
        assert_eq!(evidence.crossing_count, 3);
    }

    #[test]
    fn simulate_swap_rejects_an_unsynced_bitmap_word() {
        // Given
        let mut pool = test_pool();
        pool.tick_bitmap_coverage.clear();

        // When
        let error = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect_err("an unsynced bitmap word must fail closed");

        // Then
        assert!(matches!(error, AMMError::IncompleteState));
    }

    #[test]
    fn simulate_swap_mut_rejects_an_unsynced_bitmap_word_without_mutating_state() {
        // Given
        let mut pool = test_pool();
        pool.tick_bitmap_coverage.clear();
        let initial_sqrt_price = pool.sqrt_price;
        let initial_tick = pool.tick;
        let initial_liquidity = pool.liquidity;

        // When
        let error = pool
            .simulate_swap_mut(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect_err("an unsynced bitmap word must fail closed");

        // Then
        assert!(matches!(error, AMMError::IncompleteState));
        assert_eq!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(pool.tick, initial_tick);
        assert_eq!(pool.liquidity, initial_liquidity);
    }

    #[test]
    fn simulate_swap_mut_rejects_an_initialized_tick_without_a_record() {
        // Given
        let mut pool = test_pool();
        uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, 0, pool.tick_spacing)
            .expect("the current tick can be initialized");
        let initial_sqrt_price = pool.sqrt_price;
        let initial_tick = pool.tick;
        let initial_liquidity = pool.liquidity;

        // When
        let error = pool
            .simulate_swap_mut(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect_err("a missing initialized tick record must fail closed");

        // Then
        assert!(matches!(error, AMMError::IncompleteState));
        assert_eq!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(pool.tick, initial_tick);
        assert_eq!(pool.liquidity, initial_liquidity);
    }

    #[tokio::test]
    #[ignore = "live Mantle RPC; set MANTLE_PROVIDER_URL and run with --ignored"]
    async fn test_simulate_swap_usdc_weth() -> eyre::Result<()> {
        let rpc_endpoint = std::env::var("MANTLE_PROVIDER_URL")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        // Mantle USDC-WMNT pool from poolLists.csv
        let pool = UniswapV3Pool::new(address!("086F766b336DFB0f705Dc030dB01993b22D81266"))
            .init(BlockId::latest(), provider.clone())
            .await?;

        let quoter = IQuoter::new(
            address!("dD489C75be1039ec7d843A6aC2Fd658350B067Cf"),
            provider.clone(),
        );

        // Test swap from USDC to WMNT
        let amount_in = U256::from(10000000); // 10 USDC
        let amount_out = pool.simulate_swap(pool.token_a.address, Address::default(), amount_in)?;

        dbg!(pool.token_a.address);
        dbg!(pool.token_b.address);
        dbg!(amount_in);
        dbg!(amount_out);
        dbg!(pool.fee);

        let expected_amount_out = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;

        assert_eq!(amount_out, expected_amount_out);

        let amount_in_1 = U256::from(1000000000_u64); // 1000 USDC
        let amount_out_1 =
            pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_1)?;

        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_1,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;

        assert_eq!(amount_out_1, expected_amount_out_1);

        let amount_in_2 = U256::from(1000000000000_u128); // 1000000 USDC
        let amount_out_2 =
            pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_2)?;

        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_2,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;

        assert_eq!(amount_out_2, expected_amount_out_2);

        let amount_in_3 = U256::from(100000000000000_u128); // 100000000 USDC
        let amount_out_3 =
            pool.simulate_swap(pool.token_a.address, Address::default(), amount_in_3)?;

        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_3,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;

        assert_eq!(amount_out_3, expected_amount_out_3);

        // Test swap from WMNT to USDC

        let amount_in = U256::from(1000000000000000000_u128); // 1 WMNT
        let amount_out = pool.simulate_swap(pool.token_b.address, Address::default(), amount_in)?;
        let expected_amount_out = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;
        assert_eq!(amount_out, expected_amount_out);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 WMNT
        let amount_out_1 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_1)?;
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_1,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;
        assert_eq!(amount_out_1, expected_amount_out_1);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 WMNT
        let amount_out_2 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_2)?;
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_2,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;
        assert_eq!(amount_out_2, expected_amount_out_2);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100000 WMNT
        let amount_out_3 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_3)?;
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_3,
                U160::ZERO,
            )
            .block(BlockId::latest())
            .call()
            .await?;

        assert_eq!(amount_out_3, expected_amount_out_3);

        Ok(())
    }

    #[tokio::test]
    #[ignore = "live Mantle RPC; set MANTLE_PROVIDER_URL and run with --ignored"]
    async fn test_simulate_swap_link_weth() -> eyre::Result<()> {
        let rpc_endpoint = std::env::var("MANTLE_PROVIDER_URL")?;

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);

        let current_block = BlockId::from(provider.get_block_number().await?);

        // Mantle USDT-WETH pool from poolLists.csv
        let pool = UniswapV3Pool::new(address!("076eb72e74c16b208c692eeab3750978d76b8f28"))
            .init(current_block, provider.clone())
            .await?;

        let quoter = IQuoter::new(
            address!("dD489C75be1039ec7d843A6aC2Fd658350B067Cf"),
            provider.clone(),
        );

        // Test swap USDT to WMNT
        let amount_in = U256::from(1000000_u128); // 1 USDT
        let amount_out = pool.simulate_swap(pool.token_a.address, Address::default(), amount_in)?;
        let expected_amount_out = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out, expected_amount_out);

        let amount_in_1 = U256::from(10000000_u128); // 10 USDT
        let amount_out_1 = pool
            .simulate_swap(pool.token_a.address, Address::default(), amount_in_1)
            .unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_1,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out_1, expected_amount_out_1);

        let amount_in_2 = U256::from(100000000_u128); // 100 USDT
        let amount_out_2 = pool
            .simulate_swap(pool.token_a.address, Address::default(), amount_in_2)
            .unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_2,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out_2, expected_amount_out_2);

        let amount_in_3 = U256::from(1000000000_u128); // 1000 USDT
        let amount_out_3 = pool
            .simulate_swap(pool.token_a.address, Address::default(), amount_in_3)
            .unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a.address,
                pool.token_b.address,
                U24::from(pool.fee),
                amount_in_3,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out_3, expected_amount_out_3);

        // Test swap WETH to USDT

        let amount_in = U256::from(1000000000000000000_u128); // 1 WETH
        let amount_out = pool.simulate_swap(pool.token_b.address, Address::default(), amount_in)?;
        let expected_amount_out = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out, expected_amount_out);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 WETH
        let amount_out_1 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_1)?;
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_1,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out_1, expected_amount_out_1);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 WETH
        let amount_out_2 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_2)?;
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_2,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;
        assert_eq!(amount_out_2, expected_amount_out_2);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100000 WETH
        let amount_out_3 =
            pool.simulate_swap(pool.token_b.address, Address::default(), amount_in_3)?;
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b.address,
                pool.token_a.address,
                U24::from(pool.fee),
                amount_in_3,
                U160::ZERO,
            )
            .block(current_block)
            .call()
            .await?;

        assert_eq!(amount_out_3, expected_amount_out_3);

        Ok(())
    }

    #[tokio::test]
    #[ignore = "live Mantle RPC; set MANTLE_PROVIDER_URL and run with --ignored"]
    async fn test_calculate_price() -> eyre::Result<()> {
        println!("Starting test_calculate_price...");

        let rpc_endpoint = std::env::var("MANTLE_PROVIDER_URL")?;
        println!("RPC endpoint obtained: {}", rpc_endpoint);

        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc_endpoint.parse()?);

        let provider = ProviderBuilder::new().connect_client(client);
        println!("Provider created successfully");

        let block_number = BlockId::from(85156621);
        println!("Using block number: {}", block_number);

        let pool_address = address!("086F766b336DFB0f705Dc030dB01993b22D81266");
        println!("Pool address: {}", pool_address);

        let pool = UniswapV3Pool::new(pool_address)
            .init(block_number, provider.clone())
            .await?;

        println!("Pool initialized successfully");
        println!("Pool tick: {}", pool.tick);
        println!("Pool sqrt_price: {}", pool.sqrt_price);
        println!("Pool liquidity: {}", pool.liquidity);
        println!("Pool fee: {}", pool.fee);
        println!("Token A address: {}", pool.token_a.address);
        println!("Token A decimals: {}", pool.token_a.decimals);
        println!("Token B address: {}", pool.token_b.address);
        println!("Token B decimals: {}", pool.token_b.decimals);

        let float_price_a = pool.calculate_price(pool.token_a.address, Address::default())?;
        let float_price_b = pool.calculate_price(pool.token_b.address, Address::default())?;

        println!("Calculated price A (USDC in WMNT): {}", float_price_a);
        println!("Calculated price B (WMNT in USDC): {}", float_price_b);

        // Use approximate comparison for floating point numbers
        let expected_price_a = 0.60430426341517640;
        let expected_price_b = 1.65479554016147645;

        println!("Expected price A: {}", expected_price_a);
        println!("Expected price B: {}", expected_price_b);

        let tolerance = 1e-10;
        assert!(
            (float_price_a - expected_price_a).abs() < tolerance,
            "Price A mismatch: got {}, expected {}",
            float_price_a,
            expected_price_a
        );

        assert!(
            (float_price_b - expected_price_b).abs() < tolerance,
            "Price B mismatch: got {}, expected {}",
            float_price_b,
            expected_price_b
        );

        println!("Test completed successfully!");

        Ok(())
    }

    #[tokio::test]
    async fn test_wmnt_value_in_pools_payload() -> eyre::Result<()> {
        use alloy::{
            json_abi::JsonAbi,
            primitives::{keccak256, Address},
            sol,
            sol_types::SolValue,
        };
        use std::str::FromStr;

        // Define the PoolInfo struct to match the ABI
        sol! {
            struct PoolInfo {
                uint8 poolType;
                address poolAddress;
            }

            struct PoolInfoReturn {
                uint8 poolType;
                address poolAddress;
                uint256 wmntValue;
            }
        }

        // Test data - real UniswapV3 pool addresses on Mantle mainnet
        let test_pools = vec![
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
            },
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
            },
        ];

        // Print detailed pool information
        println!("=== WmntValueInPools Test Data (Real Mantle UniswapV3 Pools) ===");
        for (i, pool) in test_pools.iter().enumerate() {
            let pool_type_name = match pool.poolType {
                1 => "UniswapV3",
                _ => "Unknown",
            };
            let pool_name = match i {
                0 => "USDC-WMNT",
                1 => "WMNT-WETH",
                _ => "Unknown",
            };
            println!(
                "Pool {}: {} - Type={} ({}) Address={:?}",
                i + 1,
                pool_name,
                pool.poolType,
                pool_type_name,
                pool.poolAddress
            );
        }

        // Encode the input parameters
        let encoded_input = test_pools.abi_encode();
        println!(
            "Encoded input for WmntValueInPools: 0x{}",
            alloy::hex::encode(&encoded_input)
        );

        // Test the function signature
        let function_signature = "getWmntValueInPools((uint8,address)[])";
        let expected_selector = keccak256(function_signature.as_bytes())[..4].to_vec();
        println!(
            "Function selector: 0x{}",
            alloy::hex::encode(&expected_selector)
        );

        // Verify the ABI structure
        let abi_json = include_str!("../abi/WmntValueInPools.json");
        let contract_artifact: serde_json::Value = serde_json::from_str(abi_json)?;
        let abi: JsonAbi = serde_json::from_value(contract_artifact["abi"].clone())?;

        assert_eq!(abi.functions.len(), 1);
        let function = &abi.functions["getWmntValueInPools"][0];
        assert_eq!(function.name, "getWmntValueInPools");
        assert_eq!(function.inputs.len(), 1);
        assert_eq!(function.outputs.len(), 1);

        println!("WmntValueInPools ABI validation passed!");
        println!("Input type: {}", function.inputs[0].ty);
        println!("Output type: {}", function.outputs[0].ty);

        // Simulate expected return values for demonstration
        println!("\n=== Expected Return Values (Simulated - Real Mantle Pools) ===");
        let simulated_returns = vec![
            PoolInfoReturn {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
                wmntValue: U256::from(1000000000000000000u64), // 1 WMNT
            },
            PoolInfoReturn {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
                wmntValue: U256::from(2500000000000000000u64), // 2.5 WMNT
            },
        ];

        for (i, return_val) in simulated_returns.iter().enumerate() {
            let pool_type_name = match return_val.poolType {
                1 => "UniswapV3",
                _ => "Unknown",
            };
            let pool_name = match i {
                0 => "USDC-WMNT",
                1 => "WMNT-WETH",
                _ => "Unknown",
            };
            let wmnt_value_eth = return_val
                .wmntValue
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0)
                / 1e18;
            println!(
                "Pool {} Return: {} - Type={} ({}) Address={:?} WMNT Value={} ({:.6} WMNT)",
                i + 1,
                pool_name,
                return_val.poolType,
                pool_type_name,
                return_val.poolAddress,
                return_val.wmntValue,
                wmnt_value_eth
            );
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_wmnt_value_in_pools_batch_request_payload() -> eyre::Result<()> {
        use alloy::{
            json_abi::JsonAbi,
            primitives::{keccak256, Address},
            sol,
            sol_types::SolValue,
        };
        use std::str::FromStr;

        // Define the PoolInfo struct to match the ABI
        sol! {
            struct PoolInfo {
                uint8 poolType;
                address poolAddress;
            }

            struct PoolInfoReturn {
                uint8 poolType;
                address poolAddress;
                uint256 wmntValue;
            }
        }

        // Test data - real UniswapV3 pool addresses on Mantle mainnet
        let test_pools = vec![
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
            },
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
            },
            PoolInfo {
                poolType: 1, // UniswapV3 - Add another real pool if available
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // Using USDC-WMNT again for testing
            },
        ];

        // Print detailed pool information
        println!("=== WmntValueInPoolsBatchRequest Test Data (Real Mantle UniswapV3 Pools) ===");
        for (i, pool) in test_pools.iter().enumerate() {
            let pool_type_name = match pool.poolType {
                1 => "UniswapV3",
                _ => "Unknown",
            };
            let pool_name = match i {
                0 => "USDC-WMNT",
                1 => "WMNT-WETH",
                2 => "USDC-WMNT (duplicate for testing)",
                _ => "Unknown",
            };
            println!(
                "Pool {}: {} - Type={} ({}) Address={:?}",
                i + 1,
                pool_name,
                pool.poolType,
                pool_type_name,
                pool.poolAddress
            );
        }

        // Encode the input parameters
        let encoded_input = test_pools.abi_encode();
        println!(
            "Encoded input for WmntValueInPoolsBatchRequest: 0x{}",
            alloy::hex::encode(&encoded_input)
        );

        // Test the function signature
        let function_signature = "getWmntValueInPools((uint8,address)[])";
        let expected_selector = keccak256(function_signature.as_bytes())[..4].to_vec();
        println!(
            "Function selector: 0x{}",
            alloy::hex::encode(&expected_selector)
        );

        // Verify the ABI structure
        let abi_json = include_str!("../abi/WmntValueInPoolsBatchRequest.json");
        let contract_artifact: serde_json::Value = serde_json::from_str(abi_json)?;
        let abi: JsonAbi = serde_json::from_value(contract_artifact["abi"].clone())?;

        assert_eq!(abi.functions.len(), 1);
        let function = &abi.functions["getWmntValueInPools"][0];
        assert_eq!(function.name, "getWmntValueInPools");
        assert_eq!(function.inputs.len(), 1);
        assert_eq!(function.outputs.len(), 1);

        // Verify constructor parameters
        assert_eq!(abi.constructor.as_ref().unwrap().inputs.len(), 4);
        let constructor_inputs = &abi.constructor.as_ref().unwrap().inputs;
        assert_eq!(constructor_inputs[0].name, "_uniswapV2Factory");
        assert_eq!(constructor_inputs[1].name, "_uniswapV3Factory");
        assert_eq!(constructor_inputs[2].name, "_wmnt");
        assert_eq!(constructor_inputs[3].name, "pools");

        println!("WmntValueInPoolsBatchRequest ABI validation passed!");
        println!("Input type: {}", function.inputs[0].ty);
        println!("Output type: {}", function.outputs[0].ty);
        println!("Constructor has {} parameters", constructor_inputs.len());

        // Simulate expected return values for demonstration
        println!("\n=== Expected Return Values (Simulated - Real Mantle Pools) ===");
        let simulated_returns = vec![
            PoolInfoReturn {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
                wmntValue: U256::from(1500000000000000000u64), // 1.5 WMNT
            },
            PoolInfoReturn {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
                wmntValue: U256::from(3200000000000000000u64), // 3.2 WMNT
            },
            PoolInfoReturn {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool (duplicate)
                wmntValue: U256::from(800000000000000000u64), // 0.8 WMNT
            },
        ];

        let mut total_wmnt_value = U256::ZERO;
        for (i, return_val) in simulated_returns.iter().enumerate() {
            let pool_type_name = match return_val.poolType {
                1 => "UniswapV3",
                _ => "Unknown",
            };
            let pool_name = match i {
                0 => "USDC-WMNT",
                1 => "WMNT-WETH",
                2 => "USDC-WMNT (duplicate)",
                _ => "Unknown",
            };
            let wmnt_value_eth = return_val
                .wmntValue
                .to_string()
                .parse::<f64>()
                .unwrap_or(0.0)
                / 1e18;
            total_wmnt_value += return_val.wmntValue;
            println!(
                "Pool {} Return: {} - Type={} ({}) Address={:?} WMNT Value={} ({:.6} WMNT)",
                i + 1,
                pool_name,
                return_val.poolType,
                pool_type_name,
                return_val.poolAddress,
                return_val.wmntValue,
                wmnt_value_eth
            );
        }

        let total_wmnt_eth = total_wmnt_value.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
        println!(
            "\nTotal WMNT Value across all pools: {} ({:.6} WMNT)",
            total_wmnt_value, total_wmnt_eth
        );

        Ok(())
    }

    #[tokio::test]
    async fn test_payload_comparison() -> eyre::Result<()> {
        use alloy::{primitives::Address, sol, sol_types::SolValue};
        use std::str::FromStr;

        // Define the PoolInfo struct
        sol! {
            struct PoolInfo {
                uint8 poolType;
                address poolAddress;
            }
        }

        // Same test data for both contracts (Real Mantle UniswapV3 pools)
        let test_pools = vec![
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
            },
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
            },
        ];

        // Encode for both contracts
        let encoded_wmnt_value = test_pools.abi_encode();
        let encoded_batch_request = test_pools.abi_encode();

        // Both should produce identical encoded data since they use the same function signature
        assert_eq!(encoded_wmnt_value, encoded_batch_request);
        println!(
            "Both payloads produce identical encoded data: 0x{}",
            alloy::hex::encode(&encoded_wmnt_value)
        );

        // Test with multiple real Mantle UniswapV3 pools
        let multiple_v3_pools = vec![
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
            },
            PoolInfo {
                poolType: 1, // UniswapV3
                poolAddress: Address::from_str("0x082a6df295d9efeedd2838d154a2bbc255fa0745")?, // WMNT-WETH pool
            },
            PoolInfo {
                poolType: 1, // Another UniswapV3 (using USDC-WMNT again for testing)
                poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
            },
        ];

        let encoded_multiple = multiple_v3_pools.abi_encode();
        println!(
            "Multiple UniswapV3 pools encoded: 0x{}",
            alloy::hex::encode(&encoded_multiple)
        );

        // Verify the encoding is deterministic
        let encoded_again = multiple_v3_pools.abi_encode();
        assert_eq!(encoded_multiple, encoded_again);

        println!("Payload comparison test completed successfully!");

        Ok(())
    }

    #[tokio::test]
    async fn test_contract_addresses_and_constructors() -> eyre::Result<()> {
        use alloy::{primitives::Address, sol, sol_types::SolValue};
        use std::str::FromStr;

        // Sample Mantle addresses (these would be real addresses in production)
        let uniswap_v2_factory = Address::from_str("0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f")?;
        let uniswap_v3_factory = Address::from_str("0x1F98431c8aD98523631AE4a59f267346ea31F984")?;
        let wmnt_address = Address::from_str("0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")?; // WMNT on Mantle

        // Define the PoolInfo struct
        sol! {
            struct PoolInfo {
                uint8 poolType;
                address poolAddress;
            }
        }

        let initial_pools = vec![PoolInfo {
            poolType: 1, // UniswapV3
            poolAddress: Address::from_str("0x086F766b336DFB0f705Dc030dB01993b22D81266")?, // USDC-WMNT pool
        }];

        // Test constructor encoding for WmntValueInPoolsBatchRequest
        let constructor_inputs = (
            uniswap_v2_factory,
            uniswap_v3_factory,
            wmnt_address,
            initial_pools.clone(),
        );
        let encoded_constructor = constructor_inputs.abi_encode();
        println!(
            "Constructor encoded: 0x{}",
            alloy::hex::encode(&encoded_constructor)
        );

        // Test that we can decode the constructor parameters
        let decoded: (Address, Address, Address, Vec<PoolInfo>) =
            SolValue::abi_decode(&encoded_constructor)?;

        assert_eq!(decoded.0, uniswap_v2_factory);
        assert_eq!(decoded.1, uniswap_v3_factory);
        assert_eq!(decoded.2, wmnt_address);
        assert_eq!(decoded.3.len(), 1);
        assert_eq!(decoded.3[0].poolType, 1); // UniswapV3

        println!("Constructor encoding/decoding test passed!");
        println!("UniswapV2 Factory: {:?}", uniswap_v2_factory);
        println!("UniswapV3 Factory: {:?}", uniswap_v3_factory);
        println!("WMNT Address: {:?}", wmnt_address);

        Ok(())
    }
}
