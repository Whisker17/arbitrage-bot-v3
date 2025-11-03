# State Space 模块 - 链上状态管理

## 模块概述

State Space 模块负责管理所有 AMM 池的链上状态，提供高效的状态同步、reorg 处理、过滤机制和实时订阅功能。它是连接区块链事件和套利逻辑的关键桥梁。

## 目录结构

```
src/state_space/
├── mod.rs           # 模块入口、StateSpace 和 StateSpaceManager
├── discovery.rs     # 池发现管理器
├── cache.rs         # 状态变更缓存（支持 reorg 回滚）
├── error.rs         # 错误类型定义
└── filters/         # 池过滤器
    ├── mod.rs       # 过滤器 trait 和枚举
    ├── blacklist.rs # 黑名单过滤
    ├── whitelist.rs # 白名单过滤
    └── value.rs     # 价值过滤
```

## 核心设计思路

### 1. 分层架构

```
┌──────────────────────────────────┐
│   StateSpaceManager (管理层)      │
│  - 订阅区块事件                   │
│  - 触发状态同步                   │
│  - 提供查询接口                   │
└────────────┬─────────────────────┘
             │
             ▼
┌──────────────────────────────────┐
│   StateSpace (存储层)             │
│  - HashMap<Address, AMM>         │
│  - StateChangeCache              │
│  - 状态同步逻辑                   │
└────────────┬─────────────────────┘
             │
             ▼
┌──────────────────────────────────┐
│   AMM (数据层)                    │
│  - 池的详细状态                   │
│  - 交换模拟                       │
└──────────────────────────────────┘
```

### 2. 构建器模式

`StateSpaceBuilder` 提供灵活的状态空间构建：

```rust
pub struct StateSpaceBuilder<N, P> {
    pub provider: P,
    pub latest_block: u64,
    pub factories: Vec<Factory>,
    pub amms: Vec<AMM>,
    pub filters: Vec<PoolFilter>,
    phantom: PhantomData<N>,
}

impl<N, P> StateSpaceBuilder<N, P> {
    pub fn new(provider: P) -> Self { /* ... */ }
    
    pub fn block(self, latest_block: u64) -> Self { /* ... */ }
    
    pub fn with_factories(self, factories: Vec<Factory>) -> Self { /* ... */ }
    
    pub fn with_amms(self, amms: Vec<AMM>) -> Self { /* ... */ }
    
    pub fn with_filters(self, filters: Vec<PoolFilter>) -> Self { /* ... */ }
    
    pub async fn sync(self) -> Result<StateSpaceManager<N, P>, AMMError> { /* ... */ }
}
```

**使用示例**:

```rust
let state_manager = StateSpaceBuilder::new(provider)
    .with_factories(vec![
        UniswapV2Factory::new(factory_addr, fee, creation_block).into(),
        UniswapV3Factory::new(factory_addr, creation_block).into(),
    ])
    .with_filters(vec![
        BlacklistFilter::new(blacklisted_pools).into(),
        TokenWhitelistFilter::new(whitelisted_tokens).into(),
    ])
    .sync()
    .await?;
```

## 核心数据结构

### 1. StateSpace

```rust
#[derive(Debug, Default, Clone)]
pub struct StateSpace {
    pub state: HashMap<Address, AMM>,          // 池地址 → AMM 实例
    pub latest_block: Arc<AtomicU64>,          // 最新同步的区块号
    cache: StateChangeCache<CACHE_SIZE>,       // 状态变更缓存（30 个区块）
}

impl StateSpace {
    // 获取池（只读）
    pub fn get(&self, address: &Address) -> Option<&AMM> {
        self.state.get(address)
    }
    
    // 获取池（可变）
    pub fn get_mut(&mut self, address: &Address) -> Option<&mut AMM> {
        self.state.get_mut(address)
    }
    
    // 同步日志事件
    pub fn sync(&mut self, logs: &[Log]) -> Result<Vec<Address>, StateSpaceError> {
        // ...
    }
}
```

**设计要点**:
- 使用 `HashMap` 实现 O(1) 池查找
- `Arc<AtomicU64>` 支持并发读取最新区块号
- 环形缓存限制为 30 个区块（避免内存无限增长）

### 2. StateSpaceManager

```rust
#[derive(Clone)]
pub struct StateSpaceManager<N, P> {
    pub state: Arc<RwLock<StateSpace>>,        // 共享状态（读写锁）
    pub latest_block: Arc<AtomicU64>,          // 最新区块号（原子）
    pub block_filter: Filter,                  // 事件过滤器
    pub provider: P,                           // Provider
    phantom: PhantomData<N>,
}

impl<N, P> StateSpaceManager<N, P> {
    // 订阅区块更新
    pub async fn subscribe(&self) -> Result<
        Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
        StateSpaceError,
    > {
        // ...
    }
}
```

**并发设计**:
- `Arc<RwLock<StateSpace>>`: 支持多读单写
- `Arc<AtomicU64>`: 无锁读取最新区块号
- `Clone`: Manager 可以安全地跨线程共享

### 3. StateChangeCache

```rust
#[derive(Debug, Clone)]
pub struct StateChangeCache<const CAP: usize> {
    oldest_block: u64,                          // 最旧缓存的区块号
    cache: ArrayDeque<StateChange, CAP>,        // 环形缓存
}

#[derive(Debug, Clone)]
pub struct StateChange {
    pub state_change: Vec<AMM>,                 // 该区块中变化的池（变化前的状态）
    pub block_number: u64,                      // 区块号
}

impl<const CAP: usize> StateChangeCache<CAP> {
    pub fn push(&mut self, state_change: StateChange) {
        if self.cache.is_full() {
            self.cache.pop_back();  // 移除最旧的
            self.oldest_block = self.cache.back().unwrap().block_number;
        }
        self.cache.push_front(state_change).unwrap();
    }
    
    // 回滚到指定区块
    pub fn unwind_state_changes(&mut self, block_to_unwind: u64) -> Vec<AMM> {
        // 找到需要回滚的区块范围
        let pivot_idx = self.cache.iter()
            .position(|sc| sc.block_number < block_to_unwind);
        
        let state_changes = if let Some(pivot_idx) = pivot_idx {
            self.cache.drain(..pivot_idx).collect()
        } else {
            self.cache.drain(..).collect()
        };
        
        // 扁平化状态变更（保留每个池最早的状态）
        self.flatten_state_changes(state_changes)
    }
    
    fn flatten_state_changes(&self, state_changes: Vec<StateChange>) -> Vec<AMM> {
        state_changes.into_iter()
            .rev()  // 从最旧的开始
            .fold(HashMap::new(), |mut amms, state_change| {
                for amm in state_change.state_change {
                    // 只保留最早的状态（如果已存在则不覆盖）
                    amms.entry(amm.address()).or_insert(amm);
                }
                amms
            })
            .into_values()
            .collect()
    }
}
```

**Reorg 处理原理**:

假设当前在区块 100，缓存了区块 95-100 的状态变更：

```
Blocks:  95  96  97  98  99  100
Cache:   [A] [B] [C] [D] [E] [F]
         ↑                      ↑
     oldest              latest
```

如果检测到 reorg，需要回滚到区块 97：

1. 找到需要回滚的区块：98, 99, 100
2. 提取这些区块的状态变更：[D, E, F]
3. 扁平化（从旧到新遍历）：
   - 区块 100 (F): Pool X → 状态 X100
   - 区块 99 (E): Pool X → 状态 X99 (忽略，已有 X100)
   - 区块 98 (D): Pool Y → 状态 Y98
4. 返回 {Pool X: X100, Pool Y: Y98}
5. 恢复到主状态空间

**为什么从最旧开始遍历？**
- 缓存中存储的是**变化前**的状态
- 最旧的状态是最接近回滚目标的状态

## 状态同步流程

### 1. 初始化同步

```rust
pub async fn sync(self) -> Result<StateSpaceManager<N, P>, AMMError> {
    let chain_tip = BlockId::from(self.provider.get_block_number().await?);
    let factories = self.factories.clone();
    let mut futures = FuturesUnordered::new();
    
    // 1. 收集所有需要监听的事件
    let mut filter_set = HashSet::new();
    for factory in &self.factories {
        for event in factory.pool_events() {
            filter_set.insert(event);
        }
    }
    for amm in self.amms.iter() {
        for event in amm.sync_events() {
            filter_set.insert(event);
        }
    }
    
    // 2. 创建事件过滤器
    let block_filter = Filter::new()
        .event_signature(FilterSet::from(filter_set.into_iter().collect()));
    
    // 3. 按协议分组池
    let mut amm_variants: HashMap<Variant, Vec<AMM>> = HashMap::new();
    for amm in self.amms {
        amm_variants.entry(amm.variant()).or_default().push(amm);
    }
    
    // 4. 并发发现和同步各工厂的池
    for factory in factories {
        let provider = self.provider.clone();
        let filters = self.filters.clone();
        let extension = amm_variants.remove(&factory.variant());
        
        futures.push(tokio::spawn(async move {
            // a. 发现池
            let mut discovered_amms = factory.discover(chain_tip, provider.clone()).await?;
            
            // b. 合并手动添加的池
            if let Some(manual_amms) = extension {
                discovered_amms.extend(manual_amms);
            }
            
            // c. 应用发现阶段过滤器
            for filter in filters.iter() {
                if filter.stage() == FilterStage::Discovery {
                    discovered_amms = filter.filter(discovered_amms).await?;
                }
            }
            
            // d. 同步池状态
            discovered_amms = factory.sync(discovered_amms, chain_tip, provider).await?;
            
            // e. 应用同步阶段过滤器
            for filter in filters.iter() {
                if filter.stage() == FilterStage::Sync {
                    discovered_amms = filter.filter(discovered_amms).await?;
                }
            }
            
            Ok::<Vec<AMM>, AMMError>(discovered_amms)
        }));
    }
    
    // 5. 收集所有同步后的池
    let mut state_space = StateSpace::default();
    while let Some(res) = futures.next().await {
        let synced_amms = res??;
        for amm in synced_amms {
            state_space.state.insert(amm.address(), amm);
        }
    }
    
    // 6. 同步剩余的手动添加的池
    for (_, remaining_amms) in amm_variants.drain() {
        for mut amm in remaining_amms {
            let address = amm.address();
            amm = amm.init(chain_tip, self.provider.clone()).await?;
            state_space.state.insert(address, amm);
        }
    }
    
    // 7. 创建 Manager
    Ok(StateSpaceManager {
        latest_block: Arc::new(AtomicU64::new(self.latest_block)),
        state: Arc::new(RwLock::new(state_space)),
        block_filter,
        provider: self.provider,
        phantom: PhantomData,
    })
}
```

**性能优化**:
- 并发发现和同步不同协议的池（`tokio::spawn`）
- 两阶段过滤减少不必要的同步开销
- 共享事件过滤器（避免重复监听）

### 2. 增量同步

```rust
impl StateSpace {
    pub fn sync(&mut self, logs: &[Log]) -> Result<Vec<Address>, StateSpaceError> {
        let latest = self.latest_block.load(Ordering::Relaxed);
        let Some(mut block_number) = logs.first()
            .map(|log| log.block_number.ok_or(StateSpaceError::MissingBlockNumber))
            .transpose()? 
        else {
            return Ok(vec![]);
        };
        
        // 1. 检测 reorg
        if latest >= block_number {
            info!(
                target: "state_space::sync",
                from = %latest,
                to = %block_number - 1,
                "Unwinding state changes (reorg detected)"
            );
            
            let cached_state = self.cache.unwind_state_changes(block_number);
            for amm in cached_state {
                self.state.insert(amm.address(), amm);
            }
        }
        
        // 2. 逐日志同步
        let mut cached_amms = HashSet::new();
        let mut affected_amms = HashSet::new();
        
        for log in logs {
            let log_block_number = log.block_number
                .ok_or(StateSpaceError::MissingBlockNumber)?;
            
            // 如果区块号变化，缓存上一个区块的状态变更
            if log_block_number != block_number {
                let amms = cached_amms.drain().collect::<Vec<AMM>>();
                affected_amms.extend(amms.iter().map(|amm| amm.address()));
                self.cache.push(StateChange::new(amms, block_number));
                block_number = log_block_number;
            }
            
            // 如果池在状态空间中，缓存旧状态并同步
            let address = log.address();
            if let Some(amm) = self.state.get_mut(&address) {
                cached_amms.insert(amm.clone());  // 缓存旧状态
                amm.sync(log)?;                    // 更新新状态
            }
        }
        
        // 3. 缓存最后一个区块的状态变更
        if !cached_amms.is_empty() {
            let amms = cached_amms.drain().collect::<Vec<AMM>>();
            affected_amms.extend(amms.iter().map(|amm| amm.address()));
            self.cache.push(StateChange::new(amms, block_number));
        }
        
        Ok(affected_amms.into_iter().collect())
    }
}
```

**关键设计点**:
- **Reorg 检测**: 通过比较区块号判断
- **状态缓存**: 更新前先克隆旧状态
- **增量更新**: 只更新有事件的池
- **受影响池列表**: 返回本次同步中变化的池地址

### 3. 实时订阅

```rust
pub async fn subscribe(&self) -> Result<
    Pin<Box<dyn Stream<Item = Result<Vec<Address>, StateSpaceError>> + Send>>,
    StateSpaceError,
> {
    let provider = self.provider.clone();
    let latest_block = self.latest_block.clone();
    let state = self.state.clone();
    let mut block_filter = self.block_filter.clone();
    
    // 订阅新区块
    let block_stream = provider.subscribe_blocks().await?.into_stream();
    
    Ok(Box::pin(stream! {
        tokio::pin!(block_stream);
        
        while let Some(block) = block_stream.next().await {
            let block_number = block.number();
            
            // 更新过滤器到新区块
            block_filter = block_filter.select(block_number);
            
            // 获取该区块的日志
            let logs = provider.get_logs(&block_filter).await?;
            
            // 同步状态
            let affected_amms = state.write().await.sync(&logs)?;
            
            // 更新最新区块号
            latest_block.store(block_number, Ordering::Relaxed);
            
            yield Ok(affected_amms);
        }
    }))
}
```

**流式处理**:
- 使用 `async_stream::stream!` 宏创建异步流
- 每个新区块触发一次状态同步
- 返回受影响的池地址列表

## 过滤器系统

### 1. 过滤器 Trait

```rust
#[async_trait]
pub trait AMMFilter {
    async fn filter(&self, amms: Vec<AMM>) -> Result<Vec<AMM>, AMMError>;
    fn stage(&self) -> FilterStage;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterStage {
    Discovery,  // 发现后立即过滤（减少同步开销）
    Sync,       // 同步后过滤（需要完整状态）
}

pub enum PoolFilter {
    BlacklistFilter(BlacklistFilter),
    PoolWhitelistFilter(PoolWhitelistFilter),
    TokenWhitelistFilter(TokenWhitelistFilter),
    // ValueFilter(ValueFilter),  // 可扩展
}
```

### 2. 黑名单过滤器

```rust
#[derive(Debug, Clone)]
pub struct BlacklistFilter {
    pub blacklisted_pools: HashSet<Address>,
    pub stage: FilterStage,
}

#[async_trait]
impl AMMFilter for BlacklistFilter {
    async fn filter(&self, amms: Vec<AMM>) -> Result<Vec<AMM>, AMMError> {
        Ok(amms.into_iter()
            .filter(|amm| !self.blacklisted_pools.contains(&amm.address()))
            .collect())
    }
    
    fn stage(&self) -> FilterStage {
        self.stage
    }
}
```

**使用场景**:
- 排除已知有问题的池
- 排除流动性极低的池
- 排除手续费过高的池

### 3. 白名单过滤器

```rust
#[derive(Debug, Clone)]
pub struct PoolWhitelistFilter {
    pub whitelisted_pools: HashSet<Address>,
    pub stage: FilterStage,
}

#[derive(Debug, Clone)]
pub struct TokenWhitelistFilter {
    pub whitelisted_tokens: HashSet<Address>,
    pub stage: FilterStage,
}

#[async_trait]
impl AMMFilter for TokenWhitelistFilter {
    async fn filter(&self, amms: Vec<AMM>) -> Result<Vec<AMM>, AMMError> {
        Ok(amms.into_iter()
            .filter(|amm| {
                amm.tokens().iter()
                    .all(|token| self.whitelisted_tokens.contains(token))
            })
            .collect())
    }
    
    fn stage(&self) -> FilterStage {
        self.stage
    }
}
```

**使用场景**:
- 只关注主流代币（WMNT, USDC, USDT, WETH）
- 只监控特定的池子
- 减少状态空间大小

### 4. 价值过滤器（待实现）

```rust
pub struct ValueFilter {
    pub min_liquidity_usd: f64,
    pub provider: P,
    pub oracle: PriceOracle,
    pub stage: FilterStage,
}

#[async_trait]
impl AMMFilter for ValueFilter {
    async fn filter(&self, amms: Vec<AMM>) -> Result<Vec<AMM>, AMMError> {
        let mut filtered = vec![];
        for amm in amms {
            let liquidity_usd = self.oracle.estimate_liquidity_usd(&amm, self.provider).await?;
            if liquidity_usd >= self.min_liquidity_usd {
                filtered.push(amm);
            }
        }
        Ok(filtered)
    }
    
    fn stage(&self) -> FilterStage {
        FilterStage::Sync  // 需要完整状态才能计算价值
    }
}
```

## 并发安全设计

### 1. 读写锁策略

```rust
// 读取状态（共享）
let state_guard = state_manager.state.read().await;
let pool = state_guard.get(&pool_address);
// 多个读者可以并发访问

// 写入状态（独占）
let mut state_guard = state_manager.state.write().await;
state_guard.sync(&logs)?;
// 只有一个写者，阻塞所有读者
```

### 2. 原子操作

```rust
// 无锁读取最新区块号
let latest = state_manager.latest_block.load(Ordering::Relaxed);

// 无锁更新最新区块号
state_manager.latest_block.store(new_block, Ordering::Relaxed);
```

### 3. Arc 共享

```rust
// StateSpaceManager 可以安全地跨线程共享
let manager1 = state_manager.clone();
let manager2 = state_manager.clone();

tokio::spawn(async move {
    let state = manager1.state.read().await;
    // ...
});

tokio::spawn(async move {
    let state = manager2.state.read().await;
    // ...
});
```

## 错误处理

```rust
#[derive(Error, Debug)]
pub enum StateSpaceError {
    #[error(transparent)]
    AMMError(#[from] AMMError),
    
    #[error("Missing block number in log")]
    MissingBlockNumber,
}
```

**错误传播**:
```
AMMError (底层) → StateSpaceError → ArbitrageError (上层)
```

## Discovery Manager（可选）

```rust
#[derive(Debug, Default, Clone)]
pub struct DiscoveryManager {
    pub factories: HashMap<Address, Factory>,
    pub pool_filters: Option<Vec<PoolFilter>>,
    pub token_decimals: HashMap<Address, u8>,
}

impl DiscoveryManager {
    pub fn new(factories: Vec<Factory>) -> Self {
        let factories = factories.into_iter()
            .map(|factory| (factory.address(), factory))
            .collect();
        Self { factories, ..Default::default() }
    }
    
    pub fn with_pool_filters(self, pool_filters: Vec<PoolFilter>) -> Self {
        Self { pool_filters: Some(pool_filters), ..self }
    }
    
    pub fn disc_events(&self) -> HashSet<FixedBytes<32>> {
        self.factories.iter()
            .map(|(_, factory)| factory.discovery_event())
            .collect()
    }
}
```

**用途**:
- 集中管理多个工厂
- 预加载 token decimals
- 统一过滤策略

## 组件内协同

### 1. StateSpaceBuilder ↔ StateSpace

```
Builder.with_factories()
    ↓
Builder.sync()
    ↓
factories.discover() → Vec<AMM>
    ↓
factories.sync() → Vec<AMM>
    ↓
StateSpace { state: HashMap<Address, AMM> }
```

### 2. StateSpace ↔ StateChangeCache

```
Event → StateSpace.sync()
    ↓
cache old state → StateChangeCache.push()
    ↓
update new state → state.insert()
    ↓
Reorg detected → StateChangeCache.unwind()
    ↓
restore old state → state.insert()
```

### 3. StateSpaceManager ↔ StateSpace

```
Manager.subscribe()
    ↓
block_stream.next()
    ↓
provider.get_logs()
    ↓
state.write().sync(&logs)
    ↓
yield affected_amms
```

## 组件间协同

### 1. State Space → Arbitrage

```
ArbitrageMonitor::new(provider, config)
    ↓
StateSpaceBuilder::sync() → StateSpaceManager
    ↓
monitor.state_manager.state.clone()
    ↓
ArbitrageMonitor { state: Arc<RwLock<StateSpace>> }
    ↓
monitor.opportunistic_scan()
    ↓
state.read().await → &StateSpace
    ↓
build_graph(&state) → PoolGraph
```

### 2. State Space → Execution

```
Execution 不直接访问 StateSpace
    ↓
Arbitrage 提供 pools_for_path() → Vec<AMM>
    ↓
Executor.execute(pools)
```

### 3. State Space ← AMMs

```
Factory.discover() → Vec<AMM>
    ↓
Factory.sync() → Vec<AMM>
    ↓
StateSpace.state.insert(amm.address(), amm)
    ↓
Event → AMM.sync(&log)
    ↓
StateSpace.state.get_mut(&address).unwrap().sync(&log)
```

## 性能优化

### 1. 批量操作

```rust
// 不要逐个同步
for log in logs {
    state.sync(&[log])?;  // ❌ 每次都创建缓存
}

// 批量同步
state.sync(&logs)?;  // ✅ 一次性处理所有日志
```

### 2. 增量更新

```rust
// 不要全量刷新
let all_pools = factory.discover().await?;
let synced = factory.sync(all_pools).await?;  // ❌ 重新同步所有池

// 增量更新
for log in logs {
    if let Some(amm) = state.get_mut(&log.address()) {
        amm.sync(&log)?;  // ✅ 只更新变化的池
    }
}
```

### 3. 并发读取

```rust
// 多个任务并发读取状态
let state1 = state_manager.state.clone();
let state2 = state_manager.state.clone();

let (result1, result2) = tokio::join!(
    async { state1.read().await.get(&addr1) },
    async { state2.read().await.get(&addr2) },
);
```

### 4. 内存管理

```rust
// 限制缓存大小
pub const CACHE_SIZE: usize = 30;  // 只缓存 30 个区块

// 环形缓存自动淘汰
impl StateChangeCache<CAP> {
    pub fn push(&mut self, state_change: StateChange) {
        if self.cache.is_full() {
            self.cache.pop_back();  // 移除最旧的
        }
        self.cache.push_front(state_change).unwrap();
    }
}
```

## 测试策略

### 1. Reorg 测试

```rust
#[test]
fn test_state_change_cache_unwind() {
    let mut cache = StateChangeCache::<5>::new();
    
    // 模拟区块 95-99 的状态变更
    cache.push(StateChange::new(vec![pool_x_at_99], 99));
    cache.push(StateChange::new(vec![pool_y_at_98], 98));
    cache.push(StateChange::new(vec![pool_x_at_97], 97));
    
    // 回滚到区块 97
    let unwound = cache.unwind_state_changes(97);
    
    // 应该恢复 Pool X 在区块 97 的状态
    assert_eq!(unwound.len(), 2);
    assert!(unwound.iter().any(|amm| amm.address() == pool_x.address()));
}
```

### 2. 订阅测试

```rust
#[tokio::test]
async fn test_state_space_subscribe() {
    let manager = StateSpaceBuilder::new(provider)
        .with_factories(factories)
        .sync()
        .await?;
    
    let mut stream = manager.subscribe().await?;
    
    // 等待第一个事件
    let affected_amms = stream.next().await.unwrap()?;
    
    assert!(!affected_amms.is_empty());
}
```

### 3. 并发测试

```rust
#[tokio::test]
async fn test_concurrent_reads() {
    let manager = StateSpaceBuilder::new(provider).sync().await?;
    
    let handles: Vec<_> = (0..10).map(|_| {
        let manager = manager.clone();
        tokio::spawn(async move {
            let state = manager.state.read().await;
            state.state.len()
        })
    }).collect();
    
    let results = futures::future::join_all(handles).await;
    
    // 所有任务应该读取到相同的状态
    let counts: HashSet<_> = results.into_iter()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(counts.len(), 1);
}
```

## 总结

State Space 模块通过以下设计实现了高效的链上状态管理：

1. **Reorg 容忍**: 环形缓存支持自动回滚
2. **增量同步**: 只更新变化的池，避免全量刷新
3. **并发安全**: 读写锁 + 原子操作
4. **过滤机制**: 两阶段过滤减少同步开销
5. **实时订阅**: 流式处理新区块事件
6. **内存优化**: 限制缓存大小，及时淘汰旧数据
7. **可扩展性**: Trait-based 过滤器，易于添加新策略

该模块为套利系统提供了可靠、实时、高效的链上状态视图。

