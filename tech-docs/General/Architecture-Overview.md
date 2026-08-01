# Arbitrage Bot V3 - 架构总览

## 项目简介

这是一个基于 Rust 的高性能链上套利机器人，专为 Mantle 链设计，支持多种 DEX 协议（UniswapV2、UniswapV3、Agni、Moe）的套利交易。该系统采用模块化设计，具有实时状态同步、智能路径发现、交易优化和可靠执行等核心能力。

## 系统架构图

```
┌─────────────────────────────────────────────────────────────────┐
│                         Application Layer                        │
│                    (ArbitrageMonitor & Examples)                │
└────────────────────────────┬────────────────────────────────────┘
                             │
        ┌────────────────────┼────────────────────┐
        │                    │                    │
        ▼                    ▼                    ▼
┌──────────────┐    ┌───────────────┐   ┌──────────────┐
│  Arbitrage   │    │  State Space  │   │  Execution   │
│   Module     │◄───┤    Manager    │   │   Module     │
└──────┬───────┘    └───────┬───────┘   └──────┬───────┘
       │                    │                    │
       │                    │                    │
       └────────────────────┼────────────────────┘
                            │
                            ▼
                   ┌────────────────┐
                   │   AMMs Module  │
                   │  (Pool Models) │
                   └────────────────┘
```

## 核心模块概览

### 1. AMMs 模块 (`src/amms/`)

**职责**: 自动做市商池的抽象和实现

**核心功能**:
- 统一的 AMM trait 接口 (`AutomatedMarketMaker`)
- 多协议支持：UniswapV2、UniswapV3、Agni、Moe LB
- 池状态同步与事件监听
- 高精度交换模拟
- Factory 模式的池发现机制

**关键组件**:
- `AMM` enum: 统一的池类型枚举
- `Factory` enum: 工厂合约抽象
- 各协议的具体实现（`uniswap_v2`, `uniswap_v3`, `agni`, `moe`）

### 2. State Space 模块 (`src/state_space/`)

**职责**: 链上状态的高效管理和同步

**核心功能**:
- 池状态的增量同步
- Reorg（链重组）检测和回滚
- 基于环形缓存的状态快照
- 池过滤和筛选机制
- 实时事件订阅流

**关键组件**:
- `StateSpaceManager`: 状态空间管理器
- `StateSpace`: 池状态存储
- `StateChangeCache`: 状态变更缓存（支持回滚）
- `PoolFilter`: 池过滤器（黑名单、白名单、价值过滤）

### 3. Arbitrage 模块 (`src/arbitrage/`)

**职责**: 套利机会的发现和优化

**核心功能**:
- 基于图的交易路径搜索
- 环路（Cycle）检测
- 双池价差（Two-pool Misprice）识别
- 路径模拟和优化
- 利润计算和Gas成本估算

**关键组件**:
- `PoolGraph`: 交易对图结构
- `PathFinder`: 路径搜索算法
- `PathOptimizer`: 输入量优化器
- `ArbitrageMonitor`: 套利监控器

### 4. Execution 模块 (`src/execution/`)

**职责**: 交易的构造和链上执行

**核心功能**:
- 智能合约交互（ArbitrageExecutor）
- Gas 价格动态计算
- Nonce 管理
- 多协议 Swap 执行
- 交易前飞行检查

**关键组件**:
- `Executor`: 主执行器
- `SwapExecutor`: 单步Swap执行
- `GasSchedule`: Gas 估算
- `ExecutionParams`: 执行参数构建

## 数据流图

```
┌──────────┐
│ Blockchain│
│  Events   │
└─────┬─────┘
      │
      ▼
┌─────────────────┐
│ StateSpaceManager│  ◄──── subscribe()
│  (Event Stream)  │
└────────┬─────────┘
         │ Log Events
         ▼
    ┌─────────┐
    │StateSpace│
    │  Sync    │
    └────┬─────┘
         │ Updated Pools
         ▼
  ┌──────────────┐
  │ ArbitrageMonitor │ ◄──── opportunistic_scan()
  │  Build Graph  │
  └──────┬────────┘
         │
         ▼
  ┌──────────────┐
  │  PathFinder  │  ──► find_cycles()  // closed settlement only (WHI-529)
  └──────┬───────┘
         │ Paths
         ▼
  ┌──────────────┐
  │PathOptimizer │  ──► optimize() (二分搜索最优输入)
  └──────┬───────┘
         │ OptimizationResult
         ▼
  ┌──────────────┐
  │   Executor   │  ──► build_params()
  │              │  ──► execute()
  └──────┬───────┘
         │ Transaction
         ▼
  ┌─────────────┐
  │ Blockchain  │
  └─────────────┘
```

## 关键设计模式

### 1. **Trait-based 多态**
- `AutomatedMarketMaker` trait 统一不同协议的接口
- `AutomatedMarketMakerFactory` trait 统一工厂发现逻辑
- `AMMFilter` trait 支持可插拔的过滤策略

### 2. **Builder Pattern**
- `StateSpaceBuilder` 用于灵活构建状态空间
- `ExecutionParams` 构建复杂的执行参数

### 3. **Strategy Pattern**
- 不同的路径搜索策略（cycles vs two-pool）
- 可配置的优化策略（tolerance, iterations）

### 4. **Observer Pattern**
- StateSpaceManager 提供事件订阅流
- 实时响应链上变化

### 5. **Snapshot & Rollback**
- 基于环形缓存的状态快照
- 支持 Reorg 的自动回滚

## 性能优化点

### 1. **异步并发**
- 大量使用 `FuturesUnordered` 并行处理
- 池发现和同步的批量异步请求
- Rayon 并行迭代器过滤池

### 2. **批量合约调用**
- 自定义 Solidity 批量请求合约
- 减少 RPC 调用次数
- 优化网络往返时延

### 3. **高效数据结构**
- `HashMap` 快速池查找
- `petgraph` 高性能图算法
- 固定大小环形缓存 `ArrayDeque`

### 4. **精确数学计算**
- 使用 `U256`、`I256` 避免浮点误差
- Uniswap V3 的精确 sqrt price 计算
- 高精度 tick 和 liquidity 计算

### 5. **增量状态更新**
- 仅同步发生变化的池
- 事件驱动的状态更新
- 避免全量状态刷新

## 错误处理策略

### 1. **类型化错误**
- 每个模块定义自己的 Error 枚举
- 使用 `thiserror` 实现错误转换
- 错误链追踪（`#[from]`）

### 2. **优雅降级**
- 单个池同步失败不影响整体
- 路径模拟失败返回 `None` 而非崩溃
- 执行前检查避免浪费 Gas

### 3. **日志记录**
- 结构化日志 (`tracing`)
- 不同级别的日志（debug, info, warn, error）
- 关键操作的详细追踪

## 安全考虑

### 1. **交易安全**
- Slippage 容限检查
- 最小输出量保护
- Gas 价格上限设置
- 非负利润强制执行

### 2. **状态一致性**
- Reorg 检测和回滚
- 原子性的状态更新
- 并发安全的读写锁

### 3. **私钥管理**
- 使用环境变量存储私钥
- 不在代码中硬编码敏感信息

## 可扩展性

### 1. **新协议支持**
- 实现 `AutomatedMarketMaker` trait
- 添加到 `AMM` enum
- 实现对应的 Factory

### 2. **新过滤器**
- 实现 `AMMFilter` trait
- 添加到 `PoolFilter` enum

### 3. **新优化策略**
- 修改 `PathOptimizer` 的优化算法
- 调整 `OptimizationConfig` 参数

### 4. **新执行策略**
- 扩展 `Executor` 的 Gas 计算逻辑
- 支持更复杂的交易构建

## 关键性能指标

### 1. **延迟**
- 事件到执行: < 200ms (目标)
- 路径搜索: < 100ms
- 池同步: < 50ms (增量)

### 2. **吞吐量**
- 支持 1000+ 池的实时监控
- 每秒处理 100+ 事件
- 并发处理多个套利机会

### 3. **准确性**
- 交换模拟误差 < 0.01%
- Gas 估算误差 < 5%
- 利润预测偏差 < 1%

## 部署和运维

### 1. **配置管理**
- 环境变量配置（`.env`）
- 可配置的工厂地址和创建区块
- 可调整的优化参数

### 2. **监控**
- 日志输出到文件
- CSV 格式的套利记录
- 池更新和最佳路径快照

### 3. **测试**
- 单元测试（各模块）
- 集成测试（end-to-end）
- 基准测试（benches/）

## 总结

该架构设计具有以下优势：

1. **模块化**: 清晰的职责分离，易于维护和扩展
2. **高性能**: 异步并发、批量调用、精确计算
3. **可靠性**: 错误处理、状态回滚、安全检查
4. **灵活性**: Trait 抽象、可插拔组件、可配置参数
5. **可观测**: 详细日志、结构化输出、性能指标

系统通过四个核心模块的协同工作，实现了从链上状态同步到套利执行的完整闭环。

