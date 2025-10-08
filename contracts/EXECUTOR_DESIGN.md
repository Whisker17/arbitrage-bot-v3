# ArbitrageExecutor 最佳实现设计文档

## 概述

`OptimizedArbitrageExecutor` 是一个支持 Uniswap V2 和 Agni (Uniswap V3 风格) 混合路径的原子套利执行合约。

## 设计决策

### 1. 接口选择

**决策：使用 `slot0()` 而非 `globalState()`**

- ✅ `slot0()` 是 Agni 池的标准接口（与 Uniswap V3 兼容）
- ❌ `globalState()` 是 Algebra 风格接口，Agni 不使用

```solidity
// ✅ 正确的 Agni 接口
(uint160 sqrtPrice, , , , , , ) = IAgniPool(pool).slot0();
uint128 liq = IAgniPool(pool).liquidity();
```

### 2. 状态预检策略

**决策：检查 `sqrtPriceX96` 和 `liquidity`**

对于 V2 池：
- ✅ `reserve0`, `reserve1` - 必需，决定价格和可用流动性

对于 Agni (V3) 池：
- ✅ `sqrtPriceX96` - 必需，当前价格
- ✅ `liquidity` - 必需，当前活跃流动性
- ❌ `tick` - 冗余（价格已包含 tick 信息）
- ❌ `feeZtO`, `feeOtZ` - 非必需（费用通常固定）

**优势：**
- 最小化链上读取操作（gas 优化）
- 检查最关键的状态变量
- 与 Rust 后端的状态同步一致

### 3. 资产传递模式

**决策：链式传递（Daisy-Chain）+ 回调支付**

```solidity
// V2 池：链式传递
if (下一步是 V2) {
    to = 下一个池子地址;  // 直接传递给下一个池子
} else {
    to = address(this);   // 传递给合约（等待 V3 回调）
}

// V3 池：通过回调支付
function agniSwapCallback(...) {
    // 池子调用此函数索要输入代币
    IERC20(token).transfer(msg.sender, amount);
}
```

**优势 vs Hub-and-Spoke 模式：**
- ✅ 减少代币转移次数（节省 ~5k gas/跳）
- ✅ V2 到 V2 直接传递，无需经过合约
- ✅ 利用 V3 回调机制，无需预先 approve

### 4. Swap 模式选择

**决策：使用 Exact Input 模式**

```solidity
// Agni swap: amountSpecified 为正数 = exact input
IAgniPool(pool).swap(
    recipient,
    zeroForOne,
    int256(amountIn),  // 正数 = 精确输入
    sqrtPriceLimitX96,
    data
);
```

**Exact Input vs Exact Output：**

| 特性 | Exact Input (✅ 采用) | Exact Output |
|------|----------------------|--------------|
| 链下计算 | 简单（从输入算输出） | 复杂（从输出反推输入） |
| 链上执行 | 直接传入 `amountIn` | 需要负数 `amountSpecified` |
| 滑点控制 | 通过最终检查 | 通过每步检查 |
| Gas 成本 | 更低 | 稍高 |

### 5. 安全机制

**三层安全保障：**

1. **预检（Pre-Flight Check）**
   ```solidity
   // 验证链上状态与快照一致
   require(sqrtPrice == expectedPrice, "PRICE_MISMATCH");
   require(liquidity == expectedLiq, "LIQ_MISMATCH");
   ```

2. **回调验证**
   ```solidity
   // 防止 EOA 直接调用
   require(msg.sender != tx.origin, "NO_EOA_CALLBACK");
   // 验证转账成功
   require(IERC20(token).transfer(pool, amount), "TRANSFER_FAILED");
   ```

3. **最终检查（Post-Flight Check）**
   ```solidity
   // 确保达到预期输出或有利润
   uint256 minExpected = balanceBefore - _amountIn + _amountsOut[last];
   require(balanceAfter >= minExpected, "INSUFFICIENT_OUTPUT");
   ```

### 6. 参数设计

**决策：使用统一的状态数组（串联模式）**

```solidity
// _expectedStates 统一数组（按池子顺序串联）
// V2 池: [r0, r1, r0, r1, ...]
// V3 池: [sqrtPrice, liq, sqrtPrice, liq, ...]
// 混合: [r0_v2, r1_v2, sqrtPrice_v3, liq_v3, r0_v2, r1_v2, ...]
```

**优势 vs 分离数组：**
- ✅ 链下构造更简单（按路径顺序）
- ✅ 链上验证逻辑清晰（单指针遍历）
- ✅ 减少 calldata 开销（一个数组长度字段）

### 7. Gas 优化技巧

1. **Immutable 变量**
   ```solidity
   address public immutable owner;
   address public immutable WMNT;
   ```
   每次读取节省 ~2100 gas

2. **短路错误信息**
   ```solidity
   require(condition, "SHORT_ERROR");  // 节省 calldata
   ```

3. **避免冗余检查**
   - 不检查 tick（price 已包含）
   - 不检查 fee（通常固定）

4. **最小化存储操作**
   - 无需存储中间状态
   - 一次性执行完整路径

## 版本对比

| 特性 | V2 | V3 | 最佳实现 |
|------|----|----|---------|
| Agni 接口 | ❌ globalState | ✅ slot0 | ✅ slot0 |
| 预检状态 | 过多（5项） | 简化（2项） | ✅ 平衡（2项）|
| 传递模式 | ✅ 链式 | Hub-and-Spoke | ✅ 链式 |
| Swap 模式 | Exact Input | Exact Output | ✅ Exact Input |
| 参数设计 | ✅ 统一数组 | 分离数组 | ✅ 统一数组 |
| 回调安全 | 基础 | ✅ 完善 | ✅ 完善 |
| Gas 效率 | 高 | 中 | ✅ 高 |

## 使用示例

```solidity
// 套利路径: WMNT -> TokenA (V2) -> TokenB (Agni) -> WMNT (V2)

address[] memory path = [WMNT, TokenA, TokenB, WMNT];
address[] memory pools = [PoolV2_1, PoolAgni, PoolV2_2];
uint8[] memory poolTypes = [0, 1, 0];  // 0=V2, 1=Agni

uint256[] memory expectedStates = [
    reserve0_pool1,    // V2: r0
    reserve1_pool1,    // V2: r1
    sqrtPrice_agni,    // Agni: sqrtPrice
    liquidity_agni,    // Agni: liquidity
    reserve0_pool2,    // V2: r0
    reserve1_pool2     // V2: r1
];

uint256[] memory amountsOut = [
    amountOut1,        // V2 输出
    amountOut2,        // Agni 输出
    amountOut3         // V2 输出（最终）
];

executor.executeArbitrage(
    amountIn,
    path,
    pools,
    poolTypes,
    expectedStates,
    amountsOut
);
```

## 测试清单

- [ ] V2 -> V2 -> V2 纯 V2 路径
- [ ] Agni -> Agni -> Agni 纯 V3 路径
- [ ] V2 -> Agni -> V2 混合路径
- [ ] Agni -> V2 -> Agni 混合路径
- [ ] 状态不匹配时的回滚
- [ ] 最终输出不足时的回滚
- [ ] 回调函数的安全性
- [ ] 资金提取功能

## 部署检查

1. ✅ WMNT 地址正确
2. ✅ Owner 权限设置
3. ✅ 初始资金充值
4. ✅ 合约地址记录（用于 Rust 后端）

## 后续优化方向

1. **批量套利**：支持一次交易执行多个独立套利路径
2. **Flash Loan 集成**：无需预先充值，使用闪电贷
3. **动态费用**：支持 Agni 的动态费用池
4. **MEV 保护**：集成 Flashbots 或其他 MEV 解决方案
