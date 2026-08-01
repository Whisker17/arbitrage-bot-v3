# 模块集成与协同 - 系统工作流程

## 概述

本文档详细说明各模块之间的协同工作方式，展示从链上事件到套利执行的完整数据流和控制流。

## 整体架构回顾

```
┌─────────────────────────────────────────────────────────────┐
│                    Application Layer                        │
│              (Main Loop / ArbitrageMonitor)                 │
└──────────┬────────────────────┬──────────────────┬──────────┘
           │                    │                  │
           ▼                    ▼                  ▼
    ┌────────────┐      ┌──────────────┐   ┌──────────────┐
    │ Arbitrage  │◄─────┤State Space   │   │  Execution   │
    │  Module    │      │   Manager    │   │   Module     │
    └──────┬─────┘      └──────┬───────┘   └──────┬───────┘
           │                   │                   │
           └───────────────────┼───────────────────┘
                               │
                               ▼
                      ┌────────────────┐
                      │  AMMs Module   │
                      └────────────────┘
```

## 完整工作流程

### 阶段 1: 系统初始化

```
1. 加载配置
   ├── RPC Provider URL
   ├── Factory 地址和创建区块
   ├── 路径约束 (max_length, required_token)
   ├── 优化配置 (max_iterations, tolerance)
   └── 执行配置 (gas_limit, slippage)

2. 构建 StateSpaceManager
   ├── StateSpaceBuilder::new(provider)
   ├── with_factories(factories)
   ├── with_filters(filters)
   └── sync()
       ├── 并发发现池: factory.discover()
       │   └── 查询 PoolCreated 事件
       ├── 应用发现阶段过滤器
       ├── 批量同步池状态: factory.sync()
       │   ├── sync_slot_0()
       │   ├── sync_token_decimals()
       │   ├── sync_tick_bitmaps()
       │   └── sync_tick_data()
       ├── 应用同步阶段过滤器
       └── 创建 StateSpace { state: HashMap<Address, AMM> }

3. 创建 ArbitrageMonitor
   ├── 持有 StateSpaceManager 引用
   ├── 创建 PathOptimizer
   └── 准备事件订阅
```

**初始化代码示例**:

```rust
// 1. 配置
let factories = vec![
    UniswapV2Factory::new(factory_addr, 300, creation_block).into(),
    UniswapV3Factory::new(factory_addr, creation_block).into(),
];

let filters = vec![
    TokenWhitelistFilter::new(vec![WMNT, USDC, USDT, WETH]).into(),
];

let constraints = PathConstraints::settlement_cycle(WMNT, DEFAULT_MAX_HOPS);

// 2. 构建状态空间
let state_manager = StateSpaceBuilder::new(provider.clone())
    .with_factories(factories)
    .with_filters(filters)
    .sync()
    .await?;

// 3. 创建套利监控器
let monitor = ArbitrageMonitor::new(
    provider.clone(),
    MonitorConfig {
        factories,
        constraints,
        optimization: OptimizationConfig::default(),
        ..Default::default()
    }
).await?;
```

### 阶段 2: 实时监控与事件处理

```
Main Loop
    │
    ├─► subscribe_blocks()
    │      ↓
    │   New Block
    │      ↓
    ├─► StateSpaceManager.subscribe()
    │      ↓
    │   Get Logs (filter by event signatures)
    │      ↓
    │   StateSpace.sync(&logs)
    │      ├── 检测 Reorg
    │      │   └─► StateChangeCache.unwind()
    │      ├── 逐日志更新池状态
    │      │   └─► AMM.sync(log)
    │      └── 返回受影响的池地址
    │      ↓
    │   yield Vec<Address>  // 受影响的池
    │
    └─► ArbitrageMonitor.handle_updates(affected_pools)
           ↓
        refresh_graph()
           ↓
        opportunistic_scan()
```

**实时监控代码示例**:

```rust
let mut event_stream = monitor.subscribe().await?;

while let Some(affected_pools) = event_stream.next().await {
    let affected_pools = affected_pools?;
    
    info!(
        target: "main",
        affected_count = affected_pools.len(),
        "Pools updated"
    );
    
    // 处理更新
    monitor.handle_updates(affected_pools).await?;
    
    // 扫描套利机会
    let result = monitor.opportunistic_scan().await?;
    
    if !result.opportunities.is_empty() {
        info!(
            target: "main",
            block = result.block_number,
            opportunities = result.opportunities.len(),
            "Found arbitrage opportunities"
        );
        
        // 执行套利（见阶段 3）
    }
}
```

### 阶段 3: 套利机会发现

```
ArbitrageMonitor.opportunistic_scan()
    │
    ├─► 1. 读取 StateSpace
    │      ↓
    │   state.read().await
    │      ↓
    │   HashMap<Address, AMM>
    │
    ├─► 2. 构建交易图
    │      ↓
    │   build_graph(state_space)
    │      ├── for each pool in state_space
    │      │   ├── extract_pool(amm) → PoolExtraction
    │      │   └── into_edges() → (PoolEdge, PoolEdge)
    │      └── PoolGraph { graph: DiGraph<Address, PoolEdge> }
    │
    ├─► 3. 搜索套利路径
    │      ↓
    │   PathFinder::new(graph, constraints)
    │      └── find_cycles()
    │          └── BFS 搜索闭环结算循环（WHI-529；开放 misprice 已删除）
    │      ↓
    │   Vec<ArbitragePath>
    │
    ├─► 4. 优化每条路径
    │      ↓
    │   for path in paths
    │      ├── pools_for_path(path) → Vec<AMM>
    │      ├── PathOptimizer.optimize(path, pools)
    │      │   └── Binary search for optimal input
    │      │       ├── simulate_path(amount_in)
    │      │       │   └── for each hop: AMM.simulate_swap()
    │      │       └── if profit > min_profit: keep
    │      └── OptimizationResult { optimal_input, expected_profit }
    │
    └─► 5. 记录日志并返回
           ↓
        OpportunisticScanResult { opportunities }
```

**路径搜索详细流程**:

```
find_cycles() - BFS 搜索
    │
    ├─► For each start node (token)
    │      │
    │      ├─► 初始化队列: (node, path[], seen_tokens)
    │      │
    │      └─► While queue not empty
    │             │
    │             ├─► 弹出 (current_node, path, seen)
    │             │
    │             ├─► For each neighbor
    │             │      │
    │             │      ├─► If neighbor == start
    │             │      │      └─► 找到循环！
    │             │      │          └─► 添加到 cycles
    │             │      │
    │             │      └─► Else if not seen
    │             │             └─► 入队 (neighbor, path + edge, seen + neighbor)
    │             │
    │             └─► 继续
    │
    └─► 去重并返回 Vec<ArbitragePath>
```

### 阶段 4: 路径优化

```
PathOptimizer.optimize(path, pools)
    │
    ├─► 初始化二分搜索
    │      ├── low = 0
    │      ├── high = max_input (e.g., 10^24)
    │      └── best_result = None
    │
    ├─► For iteration in 0..max_iterations
    │      │
    │      ├─► mid = (low + high) / 2
    │      │
    │      ├─► simulate_path(path, pools, mid)
    │      │      │
    │      │      ├─► current_amount = mid
    │      │      │
    │      │      ├─► For each (hop, pool) in zip(path.hops, pools)
    │      │      │      ├── output = pool.simulate_swap(
    │      │      │      │      hop.token_in,
    │      │      │      │      hop.token_out,
    │      │      │      │      current_amount
    │      │      │      │   )
    │      │      │      └── current_amount = output
    │      │      │
    │      │      ├─► expected_profit = current_amount - mid
    │      │      │
    │      │      └─► Return OptimizationResult { mid, profit, current_amount }
    │      │
    │      ├─► If profit > min_profit
    │      │      ├── best_result = Some(result)
    │      │      └── low = mid + 1  // 尝试更大的输入
    │      │
    │      └─► Else
    │             └── high = mid - 1  // 减小输入
    │
    └─► Return best_result
```

**优化算法可视化**:

```
Profit vs Input Amount

Profit ▲
       │        *  ← optimal
       │      *   *
       │    *       *
       │  *           *
       │*               *
       └──────────────────────► Amount In
       0                    max_input
       
Iteration 1: mid = max_input / 2
Iteration 2: mid = 3/4 * max_input  (profit increased)
Iteration 3: mid = 7/8 * max_input  (profit increased)
...
Iteration N: converged to optimal
```

### 阶段 5: 交易执行

```
Execute Arbitrage Opportunity
    │
    ├─► 1. 构建执行参数
    │      │
    │      ├─► Executor.build_params(opportunity)
    │      │      │
    │      │      ├─► 构造 token_path 和 pool_addresses
    │      │      │
    │      │      ├─► 从链上查询当前储备
    │      │      │   └─► for each pool: pool.getReserves()
    │      │      │
    │      │      ├─► 计算每一跳的预期输出
    │      │      │   └─► Uniswap V2 formula
    │      │      │
    │      │      ├─► 计算 min_amount_out
    │      │      │   ├── 应用滑点容忍度
    │      │      │   └── 包含 Gas 成本（可选）
    │      │      │
    │      │      └─► ExecutionParams
    │      │
    │      └─► ExecutionParams { amount_in, token_path, pools, min_out, ... }
    │
    ├─► 2. 计算 Gas 费用
    │      │
    │      ├─► compute_fee_plan(hops, net_profit)
    │      │      │
    │      │      ├─► 根据跳数确定 Gas limit
    │      │      │   └─► 2 hops: 450M, 4 hops: 750M
    │      │      │
    │      │      ├─► 根据利润设置全局上限
    │      │      │   ├── < 0.1 WMNT: 0.5 gwei
    │      │      │   ├── 0.1-1 WMNT: 0.5 gwei
    │      │      │   ├── 1-5 WMNT: 3 gwei
    │      │      │   └── > 5 WMNT: 无上限
    │      │      │
    │      │      ├─► 计算基于利润的 Gas 价格
    │      │      │   └─► total_cap = net_profit / gas_limit
    │      │      │
    │      │      └─► FeePlan { max_fee, max_priority_fee, ... }
    │      │
    │      └─► FeePlan
    │
    ├─► 3. 预飞行检查
    │      │
    │      ├─► 验证非负利润
    │      │   └─► if enforce_non_loss && net_profit == 0: abort
    │      │
    │      ├─► 验证输出量满足要求
    │      │   └─► if computed_last < required_out: abort
    │      │
    │      └─► 验证 EIP-1559 参数
    │          └─► if max_priority > max_fee: abort
    │
    ├─► 4. 构造合约调用
    │      │
    │      ├─► IArbitrageExecutor::executeArbitrage(
    │      │      amount_in,
    │      │      token_path,
    │      │      pool_addresses,
    │      │      pool_types,
    │      │      expected_reserves,
    │      │      step_amounts_out
    │      │   )
    │      │
    │      └─► Set gas(gas_limit)
    │          Set max_fee_per_gas(max_fee)
    │          Set max_priority_fee_per_gas(max_priority_fee)
    │
    └─► 5. 发起交易
           │
           ├─► send().await
           │      └─► 返回 PendingTransaction
           │
           ├─► watch().await
           │      └─► 等待交易上链
           │
           └─► Return TxHash
```

**执行参数构建详细流程**:

```
build_params(opportunity)
    │
    ├─► Token Path
    │      ├── 从 opportunity.path.tokens 提取
    │      └── 确保首尾是 WMNT
    │
    ├─► Pool Addresses
    │      └── 从 opportunity.path.pools 提取
    │
    ├─► Expected Reserves (实时查询)
    │      └── For each pool
    │             ├── pair = IMoePair::new(pool, provider)
    │             ├── (reserve0, reserve1) = pair.getReserves().call()
    │             └── push [reserve0, reserve1]
    │
    ├─► Step Amounts Out (链上计算)
    │      ├── current = amount_in
    │      └── For each pool
    │             ├── (reserve_in, reserve_out) = get reserves by direction
    │             ├── out = v2_formula(current, reserve_in, reserve_out)
    │             ├── push out
    │             └── current = out
    │
    └─► Min Amount Out (滑点保护)
           ├── expected_profit = final_out - amount_in
           ├── slippage_allowance = expected_profit * slippage_tolerance
           ├── min_out = final_out - slippage_allowance
           └── If include_gas_cost:
                  └── min_out = max(min_out, amount_in + gas_cost)
```

## 数据流图

### 数据流向（Data Flow）

```
Blockchain Events
        │
        ▼
    Log Stream
        │
        ▼
  StateSpace.sync()
        │
        ├─► AMM.sync(log)  (更新池状态)
        │      ↓
        │   Updated AMM
        │
        └─► Affected Pool Addresses
               │
               ▼
   ArbitrageMonitor.handle_updates()
               │
               ▼
        refresh_graph()
               │
               ▼
          PoolGraph
               │
               ▼
      PathFinder.find_cycles()
               │
               ▼
       Vec<ArbitragePath>
               │
               ▼
    PathOptimizer.optimize()
               │
               ├─► AMM.simulate_swap()  (读取池状态)
               │
               ▼
    Vec<OptimizationResult>
               │
               ▼
     Executor.build_params()
               │
               ├─► Query reserves from blockchain
               │
               ▼
       ExecutionParams
               │
               ▼
       Executor.execute()
               │
               ▼
      Transaction to Blockchain
```

### 控制流向（Control Flow）

```
Main Loop (Tokio Runtime)
    │
    ├─► Task 1: Event Subscription
    │      └─► StateSpaceManager.subscribe()
    │             └─► Yield affected pools
    │
    ├─► Task 2: Arbitrage Monitoring
    │      ├─► Wait for affected pools
    │      ├─► ArbitrageMonitor.refresh_graph()
    │      └─► ArbitrageMonitor.opportunistic_scan()
    │             └─► Yield opportunities
    │
    └─► Task 3: Execution
           ├─► Wait for opportunities
           └─► For each opportunity
                  ├─► Executor.build_params()
                  └─► Executor.execute()
```

## 模块间接口

### 1. AMMs ↔ State Space

**接口**:
```rust
// AMMs 提供
pub trait AutomatedMarketMaker {
    fn sync(&mut self, log: &Log) -> Result<(), AMMError>;
    fn simulate_swap(&self, base_token: Address, quote_token: Address, amount_in: U256) 
        -> Result<U256, AMMError>;
}

pub trait AutomatedMarketMakerFactory {
    fn discover(&self, to_block: BlockId, provider: P) -> Future<Output = Vec<AMM>>;
    fn sync(&self, amms: Vec<AMM>, to_block: BlockId, provider: P) -> Future<Output = Vec<AMM>>;
}

// State Space 使用
StateSpaceBuilder::sync() {
    factories.discover() → Vec<AMM>
    factories.sync() → Vec<AMM>
    StateSpace::insert(amm)
}

StateSpace::sync(&logs) {
    amm.sync(log)?
}
```

### 2. State Space ↔ Arbitrage

**接口**:
```rust
// State Space 提供
pub struct StateSpaceManager {
    pub async fn subscribe(&self) -> Stream<Vec<Address>>;
}

pub struct StateSpace {
    pub state: HashMap<Address, AMM>;
}

// Arbitrage 使用
ArbitrageMonitor::new() {
    StateSpaceBuilder::sync() → StateSpaceManager
    state_manager.state.clone()
}

ArbitrageMonitor::opportunistic_scan() {
    state.read().await
    build_graph(&state) → PoolGraph
    PathFinder::find_cycles() → Vec<ArbitragePath>
    PathOptimizer::optimize() → Vec<OptimizationResult>
}
```

### 3. Arbitrage ↔ Execution

**接口**:
```rust
// Arbitrage 提供
pub struct OptimizationResult {
    pub path: ArbitragePath,
    pub optimal_input: U256,
    pub expected_profit: U256,
}

pub fn pools_for_path(path: &ArbitragePath, state_pools: &[AMM]) -> Vec<AMM>;

// Execution 使用
Executor::execute_opportunity(opportunity) {
    build_params(opportunity)
    execute(params)
}
```

## 错误处理链

```
Bottom → Top (错误向上传播)

TransportError (alloy)
    ↓
AMMError
    ↓
StateSpaceError
    ↓
ArbitrageError
    ↓
Main Loop (eyre::Error)
```

**错误恢复策略**:

```
Level 1: AMM Pool Sync Error
    └─► Skip this pool, continue with others
    
Level 2: State Space Sync Error
    └─► Log error, wait for next block
    
Level 3: Path Simulation Error
    └─► Skip this path, try next
    
Level 4: Execution Build Error
    └─► Skip this opportunity, wait for next
    
Level 5: Transaction Send Error
    └─► Log error, continue monitoring
```

## 并发模型

### 1. 初始化阶段

```
tokio::spawn {
    Factory 1: discover + sync
}

tokio::spawn {
    Factory 2: discover + sync
}

tokio::spawn {
    Factory 3: discover + sync
}

futures::join_all() → 等待所有完成
```

### 2. 运行时

```
Main Loop (Single Thread)
    │
    ├─► Arc<RwLock<StateSpace>> (共享状态)
    │      │
    │      ├─► Multiple Readers (并发读取)
    │      │      ├── PathFinder.find_cycles()
    │      │      ├── PathOptimizer.optimize()
    │      │      └── pools_for_path()
    │      │
    │      └─► Single Writer (独占写入)
    │             └── StateSpace.sync(&logs)
    │
    └─► Arc<AtomicU64> (无锁读取最新区块号)
```

## 性能优化点总结

### 1. 批量操作

| 模块 | 操作 | 批量大小 | 性能提升 |
|-----|------|---------|---------|
| AMMs | Token Decimals | 765 | ~765x |
| AMMs | V2 Pool Data | 120 | ~120x |
| AMMs | V3 Slot0 | 255 | ~255x |
| AMMs | Tick Bitmaps | 6900 words | ~数百倍 |
| State Space | Event Sync | 一个区块所有事件 | ~10x |

### 2. 并发处理

- **初始化**: 并发发现和同步不同工厂的池
- **路径搜索**: 单线程 BFS（图算法不易并行）
- **路径优化**: 可并行优化多条路径（未实现）
- **状态读取**: 多读者并发访问

### 3. 增量更新

- **State Space**: 只同步有事件的池
- **Graph Refresh**: 只在池更新时重建图
- **Cache**: 环形缓存限制内存使用

### 4. 提前终止

- **路径搜索**: 达到最大长度立即剪枝
- **路径模拟**: 任何一跳输出为零立即返回
- **执行检查**: 预飞行失败不发送交易

## 典型场景示例

### 场景 1: 新区块到达

```
1. WebSocket 接收到新区块通知
2. StateSpaceManager.subscribe() yield 新区块
3. 获取该区块的事件日志
4. StateSpace.sync(&logs)
   - 检测到 Pool A 的 Swap 事件
   - 更新 Pool A 的储备和价格
   - 缓存旧状态到 StateChangeCache
5. 返回 affected_pools = [Pool A]
6. ArbitrageMonitor.handle_updates([Pool A])
   - 重建 PoolGraph（包含更新后的 Pool A）
7. ArbitrageMonitor.opportunistic_scan()
   - 搜索路径
   - 优化路径
   - 发现利润 > 0 的机会
8. 执行套利
```

### 场景 2: 检测到 Reorg

```
1. 新区块到达，区块号 = 100
2. StateSpace.sync(&logs)
   - latest_block = 101 (!)
   - block_number < latest_block
   - 检测到 Reorg!
3. StateChangeCache.unwind(100)
   - 找到区块 100-101 的状态变更
   - 提取这些区块中变化的池的旧状态
   - 返回 [Pool A at block 99, Pool B at block 99]
4. 恢复状态
   - state.insert(Pool A, old_state)
   - state.insert(Pool B, old_state)
5. 正常同步区块 100 的事件
6. 继续运行
```

### 场景 3: 发现并执行套利

```
1. OpportunisticScan 发现机会
   - Path: WMNT → USDC → WETH → WMNT
   - Optimal Input: 10 WMNT
   - Expected Profit: 0.5 WMNT
   
2. Executor.build_params()
   - 查询 3 个池的当前储备
   - 计算每一跳的预期输出
   - min_out = 10.45 WMNT (10 + 0.5 - 0.05 滑点)
   
3. compute_fee_plan()
   - 利润 0.5 WMNT → 全局上限 0.5 gwei
   - 3 跳 → Gas limit 600M
   - max_fee = min(profit/gas_limit, 0.5 gwei) = 0.00083 gwei
   
4. 预飞行检查
   - 预期最后输出 10.5 WMNT > min_out 10.45 WMNT ✓
   - 净利润 0.5 WMNT > 0 ✓
   
5. 发送交易
   - ArbitrageExecutor.executeArbitrage(...)
   - Gas: 600M, MaxFee: 0.00083 gwei
   
6. 等待确认
   - watch().await
   - 交易成功！实际利润: 0.48 WMNT
```

## 总结

整个系统的协同工作遵循以下原则：

1. **单一职责**: 每个模块专注于特定功能
2. **接口清晰**: 模块间通过明确定义的 trait 通信
3. **数据驱动**: 事件驱动状态更新，状态驱动套利发现
4. **异步并发**: 充分利用 Tokio 的异步能力
5. **错误隔离**: 底层错误不会导致整个系统崩溃
6. **性能优化**: 批量操作、增量更新、提前终止
7. **可观测性**: 详细日志、结构化输出、性能指标

通过各模块的精心设计和协同工作，系统实现了从链上事件到套利执行的完整闭环，具备高性能、高可靠性和高可扩展性。

