# Arbitrage 模块 - 套利机会发现与优化

## 模块概述

Arbitrage 模块是套利系统的核心大脑，负责从链上状态中发现套利机会、优化交易路径、估算利润和 Gas 成本。该模块将池状态转换为图结构，使用图算法搜索套利路径，并通过数值优化找到最佳输入量。

## 目录结构

```
src/arbitrage/
├── mod.rs           # 模块入口、公共导出
├── data.rs          # 数据结构定义（TokenState, PoolEdge, PoolExtraction）
├── graph.rs         # 图构建和结构
├── pathfinder.rs    # 路径搜索算法
├── optimizer.rs     # 路径优化器
├── monitor.rs       # 套利监控器
├── gas.rs           # Gas 估算
├── mock.rs          # 模拟数据生成
└── error.rs         # 错误类型定义
```

## 核心设计思路

### 1. 三层架构

```
┌──────────────────────────────────┐
│   ArbitrageMonitor (应用层)       │
│  - opportunistic_scan()          │
│  - handle_updates()              │
│  - refresh_graph()               │
└────────────┬─────────────────────┘
             │
        ┌────┴────┬────────┐
        ▼         ▼        ▼
┌─────────┐ ┌──────────┐ ┌────────┐
│PathFinder│ │PathOptimizer│ │PoolGraph│
│(搜索层)  │ │(优化层)   │ │(数据层)│
└─────────┘ └──────────┘ └────────┘
```

### 2. 从状态到图的转换

```
StateSpace (HashMap<Address, AMM>)
    ↓
extract_pool(amm) → PoolExtraction { token_a, token_b, fee_bps }
    ↓
into_edges(pool_address) → (PoolEdge, PoolEdge)  // 正向和反向
    ↓
build_graph() → PoolGraph
    ↓
DiGraph<Address, PoolEdge>
```

## 核心数据结构

### 1. TokenState

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenState {
    pub address: Address,
    pub decimals: u8,
}

impl TokenState {
    pub fn from_token(token: &Token) -> Result<Self, ArbitrageError> {
        if token.decimals == 0 {
            return Err(ArbitrageError::MissingTokenDecimals(token.address));
        }
        Ok(Self {
            address: token.address,
            decimals: token.decimals,
        })
    }
}
```

**设计要点**:
- 包含 `decimals` 用于价格计算和数量转换
- `PartialEq` 和 `Hash` 基于 `address`（忽略 `decimals`）
- 确保 `decimals` 非零（避免除零错误）

### 2. PoolEdge

```rust
#[derive(Debug, Clone)]
pub struct PoolEdge {
    pub pool_address: Address,
    pub token_in: TokenState,
    pub token_out: TokenState,
    pub fee_bps: u32,
}

impl PoolEdge {
    pub fn reversed(&self) -> Self {
        Self {
            pool_address: self.pool_address,
            token_in: self.token_out.clone(),
            token_out: self.token_in.clone(),
            fee_bps: self.fee_bps,
        }
    }
}
```

**设计要点**:
- 有向边：token_in → token_out
- 双向池需要两条边（正向和反向）
- `fee_bps` 用于路径排序和过滤

### 3. PoolExtraction

```rust
#[derive(Debug, Clone)]
pub struct PoolExtraction {
    pub token_a: TokenState,
    pub token_b: TokenState,
    pub fee_bps: u32,
}

impl PoolExtraction {
    pub fn into_edges(self, pool_address: Address) -> (PoolEdge, PoolEdge) {
        let forward = PoolEdge {
            pool_address,
            token_in: self.token_a.clone(),
            token_out: self.token_b.clone(),
            fee_bps: self.fee_bps,
        };
        let reverse = forward.reversed();
        (forward, reverse)
    }
}
```

**协议适配器**:

```rust
pub fn extract_pool(pool: &AMM) -> Result<PoolExtraction, ArbitrageError> {
    match pool {
        AMM::UniswapV3Pool(inner) => extract_uniswap_v3(inner),
        AMM::AgniPool(inner) => extract_agni(inner),
        AMM::UniswapV2Pool(inner) => extract_uniswap_v2(inner),
        AMM::MoeLbPair(inner) => extract_moe(inner),
    }
}

fn extract_uniswap_v3(pool: &UniswapV3Pool) -> Result<PoolExtraction, ArbitrageError> {
    Ok(PoolExtraction {
        token_a: TokenState::from_token(&pool.token_a)?,
        token_b: TokenState::from_token(&pool.token_b)?,
        fee_bps: pool.fee,  // 已经是 bps
    })
}

fn extract_uniswap_v2(pool: &UniswapV2Pool) -> Result<PoolExtraction, ArbitrageError> {
    Ok(PoolExtraction {
        token_a: TokenState::from_token(&pool.token_a)?,
        token_b: TokenState::from_token(&pool.token_b)?,
        fee_bps: (pool.fee as u32) / 10u32,  // 从 1e5 转换到 bps
    })
}

fn extract_moe(pool: &MoeLbPair) -> Result<PoolExtraction, ArbitrageError> {
    Ok(PoolExtraction {
        token_a: TokenState::from_token(&pool.token_x)?,
        token_b: TokenState::from_token(&pool.token_y)?,
        fee_bps: pool.bin_step as u32,  // 使用 bin_step 作为 fee_bps 的代理
    })
}
```

### 4. PoolGraph

```rust
#[derive(Debug, Clone)]
pub struct PoolGraph {
    pub graph: DiGraph<Address, PoolEdge>,          // 有向图
    pub node_tokens: HashMap<NodeIndex, Address>,   // 节点 → Token 地址
    pub node_decimals: HashMap<NodeIndex, u8>,      // 节点 → Decimals
}

impl PoolGraph {
    // 获取节点的出边
    pub fn edges_from(&self, node: NodeIndex) -> impl Iterator<Item = PoolEdge> + '_ {
        self.graph.edges(node).map(|edge| edge.weight().clone())
    }
    
    // 获取节点的邻居
    pub fn neighbors(&self, node: NodeIndex) -> impl Iterator<Item = NodeIndex> + '_ {
        self.graph.neighbors(node)
    }
    
    // 获取节点对应的 Token
    pub fn token_of(&self, node: NodeIndex) -> Option<&Address> {
        self.node_tokens.get(&node)
    }
    
    // 获取 Token 对应的节点
    pub fn node_for_token(&self, token: Address) -> Option<NodeIndex> {
        self.node_tokens.iter()
            .find_map(|(node, addr)| if *addr == token { Some(*node) } else { None })
    }
}
```

**图构建算法**:

```rust
pub fn build_graph(state_space: &StateSpace) -> Result<PoolGraph, ArbitrageError> {
    let mut graph: DiGraph<Address, PoolEdge> = DiGraph::new();
    let mut node_tokens: HashMap<NodeIndex, Address> = HashMap::new();
    let mut node_decimals: HashMap<NodeIndex, u8> = HashMap::new();
    let mut token_nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let mut edge_seen: HashSet<(Address, Address, Address)> = HashSet::new();
    
    for pool in state_space.state.values() {
        // 1. 提取池信息
        let extraction = match extract_pool(pool) {
            Ok(ext) => ext,
            Err(e) => {
                tracing::debug!("Skipping pool {:?}: {e:?}", pool.address());
                continue;
            }
        };
        
        // 2. 生成正向和反向边
        let (forward_edge, reverse_edge) = extraction.into_edges(pool.address());
        
        // 3. 确保节点存在
        let token_a_idx = *token_nodes.entry(forward_edge.token_in.address)
            .or_insert_with(|| {
                let idx = graph.add_node(forward_edge.token_in.address);
                node_tokens.insert(idx, forward_edge.token_in.address);
                node_decimals.insert(idx, forward_edge.token_in.decimals);
                idx
            });
        
        let token_b_idx = *token_nodes.entry(forward_edge.token_out.address)
            .or_insert_with(|| {
                let idx = graph.add_node(forward_edge.token_out.address);
                node_tokens.insert(idx, forward_edge.token_out.address);
                node_decimals.insert(idx, forward_edge.token_out.decimals);
                idx
            });
        
        // 4. 添加边（去重）
        let forward_key = (
            forward_edge.pool_address,
            forward_edge.token_in.address,
            forward_edge.token_out.address,
        );
        if edge_seen.insert(forward_key) {
            graph.add_edge(token_a_idx, token_b_idx, forward_edge);
        }
        
        let reverse_key = (
            reverse_edge.pool_address,
            reverse_edge.token_in.address,
            reverse_edge.token_out.address,
        );
        if edge_seen.insert(reverse_key) {
            graph.add_edge(token_b_idx, token_a_idx, reverse_edge);
        }
    }
    
    Ok(PoolGraph { graph, node_tokens, node_decimals })
}
```

**图结构示例**:

```
        Pool1 (USDC/WMNT)
USDC ──────────────────────► WMNT
  │                            ▲
  │ Pool2                      │ Pool3
  │ (USDC/WETH)                │ (WETH/WMNT)
  │                            │
  └──────────► WETH ───────────┘
```

对应的图：

```
Nodes:
  0: USDC
  1: WMNT
  2: WETH

Edges:
  0 → 1: PoolEdge { pool: Pool1, fee_bps: 500 }
  1 → 0: PoolEdge { pool: Pool1, fee_bps: 500 }
  0 → 2: PoolEdge { pool: Pool2, fee_bps: 3000 }
  2 → 0: PoolEdge { pool: Pool2, fee_bps: 3000 }
  2 → 1: PoolEdge { pool: Pool3, fee_bps: 500 }
  1 → 2: PoolEdge { pool: Pool3, fee_bps: 500 }
```

## 路径搜索算法

### 1. PathFinder

```rust
#[derive(Debug, Clone, Copy)]
pub struct PathConstraints {
    pub max_length: usize,                      // 最大跳数（默认 4）
    pub allow_self_cycle: bool,                 // 是否允许访问重复的 token
    pub required_start_token: Option<Address>,  // 必须从此 token 开始
    pub required_end_token: Option<Address>,    // 必须以此 token 结束
}

pub struct PathFinder<'a> {
    graph: &'a PoolGraph,
    constraints: PathConstraints,
}

impl<'a> PathFinder<'a> {
    pub fn new(graph: &'a PoolGraph, constraints: PathConstraints) -> Self {
        Self { graph, constraints }
    }
    
    // 查找所有闭环结算循环路径（唯一机会原语，WHI-529）
    pub fn find_cycles(&self) -> Vec<ArbitragePath> { /* ... */ }
}
```

### 2. 循环路径搜索（BFS）

```rust
pub fn find_cycles(&self) -> Vec<ArbitragePath> {
    let mut cycles = Vec::new();
    
    // 遍历所有可能的起点
    for start in self.graph.graph.node_indices() {
        let start_token = match self.graph.token_of(start) {
            Some(token) => *token,
            None => continue,
        };
        
        // 检查起点约束
        if let Some(required_start) = self.constraints.required_start_token {
            if start_token != required_start {
                continue;
            }
        }
        
        // 初始化 BFS 队列
        let mut queue = VecDeque::new();
        let mut seen_tokens = HashSet::new();
        seen_tokens.insert(start_token);
        queue.push_back((start, vec![], seen_tokens));
        
        // BFS 搜索
        while let Some((node, path, tokens_seen)) = queue.pop_front() {
            // 检查路径长度
            if path.len() >= self.constraints.max_length {
                continue;
            }
            
            // 遍历邻居
            for edge in self.graph.graph.edges(node) {
                let target = edge.target();
                let target_token = match self.graph.token_of(target) {
                    Some(token) => *token,
                    None => continue,
                };
                
                // 检查是否允许重复访问
                if !self.constraints.allow_self_cycle 
                    && tokens_seen.contains(&target_token)
                    && target != start 
                {
                    continue;
                }
                
                let mut new_path = path.clone();
                new_path.push(edge.weight().clone());
                
                // 找到循环！
                if target == start {
                    if let Some(arbitrage_path) = convert_edges(&new_path) {
                        if path_matches_constraints(&arbitrage_path, &self.constraints) {
                            cycles.push(arbitrage_path);
                        }
                    }
                    continue;
                }
                
                // 继续搜索
                let mut next_tokens = tokens_seen.clone();
                next_tokens.insert(target_token);
                queue.push_back((target, new_path, next_tokens));
            }
        }
    }
    
    // 去重
    cycles.into_iter()
        .unique_by(|path| {
            path.hops.iter()
                .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
                .collect::<Vec<_>>()
        })
        .collect()
}
```

**算法特点**:
- **广度优先搜索**（BFS）：确保找到最短路径
- **剪枝**：限制路径长度、禁止重复访问（可选）
- **去重**：基于有序 (pool, token_in, token_out) 跳序列，旋转规范化（仅旋转，保留反向路径）

**时间复杂度**:
- O(V * E^L)，其中 V 是节点数，E 是边数，L 是最大路径长度
- 实际运行时间受剪枝和去重影响

### 3. 双池价差搜索（已删除，WHI-529）

开放路径 `A -> mid -> B`（`start != end`）在 `simulate_path` 里用 `amount_out - amount_in` 做利润算术，当两端资产小数位不同时结果无量纲意义。

`PathFinder` 的开放双池价差搜索 API **已删除**。并行双池闭环（`WMNT -> X (pool A) -> WMNT (pool B)`）改由 `find_cycles` 在 `settlement_cycle` 约束下发现；相邻同池往返被拒绝。

### 1. OptimizationConfig

```rust
#[derive(Debug, Clone)]
pub struct OptimizationConfig {
    pub max_iterations: usize,  // 最大迭代次数（默认 16）
    pub tolerance_bps: u32,     // 收敛容差（默认 5 bps）
    pub min_profit: U256,       // 最小利润阈值（默认 1000 wei）
    pub max_input: U256,        // 最大输入量（默认 10^24）
}
```

### 2. OptimizationResult

```rust
#[derive(Debug, Clone)]
pub struct OptimizationResult {
    pub path: ArbitragePath,
    pub optimal_input: U256,        // 最优输入量
    pub expected_profit: U256,      // 预期利润
    pub output_amount: U256,        // 输出量
}
```

### 3. PathOptimizer

```rust
#[derive(Clone)]
pub struct PathOptimizer {
    config: OptimizationConfig,
}

impl PathOptimizer {
    pub fn optimize(
        &self,
        path: &ArbitragePath,
        pools: &[AMM],
    ) -> Result<Option<OptimizationResult>, ArbitrageError> {
        if pools.len() != path.hops.len() {
            return Err(ArbitrageError::Optimization(
                "Mismatch between path hops and pools".into()
            ));
        }
        
        // 二分搜索最优输入量
        let initial_guess = U256::from(10_u128.pow(18));  // 1.0 token
        let mut low = U256::ZERO;
        let mut high = initial_guess.min(self.config.max_input);
        let mut best_result: Option<OptimizationResult> = None;
        
        for _ in 0..self.config.max_iterations {
            let mid = (low + high) >> 1;  // 位移除以 2
            let simulation = simulate_path(path, pools, mid)?;
            
            if let Some(sim) = simulation {
                if sim.expected_profit > self.config.min_profit {
                    best_result = Some(sim.clone());
                    low = mid + U256::from(1);  // 尝试更大的输入
                } else {
                    high = mid.saturating_sub(U256::from(1));  // 减小输入
                }
            } else {
                high = mid.saturating_sub(U256::from(1));
            }
        }
        
        Ok(best_result)
    }
}
```

**优化算法**:
- **二分搜索**：在 [0, max_input] 范围内搜索
- **迭代终止条件**：
  1. 达到最大迭代次数
  2. 搜索区间收敛（high - low < tolerance）
- **目标函数**：`profit(amount_in) = output - input - gas_cost`

**为什么二分搜索有效？**

套利利润函数通常是单峰函数（unimodal）：

```
Profit
  ▲
  │      *
  │    *   *
  │  *       *
  │*           *
  └───────────────► Input Amount
      ↑
   optimal
```

在最优点之前，增加输入会增加利润；在最优点之后，增加输入会减少利润（滑点过大）。

### 4. 路径模拟

```rust
pub fn simulate_path(
    path: &ArbitragePath,
    pools: &[AMM],
    amount_in: U256,
) -> Result<Option<OptimizationResult>, ArbitrageError> {
    if path.hops.is_empty() || amount_in.is_zero() {
        return Ok(None);
    }
    
    let mut current_amount = amount_in;
    
    // 逐跳模拟
    for (index, (hop, amm)) in path.hops.iter().zip(pools.iter()).enumerate() {
        tracing::debug!(
            target: "simulate.path",
            hop_index = index,
            pool = %hop.pool_address,
            token_in = %hop.token_in,
            token_out = %hop.token_out,
            input_amount = %current_amount,
            "Simulating hop"
        );
        
        let output = simulate_hop(amm, hop, current_amount)?;
        
        if output.is_zero() {
            tracing::warn!(
                target: "simulate.path",
                hop_index = index,
                pool = %hop.pool_address,
                "Simulation produced zero output; aborting path"
            );
            return Ok(None);
        }
        
        current_amount = output;
    }
    
    // 计算利润
    let expected_profit = current_amount.checked_sub(amount_in);
    
    match expected_profit {
        Some(profit) => {
            Ok(Some(OptimizationResult {
                path: path.clone(),
                optimal_input: amount_in,
                expected_profit: profit,
                output_amount: current_amount,
            }))
        }
        None => Ok(None),  // 负利润
    }
}

fn simulate_hop(amm: &AMM, hop: &PathHop, amount_in: U256) -> Result<U256, ArbitrageError> {
    amm.simulate_swap(hop.token_in, hop.token_out, amount_in)
        .map_err(|e| ArbitrageError::Simulation(e.to_string()))
}
```

**关键点**:
- 使用 `simulate_swap`（不可变）而非 `simulate_swap_mut`
- 链式模拟：每一跳的输出作为下一跳的输入
- 提前终止：任何一跳输出为零则整个路径失败
- 溢出检查：使用 `checked_sub` 避免 panic

## ArbitrageMonitor

### 1. MonitorConfig

```rust
#[derive(Clone)]
pub struct MonitorConfig {
    pub factories: Vec<Factory>,
    pub manual_pools: Vec<AMM>,
    pub constraints: PathConstraints,
    pub optimization: OptimizationConfig,
    pub opportunity_log_path: Option<PathBuf>,      // 套利机会日志
    pub best_snapshot_log_path: Option<PathBuf>,    // 最佳路径快照
    pub pool_update_log_path: Option<PathBuf>,      // 池更新日志
}
```

### 2. ArbitrageMonitor

```rust
pub struct ArbitrageMonitor<N, P>
where
    N: Network<BlockResponse = Block>,
    P: Provider<N> + Clone + 'static,
{
    provider: P,
    config: MonitorConfig,
    state_manager: StateSpaceManager<N, P>,
    state: Arc<RwLock<StateSpace>>,
    optimizer: PathOptimizer,
    graph: RwLock<Option<PoolGraph>>,
    phantom: std::marker::PhantomData<N>,
}

impl<N, P> ArbitrageMonitor<N, P> {
    pub async fn new(provider: P, config: MonitorConfig) -> Result<Self, ArbitrageError> {
        // 1. 构建状态空间
        let state_manager = StateSpaceBuilder::new(provider.clone())
            .with_factories(config.factories.clone())
            .with_amms(config.manual_pools.clone())
            .sync()
            .await?;
        
        let state = state_manager.state.clone();
        
        Ok(Self {
            provider,
            config: config.clone(),
            state_manager,
            state,
            optimizer: PathOptimizer::new(config.optimization.clone()),
            graph: RwLock::new(None),
            phantom: std::marker::PhantomData,
        })
    }
    
    // 主动扫描套利机会
    pub async fn opportunistic_scan(&self) -> Result<OpportunisticScanResult, ArbitrageError> {
        // ...
    }
    
    // 订阅事件流
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<Vec<Address>, ArbitrageError>>, ArbitrageError> {
        // ...
    }
    
    // 处理状态更新
    pub async fn handle_updates(&self, updated: Vec<Address>) -> Result<(), ArbitrageError> {
        // ...
    }
    
    // 刷新图结构
    pub async fn refresh_graph(&self) -> Result<(), ArbitrageError> {
        // ...
    }
}
```

### 3. 机会扫描流程

```rust
pub async fn opportunistic_scan(&self) -> Result<OpportunisticScanResult, ArbitrageError> {
    let block_number = self.provider.get_block_number().await?;
    
    // 1. 构建图
    let state = self.state();
    let state_guard = state.read().await;
    let graph = build_graph(&state_guard)?;
    drop(state_guard);
    
    // 2. 搜索路径
    let path_finder = PathFinder::new(&graph, self.config.constraints);
    // 仅闭环结算循环（WHI-529）；开放 misprice 路径已删除
    let paths = path_finder.find_cycles();
    
    // 3. 优化每条路径
    let mut opportunities = Vec::new();
    let state_guard = state.read().await;
    let pools_snapshot: Vec<AMM> = state_guard.state.values().cloned().collect();
    
    for path in &paths {
        let pools = pools_for_path(&path, &pools_snapshot)?;
        if let Some(result) = self.optimizer.optimize(&path, &pools)? {
            if !result.expected_profit.is_zero() {
                opportunities.push(result);
            }
        }
    }
    
    // 4. 记录日志
    self.log_opportunities(block_number, &opportunities)?;
    self.log_best_snapshot(block_number, &paths, &pools_snapshot)?;
    self.log_pool_presence(block_number, &state_guard)?;
    
    Ok(OpportunisticScanResult {
        block_number,
        opportunities,
    })
}
```

**工作流程**:

```
1. 读取状态空间 → StateSpace
    ↓
2. 构建交易图 → PoolGraph
    ↓
3. 搜索套利路径 → Vec<ArbitragePath>
    ↓
4. 优化每条路径 → Vec<OptimizationResult>
    ↓
5. 过滤利润 > 0 的机会
    ↓
6. 记录到 CSV 日志
```

### 4. 日志记录

```rust
fn log_opportunities(
    &self,
    block_number: u64,
    opportunities: &[OptimizationResult],
) -> Result<(), ArbitrageError> {
    let Some(path) = &self.config.opportunity_log_path else {
        return Ok(());
    };
    
    // 创建目录
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    
    // 检查是否需要写入 header
    let need_header = !path.exists() || std::fs::metadata(path)?.len() == 0;
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut writer = WriterBuilder::new().has_headers(false).from_writer(file);
    
    if need_header {
        writer.write_record([
            "block_number",
            "opportunity_index",
            "path_length",
            "optimal_input",
            "expected_profit",
            "hops",
        ])?;
    }
    
    for (idx, opportunity) in opportunities.iter().enumerate() {
        let path_desc = opportunity.path.hops.iter()
            .map(|hop| format!(
                "{:#x}->{:#x}@{:#x}(fee_bps={})",
                hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
            ))
            .collect::<Vec<_>>()
            .join(" | ");
        
        writer.write_record([
            block_number.to_string(),
            idx.to_string(),
            opportunity.path.hops.len().to_string(),
            opportunity.optimal_input.to_string(),
            opportunity.expected_profit.to_string(),
            path_desc,
        ])?;
    }
    
    writer.flush()?;
    Ok(())
}
```

**日志格式**:

```csv
block_number,opportunity_index,path_length,optimal_input,expected_profit,hops
100000,0,3,1000000000000000000,50000000000000000,0xA->0xB@0xPool1(fee_bps=500) | 0xB->0xC@0xPool2(fee_bps=3000) | 0xC->0xA@0xPool3(fee_bps=500)
100000,1,2,2000000000000000000,30000000000000000,0xA->0xB@0xPool1(fee_bps=500) | 0xB->0xA@0xPool4(fee_bps=500)
```

## Gas 估算

```rust
#[derive(Debug, Clone)]
pub struct GasConfig {
    pub base_gas: u64,             // 基础 Gas（交易开销）
    pub per_hop_gas: u64,          // 每跳 Gas
    pub gas_price_gwei: u64,       // Gas 价格（Gwei）
}

pub fn estimate_gas_cost(path: &ArbitragePath, config: &GasConfig) -> U256 {
    let total_gas = config.base_gas + (path.hops.len() as u64) * config.per_hop_gas;
    let gas_price_wei = U256::from(config.gas_price_gwei) * U256::from(1_000_000_000u64);
    U256::from(total_gas) * gas_price_wei
}
```

**实际使用**:

```rust
let gas_cost = estimate_gas_cost(&path, &gas_config);
let net_profit = expected_profit.saturating_sub(gas_cost);

if net_profit > min_profit_threshold {
    // 值得执行
}
```

## 错误处理

```rust
#[derive(Error, Debug)]
pub enum ArbitrageError {
    #[error(transparent)]
    AMMError(#[from] AMMError),
    
    #[error(transparent)]
    StateSpaceError(#[from] StateSpaceError),
    
    #[error("Missing token decimals for {0}")]
    MissingTokenDecimals(Address),
    
    #[error("Optimization error: {0}")]
    Optimization(String),
    
    #[error("Simulation error: {0}")]
    Simulation(String),
    
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    
    #[error(transparent)]
    CsvError(#[from] csv::Error),
    
    #[error(transparent)]
    TransportError(#[from] alloy::transports::RpcError<alloy::transports::TransportErrorKind>),
}
```

## 组件内协同

### 1. PoolGraph ↔ PathFinder

```
StateSpace → build_graph() → PoolGraph
                               ↓
                       PathFinder::new(graph)
                               ↓
                       find_cycles() → Vec<ArbitragePath>
```

### 2. PathFinder ↔ PathOptimizer

```
PathFinder → Vec<ArbitragePath>
                ↓
for path in paths {
    pools = pools_for_path(path, state_space)?
    PathOptimizer::optimize(path, pools)?
        ↓
    OptimizationResult
}
```

### 3. ArbitrageMonitor 内部协同

```
Monitor.opportunistic_scan()
    ↓
state.read() → StateSpace
    ↓
build_graph() → PoolGraph
    ↓
PathFinder → Vec<ArbitragePath>
    ↓
PathOptimizer → Vec<OptimizationResult>
    ↓
log_opportunities()
```

## 组件间协同

### 1. Arbitrage ← State Space

```
StateSpaceManager.subscribe()
    ↓
stream of updated pool addresses
    ↓
ArbitrageMonitor.handle_updates(updated)
    ↓
refresh_graph()
    ↓
opportunistic_scan()
```

### 2. Arbitrage → Execution

```
ArbitrageMonitor.opportunistic_scan()
    ↓
OpportunisticScanResult { opportunities }
    ↓
for opp in opportunities {
    Executor.execute_opportunity(opp)?
}
```

### 3. Arbitrage ← AMMs

```
AMM.simulate_swap() ← simulate_hop()
                        ↑
                 simulate_path()
                        ↑
                 PathOptimizer::optimize()
```

## 性能优化

### 1. 图构建优化

```rust
// 使用 HashMap 避免重复节点
let mut token_nodes: HashMap<Address, NodeIndex> = HashMap::new();

// 使用 HashSet 去重边
let mut edge_seen: HashSet<(Address, Address, Address)> = HashSet::new();
```

### 2. 路径去重

```rust
cycles.into_iter()
    .unique_by(|path| {
        path.hops.iter()
            .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
            .collect::<Vec<_>>()
    })
    .collect()
```

### 3. 提前终止

```rust
// 路径长度限制
if path.len() >= self.constraints.max_length {
    continue;
}

// 零输出立即返回
if output.is_zero() {
    return Ok(None);
}
```

### 4. 缓存图结构

```rust
pub struct ArbitrageMonitor {
    graph: RwLock<Option<PoolGraph>>,  // 缓存图
}

pub async fn refresh_graph(&self) -> Result<(), ArbitrageError> {
    let state = self.state();
    let state_guard = state.read().await;
    let graph = build_graph(&state_guard)?;
    drop(state_guard);
    
    *self.graph.write().await = Some(graph);  // 更新缓存
    Ok(())
}
```

## 测试策略

### 1. 单元测试

```rust
#[test]
fn find_cycles_returns_three_hop_cycle() {
    let graph = simple_graph();  // A → B → C → A
    let finder = PathFinder::new(&graph, PathConstraints::default());
    let cycles = finder.find_cycles();
    
    assert_eq!(cycles.len(), 3);  // 可以从 A, B, C 任一开始
    assert_eq!(cycles[0].hops.len(), 3);
}

#[test]
fn simulate_path_zero_amount_returns_none() {
    let path = ArbitragePath { hops: vec![hop1] };
    let result = simulate_path(&path, &[pool1], U256::ZERO).unwrap();
    assert!(result.is_none());
}
```

### 2. 集成测试

```rust
#[tokio::test]
async fn test_arbitrage_monitor_scan() {
    let config = MonitorConfig {
        factories: vec![factory],
        constraints: PathConstraints::default(),
        optimization: OptimizationConfig::default(),
        ..Default::default()
    };
    
    let monitor = ArbitrageMonitor::new(provider, config).await?;
    let result = monitor.opportunistic_scan().await?;
    
    assert!(result.opportunities.len() > 0);
}
```

## 总结

Arbitrage 模块通过以下设计实现了高效的套利发现和优化：

1. **图抽象**: 将池状态转换为有向图，支持高效路径搜索
2. **智能搜索**: BFS 保证找到最短路径，剪枝提高效率
3. **数值优化**: 二分搜索找到最优输入量
4. **实时监控**: 订阅状态更新，自动刷新图结构
5. **详细日志**: CSV 格式记录所有机会，便于分析
6. **可配置**: 路径约束、优化参数可灵活调整
7. **类型安全**: 强类型保证路径和池的一致性

该模块为套利系统提供了智能的机会发现和优化能力。
