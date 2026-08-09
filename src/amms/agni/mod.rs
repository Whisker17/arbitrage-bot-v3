// agni.rs

use super::{
    amm::{AutomatedMarketMaker, AMM},
    error::{AMMError, BatchContractError},
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals, Token,
};
use crate::amms::{
    agni::GetAgniPoolTickBitmapBatchRequest::TickBitmapInfo,
    batch_create::{
        self, bisect_i16_range, bisect_tick_list, is_execution_reverted, v3_slot0_chunk_size,
        with_create_size_split, V3_SLOT0_RETURN_BYTES_PER_POOL,
    },
    consts::U256_1,
    logs::{block_number_for_range, fetch_logs_in_ranges, LogRangeConfig},
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
use GetAgniPoolTickDataBatchRequest::TickDataInfo;

// Interfaces
sol! {
    #[allow(missing_docs)]
    #[derive(Debug)]
    #[sol(rpc)]
    contract IAgniFactory {
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
    contract IAgniPoolEvents {
        event Mint(
            address sender,
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        event Burn(
            address indexed owner,
            int24 indexed tickLower,
            int24 indexed tickUpper,
            uint128 amount,
            uint256 amount0,
            uint256 amount1
        );
        /// Agni-native Swap: two trailing protocol-fee fields after `tick`.
        /// Topic0 differs from the UniV3-family Swap (see [`IUniV3FamilyPoolEvents`]).
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick,
            uint128 protocolFeesToken0,
            uint128 protocolFeesToken1
        );
    }

    /// Standard UniV3 Swap (no protocol-fee fields). Emitted by drop-in Mantle
    /// venues loaded through `AgniPool` (Fluxion, FusionX, Butter, Uniswap V3
    /// Mantle, …). WHI-980: without this topic in `sync_events`, per-block
    /// `eth_getLogs` never matches those venues and the dirty set stays empty.
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniV3FamilyPoolEvents {
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
    contract IAgniPool {
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256);
        function tickSpacing() external view returns (int24);
        function fee() external view returns (uint24);
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

// Batch request ABIs
sol! { #[sol(rpc)] GetAgniPoolSlot0BatchRequest, "src/amms/abi/GetAgniPoolSlot0BatchRequest.json", }
sol! { #[sol(rpc)] GetAgniPoolTickBitmapBatchRequest, "src/amms/abi/GetAgniPoolTickBitmapBatchRequest.json", }
sol! { #[sol(rpc)] GetAgniPoolTickDataBatchRequest, "src/amms/abi/GetAgniPoolTickDataBatchRequest.json" }

#[derive(Error, Debug)]
pub enum AgniError {
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
    #[error("Liquidity Underflow")]
    LiquidityUnderflow,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgniPool {
    pub address: Address,
    pub token_a: Token,
    pub token_b: Token,
    pub liquidity: u128,
    pub sqrt_price: U256,
    pub fee: u32,
    pub tick: i32,
    pub tick_spacing: i32,
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
        Self {
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

struct SimulatedSwap {
    amount_out: U256,
    sqrt_price: U256,
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

impl AutomatedMarketMaker for AgniPool {
    fn address(&self) -> Address {
        self.address
    }
    fn sync_events(&self) -> Vec<B256> {
        // Mint/Burn topic0 is identical for Agni and UniV3-family pools.
        // Swap topic0 is not: Agni appends protocol-fee fields. Subscribe to both
        // so drop-in venues (Fluxion, FusionX, …) land in the dirty set (WHI-980).
        vec![
            IAgniPoolEvents::Mint::SIGNATURE_HASH,
            IAgniPoolEvents::Burn::SIGNATURE_HASH,
            IAgniPoolEvents::Swap::SIGNATURE_HASH,
            IUniV3FamilyPoolEvents::Swap::SIGNATURE_HASH,
        ]
    }
    fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
        let sig = log.topics()[0];
        match sig {
            s if s == IAgniPoolEvents::Swap::SIGNATURE_HASH => {
                let e = IAgniPoolEvents::Swap::decode_log(log.as_ref())?;
                self.apply_swap_state(e.sqrtPriceX96.to(), e.liquidity, e.tick.unchecked_into());
            }
            s if s == IUniV3FamilyPoolEvents::Swap::SIGNATURE_HASH => {
                let e = IUniV3FamilyPoolEvents::Swap::decode_log(log.as_ref())?;
                self.apply_swap_state(e.sqrtPriceX96.to(), e.liquidity, e.tick.unchecked_into());
            }
            s if s == IAgniPoolEvents::Mint::SIGNATURE_HASH => {
                let e = IAgniPoolEvents::Mint::decode_log(log.as_ref())?;
                self.modify_position(
                    e.tickLower.unchecked_into(),
                    e.tickUpper.unchecked_into(),
                    e.amount as i128,
                )?;
            }
            s if s == IAgniPoolEvents::Burn::SIGNATURE_HASH => {
                let e = IAgniPoolEvents::Burn::decode_log(log.as_ref())?;
                self.modify_position(
                    e.tickLower.unchecked_into(),
                    e.tickUpper.unchecked_into(),
                    -(e.amount as i128),
                )?;
            }
            _ => return Err(AMMError::UnrecognizedEventSignature(sig)),
        }
        Ok(())
    }
    fn simulate_swap(
        &self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        Ok(self
            .simulate_swap_with_state(base_token, amount_in)?
            .amount_out)
    }
    fn simulate_swap_mut(
        &mut self,
        base_token: Address,
        _quote_token: Address,
        amount_in: U256,
    ) -> Result<U256, AMMError> {
        let simulated_swap = self.simulate_swap_with_state(base_token, amount_in)?;
        self.sqrt_price = simulated_swap.sqrt_price;
        self.tick = simulated_swap.tick;
        self.liquidity = simulated_swap.liquidity;
        Ok(simulated_swap.amount_out)
    }
    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a.address, self.token_b.address]
    }
    fn calculate_price(&self, base_token: Address, _quote_token: Address) -> Result<f64, AMMError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.sqrt_price)
            .map_err(AgniError::from)?;
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
        let pool = IAgniPool::new(self.address, provider.clone());
        self.tick_spacing = pool.tickSpacing().call().await?.as_i32();
        self.fee = pool.fee().call().await?.to::<u32>();
        self.token_a = Token::new(pool.token0().call().await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().await?, provider.clone()).await?;
        let mut pool_vec = vec![self.into()];
        AgniFactory::sync_slot_0(&mut pool_vec, block_number, provider.clone()).await?;
        AgniFactory::sync_token_decimals(&mut pool_vec, provider.clone()).await?;
        AgniFactory::sync_tick_bitmaps(&mut pool_vec, block_number, provider.clone()).await?;
        AgniFactory::sync_tick_data(&mut pool_vec, block_number, provider.clone()).await?;
        let AMM::AgniPool(p) = pool_vec.remove(0) else {
            unreachable!()
        };
        Ok(p)
    }
}

impl AgniPool {
    /// Apply slot0 fields from a decoded Swap event (Agni-native or UniV3-family).
    fn apply_swap_state(&mut self, sqrt_price: U256, liquidity: u128, tick: i32) {
        self.sqrt_price = sqrt_price;
        self.liquidity = liquidity;
        self.tick = tick;
    }

    /// Whether this pool can be quoted after tick sync (WHI-938).
    ///
    /// A pool with non-zero tick-bitmap bits but an empty `ticks` map is the
    /// Cleopatra failure mode: the Agni **tick-data batch CREATE** reverted and
    /// the single-item skip path returned empty tick data. Such a pool must not
    /// sit in the graph and silently quote as zero-liquidity.
    ///
    /// Empty bitmap (no initialized ticks) is still quotable within the current
    /// tick using `liquidity` alone.
    pub fn is_tick_data_quotable(&self) -> bool {
        let has_initialized_bits = self.tick_bitmap.values().any(|b| *b != U256::ZERO);
        if has_initialized_bits && self.ticks.is_empty() {
            return false;
        }
        true
    }

    pub fn simulate_swap_with_crossing_evidence(
        &self,
        base_token: Address,
        amount_in: U256,
    ) -> Result<crate::amms::amm::SwapSimulationEvidence, AMMError> {
        let initial_tick = self.tick;
        let zero_for_one = base_token == self.token_a.address;
        let simulated = self.simulate_swap_with_state(base_token, amount_in)?;
        let crossing_count = self
            .ticks
            .iter()
            .filter(|(tick, info)| {
                info.initialized
                    && if zero_for_one {
                        simulated.tick < **tick && **tick <= initial_tick
                    } else {
                        initial_tick < **tick && **tick <= simulated.tick
                    }
            })
            .count()
            .try_into()
            .unwrap_or(u32::MAX);
        Ok(crate::amms::amm::SwapSimulationEvidence {
            amount_out: simulated.amount_out,
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

    fn simulate_swap_with_state(
        &self,
        base_token: Address,
        amount_in: U256,
    ) -> Result<SimulatedSwap, AMMError> {
        if amount_in.is_zero() {
            return Ok(SimulatedSwap {
                amount_out: U256::ZERO,
                sqrt_price: self.sqrt_price,
                tick: self.tick,
                liquidity: self.liquidity,
            });
        }

        let zero_for_one = base_token == self.token_a.address;
        let sqrt_price_limit_x_96 = if zero_for_one {
            MIN_SQRT_RATIO + U256_1
        } else {
            MAX_SQRT_RATIO - U256_1
        };
        let mut state = CurrentState {
            sqrt_price_x_96: self.sqrt_price,
            amount_calculated: I256::ZERO,
            amount_specified_remaining: I256::from_raw(amount_in),
            tick: self.tick,
            liquidity: self.liquidity,
        };
        while state.amount_specified_remaining != I256::ZERO
            && state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            let mut step = StepComputations {
                sqrt_price_start_x_96: state.sqrt_price_x_96,
                ..Default::default()
            };
            self.ensure_tick_bitmap_coverage(state.tick, zero_for_one)?;
            (step.tick_next, step.initialized) =
                uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )
                .map_err(AgniError::from)?;
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)
                    .map_err(AgniError::from)?;
            let target = if zero_for_one {
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
            (
                state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                state.sqrt_price_x_96,
                target,
                state.liquidity,
                state.amount_specified_remaining,
                self.fee,
            )
            .map_err(AgniError::from)?;
            state.amount_specified_remaining = state
                .amount_specified_remaining
                .overflowing_sub(I256::from_raw(
                    step.amount_in.overflowing_add(step.fee_amount).0,
                ))
                .0;
            state.amount_calculated -= I256::from_raw(step.amount_out);
            if state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    let mut liquidity_net = self
                        .ticks
                        .get(&step.tick_next)
                        .ok_or(AMMError::IncompleteState)?
                        .liquidity_net;
                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }
                    state.liquidity = if liquidity_net < 0 {
                        if state.liquidity < (-liquidity_net as u128) {
                            return Err(AgniError::LiquidityUnderflow.into());
                        }
                        state.liquidity - (-liquidity_net as u128)
                    } else {
                        state.liquidity + (liquidity_net as u128)
                    };
                }
                state.tick = if zero_for_one {
                    step.tick_next.wrapping_sub(1)
                } else {
                    step.tick_next
                };
            } else if state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                state.tick =
                    uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x_96)
                        .map_err(AgniError::from)?;
            }
        }
        Ok(SimulatedSwap {
            amount_out: (-state.amount_calculated).into_raw(),
            sqrt_price: state.sqrt_price_x_96,
            tick: state.tick,
            liquidity: state.liquidity,
        })
    }

    pub fn new(address: Address) -> Self {
        Self {
            address,
            ..Default::default()
        }
    }

    pub async fn init_basic<N, P>(
        mut self,
        block_number: BlockId,
        provider: P,
    ) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let pool = IAgniPool::new(self.address, provider.clone());
        self.tick_spacing = pool.tickSpacing().call().await?.as_i32();
        self.fee = pool.fee().call().await?.to::<u32>();
        self.token_a = Token::new(pool.token0().call().await?, provider.clone()).await?;
        self.token_b = Token::new(pool.token1().call().await?, provider.clone()).await?;

        let mut pool_vec = vec![self.into()];
        AgniFactory::sync_slot_0(&mut pool_vec, block_number, provider.clone()).await?;
        AgniFactory::sync_token_decimals(&mut pool_vec, provider.clone()).await?;
        AgniFactory::sync_tick_bitmaps(&mut pool_vec, block_number, provider.clone()).await?;
        AgniFactory::sync_tick_data(&mut pool_vec, block_number, provider.clone()).await?;

        let AMM::AgniPool(pool) = pool_vec.remove(0) else {
            unreachable!()
        };

        Ok(pool)
    }

    pub fn modify_position(
        &mut self,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
    ) -> Result<(), AMMError> {
        self.update_position(tick_lower, tick_upper, liquidity_delta)?;
        if liquidity_delta != 0 && self.tick >= tick_lower && self.tick < tick_upper {
            self.liquidity = if liquidity_delta < 0 {
                // Use saturating_sub to prevent underflow panic
                self.liquidity.saturating_sub((-liquidity_delta) as u128)
            } else {
                // Use saturating_add to prevent overflow
                self.liquidity.saturating_add(liquidity_delta as u128)
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
                self.flip_tick(tick_lower);
            }
            if flipped_upper {
                self.flip_tick(tick_upper);
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
        let before = info.liquidity_gross;
        let after = if liquidity_delta < 0 {
            // Use saturating_sub to prevent underflow panic
            // If liquidity to remove exceeds available, clamp to 0
            before.saturating_sub((-liquidity_delta) as u128)
        } else {
            // Use saturating_add to prevent overflow
            before.saturating_add(liquidity_delta as u128)
        };
        let flipped = (after == 0) != (before == 0);
        if before == 0 {
            info.initialized = true;
        }
        info.liquidity_gross = after;
        info.liquidity_net = if upper {
            info.liquidity_net.saturating_sub(liquidity_delta)
        } else {
            info.liquidity_net.saturating_add(liquidity_delta)
        };
        Ok(flipped)
    }
    pub fn flip_tick(&mut self, tick: i32) {
        let (word_pos, bit_pos) = uniswap_v3_math::tick_bitmap::position(tick / self.tick_spacing);
        let mask = U256::from(1) << bit_pos;
        *self.tick_bitmap.entry(word_pos).or_default() ^= mask;
    }
    pub fn swap_calldata(
        &self,
        recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Result<Bytes, AMMError> {
        Ok(IAgniPool::swapCall {
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
pub struct AgniFactory {
    pub address: Address,
    pub creation_block: u64,
}
impl AgniFactory {
    pub fn new(address: Address, creation_block: u64) -> Self {
        Self {
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
        let disc = Filter::new()
            .event_signature(FilterSet::from(vec![self.pool_creation_event()]))
            .address(vec![self.address()]);
        let to_block = block_number_for_range::<N, _>(&provider, block_number).await?;
        let result = fetch_logs_in_ranges::<N, _>(
            provider,
            disc,
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
        Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        Self::sync_token_decimals(&mut pools, provider.clone()).await?;
        pools = pools
            .par_drain(..)
            .filter(|p| match p {
                AMM::AgniPool(x) => {
                    x.liquidity > 0 && x.token_a.decimals > 0 && x.token_b.decimals > 0
                }
                _ => true,
            })
            .collect();
        Self::sync_tick_bitmaps(&mut pools, block_number, provider.clone()).await?;
        Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?;
        // WHI-938: never carry empty-tick pools; whole-batch failure aborts.
        retain_quotable_agni_pools(pools, "agni_v3_tick_data")
    }

    /// Batch-init a frozen-universe Agni set (WHI-936).
    ///
    /// Unlike factory discovery, the CSV may only carry addresses/tokens — not
    /// `fee` / `tick_spacing`. Those are filled concurrently, then the existing
    /// size-derived batch CREATE path runs for slot0 / decimals / ticks.
    ///
    /// **Does not drop** zero-liquidity pools: the frozen universe is already
    /// curated, and per-pool `init` keeps every pool — **except** pools whose
    /// tick-data batch reverted to empty ticks (WHI-938); those are dropped so
    /// `synced pool state` equals the quotable set.
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
        populate_agni_static_fields(&mut pools, block_number, provider.clone()).await?;
        Self::sync_slot_0(&mut pools, block_number, provider.clone()).await?;
        Self::sync_token_decimals(&mut pools, provider.clone()).await?;
        Self::sync_tick_bitmaps(&mut pools, block_number, provider.clone()).await?;
        Self::sync_tick_data(&mut pools, block_number, provider.clone()).await?;
        // WHI-938: never carry empty-tick pools; whole-batch failure aborts.
        retain_quotable_agni_pools(pools, "agni_v3_tick_data")
    }
    async fn sync_token_decimals<N, P>(
        pools: &mut [AMM],
        provider: P,
    ) -> Result<(), BatchContractError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let mut tokens = HashSet::new();
        for pool in pools.iter() {
            for t in pool.tokens() {
                tokens.insert(t);
            }
        }
        let token_decimals = get_token_decimals(tokens.into_iter().collect(), provider).await?;
        for pool in pools.iter_mut() {
            let AMM::AgniPool(p) = pool else {
                unreachable!()
            };
            if let Some(d) = token_decimals.get(&p.token_a.address) {
                p.token_a.decimals = *d;
            }
            if let Some(d) = token_decimals.get(&p.token_b.address) {
                p.token_b.decimals = *d;
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
        // WHI-925: chunk by expected return payload (Slot0Data = 3 ABI words),
        // not a hard-coded count. Old `step = 255` put all 94 agni-v3 pools in
        // one CREATE and hit EIP-170 (`CreateContractSizeLimit`).
        let step = v3_slot0_chunk_size();
        info!(
            target: "amms.agni.sync",
            path = "agni_v3_slot0",
            pool_count = pools.len(),
            chunk_size = step,
            item_count = pools.len(),
            per_item_bytes = V3_SLOT0_RETURN_BYTES_PER_POOL,
            budget_bytes = batch_create::create_return_budget_bytes(),
            "Agni V3 slot0 batch sync starting"
        );

        let addresses: Vec<Address> = pools.iter().map(|p| p.address()).collect();
        let mut all_slot0: Vec<(i32, u128, U256)> = Vec::with_capacity(addresses.len());

        for group in addresses.chunks(step) {
            let provider = provider.clone();
            let decoded = with_create_size_split(
                group.to_vec(),
                "agni_v3_slot0",
                |a: &Address| Some(*a),
                |_| None,
                |_| None, // address is atomic — cannot narrow further
                move |chunk| {
                    let provider = provider.clone();
                    async move {
                        let ret = GetAgniPoolSlot0BatchRequest::deploy_builder(provider, chunk)
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
                path: "agni_v3_slot0",
                expected: pools.len(),
                actual: all_slot0.len(),
            }
            .into());
        }

        for (slot0, pool) in all_slot0.iter().zip(pools.iter_mut()) {
            let AMM::AgniPool(p) = pool else {
                unreachable!()
            };
            p.tick = slot0.0;
            p.liquidity = slot0.1;
            p.sqrt_price = slot0.2;
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
        // Per-item return size varies with non-zero word density — do not try
        // to precompute a fixed count (see batch_create docs).
        let max_range = 6900;
        info!(
            target: "amms.agni.sync",
            path = "agni_v3_tick_bitmap",
            pool_count = pools.len(),
            chunk_size = max_range,
            item_count = pools.len(),
            max_range_words = max_range,
            "Agni V3 tick-bitmap batch sync starting"
        );

        // Collect groups first, then process **sequentially**. Concurrent
        // FuturesUnordered fan-out on dense pools (hundreds of groups) piles
        // behind the HTTP throttle and trips the per-request timeout (WHI-929
        // live cold-start). Size-split recovery already re-issues work; do not
        // amplify load with unbounded concurrency.
        let mut groups: Vec<Vec<TickBitmapInfo>> = Vec::new();
        let mut group_range = 0;
        let mut group = vec![];
        for pool in pools.iter() {
            let AMM::AgniPool(p) = pool else {
                unreachable!()
            };
            let mut min_word = tick_to_word(MIN_TICK, p.tick_spacing);
            let max_word = tick_to_word(MAX_TICK, p.tick_spacing);
            while min_word <= max_word {
                let remaining = max_range - group_range;
                let range = (max_word - min_word + 1).min(remaining);
                let max_chunk = min_word + range - 1;
                group.push(TickBitmapInfo {
                    pool: p.address,
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
            .map(|p| (p.address(), p))
            .collect::<HashMap<Address, &mut AMM>>();
        for calldata in groups {
            let leaves =
                fetch_agni_tick_bitmaps(provider.clone(), block_number, calldata).await?;
            for (info, bitmaps) in leaves {
                let pool = pool_set.get_mut(&info.pool).unwrap();
                let AMM::AgniPool(p) = pool else {
                    unreachable!()
                };
                p.tick_bitmap_coverage.extend(info.minWord..=info.maxWord);
                for chunk in bitmaps.chunks_exact(2) {
                    let word_pos = I256::from_raw(chunk[0]).as_i16();
                    let bitmap = chunk[1];
                    p.tick_bitmap.insert(word_pos, bitmap);
                    p.tick_bitmap_coverage.insert(word_pos);
                }
            }
        }
        Ok(())
    }
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
                if let AMM::AgniPool(p) = pool {
                    let min_word = tick_to_word(MIN_TICK, p.tick_spacing);
                    let max_word = tick_to_word(MAX_TICK, p.tick_spacing);
                    let ticks: Vec<Signed<24, 1>> = (min_word..=max_word)
                        .filter_map(|w| {
                            p.tick_bitmap
                                .get(&(w as i16))
                                .filter(|&b| *b != U256::ZERO)
                                .map(|&b| (w, b))
                        })
                        .flat_map(|(w, b)| {
                            (0..256)
                                .filter(move |i| {
                                    (b & (U256::from(1) << U256::from(*i))) != U256::ZERO
                                })
                                .map(move |i| {
                                    let idx = (w * 256 + i) * p.tick_spacing;
                                    Signed::<24, 1>::from_str(&idx.to_string()).unwrap()
                                })
                        })
                        .collect();
                    if !ticks.is_empty() {
                        Some((p.address, ticks))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<Vec<(Address, Vec<Signed<24, 1>>)>>();

        // WHI-929: optimistic max_ticks + CREATE-size split/bisect. Tick density
        // is not knowable before the call; split-on-failure is the budget.
        let max_ticks = 60;
        info!(
            target: "amms.agni.sync",
            path = "agni_v3_tick_data",
            pool_count = pool_ticks.len(),
            chunk_size = max_ticks,
            item_count = pool_ticks.len(),
            max_ticks_per_batch = max_ticks,
            "Agni V3 tick-data batch sync starting"
        );

        // Sequential batch processing — same rationale as tick-bitmap (WHI-929).
        let mut groups: Vec<Vec<TickDataInfo>> = Vec::new();
        let mut group_ticks = 0;
        let mut group = vec![];
        for (addr, mut ticks) in pool_ticks {
            while !ticks.is_empty() {
                let remaining = max_ticks - group_ticks;
                let selected = ticks.drain(0..remaining.min(ticks.len()));
                group_ticks += selected.len();
                group.push(GetAgniPoolTickDataBatchRequest::TickDataInfo {
                    pool: addr,
                    ticks: selected.collect(),
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
            .map(|p| (p.address(), p))
            .collect::<HashMap<Address, &mut AMM>>();
        for calldata in groups {
            let leaves = fetch_agni_tick_data(provider.clone(), block_number, calldata).await?;
            for (info, ticks_vec) in leaves {
                let pool = pool_set.get_mut(&info.pool).unwrap();
                let AMM::AgniPool(p) = pool else {
                    unreachable!()
                };
                for (tick, idx) in ticks_vec.iter().zip(info.ticks.iter()) {
                    let inf = Info {
                        liquidity_gross: tick.1,
                        liquidity_net: tick.2,
                        initialized: tick.0,
                    };
                    p.ticks.insert(idx.as_i32(), inf);
                }
            }
        }
        Ok(())
    }
}

/// Fetch one Agni tick-bitmap batch with CREATE-size split + word-range bisect
/// (WHI-929). Returns leaf `(request, bitmaps)` pairs — cardinality may grow
/// when a single dense range is bisected.
async fn fetch_agni_tick_bitmaps<N, P>(
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
        target: "amms.agni.sync",
        path = "agni_v3_tick_bitmap",
        chunk_size = item_count,
        item_count,
        "Agni V3 tick-bitmap batch CREATE"
    );
    with_create_size_split(
        items,
        "agni_v3_tick_bitmap",
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
                let ret =
                    match GetAgniPoolTickBitmapBatchRequest::deploy_builder(provider, chunk.clone())
                        .call_raw()
                        .block(block_number)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let err: AMMError = e.into();
                            // Non-drop-in V3 venues can revert in the batch
                            // constructor; skip that leaf so cold-start can
                            // finish (empty bitmap coverage for the range).
                            if chunk.len() == 1 && is_execution_reverted(&err) {
                                tracing::error!(
                                    target: "amms.agni.sync",
                                    path = "agni_v3_tick_bitmap",
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

/// Fetch one Agni tick-data batch with CREATE-size split + tick-list bisect
/// (WHI-929). Returns leaf `(request, tick infos)` pairs.
async fn fetch_agni_tick_data<N, P>(
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
        target: "amms.agni.sync",
        path = "agni_v3_tick_data",
        chunk_size = item_count,
        item_count,
        tick_count,
        "Agni V3 tick-data batch CREATE"
    );
    with_create_size_split(
        items,
        "agni_v3_tick_data",
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
                let ret =
                    match GetAgniPoolTickDataBatchRequest::deploy_builder(provider, chunk.clone())
                        .call_raw()
                        .block(block_number)
                        .await
                    {
                        Ok(r) => r,
                        Err(e) => {
                            let err: AMMError = e.into();
                            // Cleopatra CL (and other non-drop-in V3) can revert
                            // on ticks() under the Agni batch ABI — observed
                            // pool 0x5d9e… (WHI-929 live). Skip so the rest of
                            // the universe can still cold-start.
                            if chunk.len() == 1 && is_execution_reverted(&err) {
                                tracing::error!(
                                    target: "amms.agni.sync",
                                    path = "agni_v3_tick_data",
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

/// Drop Agni pools that have initialized tick-bitmap bits but empty tick data
/// (WHI-938 skip path). A single-pool skip is an ERROR + drop; if **every**
/// pool that needed tick data is empty, abort as a whole-batch configuration
/// error (venue ABI mismatch).
fn retain_quotable_agni_pools(
    pools: Vec<AMM>,
    path: &'static str,
) -> Result<Vec<AMM>, AMMError> {
    let mut kept = Vec::with_capacity(pools.len());
    let mut dropped: Vec<Address> = Vec::new();
    let mut needed_tick_data = 0usize;
    let mut missing_tick_data = 0usize;

    for p in pools {
        match &p {
            AMM::AgniPool(pool) => {
                let has_bits = pool.tick_bitmap.values().any(|b| *b != U256::ZERO);
                if has_bits {
                    needed_tick_data += 1;
                    if !pool.is_tick_data_quotable() {
                        missing_tick_data += 1;
                        tracing::error!(
                            target: "amms.agni.sync",
                            path,
                            pool = ?pool.address,
                            liquidity = pool.liquidity,
                            bitmap_words = pool.tick_bitmap.len(),
                            "dropping pool with empty tick data after tick-data CREATE skip (WHI-938)"
                        );
                        dropped.push(pool.address);
                        continue;
                    }
                }
                kept.push(p);
            }
            _ => kept.push(p),
        }
    }

    if missing_tick_data > 0 {
        tracing::error!(
            target: "amms.agni.sync",
            path,
            dropped = missing_tick_data,
            needed_tick_data,
            sample_pools = ?dropped.iter().take(8).collect::<Vec<_>>(),
            "tick-data empty after CREATE: dropped unusable pools (WHI-938)"
        );
    }

    // Whole-batch: every pool that needed tick data failed → config error.
    if needed_tick_data > 0 && missing_tick_data == needed_tick_data {
        let sample_pools: Vec<Address> = dropped.into_iter().take(8).collect();
        // AgniPool shells do not carry factory; name the venue by mapping
        // sample_pools → universe CSV `factory` (whole-venue config error).
        tracing::error!(
            target: "amms.agni.sync",
            path,
            failed = missing_tick_data,
            total_needing = needed_tick_data,
            sample_pools = ?sample_pools,
            "WHOLE-BATCH tick-data failure (WHI-938): every pool that needed \
             tick data is empty — treat as venue/factory configuration error. \
             Map sample_pools to the universe `factory` column and quarantine \
             that factory (or add a venue-specific tick-data batch ABI)."
        );
        return Err(BatchContractError::WholeVenueTickDataFailure {
            path,
            failed: missing_tick_data,
            total_needing: needed_tick_data,
            sample_pools,
        }
        .into());
    }

    Ok(kept)
}

/// Concurrently fetch `fee` + `tickSpacing` for Agni pools that lack them.
///
/// Frozen-universe rows ship tokens only; factory-discovered pools already have
/// these from `PoolCreated`. Pipelines over the RPC throttle rather than
/// awaiting one pool at a time (WHI-936).
async fn populate_agni_static_fields<N, P>(
    pools: &mut [AMM],
    block_number: BlockId,
    provider: P,
) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    use futures::stream::{self, StreamExt};

    // WHI-968: concurrency ≤ active HTTP throttle so queue wait stays under the
    // per-request timeout (timeout wraps throttle — see rpc_pipeline docs).
    let concurrency = crate::rpc_pipeline::active_pipelined_rpc_concurrency();

    let need: Vec<(usize, Address)> = pools
        .iter()
        .enumerate()
        .filter_map(|(i, amm)| match amm {
            AMM::AgniPool(p) if p.fee == 0 || p.tick_spacing == 0 => Some((i, p.address)),
            _ => None,
        })
        .collect();
    if need.is_empty() {
        return Ok(());
    }

    info!(
        target: "amms.agni.sync",
        pool_count = need.len(),
        concurrency,
        throttle_rps = crate::rpc_pipeline::active_throttle_rps(),
        "populating Agni fee/tick_spacing (pipelined eth_calls)"
    );

    let mut stream = stream::iter(need.into_iter().map(|(idx, address)| {
        let provider = provider.clone();
        async move {
            let pool = IAgniPool::new(address, provider);
            // Sequential per pool: two view calls on the same contract. Fan-out
            // across pools is what pipelines the throttle.
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
    .buffer_unordered(concurrency);

    while let Some(result) = stream.next().await {
        let (idx, fee, tick_spacing) = result?;
        let AMM::AgniPool(p) = &mut pools[idx] else {
            unreachable!("populate_agni_static_fields only indexes AgniPool")
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

impl AutomatedMarketMakerFactory for AgniFactory {
    type PoolVariant = AgniPool;
    fn address(&self) -> Address {
        self.address
    }
    fn pool_creation_event(&self) -> B256 {
        IAgniFactory::PoolCreated::SIGNATURE_HASH
    }
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError> {
        let ev: alloy::primitives::Log<IAgniFactory::PoolCreated> =
            IAgniFactory::PoolCreated::decode_log(&log.inner)?;
        Ok(AMM::AgniPool(AgniPool {
            address: ev.pool,
            token_a: ev.token0.into(),
            token_b: ev.token1.into(),
            fee: ev.fee.to::<u32>(),
            tick_spacing: ev.tickSpacing.unchecked_into(),
            ..Default::default()
        }))
    }
    fn creation_block(&self) -> u64 {
        self.creation_block
    }
}
impl DiscoverySync for AgniFactory {
    fn discover<N, P>(
        &self,
        to_block: BlockId,
        provider: P,
    ) -> impl Future<Output = Result<Vec<AMM>, AMMError>>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        info!(target = "amms::agni::discover", address = ?self.address, "Discovering all pools");
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
        info!(target = "amms::agni::sync", address = ?self.address, "Syncing all pools");
        AgniFactory::sync_all_pools(amms, to_block, provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::{
        network::Ethereum,
        primitives::{address, Bytes, U256},
        providers::ProviderBuilder,
        rpc::client::ClientBuilder,
        sol_types::{SolType, SolValue},
        transports::layers::{RetryBackoffLayer, ThrottleLayer},
        transports::mock::Asserter,
    };

    fn test_pool() -> AgniPool {
        let mut pool = AgniPool {
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

    #[tokio::test]
    async fn init_basic_syncs_tick_bitmap_coverage_before_quoting() {
        let pool_address = address!("0000000000000000000000000000000000000003");
        let token_a = address!("0000000000000000000000000000000000000001");
        let token_b = address!("0000000000000000000000000000000000000002");
        let sqrt_price = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(0)
            .expect("zero tick has a valid sqrt ratio");
        let asserter = Asserter::new();

        asserter.push_success(&Bytes::from(200i32.abi_encode()));
        asserter.push_success(&Bytes::from(3_000u32.abi_encode()));
        asserter.push_success(&Bytes::from(token_a.abi_encode()));
        asserter.push_success(&Bytes::from(token_b.abi_encode()));
        asserter.push_success(&Bytes::from(
            alloy::sol_types::sol_data::Uint::<8>::abi_encode(&18u8),
        ));
        asserter.push_success(&Bytes::from(
            alloy::sol_types::sol_data::Uint::<8>::abi_encode(&18u8),
        ));

        asserter.push_success(&Bytes::from(
            vec![(0i32, 1_000_000u128, sqrt_price)].abi_encode(),
        ));
        asserter.push_success(&Bytes::from(alloy::sol_types::sol_data::Array::<
            alloy::sol_types::sol_data::Uint<8>,
        >::abi_encode(&vec![18u8, 18u8])));

        asserter.push_success(&Bytes::from(vec![Vec::<U256>::new()].abi_encode()));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let pool = AgniPool {
            address: pool_address,
            ..Default::default()
        }
        .init_basic::<Ethereum, _>(1u64.into(), provider)
        .await
        .expect("mocked startup sync should succeed");

        assert_eq!(pool.tick_spacing, 200);
        assert_eq!(pool.fee, 3_000);
        assert_eq!(pool.token_a.decimals, 18);
        assert_eq!(pool.token_b.decimals, 18);
        assert_eq!(pool.tick, 0);
        assert_eq!(pool.liquidity, 1_000_000);
        assert_eq!(pool.sqrt_price, sqrt_price);
        assert_eq!(pool.tick_bitmap, HashMap::new());
        assert_eq!(pool.tick_bitmap_coverage, (-18i16..=17i16).collect());
        assert_eq!(
            pool.simulate_swap(token_a, token_b, U256::from(10_000))
                .expect("a fully covered startup pool should quote"),
            U256::from(9_871)
        );
    }

    #[test]
    fn simulate_swap_mut_advances_state_when_swap_succeeds() {
        // Given
        let mut pool = test_pool();
        let initial_sqrt_price = pool.sqrt_price;

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
        assert_ne!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(
            pool.sqrt_price,
            U256::from(78_446_055_342_499_616_417_857_907_004u128)
        );
        assert_eq!(pool.tick, -199);
        assert_eq!(pool.liquidity, 1_000_000);
    }

    #[test]
    fn simulate_swap_mut_uses_prior_state_when_swaps_are_sequential() {
        // Given
        let mut pool = test_pool();
        let fresh_pool = pool.clone();
        let amount_in = U256::from(10_000);

        // When
        let _first_amount_out = pool
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, amount_in)
            .expect("the first deterministic swap succeeds");
        let second_amount_out = pool
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, amount_in)
            .expect("the second deterministic swap succeeds");
        let fresh_amount_out = fresh_pool
            .simulate_swap(
                fresh_pool.token_a.address,
                fresh_pool.token_b.address,
                amount_in,
            )
            .expect("the fresh deterministic swap succeeds");

        // Then
        assert_ne!(second_amount_out, fresh_amount_out);
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
    fn simulate_swap_mut_preserves_state_when_initialized_tick_is_missing() {
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
    fn simulate_swap_rejects_an_initialized_tick_without_a_record() {
        // Given
        let mut pool = test_pool();
        uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, 0, pool.tick_spacing)
            .expect("the current tick can be initialized");

        // When
        let error = pool
            .simulate_swap(
                pool.token_a.address,
                pool.token_b.address,
                U256::from(10_000),
            )
            .expect_err("a missing initialized tick record must fail closed");

        // Then
        assert!(matches!(error, AMMError::IncompleteState));
    }

    #[test]
    fn simulate_swap_mut_preserves_state_when_tick_crossing_fails() {
        // Given
        let mut pool = test_pool();
        uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, 0, pool.tick_spacing)
            .expect("the adjacent tick can be initialized");
        pool.ticks.insert(0, Info::new(2_000_000, 2_000_000, true));
        let initial_sqrt_price = pool.sqrt_price;
        let initial_tick = pool.tick;
        let initial_liquidity = pool.liquidity;

        // When
        let result =
            pool.simulate_swap_mut(pool.token_a.address, pool.token_b.address, U256::from(100));

        // Then
        assert!(matches!(
            result,
            Err(AMMError::AgniError(AgniError::LiquidityUnderflow))
        ));
        assert_eq!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(pool.tick, initial_tick);
        assert_eq!(pool.liquidity, initial_liquidity);
    }

    /// WHI-938: bitmap bits + empty ticks is the Cleopatra skip mode — not quotable.
    #[test]
    fn empty_tick_data_with_bitmap_bits_is_not_quotable() {
        let mut pool = test_pool();
        uniswap_v3_math::tick_bitmap::flip_tick(&mut pool.tick_bitmap, 0, pool.tick_spacing)
            .expect("initialize a bitmap bit");
        assert!(pool.ticks.is_empty());
        assert!(!pool.is_tick_data_quotable());

        // Within-tick only (no initialized bits) remains quotable.
        let clean = test_pool();
        assert!(clean.tick_bitmap.is_empty() || clean.tick_bitmap.values().all(|b| *b == U256::ZERO));
        assert!(clean.is_tick_data_quotable());
    }

    /// WHI-938: empty-tick pools are dropped from the sync result, not kept as
    /// silent zero-liquidity quotes. A partial drop keeps the good pool.
    #[test]
    fn retain_quotable_drops_empty_tick_pool_and_keeps_good() {
        let mut bad = test_pool();
        bad.address = Address::with_last_byte(0xBA);
        uniswap_v3_math::tick_bitmap::flip_tick(&mut bad.tick_bitmap, 0, bad.tick_spacing)
            .expect("bitmap bit");
        // ticks left empty → unusable

        let mut good = test_pool();
        good.address = Address::with_last_byte(0x60);
        uniswap_v3_math::tick_bitmap::flip_tick(&mut good.tick_bitmap, 0, good.tick_spacing)
            .expect("bitmap bit");
        good.ticks.insert(0, Info::new(1_000_000, 0, true));

        let out = retain_quotable_agni_pools(
            vec![AMM::AgniPool(bad), AMM::AgniPool(good)],
            "test_tick_data",
        )
        .expect("partial drop must not abort");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].address(), Address::with_last_byte(0x60));
    }

    /// WHI-938: every pool needing tick data empty → whole-batch abort.
    #[test]
    fn retain_quotable_aborts_when_all_tick_data_empty() {
        let mut a = test_pool();
        a.address = Address::with_last_byte(0x01);
        uniswap_v3_math::tick_bitmap::flip_tick(&mut a.tick_bitmap, 0, a.tick_spacing).unwrap();
        let mut b = test_pool();
        b.address = Address::with_last_byte(0x02);
        uniswap_v3_math::tick_bitmap::flip_tick(&mut b.tick_bitmap, 0, b.tick_spacing).unwrap();

        let err = retain_quotable_agni_pools(
            vec![AMM::AgniPool(a), AMM::AgniPool(b)],
            "test_whole_venue",
        )
        .expect_err("whole-batch empty tick data must abort");
        match err {
            AMMError::BatchContractError(BatchContractError::WholeVenueTickDataFailure {
                failed,
                total_needing,
                sample_pools,
                ..
            }) => {
                assert_eq!(failed, 2);
                assert_eq!(total_needing, 2);
                assert_eq!(sample_pools.len(), 2);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn test_agni_simulate_swap_sanity() -> eyre::Result<()> {
        let rpc = match std::env::var("MANTLE_PROVIDER_URL") {
            Ok(v) => v,
            Err(_) => {
                println!("[agni/tests] MANTLE_PROVIDER_URL not set, skipping test_agni_simulate_swap_sanity");
                return Ok(());
            }
        };
        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc.parse()?);
        let provider = ProviderBuilder::new().connect_client(client);
        let block_id = BlockId::latest();

        // Example Agni pool on Mantle (validated via agni_pool_probe.rs)
        let pool_address = address!("eafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5");

        println!(
            "[agni/tests] rpc={}, block={:?}",
            std::env::var("MANTLE_PROVIDER_URL").unwrap(),
            block_id
        );
        println!("[agni/tests] pool={:?}", pool_address);

        let pool = match AgniPool::new(pool_address)
            .init(block_id, provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("[agni/tests] init failed, skipping: {:?}", e);
                return Ok(());
            }
        };

        println!(
            "[agni/tests] token_a={:?} ({}), token_b={:?} ({}), fee={}, tick_spacing={}, tick={}, sqrt_price={}, liquidity={}",
            pool.token_a.address,
            pool.token_a.decimals,
            pool.token_b.address,
            pool.token_b.decimals,
            pool.fee,
            pool.tick_spacing,
            pool.tick,
            pool.sqrt_price,
            pool.liquidity
        );

        // token_a -> token_b: sanity check small trade yields non-zero and price math is consistent
        let amount_in_small = U256::from(1_000_000_000_000u64); // 1e12, tiny size
        let out_small =
            pool.simulate_swap(pool.token_a.address, pool.token_b.address, amount_in_small)?;
        println!("[agni/tests] out_small a->b: {}", out_small);
        assert!(out_small > U256::ZERO);

        // token_b -> token_a
        let out_small_ba =
            pool.simulate_swap(pool.token_b.address, pool.token_a.address, amount_in_small)?;
        println!("[agni/tests] out_small b->a: {}", out_small_ba);
        assert!(out_small_ba > U256::ZERO);

        Ok(())
    }

    #[tokio::test]
    async fn test_agni_calculate_price() -> eyre::Result<()> {
        let rpc = match std::env::var("MANTLE_PROVIDER_URL") {
            Ok(v) => v,
            Err(_) => {
                println!(
                    "[agni/tests] MANTLE_PROVIDER_URL not set, skipping test_agni_calculate_price"
                );
                return Ok(());
            }
        };
        let client = ClientBuilder::default()
            .layer(ThrottleLayer::new(250))
            .layer(RetryBackoffLayer::new(5, 200, 330))
            .http(rpc.parse()?);
        let provider = ProviderBuilder::new().connect_client(client);
        let block_id = BlockId::latest();
        let pool_address = address!("eafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5");

        println!(
            "[agni/tests] pool={:?} @ block {:?}",
            pool_address, block_id
        );
        let pool = match AgniPool::new(pool_address)
            .init(block_id, provider.clone())
            .await
        {
            Ok(p) => p,
            Err(e) => {
                println!("[agni/tests] init failed, skipping: {:?}", e);
                return Ok(());
            }
        };

        let price_a_in_b = pool.calculate_price(pool.token_a.address, pool.token_b.address)?;
        let price_b_in_a = pool.calculate_price(pool.token_b.address, pool.token_a.address)?;

        println!("Price token_a in token_b: {}", price_a_in_b);
        println!("Price token_b in token_a: {}", price_b_in_a);

        // Sanity: product should be ~1
        let product = price_a_in_b * price_b_in_a;
        println!(
            "[agni/tests] p_ab={}, p_ba={}, product={}",
            price_a_in_b, price_b_in_a, product
        );
        assert!((product - 1.0).abs() < 1e-9);

        // Cross-check with direct tick->price computation as in the probe
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(pool.sqrt_price).unwrap();
        let shift = (pool.token_a.decimals as i32) - (pool.token_b.decimals as i32);
        let price_from_tick = 1.0001_f64.powi(tick) * 10_f64.powi(shift);
        println!("[agni/tests] price_from_tick: {}", price_from_tick);
        assert!((price_from_tick - price_a_in_b).abs() < 1e-9);

        Ok(())
    }

    /// WHI-980: Agni and UniV3-family Swap topic0 must both be present and distinct.
    #[test]
    fn sync_events_covers_agni_and_univ3_family_swap_topics() {
        let events = test_pool().sync_events();
        assert!(
            events.contains(&IAgniPoolEvents::Swap::SIGNATURE_HASH),
            "missing Agni-native Swap topic"
        );
        assert!(
            events.contains(&IUniV3FamilyPoolEvents::Swap::SIGNATURE_HASH),
            "missing UniV3-family Swap topic (Fluxion/FusionX/Butter/…)"
        );
        assert_ne!(
            IAgniPoolEvents::Swap::SIGNATURE_HASH,
            IUniV3FamilyPoolEvents::Swap::SIGNATURE_HASH,
            "Agni and UniV3 Swap topic0 must differ (extra protocol-fee fields)"
        );
        // Canonical UniV3 Swap topic (keccak of Swap(address,address,int256,int256,uint160,uint128,int24))
        let univ3 =
            B256::from_str("0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67")
                .unwrap();
        assert_eq!(IUniV3FamilyPoolEvents::Swap::SIGNATURE_HASH, univ3);
        // Agni-native (extra protocolFeesToken0/1)
        let agni =
            B256::from_str("0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83")
                .unwrap();
        assert_eq!(IAgniPoolEvents::Swap::SIGNATURE_HASH, agni);
    }

    fn univ3_family_swap_log(pool: Address, sqrt_price: U256, liquidity: u128, tick: i32) -> Log {
        use alloy::primitives::{Signed, U160};
        let event = IUniV3FamilyPoolEvents::Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: I256::ZERO,
            amount1: I256::ZERO,
            sqrtPriceX96: U160::from(sqrt_price),
            liquidity,
            tick: Signed::<24, 1>::try_from(tick).expect("tick fits i24"),
        };
        let encoded = event.encode_log_data();
        Log {
            inner: alloy::primitives::Log::new_unchecked(
                pool,
                encoded.topics().to_vec(),
                encoded.data.clone(),
            ),
            block_hash: None,
            block_number: Some(42),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    fn agni_native_swap_log(pool: Address, sqrt_price: U256, liquidity: u128, tick: i32) -> Log {
        use alloy::primitives::{Signed, U160};
        let event = IAgniPoolEvents::Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: I256::ZERO,
            amount1: I256::ZERO,
            sqrtPriceX96: U160::from(sqrt_price),
            liquidity,
            tick: Signed::<24, 1>::try_from(tick).expect("tick fits i24"),
            protocolFeesToken0: 0,
            protocolFeesToken1: 0,
        };
        let encoded = event.encode_log_data();
        Log {
            inner: alloy::primitives::Log::new_unchecked(
                pool,
                encoded.topics().to_vec(),
                encoded.data.clone(),
            ),
            block_hash: None,
            block_number: Some(42),
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    /// WHI-980: a recorded UniV3-family Swap on an AgniPool must apply (dirty path).
    #[test]
    fn sync_applies_univ3_family_swap_log() {
        let mut pool = test_pool();
        let new_sqrt = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(100)
            .expect("tick 100 has valid sqrt ratio");
        let log = univ3_family_swap_log(pool.address, new_sqrt, 2_000_000, 100);
        pool.sync(&log).expect("UniV3-family Swap must decode");
        assert_eq!(pool.sqrt_price, new_sqrt);
        assert_eq!(pool.liquidity, 2_000_000);
        assert_eq!(pool.tick, 100);
    }

    #[test]
    fn sync_applies_agni_native_swap_log() {
        let mut pool = test_pool();
        let new_sqrt = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(-50)
            .expect("tick -50 has valid sqrt ratio");
        let log = agni_native_swap_log(pool.address, new_sqrt, 3_000_000, -50);
        pool.sync(&log).expect("Agni-native Swap must decode");
        assert_eq!(pool.sqrt_price, new_sqrt);
        assert_eq!(pool.liquidity, 3_000_000);
        assert_eq!(pool.tick, -50);
    }
}
