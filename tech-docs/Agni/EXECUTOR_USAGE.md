# 执行器使用指南

## 概述

本项目现在包含一个统一的交易执行器系统，支持 Uniswap V2 和 Agni (Uniswap V3) 风格的交易。

## 架构

### 模块结构

```
src/execution/
├── mod.rs              # 模块导出
├── types.rs            # 类型定义（PoolType, SwapStep, ExecutorConfig等）
├── contract.rs         # 合约接口（IMoePair, IAgniPool, IERC20等）
├── swap_executor.rs    # 直接交易执行器（推荐使用）
├── executor.rs         # 套利机会执行器（需要 logic 模块）
├── gas_profile.rs      # 测量得到的 Gas profile
├── gas_runtime.rs      # Runtime profile 持久化与失效处理
└── nonce.rs            # Nonce 管理
```

### 核心组件

#### 1. SwapExecutor（推荐）

`SwapExecutor` 是一个简化的执行器，用于直接执行单个或多个池子的交换。

**特性：**
- 自动检测池子类型（V2 或 V3）
- 支持单步和多步交换
- 自动处理代币授权
- 详细的日志记录

**使用示例：**

```rust
use amms::execution::{SwapExecutor, SwapStep, PoolType, ExecutorConfig};

// 方法 1: 自动构建交换步骤
let swap_step = SwapExecutor::build_swap_step(
    &provider,
    pool_address,
    token_in,
    token_out,
    amount_in,
).await?;

// 执行单个交换
let mut config = ExecutorConfig::default();
config.v3_router_address = Some(address!("e38cfa32cCd918d94E2e20230dFaD1A4Fd8aEF16"));

let amount_out = SwapExecutor::execute_swap(
    &provider,
    &swap_step,
    from_address,
    &config,
).await?;

// 方法 2: 手动构建交换步骤
let swap_step = SwapStep {
    pool_address,
    pool_type: PoolType::UniV3,
    token_in,
    token_out,
    amount_in,
    expected_amount_out: None,
    sqrt_price_limit: None,
    zero_for_one: None,
    fee: None,
    router_address: None,
};

// 执行多步交换
let results = SwapExecutor::execute_multi_swap(
    &provider,
    vec![step1, step2, step3],
    from_address,
    &config,
).await?;
```

#### 2. Executor

`Executor` 是一个更复杂的执行器，设计用于执行套利机会。它需要 `logic` 模块中的 `ArbitrageOpportunity` 类型。

**特性：**
- 动态 Gas 定价
- 滑点保护
- 预检查机制
- 利润保护

**使用示例：**

```rust
use amms::execution::{Executor, ExecutorConfig, ExecutionContext};

let config = ExecutorConfig {
    chain_id: 5003,
    slippage_tolerance: 0.10,
    gas_limit: 600_000_000,
    ..Default::default()
};

let context = ExecutionContext {
    executor_contract: executor_address,
    wmnt_address,
};

let executor = Executor::new(context, config);

// 执行套利机会
let tx_hash = executor.execute_opportunity(
    &provider,
    &opportunity,
).await?;
```

## 池子类型

### PoolType 枚举

```rust
pub enum PoolType {
    /// Uniswap V2 style (e.g., MoeLP)
    UniV2,
    /// Uniswap V3 style (e.g., Agni)
    UniV3,
}
```

### V2 vs V3 差异

| 特性 | Uniswap V2 | Uniswap V3 (Agni) |
|------|-----------|-------------------|
| 价格模型 | 恒定乘积 (x*y=k) | 集中流动性 |
| 价格限制 | 无 | sqrt price limit |
| 交换方向 | 隐式 | 显式 (zeroForOne) |
| 回调 | 可选 | 必需 |
| Gas 成本 | 较低 | 较高 |

## 示例脚本

### 1. 简单交换测试

```bash
# 使用执行器的简单交换
cargo run --example simple_swap_with_executor

# 不使用执行器的简单交换（直接调用）
cargo run --example simple_swap_test
```

### 2. 多池连环交易

```bash
# 使用执行器的多池交换
cargo run --example multi_pool_swap_with_executor

# 不使用执行器的多池交换
cargo run --example multi_pool_swap
```

### 3. 池子信息查询

```bash
# 查看所有池子的详细信息
cargo run --example pool_info
```

### 4. 余额检查

```bash
# 检查账户余额并自动包装 WMNT
cargo run --example check_balance
```

## 配置

### ExecutorConfig

```rust
pub struct ExecutorConfig {
    pub chain_id: u64,
    pub slippage_tolerance: f64,
    pub gas_limit: u64,
    pub default_priority_fee_wei: u128,
    pub global_fee_hard_cap_wei: u128,
    pub fee_mode: FeeMode,
    pub min_net_profit_mnt_wei: U256,
    pub include_gas_cost_in_min_out: bool,
    pub enforce_non_loss: bool,
    pub fixed_gas_price_wei: Option<u128>,
}
```

**默认配置：**
- Chain ID: 5000 (Mantle)
- 滑点容忍度: 10%
- Gas 限制: 600,000,000
- 优先费用: 0.0001 gwei
- 全局费用上限: 0.5 gwei
- 费用模式: Legacy
- 最小净利润: 0
- 包含 Gas 成本: true
- 强制非亏损: true

### SwapStep

```rust
pub struct SwapStep {
    pub pool_address: Address,
    pub pool_type: PoolType,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub expected_amount_out: Option<U256>,
    pub sqrt_price_limit: Option<U160>,  // V3 only
    pub zero_for_one: Option<bool>,      // V3 only
}
```

## 常见问题

### 1. 交易失败：execution reverted

**可能原因：**
- 池子流动性不足
- 价格滑点过大
- 代币未授权
- 价格限制设置不当

**解决方案：**
```rust
// 1. 检查池子流动性
cargo run --example pool_info

// 2. 使用更宽松的价格限制
let sqrt_price_limit = if zero_for_one {
    current_sqrt_price * U160::from(50u64) / U160::from(100u64)
} else {
    current_sqrt_price * U160::from(150u64) / U160::from(100u64)
};

// 3. 确保代币已授权
SwapExecutor::ensure_approval(...).await?;
```

### 2. Gas 估算失败

**可能原因：**
- 交换参数错误
- 池子状态异常
- 余额不足

**解决方案：**
```rust
// 使用 match 捕获详细错误
match swap_call.estimate_gas().await {
    Ok(estimate) => {
        // 继续执行
    }
    Err(e) => {
        error!("Gas estimation failed: {:?}", e);
        return Err(e.into());
    }
}
```

### 3. 池子类型检测错误

**解决方案：**
```rust
// 手动指定池子类型
let swap_step = SwapStep {
    pool_type: PoolType::UniV3,  // 或 PoolType::UniV2
    ..
};
```

## 最佳实践

### 1. 使用 SwapExecutor 进行直接交换

```rust
// ✅ 推荐：使用 SwapExecutor
let amount_out = SwapExecutor::execute_swap(&provider, &swap_step, from_address, &config).await?;

// ❌ 不推荐：直接调用合约
let pool = IAgniPool::new(pool_address, &provider);
pool.swap(...).await?;
```

### 2. 批量执行多步交换

```rust
// ✅ 推荐：使用 execute_multi_swap
let results = SwapExecutor::execute_multi_swap(&provider, swap_steps, from_address).await?;

// ❌ 不推荐：循环执行单步
for step in swap_steps {
    SwapExecutor::execute_swap(&provider, &step, from_address, &config).await?;
}
```

### 3. 错误处理

```rust
// ✅ 推荐：详细的错误处理
match SwapExecutor::execute_swap(&provider, &swap_step, from_address, &config).await {
    Ok(amount_out) => {
        info!("Swap successful: {}", amount_out);
    }
    Err(e) => {
        error!("Swap failed: {:?}", e);
        // 处理错误
    }
}

// ❌ 不推荐：忽略错误
let amount_out = SwapExecutor::execute_swap(&provider, &swap_step, from_address, &config).await?;
```

### 4. 日志记录

```rust
// 初始化日志
tracing_subscriber::fmt::init();

// 使用结构化日志
info!(
    target: "swap_executor",
    pool = %pool_address,
    amount_in = %amount_in,
    amount_out = %amount_out,
    "Swap completed"
);
```

## 未来改进

- [ ] 支持更多池子类型（Curve, Balancer 等）
- [ ] 添加交易模拟功能
- [ ] 优化 Gas 估算
- [ ] 添加批量交易支持
- [ ] 实现交易重试机制
- [ ] 添加更多预检查
- [ ] 支持 EIP-4844 (blob transactions)

## 参考资料

- [Uniswap V2 文档](https://docs.uniswap.org/contracts/v2/overview)
- [Uniswap V3 文档](https://docs.uniswap.org/contracts/v3/overview)
- [Agni Finance 文档](https://docs.agni.finance/)
- [Alloy 文档](https://alloy.rs/)
