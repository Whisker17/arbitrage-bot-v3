# AMMs 模块 - 自动做市商抽象层

## 模块概述

AMMs 模块是整个套利系统的基础层，负责对各种去中心化交易所（DEX）协议的池（Pool）进行抽象和统一管理。该模块提供了协议无关的接口，使得上层业务逻辑可以透明地处理不同协议的池。

## 目录结构

```
src/amms/
├── mod.rs              # 模块入口、Token定义、批量调用
├── amm.rs              # AMM trait 定义和枚举
├── factory.rs          # Factory trait 定义和枚举
├── error.rs            # 错误类型定义
├── consts.rs           # 常量定义
├── float.rs            # 浮点数转换工具
├── uniswap_v2/         # Uniswap V2 实现
│   └── mod.rs
├── uniswap_v3/         # Uniswap V3 实现
│   └── mod.rs
├── agni/               # Agni (V3-like) 实现
│   └── mod.rs
├── moe/                # Moe Liquidity Book 实现
│   ├── mod.rs
│   └── math/           # Moe 数学库
│       ├── bin_helper.rs
│       ├── price_helper.rs
│       └── ...
└── abi/                # Solidity ABI 文件
    ├── GetUniswapV2PoolDataBatchRequest.json
    ├── GetUniswapV3PoolSlot0BatchRequest.json
    └── ...
```

## 核心设计思路

### 1. Trait-based 多态设计

#### `AutomatedMarketMaker` Trait

这是整个模块的核心抽象，定义了所有 AMM 池必须实现的接口：

```rust
pub trait AutomatedMarketMaker {
    // 基本属性
    fn address(&self) -> Address;
    fn tokens(&self) -> Vec<Address>;
    fn sync_events(&self) -> Vec<B256>;
    
    // 状态同步
    fn sync(&mut self, log: &Log) -> Result<(), AMMError>;
    async fn init<N, P>(self, block_number: BlockId, provider: P) -> Result<Self, AMMError>;
    
    // 交易模拟
    fn simulate_swap(&self, base_token: Address, quote_token: Address, amount_in: U256) 
        -> Result<U256, AMMError>;
    fn simulate_swap_mut(&mut self, base_token: Address, quote_token: Address, amount_in: U256) 
        -> Result<U256, AMMError>;
    
    // 价格计算
    fn calculate_price(&self, base_token: Address, quote_token: Address) 
        -> Result<f64, AMMError>;
}
```

**设计要点**:
- **不可变模拟** (`simulate_swap`): 用于路径搜索和优化，不改变状态
- **可变模拟** (`simulate_swap_mut`): 用于多步路径模拟，更新内部状态
- **异步初始化** (`init`): 支持从链上批量获取初始状态
- **事件同步** (`sync`): 增量更新池状态

### 2. AMM Enum - 统一类型

```rust
pub enum AMM {
    UniswapV2Pool(UniswapV2Pool),
    UniswapV3Pool(UniswapV3Pool),
    AgniPool(AgniPool),
    MoeLbPair(MoeLbPair),
}
```

通过宏 `amm!` 自动实现 `AutomatedMarketMaker` trait，提供统一的多态接口：

```rust
amm!(UniswapV2Pool, UniswapV3Pool, AgniPool, MoeLbPair);
```

**优势**:
- 类型安全的多态
- 零成本抽象（编译时分发）
- 易于添加新协议

### 3. Factory Pattern - 池发现机制

#### `AutomatedMarketMakerFactory` Trait

```rust
pub trait AutomatedMarketMakerFactory: DiscoverySync {
    type PoolVariant: AutomatedMarketMaker + Default;
    
    fn address(&self) -> Address;
    fn creation_block(&self) -> u64;
    fn pool_creation_event(&self) -> B256;
    fn pool_events(&self) -> Vec<B256>;
    fn create_pool(&self, log: Log) -> Result<AMM, AMMError>;
}

pub trait DiscoverySync {
    async fn discover<N, P>(&self, to_block: BlockId, provider: P) 
        -> Result<Vec<AMM>, AMMError>;
    async fn sync<N, P>(&self, amms: Vec<AMM>, to_block: BlockId, provider: P) 
        -> Result<Vec<AMM>, AMMError>;
}
```

**两阶段池获取**:
1. **Discovery**: 从工厂合约发现所有池地址
2. **Sync**: 批量同步池的详细状态

## 协议实现详解

### 1. Uniswap V2

#### 池状态结构

```rust
pub struct UniswapV2Pool {
    pub address: Address,
    pub token_a: Token,
    pub token_b: Token,
    pub reserve_0: u128,
    pub reserve_1: u128,
    pub fee: usize,  // 以 1e5 为单位 (e.g., 300 = 0.3%)
}
```

#### 交换计算公式

Uniswap V2 使用恒定乘积做市商模型（CPMM）：

```
x * y = k  (constant product)
```

实际交换考虑手续费：

```rust
pub fn get_amount_out(&self, amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    let fee = U256_100000 - U256::from(self.fee);  // e.g., 99700 for 0.3% fee
    let amount_in_with_fee = amount_in * fee;
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256_100000 + amount_in_with_fee;
    numerator / denominator
}
```

#### 批量同步优化

通过自定义 Solidity 合约 `GetUniswapV2PoolDataBatchRequest`，一次 RPC 调用获取多个池的数据：

```rust
pub async fn sync_all_pools<N, P>(
    amms: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError> {
    let step = 120;  // 每次批量处理 120 个池
    let pairs = amms.iter().chunks(step)
        .map(|chunk| chunk.map(|amm| amm.address()).collect())
        .collect::<Vec<Vec<Address>>>();
    
    // 并发发起批量请求
    let mut futures_unordered = FuturesUnordered::new();
    for group in pairs {
        let deployer = IGetUniswapV2PoolDataBatchRequest::deploy_builder(
            provider.clone(), group.clone()
        );
        futures_unordered.push(async move {
            (group, deployer.call_raw().block(block_number).await?)
        });
    }
    
    // 解析返回数据并更新池状态
    while let Some(res) = futures_unordered.next().await {
        let (group, return_data) = res?;
        let pool_data = <Vec<(Address, Address, u128, u128, u32, u32)>>::abi_decode(&return_data)?;
        // 更新每个池的 token_a, token_b, reserve_0, reserve_1
    }
    Ok(amms)
}
```

**性能优化点**:
- 批量大小优化（120个池/批次，避免 gas limit）
- 并发发起多个批次请求
- 单次 RPC 调用获取完整池数据（tokens, reserves, decimals）

### 2. Uniswap V3

#### 池状态结构

```rust
pub struct UniswapV3Pool {
    pub address: Address,
    pub token_a: Token,
    pub token_b: Token,
    pub liquidity: u128,           // 当前活跃流动性
    pub sqrt_price: U256,          // 当前价格（Q64.96 格式）
    pub fee: u32,                  // 手续费（basis points）
    pub tick: i32,                 // 当前 tick
    pub tick_spacing: i32,         // tick 间距
    pub tick_bitmap: HashMap<i16, U256>,   // tick 位图
    pub ticks: HashMap<i32, Info>,          // tick 数据
}

pub struct Info {
    pub liquidity_gross: u128,  // 总流动性
    pub liquidity_net: i128,    // 净流动性变化
    pub initialized: bool,      // 是否已初始化
}
```

#### 交换模拟算法

Uniswap V3 的交换需要跨多个 tick 进行分步计算：

```rust
fn simulate_swap(&self, base_token: Address, _quote_token: Address, amount_in: U256) 
    -> Result<U256, AMMError> 
{
    let zero_for_one = base_token == self.token_a.address;
    let sqrt_price_limit = if zero_for_one { MIN_SQRT_RATIO + 1 } else { MAX_SQRT_RATIO - 1 };
    
    let mut current_state = CurrentState {
        sqrt_price_x_96: self.sqrt_price,
        amount_calculated: I256::ZERO,
        amount_specified_remaining: I256::from_raw(amount_in),
        tick: self.tick,
        liquidity: self.liquidity,
    };
    
    // 迭代直到输入量用完或达到价格限制
    while current_state.amount_specified_remaining != I256::ZERO 
        && current_state.sqrt_price_x_96 != sqrt_price_limit 
    {
        // 1. 找到下一个初始化的 tick
        let (tick_next, initialized) = 
            uniswap_v3_math::tick_bitmap::next_initialized_tick_within_one_word(
                &self.tick_bitmap,
                current_state.tick,
                self.tick_spacing,
                zero_for_one,
            )?;
        
        // 2. 计算到下一个 tick 的交换
        let sqrt_price_next = uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(tick_next)?;
        let (sqrt_price_new, amount_in_step, amount_out_step, fee_amount) = 
            uniswap_v3_math::swap_math::compute_swap_step(
                current_state.sqrt_price_x_96,
                sqrt_price_next,
                current_state.liquidity,
                current_state.amount_specified_remaining,
                self.fee,
            )?;
        
        // 3. 更新状态
        current_state.amount_specified_remaining -= I256::from_raw(amount_in_step + fee_amount);
        current_state.amount_calculated -= I256::from_raw(amount_out_step);
        current_state.sqrt_price_x_96 = sqrt_price_new;
        
        // 4. 如果跨越 tick，更新流动性
        if current_state.sqrt_price_x_96 == sqrt_price_next && initialized {
            let liquidity_net = self.ticks.get(&tick_next).map_or(0, |info| info.liquidity_net);
            current_state.liquidity = if zero_for_one {
                current_state.liquidity - (-liquidity_net as u128)
            } else {
                current_state.liquidity + (liquidity_net as u128)
            };
            current_state.tick = if zero_for_one { tick_next - 1 } else { tick_next };
        } else {
            // 更新 tick（价格在 tick 之间）
            current_state.tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(
                current_state.sqrt_price_x_96
            )?;
        }
    }
    
    Ok((-current_state.amount_calculated).into_raw())
}
```

**关键数学库**:
- `tick_bitmap`: 高效查找下一个初始化的 tick
- `tick_math`: tick 和 sqrt_price 的转换
- `swap_math`: 单步交换计算（考虑流动性和手续费）

#### 多阶段批量同步

V3 池的同步分为四个阶段：

```rust
pub async fn sync_all_pools<N, P>(
    mut pools: Vec<AMM>,
    block_number: BlockId,
    provider: P,
) -> Result<Vec<AMM>, AMMError> {
    // 阶段 1: 同步 slot0 (tick, liquidity, sqrt_price)
    sync_slot_0(&mut pools, block_number, provider.clone()).await?;
    
    // 阶段 2: 同步 token decimals
    sync_token_decimals(&mut pools, provider.clone()).await?;
    
    // 过滤掉无效池（流动性为0或token信息缺失）
    pools = pools.par_drain(..)
        .filter(|pool| match pool {
            AMM::UniswapV3Pool(p) => p.liquidity > 0 && p.token_a.decimals > 0,
            _ => true,
        })
        .collect();
    
    // 阶段 3: 同步 tick bitmaps
    sync_tick_bitmaps(&mut pools, block_number, provider.clone()).await?;
    
    // 阶段 4: 同步 tick data (liquidity_gross, liquidity_net)
    sync_tick_data(&mut pools, block_number, provider.clone()).await?;
    
    Ok(pools)
}
```

**Tick Bitmap 同步优化**:

```rust
async fn sync_tick_bitmaps<N, P>(pools: &mut [AMM], block_number: BlockId, provider: P) 
    -> Result<(), AMMError> 
{
    let max_range = 6900;  // 每个批次最多处理 6900 个 word
    let mut group = vec![];
    let mut group_range = 0;
    
    for pool in pools.iter() {
        let min_word = tick_to_word(MIN_TICK, pool.tick_spacing);
        let max_word = tick_to_word(MAX_TICK, pool.tick_spacing);
        let mut word_range = max_word - min_word;
        
        while word_range > 0 {
            let range = word_range.min(max_range - group_range);
            group.push(TickBitmapInfo {
                pool: pool.address,
                minWord: min_word as i16,
                maxWord: (min_word + range) as i16,
            });
            word_range -= range;
            group_range += range;
            
            // 批次满了，发起请求
            if group_range >= max_range {
                let return_data = GetUniswapV3PoolTickBitmapBatchRequest::deploy_builder(
                    provider.clone(), group.clone()
                ).call_raw().block(block_number).await?;
                
                // 解析并存储 tick bitmap
                let bitmaps = <Vec<Vec<U256>>>::abi_decode(&return_data)?;
                for (tick_bitmap, pool_addr) in bitmaps.iter().zip(group.iter()) {
                    // 更新 pool.tick_bitmap
                }
                
                group.clear();
                group_range = 0;
            }
        }
    }
    Ok(())
}
```

**Tick Data 同步优化**:

```rust
async fn sync_tick_data<N, P>(pools: &mut [AMM], block_number: BlockId, provider: P) 
    -> Result<(), AMMError> 
{
    // 1. 并行提取所有初始化的 ticks
    let pool_ticks: Vec<(Address, Vec<i32>)> = pools.par_iter()
        .filter_map(|pool| {
            let initialized_ticks: Vec<i32> = (min_word..=max_word)
                .filter_map(|word_pos| {
                    pool.tick_bitmap.get(&(word_pos as i16))
                        .filter(|&bitmap| *bitmap != U256::ZERO)
                        .map(|&bitmap| (word_pos, bitmap))
                })
                .flat_map(|(word_pos, bitmap)| {
                    (0..256).filter(move |i| (bitmap & (U256::from(1) << i)) != U256::ZERO)
                        .map(move |i| (word_pos * 256 + i) * pool.tick_spacing)
                })
                .collect();
            
            if !initialized_ticks.is_empty() {
                Some((pool.address, initialized_ticks))
            } else {
                None
            }
        })
        .collect();
    
    // 2. 批量请求 tick data
    let max_ticks = 60;  // 每批最多 60 个 ticks
    let mut futures = FuturesUnordered::new();
    
    for (pool_address, ticks) in pool_ticks {
        for chunk in ticks.chunks(max_ticks) {
            let calldata = TickDataInfo {
                pool: pool_address,
                ticks: chunk.to_vec(),
            };
            futures.push(async move {
                GetUniswapV3PoolTickDataBatchRequest::deploy_builder(provider.clone(), vec![calldata])
                    .call_raw().block(block_number).await
            });
        }
    }
    
    // 3. 解析返回数据
    while let Some(res) = futures.next().await {
        let return_data = res?;
        let tick_infos = <Vec<Vec<(bool, u128, i128)>>>::abi_decode(&return_data)?;
        // 更新 pool.ticks
    }
    
    Ok(())
}
```

**性能关键点**:
- **并行提取**：使用 Rayon 并行迭代处理 tick bitmap
- **动态批量**：根据池的 tick 数量动态调整批次大小
- **分段请求**：大池分多次请求，避免单次调用超时

### 3. Moe Liquidity Book

#### 池状态结构

```rust
pub struct MoeLbPair {
    pub address: Address,
    pub token_x: Token,
    pub token_y: Token,
    pub active_id: u32,         // 当前活跃 bin ID
    pub bin_step: u16,          // bin step (价格精度)
    pub reserve_x: u128,
    pub reserve_y: u128,
    pub bins: HashMap<u32, BinReserves>,  // bin reserves
}

pub struct BinReserves {
    pub reserve_x: u128,
    pub reserve_y: u128,
}
```

#### Bin-based 定价模型

Moe 使用离散化的 bin 而非连续的 tick：

```rust
// 价格计算
pub fn get_price_from_id(id: u32, bin_step: u16) -> Result<U256, MoeError> {
    let base = U256::from(10000u32) + U256::from(bin_step);
    let exponent = I256::from_raw(U256::from(id)) - I256::from_raw(U256::from(8388608u32));
    
    // price = (1 + binStep / 10000) ^ (id - 2^23)
    pow(base, exponent)
}

// 交换计算 (Y for X)
pub fn get_amount_out(&self, amount_x_in: U256, bin_id: u32) -> Result<(U256, u32), MoeError> {
    let bin = self.bins.get(&bin_id).ok_or(MoeError::BinNotFound)?;
    
    // 计算该 bin 能接受的最大输入
    let max_amount_in = bin.reserve_x;
    
    if amount_x_in <= max_amount_in {
        // 全部在当前 bin 完成交换
        let amount_y_out = (amount_x_in * U256::from(bin.reserve_y)) / U256::from(bin.reserve_x);
        Ok((amount_y_out, bin_id))
    } else {
        // 需要跨多个 bins
        let amount_y_out_from_this_bin = bin.reserve_y;
        let remaining_amount_x = amount_x_in - max_amount_in;
        
        // 递归到下一个 bin
        let next_bin_id = bin_id + 1;
        let (amount_y_out_from_next, final_bin_id) = 
            self.get_amount_out(remaining_amount_x, next_bin_id)?;
        
        Ok((amount_y_out_from_this_bin + amount_y_out_from_next, final_bin_id))
    }
}
```

#### 事件同步

```rust
fn sync(&mut self, log: &Log) -> Result<(), AMMError> {
    let event_signature = log.topics()[0];
    
    match event_signature {
        IMoeLBPair::Swap::SIGNATURE_HASH => {
            let swap_event = IMoeLBPair::Swap::decode_log(log.as_ref())?;
            self.active_id = swap_event.id.to::<u32>();
            
            // 更新总储备
            self.reserve_x = swap_event.reserveX.to::<u128>();
            self.reserve_y = swap_event.reserveY.to::<u128>();
        }
        IMoeLBPair::TransferBatch::SIGNATURE_HASH => {
            // Liquidity 变化，需要重新同步 bins
            // 这里可以标记为需要重新同步，或者增量更新
        }
        _ => return Err(AMMError::UnrecognizedEventSignature(event_signature)),
    }
    
    Ok(())
}
```

### 4. Agni (V3-like)

Agni 的实现与 Uniswap V3 非常相似，主要差异在于：

1. **合约地址不同**: Factory 和 Router 地址
2. **事件签名可能不同**: 但结构相同
3. **手续费等级**: 可能有不同的预设手续费

实现上基本复用 `UniswapV3Pool` 的逻辑，只是使用不同的合约实例。

## Token 抽象

```rust
pub struct Token {
    pub address: Address,
    pub decimals: u8,
}

impl Token {
    // 异步创建（从链上获取 decimals）
    pub async fn new<N, P>(address: Address, provider: P) -> Result<Self, AMMError> {
        let decimals = IERC20::new(address, provider).decimals().call().await?;
        Ok(Self { address, decimals })
    }
    
    // 同步创建（已知 decimals）
    pub const fn new_with_decimals(address: Address, decimals: u8) -> Self {
        Self { address, decimals }
    }
}

// 批量获取 decimals
pub async fn get_token_decimals<N, P>(
    tokens: Vec<Address>,
    provider: P,
) -> Result<HashMap<Address, u8>, BatchContractError> {
    let step = 765;  // 每次批量处理 765 个 token
    
    let mut futures = FuturesUnordered::new();
    for chunk in tokens.chunks(step) {
        let provider = provider.clone();
        futures.push(async move {
            GetTokenDecimalsBatchRequest::deploy_builder(provider, chunk.to_vec())
                .call_raw().await
        });
    }
    
    let mut token_decimals = HashMap::new();
    let return_type = DynSolType::Array(Box::new(DynSolType::Uint(8)));
    
    while let Some(res) = futures.next().await {
        let return_data = return_type.abi_decode_sequence(&res?)?;
        if let Some(decimals_arr) = return_data.as_array() {
            for (decimals, token_address) in decimals_arr.iter().zip(tokens.iter()) {
                token_decimals.insert(*token_address, decimals.as_uint()?.0.to::<u8>());
            }
        }
    }
    
    Ok(token_decimals)
}
```

## 错误处理

```rust
#[derive(Error, Debug)]
pub enum AMMError {
    #[error(transparent)]
    TransportError(#[from] alloy::transports::RpcError<TransportErrorKind>),
    
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    
    #[error(transparent)]
    UniswapV2Error(#[from] UniswapV2Error),
    
    #[error(transparent)]
    UniswapV3Error(#[from] UniswapV3Error),
    
    #[error(transparent)]
    AgniError(#[from] AgniError),
    
    #[error(transparent)]
    MoeError(#[from] MoeError),
    
    #[error("Unrecognized Event Signature {0}")]
    UnrecognizedEventSignature(FixedBytes<32>),
}

#[derive(Error, Debug)]
pub enum UniswapV2Error {
    #[error("Division by zero")]
    DivisionByZero,
    
    #[error("Rounding Error")]
    RoundingError,
}

#[derive(Error, Debug)]
pub enum UniswapV3Error {
    #[error(transparent)]
    UniswapV3MathError(#[from] UniswapV3MathError),
    
    #[error("Liquidity Underflow")]
    LiquidityUnderflow,
}
```

**错误传播链**:
```
TransportError → AMMError → StateSpaceError → ArbitrageError
```

## 组件内协同

### 1. AMM ↔ Factory

```
Factory (discover) → Vec<AMM> (未初始化)
                   ↓
Factory (sync)   → Vec<AMM> (已同步)
```

Factory 负责创建 AMM 实例，然后批量同步状态。

### 2. Pool ↔ Token

每个 Pool 包含 Token 引用，Token 的 decimals 用于：
- 价格计算的精度调整
- 交换量的单位转换
- UI 显示格式化

### 3. 批量合约 ↔ Pool

自定义 Solidity 批量请求合约减少 RPC 调用：

```solidity
contract GetUniswapV2PoolDataBatchRequest {
    constructor(address[] memory pools) {
        for (uint i = 0; i < pools.length; i++) {
            IUniswapV2Pair pool = IUniswapV2Pair(pools[i]);
            address token0 = pool.token0();
            address token1 = pool.token1();
            (uint112 reserve0, uint112 reserve1, ) = pool.getReserves();
            uint8 decimals0 = IERC20(token0).decimals();
            uint8 decimals1 = IERC20(token1).decimals();
            
            // ... encode and return
        }
    }
}
```

Rust 侧使用 `deploy_builder` 发起批量调用，一次获取所有数据。

## 组件间协同

### 1. AMMs → State Space

State Space 管理 AMM 实例的生命周期：

```
StateSpaceBuilder::new(provider)
    .with_factories(vec![factory1, factory2])
    .sync()
    .await?
    
    ↓
    
factories.discover() → Vec<AMM>
    ↓
factories.sync() → Vec<AMM> (fully synced)
    ↓
StateSpace { state: HashMap<Address, AMM> }
```

### 2. AMMs → Arbitrage

Arbitrage 模块使用 AMM 的 `simulate_swap` 进行路径模拟：

```
PathOptimizer::optimize(path, pools)
    ↓
for each pool in pools {
    amount_out = pool.simulate_swap(token_in, token_out, amount_in)?
    amount_in = amount_out  // 用于下一个池
}
```

### 3. AMMs → Execution

Execution 模块根据 AMM 类型构造不同的交易：

```
match amm {
    AMM::UniswapV2Pool(_) => {
        // 构造 V2 swap calldata
        pool.swap(amount0_out, amount1_out, to, data)
    }
    AMM::UniswapV3Pool(_) => {
        // 构造 V3 swap calldata (通过 Router)
        router.exactInputSingle(params)
    }
    AMM::MoeLbPair(_) => {
        // 构造 Moe swap calldata (通过 Router)
        router.swapExactTokensForTokens(amount, path)
    }
}
```

## 性能优化总结

### 1. 批量处理

| 操作 | 单次数量 | RPC 调用减少 |
|------|---------|-------------|
| V2 Pool Data | 120 pools | ~120x |
| V3 Slot0 | 255 pools | ~255x |
| Token Decimals | 765 tokens | ~765x |
| Tick Bitmaps | 6900 words | ~数百倍 |
| Tick Data | 60 ticks | ~60x |

### 2. 并发请求

使用 `FuturesUnordered` 并发发起多个批次的请求，充分利用网络带宽。

### 3. 增量同步

只在事件触发时更新池状态，避免全量刷新：

```
Event: Swap/Mint/Burn
    ↓
amm.sync(&log)  // O(1) 更新
```

### 4. 内存优化

- 使用 `HashMap` 而非 `Vec` 存储 ticks 和 bins（稀疏数据）
- 使用 `u128` 而非 `U256` 存储 reserves（节省内存）
- 使用 `i32` 而非 `i64` 存储 tick（范围足够）

## 测试策略

### 1. 单元测试

每个协议的 `simulate_swap` 都有对应的单元测试，与链上 Quoter 合约对比：

```rust
#[tokio::test]
async fn test_simulate_swap_usdc_weth() -> eyre::Result<()> {
    let pool = UniswapV3Pool::new(pool_address)
        .init(BlockId::latest(), provider.clone())
        .await?;
    
    let amount_out = pool.simulate_swap(pool.token_a.address, Address::default(), amount_in)?;
    
    let expected_amount_out = quoter
        .quoteExactInputSingle(pool.token_a.address, pool.token_b.address, pool.fee, amount_in, U160::ZERO)
        .call()
        .await?;
    
    assert_eq!(amount_out, expected_amount_out);
}
```

### 2. 集成测试

测试完整的 discover → sync 流程：

```rust
#[tokio::test]
async fn test_factory_discover_and_sync() -> eyre::Result<()> {
    let factory = UniswapV3Factory::new(factory_address, creation_block);
    let pools = factory.discover(to_block, provider.clone()).await?;
    let synced_pools = factory.sync(pools, to_block, provider.clone()).await?;
    
    assert!(synced_pools.iter().all(|pool| pool.liquidity > 0));
}
```

### 3. 基准测试

性能基准测试位于 `benches/` 目录：

```rust
fn bench_uniswap_v2_simulate_swap(c: &mut Criterion) {
    let pool = /* ... */;
    c.bench_function("uniswap_v2_simulate_swap", |b| {
        b.iter(|| pool.simulate_swap(token_in, token_out, amount_in))
    });
}
```

## 总结

AMMs 模块通过以下设计实现了高性能和可扩展性：

1. **统一抽象**: `AutomatedMarketMaker` trait 提供协议无关接口
2. **批量优化**: 自定义 Solidity 合约减少 RPC 调用数百倍
3. **并发处理**: 异步并发同步多个池
4. **精确计算**: 使用 U256 避免浮点误差
5. **增量同步**: 事件驱动的状态更新
6. **类型安全**: Enum-based 多态，编译时检查

该模块为上层提供了可靠、高效的池状态管理和交换模拟能力。

