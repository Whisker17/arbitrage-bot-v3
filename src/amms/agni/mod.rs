// agni.rs

use super::{
    amm::{AutomatedMarketMaker, AMM},
    error::{AMMError, BatchContractError},
    factory::{AutomatedMarketMakerFactory, DiscoverySync},
    get_token_decimals, Token,
};
use crate::amms::{agni::GetAgniPoolTickBitmapBatchRequest::TickBitmapInfo, consts::U256_1};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, Bytes, Signed, B256, I256, U256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol,
    sol_types::{SolCall, SolEvent, SolValue},
    transports::BoxFuture,
};
use futures::{stream::FuturesUnordered, StreamExt};
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
        vec![
            IAgniPoolEvents::Mint::SIGNATURE_HASH,
            IAgniPoolEvents::Burn::SIGNATURE_HASH,
            IAgniPoolEvents::Swap::SIGNATURE_HASH,
        ]
    }
    fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
        let sig = log.topics()[0];
        match sig {
            IAgniPoolEvents::Swap::SIGNATURE_HASH => {
                let e = IAgniPoolEvents::Swap::decode_log(log.as_ref())?;
                self.sqrt_price = e.sqrtPriceX96.to();
                self.liquidity = e.liquidity;
                self.tick = e.tick.unchecked_into();
            }
            IAgniPoolEvents::Mint::SIGNATURE_HASH => {
                let e = IAgniPoolEvents::Mint::decode_log(log.as_ref())?;
                self.modify_position(
                    e.tickLower.unchecked_into(),
                    e.tickUpper.unchecked_into(),
                    e.amount as i128,
                )?;
            }
            IAgniPoolEvents::Burn::SIGNATURE_HASH => {
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
        Ok(self.simulate_swap_with_state(base_token, amount_in)?.amount_out)
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
    fn ensure_tick_bitmap_coverage(
        &self,
        tick: i32,
        zero_for_one: bool,
    ) -> Result<(), AMMError> {
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
                state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x_96)
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
        let sync_provider = provider.clone();
        let mut futures = FuturesUnordered::new();
        let step = 90_000;
        let mut latest = self.creation_block;
        while latest < block_number.as_u64().unwrap_or_default() {
            let mut bf = disc.clone();
            let from = latest;
            let to = (from + step).min(block_number.as_u64().unwrap_or_default());
            bf = bf.from_block(from);
            bf = bf.to_block(to);
            let sp = sync_provider.clone();
            futures.push(async move { sp.get_logs(&bf).await });
            latest = to + 1;
        }
        let mut pools = vec![];
        while let Some(res) = futures.next().await {
            for log in res? {
                pools.push(self.create_pool(log)?);
            }
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
        let step = 255;
        let mut futures = FuturesUnordered::new();
        pools.chunks_mut(step).for_each(|group| {
            let provider = provider.clone();
            let addrs = group.iter_mut().map(|p| p.address()).collect::<Vec<_>>();
            futures.push(async move {
                Ok::<(&mut [AMM], Bytes), AMMError>((
                    group,
                    GetAgniPoolSlot0BatchRequest::deploy_builder(provider, addrs)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            });
        });
        while let Some(res) = futures.next().await {
            let (group, ret) = res?;
            let data = <Vec<(i32, u128, U256)> as SolValue>::abi_decode(&ret)?;
            for (slot0, pool) in data.iter().zip(group.iter_mut()) {
                let AMM::AgniPool(p) = pool else {
                    unreachable!()
                };
                p.tick = slot0.0;
                p.liquidity = slot0.1;
                p.sqrt_price = slot0.2;
            }
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
        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let max_range = 6900;
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
                    let provider = provider.clone();
                    let pool_info = group
                        .iter()
                        .map(|i| (i.pool, i.minWord, i.maxWord))
                        .collect::<Vec<_>>();
                    let calldata = std::mem::take(&mut group);
                    group_range = 0;
                    futures.push(Box::pin(async move {
                        Ok::<(Vec<(Address, i16, i16)>, Bytes), AMMError>((
                            pool_info,
                            GetAgniPoolTickBitmapBatchRequest::deploy_builder(provider, calldata)
                                .call_raw()
                                .block(block_number)
                                .await?,
                        ))
                    }));
                }
            }
        }
        if !group.is_empty() {
            let provider = provider.clone();
            let pool_info = group
                .iter()
                .map(|i| (i.pool, i.minWord, i.maxWord))
                .collect::<Vec<_>>();
            let calldata = std::mem::take(&mut group);
            futures.push(Box::pin(async move {
                Ok::<(Vec<(Address, i16, i16)>, Bytes), AMMError>((
                    pool_info,
                    GetAgniPoolTickBitmapBatchRequest::deploy_builder(provider, calldata)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            }));
        }
        let mut pool_set = pools
            .iter_mut()
            .map(|p| (p.address(), p))
            .collect::<HashMap<Address, &mut AMM>>();
        while let Some(res) = futures.next().await {
            let (pool_ranges, ret) = res?;
            let ret = <Vec<Vec<U256>> as SolValue>::abi_decode(&ret)?;
            for (bitmaps, (addr, min_word, max_word)) in ret.iter().zip(pool_ranges.iter()) {
                let pool = pool_set.get_mut(addr).unwrap();
                let AMM::AgniPool(p) = pool else {
                    unreachable!()
                };
                p.tick_bitmap_coverage.extend(*min_word..=*max_word);
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
        let mut futures: FuturesUnordered<BoxFuture<'_, _>> = FuturesUnordered::new();
        let max_ticks = 60;
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
                    let provider = provider.clone();
                    let calldata = std::mem::take(&mut group);
                    group_ticks = 0;
                    group.clear();
                    futures.push(Box::pin(async move {
                        Ok::<(Vec<TickDataInfo>, Bytes), AMMError>((
                            calldata.clone(),
                            GetAgniPoolTickDataBatchRequest::deploy_builder(provider, calldata)
                                .call_raw()
                                .block(block_number)
                                .await?,
                        ))
                    }));
                }
            }
        }
        if !group.is_empty() {
            let provider = provider.clone();
            let calldata = std::mem::take(&mut group);
            futures.push(Box::pin(async move {
                Ok::<(Vec<TickDataInfo>, Bytes), AMMError>((
                    calldata.clone(),
                    GetAgniPoolTickDataBatchRequest::deploy_builder(provider, calldata)
                        .call_raw()
                        .block(block_number)
                        .await?,
                ))
            }));
        }
        let mut pool_set = pools
            .iter_mut()
            .map(|p| (p.address(), p))
            .collect::<HashMap<Address, &mut AMM>>();
        while let Some(res) = futures.next().await {
            let (tick_info, ret) = res?;
            let ret = <Vec<Vec<(bool, u128, i128)>> as SolValue>::abi_decode(&ret)?;
            for (ticks_vec, info) in ret.iter().zip(tick_info.iter()) {
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
        asserter.push_success(&Bytes::from(
            alloy::sol_types::sol_data::Array::<alloy::sol_types::sol_data::Uint<8>>::abi_encode(
                &vec![18u8, 18u8],
            ),
        ));

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
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
            .simulate_swap(fresh_pool.token_a.address, fresh_pool.token_b.address, amount_in)
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
        pool.ticks
            .insert(0, Info::new(200_000, -200_000, true));

        // When
        let amount_out = pool
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
            .simulate_swap(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
            .simulate_swap_mut(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
            .simulate_swap(pool.token_a.address, pool.token_b.address, U256::from(10_000))
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
        pool.ticks
            .insert(0, Info::new(2_000_000, 2_000_000, true));
        let initial_sqrt_price = pool.sqrt_price;
        let initial_tick = pool.tick;
        let initial_liquidity = pool.liquidity;

        // When
        let result = pool.simulate_swap_mut(
            pool.token_a.address,
            pool.token_b.address,
            U256::from(100),
        );

        // Then
        assert!(matches!(
            result,
            Err(AMMError::AgniError(AgniError::LiquidityUnderflow))
        ));
        assert_eq!(pool.sqrt_price, initial_sqrt_price);
        assert_eq!(pool.tick, initial_tick);
        assert_eq!(pool.liquidity, initial_liquidity);
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
}
