# Execution 模块 - 交易执行与Gas管理

## 模块概述

Execution 模块负责将套利机会转换为实际的链上交易，包括交易参数构建、Gas 价格计算、Nonce 管理和合约交互。该模块确保交易以最优的 Gas 价格执行，并包含多重安全检查以避免失败和损失。

## 目录结构

```
src/execution/
├── mod.rs           # 模块入口、公共导出
├── types.rs         # 类型定义（ExecutorConfig, SwapStep, ExecutionParams）
├── executor.rs      # 主执行器（套利执行）
├── swap_executor.rs # Swap 执行器（单步交换）
├── contract.rs      # 合约接口定义
├── gas.rs           # Gas 计算逻辑
├── gas_schedule.rs  # Gas 估算表
└── nonce.rs         # Nonce 管理器
```

## 核心设计思路

### 1. 双执行器架构

```
┌─────────────────────────────────────┐
│         Executor                    │
│  (复杂套利路径执行)                 │
│  - build_params()                   │
│  - execute()                        │
│  - compute_fee_plan()               │
└────────────┬────────────────────────┘
             │
             │ 调用 ArbitrageExecutor 合约
             │
             ▼
┌─────────────────────────────────────┐
│      SwapExecutor                   │
│  (单步交换执行)                     │
│  - execute_swap()                   │
│  - execute_v2_swap()                │
│  - execute_v3_swap()                │
│  - execute_moe_lb_swap()            │
└─────────────────────────────────────┘
             │
             │ 直接调用 Router/Pool 合约
             │
             ▼
         Blockchain
```

**设计要点**:
- **Executor**: 用于多跳套利，通过自定义合约批量执行
- **SwapExecutor**: 用于单步交换，直接调用 DEX Router
- 两者可独立使用，也可组合使用

### 2. Gas 动态定价策略

```
Net Profit → Compute Fee Plan
    ↓
Base Fee (固定 20M wei on Mantle)
    +
Dynamic Priority Fee (基于利润计算)
    =
Max Fee Per Gas
    ↓
Enforce Global Cap (防止过度支付)
    ↓
Execute Transaction
```

## 核心数据结构

### 1. ExecutorConfig

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutorConfig {
    pub chain_id: u64,
    pub v3_router_address: Option<Address>,           // Uniswap V3 / Agni Router
    pub moe_router_address: Option<Address>,          // Moe Router
    pub slippage_tolerance: f64,                      // 滑点容忍度 (0.10 = 10%)
    pub gas_limit: u64,                               // 默认 Gas limit
    pub default_priority_fee_wei: u128,               // 默认优先费
    pub global_fee_hard_cap_wei: u128,                // 全局 Gas 价格上限
    pub fee_mode: FeeMode,                            // Legacy 或 EIP-1559
    pub min_net_profit_mnt_wei: U256,                 // 最小净利润
    pub include_gas_cost_in_min_out: bool,            // 是否在 min_out 中包含 Gas 成本
    pub enforce_non_loss: bool,                       // 强制非负利润
    pub fixed_gas_price_wei: Option<u128>,            // 固定 Gas 价格（可选）
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            chain_id: 5000,  // Mantle Mainnet
            v3_router_address: None,
            moe_router_address: None,
            slippage_tolerance: 0.10,
            gas_limit: 600_000_000,
            default_priority_fee_wei: 100_000,  // 0.0001 gwei in Mantle wei
            global_fee_hard_cap_wei: 500_000_000,  // 0.5 gwei
            fee_mode: FeeMode::Eip1559,
            min_net_profit_mnt_wei: U256::from(0u64),
            include_gas_cost_in_min_out: true,
            enforce_non_loss: true,
            fixed_gas_price_wei: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FeeMode {
    Legacy,    // gas_price
    Eip1559,   // max_fee_per_gas + max_priority_fee_per_gas
}
```

### 2. ExecutionContext

```rust
#[derive(Clone, Debug)]
pub struct ExecutionContext {
    pub executor_contract: Address,  // 自定义套利执行器合约地址
    pub wmnt_address: Address,       // WMNT 代币地址
}
```

### 3. ExecutionParams

```rust
#[derive(Clone, Debug)]
pub struct ExecutionParams {
    pub amount_in: U256,                        // 输入量
    pub token_path: Vec<Address>,               // Token 路径
    pub pool_addresses: Vec<Address>,           // Pool 地址
    pub expected_reserves_u112: Vec<U112>,      // 预期储备（V2 池）
    pub step_amounts_out: Vec<U256>,            // 每步输出量
    pub min_amount_out: U256,                   // 最小输出量（滑点保护）
    pub expected_net_profit_mnt_wei: U256,      // 预期净利润
}
```

### 4. SwapStep

```rust
#[derive(Clone, Debug)]
pub struct SwapStep {
    pub pool_address: Address,
    pub pool_type: PoolType,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub expected_amount_out: Option<U256>,
    pub sqrt_price_limit: Option<U160>,         // V3: 价格限制
    pub zero_for_one: Option<bool>,             // V3: 方向
    pub fee: Option<u32>,                       // V3: 手续费等级
    pub router_address: Option<Address>,        // Router 地址（可选）
    pub swap_for_y: Option<bool>,               // Moe: 方向
    pub bin_step: Option<u16>,                  // Moe: bin step
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PoolType {
    UniV2,      // Uniswap V2 style
    UniV3,      // Uniswap V3 style
    MoeLB,      // Moe Liquidity Book
}
```

## Executor - 套利执行器

### 1. 核心结构

```rust
pub struct Executor {
    pub config: ExecutorConfig,
    pub context: ExecutionContext,
}

impl Executor {
    pub fn new(context: ExecutionContext, config: ExecutorConfig) -> Self {
        Self { config, context }
    }
    
    // 构建执行参数
    pub async fn build_params<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<ExecutionParams> { /* ... */ }
    
    // 执行套利
    pub async fn execute<P: Provider>(
        &self,
        provider: &P,
        params: &ExecutionParams,
    ) -> Result<TxHash> { /* ... */ }
    
    // 便捷方法：构建 + 执行
    pub async fn execute_opportunity<P: Provider>(
        &self,
        provider: &P,
        opportunity: &ArbitrageOpportunity,
    ) -> Result<TxHash> { /* ... */ }
}
```

### 2. 参数构建

```rust
pub async fn build_params<P: Provider>(
    &self,
    provider: &P,
    opportunity: &ArbitrageOpportunity,
) -> Result<ExecutionParams> {
    // 1. 构建 token 路径
    let mut token_path: Vec<Address> = opportunity.path.tokens
        .iter()
        .map(|t| t.get_address())
        .collect();
    
    // 确保起点和终点是 WMNT
    let len = token_path.len();
    if len >= 1 {
        if token_path[0] != self.context.wmnt_address {
            token_path[0] = self.context.wmnt_address;
        }
        if token_path[len - 1] != self.context.wmnt_address {
            token_path[len - 1] = self.context.wmnt_address;
        }
    }
    
    // 2. 提取 pool 地址
    let pool_addresses: Vec<Address> = opportunity.path.pools
        .iter()
        .map(|p| p.get_address())
        .collect();
    
    // 3. 从链上获取当前储备和计算输出量
    let mut expected_reserves_u112: Vec<U112> = Vec::with_capacity(pool_addresses.len() * 2);
    let mut step_amounts_out: Vec<U256> = Vec::with_capacity(pool_addresses.len());
    let mut current_amount = opportunity.optimal_input_amount;
    
    for (i, pool_addr) in pool_addresses.iter().enumerate() {
        let pair = IMoePair::new(*pool_addr, provider);
        let reserves = pair.getReserves().call().await?;
        expected_reserves_u112.push(reserves._0);
        expected_reserves_u112.push(reserves._1);
        
        // 确定输入/输出储备
        let token_in = token_path[i];
        let token0 = pair.token0().call().await?;
        let (reserve_in, reserve_out) = if token_in == token0 {
            (U256::from(reserves._0), U256::from(reserves._1))
        } else {
            (U256::from(reserves._1), U256::from(reserves._0))
        };
        
        // Uniswap V2 公式
        let numerator = current_amount * U256::from(997u64) * reserve_out;
        let denominator = reserve_in * U256::from(1000u64) + current_amount * U256::from(997u64);
        let out = if denominator.is_zero() {
            U256::ZERO
        } else {
            numerator / denominator
        };
        
        step_amounts_out.push(out);
        current_amount = out;
    }
    
    // 4. 计算 min_amount_out (考虑滑点)
    let expected_profit = if current_amount > opportunity.optimal_input_amount {
        current_amount - opportunity.optimal_input_amount
    } else {
        U256::ZERO
    };
    let slippage_allowance = mul_fraction(expected_profit, self.config.slippage_tolerance);
    let mut min_amount_out = current_amount.saturating_sub(slippage_allowance);
    
    // 5. 强制非负利润
    if self.config.include_gas_cost_in_min_out || self.config.enforce_non_loss {
        let gas_cost = opportunity.gas_cost_mnt_wei;
        let required_out = opportunity.optimal_input_amount.saturating_add(gas_cost);
        if min_amount_out < required_out {
            min_amount_out = required_out;
        }
    }
    
    Ok(ExecutionParams {
        amount_in: opportunity.optimal_input_amount,
        token_path,
        pool_addresses,
        expected_reserves_u112,
        step_amounts_out,
        min_amount_out,
        expected_net_profit_mnt_wei: opportunity.net_profit_mnt_wei,
    })
}
```

**关键点**:
- 实时查询储备（避免过期数据）
- 计算每一跳的预期输出
- 应用滑点容忍度
- 包含 Gas 成本（可选）

### 3. Fee Plan 计算

```rust
#[derive(Clone, Copy, Debug)]
pub struct FeePlan {
    pub gas_limit: u64,
    pub base_fee_wei: u128,
    pub default_priority_fee_wei: u128,
    pub total_cap_from_profit: u128,
    pub cap_priority_fee_from_profit: u128,
    pub initial_priority_fee: u128,
    pub max_priority_fee_per_gas_wei: u128,
    pub max_fee_per_gas_wei: u128,
    pub effective_global_cap_wei: u128,
    pub is_small_profit: bool,
}

pub const MANTLE_BASE_FEE_WEI: u128 = 20_000_000u128;  // 固定 20M wei

pub fn compute_fee_plan(config: &ExecutorConfig, hops: usize, net_expected: U256) -> FeePlan {
    // 1. 根据跳数调整 Gas limit
    let gas_limit = match hops {
        4 => 750_000_000,
        2 => 450_000_000,
        _ => config.gas_limit,
    };
    
    let priority_fee_wei = config.default_priority_fee_wei;
    let base_fee_wei = MANTLE_BASE_FEE_WEI;
    let net_expected_u128 = net_expected.to_string().parse::<u128>().unwrap_or(0);
    
    // 2. 根据利润设置动态全局上限
    let one_wmnt = U256::from(1_000_000_000_000_000_000u128);
    let five_wmnt = U256::from(5_000_000_000_000_000_000u128);
    
    let effective_global_cap_wei = if net_expected < one_wmnt {
        500_000_000u128      // 0.5 gwei for small profits
    } else if net_expected < five_wmnt {
        3_000_000_000u128    // 3 gwei for medium profits
    } else {
        u128::MAX            // No cap for large profits
    };
    
    let is_very_small_profit = net_expected < U256::from(50_000_000_000_000_000u128);  // < 0.05 WMNT
    let is_small_profit = net_expected < U256::from(100_000_000_000_000_000u128);      // < 0.1 WMNT
    
    // 3. 从利润计算总费用上限
    let mut total_cap_from_profit = if gas_limit > 0 {
        net_expected_u128.saturating_div(gas_limit as u128)
    } else {
        0
    };
    
    // 4. 对小利润增加激进的 Gas 价格
    if total_cap_from_profit > 0 {
        if is_very_small_profit {
            total_cap_from_profit = total_cap_from_profit.saturating_mul(3).saturating_div(2);  // +50%
        } else if is_small_profit {
            total_cap_from_profit = total_cap_from_profit.saturating_mul(3).saturating_div(2);  // +50%
        }
    }
    
    // 5. 计算优先费上限
    let cap_priority_fee_from_profit = total_cap_from_profit.saturating_sub(base_fee_wei);
    let initial_priority_fee = priority_fee_wei.max(cap_priority_fee_from_profit);
    
    // 6. 计算最终 max_fee_per_gas
    let mut max_fee_per_gas_wei = base_fee_wei.saturating_add(initial_priority_fee);
    if max_fee_per_gas_wei > effective_global_cap_wei {
        max_fee_per_gas_wei = effective_global_cap_wei;
    }
    
    let max_priority_fee_per_gas_wei = 
        initial_priority_fee.min(max_fee_per_gas_wei.saturating_sub(base_fee_wei));
    
    FeePlan {
        gas_limit,
        base_fee_wei,
        default_priority_fee_wei: priority_fee_wei,
        total_cap_from_profit,
        cap_priority_fee_from_profit,
        initial_priority_fee,
        max_priority_fee_per_gas_wei,
        max_fee_per_gas_wei,
        effective_global_cap_wei,
        is_small_profit,
    }
}
```

**Gas 定价策略**:

| 利润范围 | 全局上限 | Gas Limit 调整 |
|---------|---------|---------------|
| < 0.05 WMNT | 0.5 gwei | 基础 + 50% |
| 0.05 ~ 0.1 WMNT | 0.5 gwei | 基础 + 50% |
| 0.1 ~ 1 WMNT | 0.5 gwei | 基础 |
| 1 ~ 5 WMNT | 3 gwei | 基础 |
| > 5 WMNT | 无上限 | 基础 |

**设计原则**:
1. **Mantle 固定 Base Fee**: 20M wei (不使用 EIP-1559 的动态 base fee)
2. **利润驱动定价**: 利润越大，愿意支付更高的 Gas
3. **防止过度支付**: 设置全局上限
4. **小利润激进**: 对小利润机会提高 Gas 价格以增加竞争力

### 4. 交易执行

```rust
pub async fn execute<P: Provider>(
    &self,
    provider: &P,
    params: &ExecutionParams,
) -> Result<TxHash> {
    // 1. 计算 Gas 费用
    let hops = params.token_path.len().saturating_sub(1);
    let net_expected = if !params.expected_net_profit_mnt_wei.is_zero() {
        params.expected_net_profit_mnt_wei
    } else {
        params.min_amount_out.saturating_sub(params.amount_in)
    };
    
    // 2. 强制非负利润检查
    if self.config.enforce_non_loss && net_expected.is_zero() {
        eyre::bail!("Abort execution: non-loss requirement not satisfied");
    }
    
    let fee_plan = compute_fee_plan(&self.config, hops, net_expected);
    
    // 3. 验证 EIP-1559 参数
    if fee_plan.max_priority_fee_per_gas_wei > fee_plan.max_fee_per_gas_wei {
        eyre::bail!(
            "Invalid EIP-1559 fees: max_priority_fee_per_gas ({}) > max_fee_per_gas ({})",
            fee_plan.max_priority_fee_per_gas_wei,
            fee_plan.max_fee_per_gas_wei
        );
    }
    
    // 4. 日志记录
    tracing::info!(
        base_fee_wei = fee_plan.base_fee_wei,
        max_priority_fee_per_gas_wei = fee_plan.max_priority_fee_per_gas_wei,
        max_fee_per_gas_wei = fee_plan.max_fee_per_gas_wei,
        "Gas caps computed"
    );
    
    // 5. 构造合约调用
    let contract = IArbitrageExecutor::new(self.context.executor_contract, provider);
    
    // 计算 Gas 成本并调整最后一跳的输出量
    let gas_cost_at_cap = (fee_plan.gas_limit as u128)
        .saturating_mul(fee_plan.max_fee_per_gas_wei);
    let required_out = if (self.config.include_gas_cost_in_min_out 
        || self.config.enforce_non_loss) 
        && !fee_plan.is_small_profit 
    {
        params.amount_in.saturating_add(U256::from(gas_cost_at_cap))
    } else {
        params.min_amount_out
    };
    
    // 调整最后一跳输出量（留一点余量避免四舍五入错误）
    let mut step_amounts_out = params.step_amounts_out.clone();
    let computed_last = *step_amounts_out.last().unwrap_or(&U256::ZERO);
    
    if computed_last < required_out {
        eyre::bail!(
            "Skip execution: expected last-hop out {} < required {} (would violate invariant)",
            computed_last,
            required_out
        );
    }
    
    let haircut = U256::from(1u64);
    let mut target_last = computed_last.saturating_sub(haircut);
    if target_last < required_out {
        target_last = required_out;
    }
    
    if let Some(last) = step_amounts_out.last_mut() {
        *last = target_last;
    }
    
    // 6. 发起交易
    let call = contract
        .executeArbitrage(
            params.amount_in,
            params.token_path.clone(),
            params.pool_addresses.clone(),
            vec![1u8; params.pool_addresses.len()],  // Pool types (暂时全部使用 V2)
            params.expected_reserves_u112.iter()
                .map(|v| U256::from(*v))
                .collect(),
            step_amounts_out,
        )
        .gas(fee_plan.gas_limit);
    
    let pending = match self.config.fee_mode {
        FeeMode::Legacy => {
            call.gas_price(fee_plan.max_fee_per_gas_wei).send().await?
        }
        FeeMode::Eip1559 => {
            call.max_fee_per_gas(fee_plan.max_fee_per_gas_wei)
                .max_priority_fee_per_gas(fee_plan.max_priority_fee_per_gas_wei)
                .send()
                .await?
        }
    };
    
    Ok(*pending.tx_hash())
}
```

**执行流程**:

```
1. 计算 Gas 费用
    ↓
2. 强制非负检查
    ↓
3. 构造合约调用
    ↓
4. 调整输出量（防止四舍五入错误）
    ↓
5. 发起交易（Legacy 或 EIP-1559）
    ↓
6. 返回交易哈希
```

**安全机制**:
- 预飞行检查：确保预期输出满足要求
- Haircut：减少 1 wei 避免四舍五入导致失败
- Gas 上限：防止过度支付
- 非负利润：可选强制执行

## SwapExecutor - 单步交换执行器

### 1. 核心结构

```rust
pub struct SwapExecutor;

impl SwapExecutor {
    // 执行单步交换
    pub async fn execute_swap<P: Provider>(
        provider: &P,
        swap_step: &SwapStep,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<U256> { /* ... */ }
    
    // 执行多步交换
    pub async fn execute_multi_swap<P: Provider>(
        provider: &P,
        swap_steps: Vec<SwapStep>,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<Vec<U256>> { /* ... */ }
    
    // 自动检测池类型并构建 SwapStep
    pub async fn build_swap_step<P: Provider>(
        provider: &P,
        pool_address: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<SwapStep> { /* ... */ }
}
```

### 2. Uniswap V2 交换

```rust
async fn execute_v2_swap<P: Provider>(
    provider: &P,
    swap_step: &SwapStep,
    from_address: Address,
) -> Result<U256> {
    let pool = IMoePair::new(swap_step.pool_address, provider);
    
    // 1. 获取池信息
    let token0 = pool.token0().call().await?;
    let reserves = pool.getReserves().call().await?;
    
    // 2. 确定交换方向
    let zero_for_one = swap_step.token_in == token0;
    let (reserve_in, reserve_out) = if zero_for_one {
        (U256::from(reserves._0), U256::from(reserves._1))
    } else {
        (U256::from(reserves._1), U256::from(reserves._0))
    };
    
    // 3. 计算预期输出
    let numerator = swap_step.amount_in * U256::from(997u64) * reserve_out;
    let denominator = reserve_in * U256::from(1000u64) + swap_step.amount_in * U256::from(997u64);
    let expected_out = if denominator.is_zero() {
        U256::ZERO
    } else {
        numerator / denominator
    };
    
    // 4. 批准 token
    Self::ensure_approval(
        provider,
        swap_step.token_in,
        swap_step.pool_address,
        swap_step.amount_in,
        from_address,
    ).await?;
    
    // 5. 转账到池
    let token_in_contract = IERC20::new(swap_step.token_in, provider);
    let transfer_tx = token_in_contract
        .transfer(swap_step.pool_address, swap_step.amount_in)
        .send()
        .await?;
    transfer_tx.watch().await?;
    
    // 6. 执行 swap
    let (amount0_out, amount1_out) = if zero_for_one {
        (U256::ZERO, expected_out)
    } else {
        (expected_out, U256::ZERO)
    };
    
    let swap_call = pool.swap(amount0_out, amount1_out, from_address, Bytes::new());
    let gas_limit = gas_limit_for_hops(1);
    let pending_tx = swap_call.gas(gas_limit).send().await?;
    pending_tx.watch().await?;
    
    Ok(expected_out)
}
```

### 3. Uniswap V3 交换（通过 Router）

```rust
async fn execute_v3_swap<P: Provider>(
    provider: &P,
    swap_step: &SwapStep,
    from_address: Address,
    config: &ExecutorConfig,
) -> Result<U256> {
    let router_address = swap_step.router_address
        .or(config.v3_router_address)
        .ok_or_else(|| eyre!("No router address configured for V3 swap"))?;
    let router = IAgniSwapRouter::new(router_address, provider);
    
    // 1. 获取 fee tier
    let fee = if let Some(fee) = swap_step.fee.map(U24::from) {
        fee
    } else {
        let pool = IAgniPool::new(swap_step.pool_address, provider);
        pool.fee().call().await.unwrap_or(U24::from(3000u32))
    };
    
    // 2. 批准 Router
    Self::ensure_approval(
        provider,
        swap_step.token_in,
        router_address,
        swap_step.amount_in,
        from_address,
    ).await?;
    
    // 3. 构造 swap 参数
    let params = IAgniSwapRouter::ExactInputSingleParams {
        tokenIn: swap_step.token_in,
        tokenOut: swap_step.token_out,
        fee,
        recipient: from_address,
        deadline: U256::MAX,
        amountIn: swap_step.amount_in,
        amountOutMinimum: U256::ZERO,
        sqrtPriceLimitX96: swap_step.sqrt_price_limit.unwrap_or(U160::from(0u64)),
    };
    
    // 4. 执行 swap
    let swap_call = router.exactInputSingle(params);
    let gas_limit = gas_limit_for_hops(1);
    let pending_tx = swap_call.gas(gas_limit).send().await?;
    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;
    
    // 5. 验证交易成功
    let receipt = provider.get_transaction_receipt(tx_hash).await?
        .ok_or_else(|| eyre!("Router swap transaction missing receipt"))?;
    
    if !receipt.status() {
        return Err(eyre!("Router swap transaction reverted"));
    }
    
    // 6. 查询余额（获取实际输出量）
    let token_out_contract = IERC20::new(swap_step.token_out, provider);
    let amount_out = token_out_contract.balanceOf(from_address).call().await?;
    
    Ok(amount_out)
}
```

### 4. Moe Liquidity Book 交换（通过 Router）

```rust
async fn execute_moe_lb_swap<P: Provider>(
    provider: &P,
    swap_step: &SwapStep,
    from_address: Address,
    config: &ExecutorConfig,
) -> Result<U256> {
    let router_address = swap_step.router_address
        .or(config.moe_router_address)
        .ok_or_else(|| eyre!("No Moe router address configured"))?;
    let router = ILBRouter::new(router_address, provider);
    
    // 1. 获取 bin step
    let bin_step = swap_step.bin_step
        .ok_or_else(|| eyre!("Bin step not provided for Moe LB swap"))?;
    
    // 2. 批准 Router
    Self::ensure_approval(
        provider,
        swap_step.token_in,
        router_address,
        swap_step.amount_in,
        from_address,
    ).await?;
    
    // 3. 构造 Path
    let path = ILBRouter::Path {
        pairBinSteps: vec![U256::from(bin_step)],
        versions: vec![2u8],  // V2_1 for current Moe LB
        tokenPath: vec![swap_step.token_in, swap_step.token_out],
    };
    
    // 4. 执行 swap
    let swap_call = router.swapExactTokensForTokens(
        swap_step.amount_in,
        U256::ZERO,
        path,
        from_address,
        U256::MAX,
    );
    
    let gas_limit = gas_limit_for_hops(1);
    let pending_tx = swap_call.gas(gas_limit).send().await?;
    let tx_hash = *pending_tx.tx_hash();
    pending_tx.watch().await?;
    
    // 5. 验证并获取输出
    let receipt = provider.get_transaction_receipt(tx_hash).await?
        .ok_or_else(|| eyre!("Moe LB swap missing receipt"))?;
    
    if !receipt.status() {
        return Err(eyre!("Moe LB swap reverted"));
    }
    
    let token_out_contract = IERC20::new(swap_step.token_out, provider);
    let amount_out = token_out_contract.balanceOf(from_address).call().await?;
    
    Ok(amount_out)
}
```

## Gas Schedule

```rust
pub fn gas_limit_for_hops(hops: usize) -> u64 {
    match hops {
        1 => 450_000_000,
        2 => 500_000_000,
        3 => 600_000_000,
        4 => 750_000_000,
        _ => 600_000_000,  // 默认
    }
}
```

**Mantle Gas 特点**:
- Gas limit 很大（相比 Ethereum）
- Base fee 固定（20M wei）
- Priority fee 很低（通常 < 1 gwei）

## Nonce Manager（TODO）

```rust
pub struct NonceManager {
    current_nonce: Arc<AtomicU64>,
    pending_nonces: Arc<RwLock<HashSet<u64>>>,
}

impl NonceManager {
    pub fn new(initial_nonce: u64) -> Self {
        Self {
            current_nonce: Arc::new(AtomicU64::new(initial_nonce)),
            pending_nonces: Arc::new(RwLock::new(HashSet::new())),
        }
    }
    
    pub async fn get_next_nonce(&self) -> u64 {
        let nonce = self.current_nonce.fetch_add(1, Ordering::SeqCst);
        self.pending_nonces.write().await.insert(nonce);
        nonce
    }
    
    pub async fn mark_confirmed(&self, nonce: u64) {
        self.pending_nonces.write().await.remove(&nonce);
    }
    
    pub async fn has_pending(&self) -> bool {
        !self.pending_nonces.read().await.is_empty()
    }
}
```

## 组件内协同

### 1. Executor ↔ Gas Schedule

```
Executor.execute()
    ↓
compute_fee_plan(hops, net_profit)
    ↓
gas_limit_for_hops(hops)
    ↓
FeePlan
```

### 2. SwapExecutor ↔ 各协议 Router

```
SwapExecutor.execute_swap()
    ↓
match pool_type {
    UniV2 => execute_v2_swap()
    UniV3 => execute_v3_swap()
    MoeLB => execute_moe_lb_swap()
}
```

## 组件间协同

### 1. Execution ← Arbitrage

```
ArbitrageMonitor.opportunistic_scan()
    ↓
OpportunisticScanResult { opportunities }
    ↓
for opp in opportunities {
    Executor.build_params(opp)?
        ↓
    Executor.execute(params)?
}
```

### 2. Execution → Blockchain

```
Executor.execute()
    ↓
IArbitrageExecutor.executeArbitrage()
    ↓
Blockchain (Mantle)
```

## 性能优化

### 1. Gas 价格优化

```rust
// 根据利润动态调整
if net_profit > 5 WMNT {
    // 无上限，争取执行
} else if net_profit > 1 WMNT {
    // 中等上限
    max_fee = 3 gwei
} else {
    // 严格上限
    max_fee = 0.5 gwei
}
```

### 2. 批量交易

使用自定义合约 `ArbitrageExecutor` 批量执行多跳：

```solidity
function executeArbitrage(
    uint256 amountIn,
    address[] memory tokenPath,
    address[] memory poolAddresses,
    uint8[] memory poolTypes,
    uint256[] memory expectedReserves,
    uint256[] memory stepAmountsOut
) external returns (uint256)
```

减少单独交易的开销。

### 3. 预检查

```rust
// 执行前验证
if computed_last < required_out {
    return Err("Would fail, skip execution");
}
```

避免浪费 Gas 在必然失败的交易上。

## 测试策略

### 1. 单元测试

```rust
#[test]
fn test_compute_fee_plan_small_profit() {
    let config = ExecutorConfig::default();
    let net_profit = U256::from(50_000_000_000_000_000u64);  // 0.05 WMNT
    let fee_plan = compute_fee_plan(&config, 3, net_profit);
    
    assert_eq!(fee_plan.effective_global_cap_wei, 500_000_000);
    assert!(fee_plan.is_small_profit);
}
```

### 2. 集成测试

```rust
#[tokio::test]
async fn test_execute_v2_swap() {
    let swap_step = SwapStep {
        pool_address: /* ... */,
        pool_type: PoolType::UniV2,
        token_in: usdc_address,
        token_out: wmnt_address,
        amount_in: U256::from(1_000_000u64),  // 1 USDC
        /* ... */
    };
    
    let amount_out = SwapExecutor::execute_swap(
        provider,
        &swap_step,
        from_address,
        &config,
    ).await?;
    
    assert!(amount_out > U256::ZERO);
}
```

## 总结

Execution 模块通过以下设计实现了安全、高效的交易执行：

1. **动态 Gas 定价**: 根据利润智能调整 Gas 价格
2. **双执行器**: 支持复杂套利和简单交换
3. **多协议支持**: 统一接口处理 V2、V3、Moe LB
4. **安全机制**: 预飞行检查、滑点保护、非负强制
5. **批量优化**: 自定义合约减少交易开销
6. **灵活配置**: Gas 上限、滑点、费用模式可调
7. **详细日志**: 完整的执行参数和费用信息

该模块为套利系统提供了可靠的链上执行能力。

