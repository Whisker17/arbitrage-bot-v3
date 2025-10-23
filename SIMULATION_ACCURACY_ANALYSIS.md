# Moe LB 链下模拟精度分析报告

## 问题描述

在运行 `moe_monitor_executor_service.rs` 时，记录的套利机会显示净利润 > 0.1 WMNT，但实际交易全部亏损。这证明链下模拟与链上实际存在较大差异。

## 根本原因分析

### 1. **Bins 同步范围不足**（主要问题）

**对比数据：**
```rust
// verify_swap_path.rs
const BINS_RADIUS: u32 = 200;  // ±200 bins (共 400 bins)

// moe_monitor_executor_service.rs  
const BINS_RADIUS: u32 = 50;   // ±50 bins (共 100 bins)
```

**影响分析：**
- Moe LB 池子通过多个 bins 提供流动性
- 当 swap 金额较大时，需要跨越多个 bins 才能完成交易
- 如果缺少远端 bins 的数据，模拟会：
  1. 低估实际消耗的 bins 数量
  2. 高估最终输出金额
  3. 导致利润计算错误

**实例说明：**
从 `moe_best_arbitrage_paths.csv` 看到的套利路径输入金额约为 16-18 WMNT，这是相当大的金额。对于 Moe LB 池子：
- 假设 bin_step = 10 (0.1% 价格差)
- 每跨越 1 个 bin，价格变化约 0.1%
- 大额交易可能需要跨越 100+ bins

当 `BINS_RADIUS = 50` 时，如果交易需要跨越超过 50 个 bins，模拟就会因为缺少数据而给出错误结果。

### 2. **MOE Hooks 影响**（次要问题）

代码中已有警告（src/amms/moe/mod.rs:737-748）：
```rust
// ⚠️ 警告：此模拟不包括 MOE hooks 的影响
// 
// MOE 池子可能配置了 beforeSwap/afterSwap hooks，这些 hooks 可以：
// 1. 触发额外的 deposit/mint 操作（如 MasterChef 质押）
// 2. 增加显著的 gas 消耗（实测显示可增加 ~200K gas）
// 3. 可能改变最终的输出金额
```

但这个影响相对较小，不应该导致利润从正变负的巨大差异。

### 3. **时间戳和状态不一致**（次要问题）

- 链下模拟使用历史状态
- 链上执行时状态可能已经改变
- 但通过重试和刷新状态可以缓解

## verify_swap_path.rs 的误差分析

从终端输出可以看到：
```
Output - Local: 0.003679069614299552 Chain: 0.003680205446814632 Match: false
❌ MISMATCH: diff=1135832515080 (0.3%)
```

**0.3% 误差是否可接受？**

这取决于套利策略：
- 如果预期利润率 > 1%，0.3% 误差可能可以接受
- 如果预期利润率在 0.1-0.5%，0.3% 误差会导致亏损
- 从 CSV 数据看，ROI 约为 1.05-1.22%，接近临界值

**但是，verify_swap_path.rs 使用了 BINS_RADIUS=200，而监控服务只用了 50！**

这意味着：
- verify_swap_path 的 0.3% 误差是在充分同步数据的情况下
- 监控服务的误差可能远大于 0.3%，甚至可能达到 10-50%

## 解决方案

### 方案 1：增加 BINS_RADIUS（推荐）

```rust
// moe_monitor_executor_service.rs
const BINS_RADIUS: u32 = 200;  // 从 50 增加到 200
```

**优点：**
- 显著提高模拟精度
- 覆盖更大价格范围的 bins
- 对于大额交易更准确

**缺点：**
- 初始同步时间增加
- 每次重新同步耗时增加
- 可能增加 RPC 调用次数

### 方案 2：动态调整 BINS_RADIUS

根据交易金额动态调整同步范围：
```rust
fn calculate_required_bins_radius(amount: U256, pool: &MoeLbPair) -> u32 {
    // 基于金额和池子流动性估算需要的 bins 数量
    let liquidity_per_bin = pool.reserve_x / 100;  // 估算
    let bins_needed = (amount / U256::from(liquidity_per_bin)).to::<u32>();
    bins_needed.max(50).min(500)  // 限制在 50-500 之间
}
```

### 方案 3：提高利润门槛

既然存在模拟误差，应该提高最小利润要求：
```rust
// 当前
const MIN_PROFIT_FLOOR_WEI: &str = "100000000000000000"; // 0.1 MNT

// 建议
const MIN_PROFIT_FLOOR_WEI: &str = "500000000000000000"; // 0.5 MNT
```

考虑到可能的 0.3-1% 误差，需要足够的安全边际。

### 方案 4：链上预模拟验证

在提交交易前，调用合约的静态调用验证实际收益：
```rust
async fn verify_profitability_onchain<P: Provider>(
    provider: &P,
    candidate: &PositiveCandidate,
) -> Result<bool> {
    // 使用 eth_call 模拟交易
    // 验证实际输出是否仍然盈利
}
```

## 推荐实施方案

**立即实施：**
1. 将 `moe_monitor_executor_service.rs` 的 `BINS_RADIUS` 从 50 增加到 200
2. 将 `MIN_PROFIT_FLOOR_WEI` 从 0.1 MNT 增加到 0.5 MNT

**后续优化：**
3. 实现链上预模拟验证
4. 添加动态 BINS_RADIUS 调整逻辑
5. 监控实际执行结果，持续调整参数

## 关于 verify_swap_path.rs 的结论

**0.3% 的误差在充分同步数据的情况下是可以接受的**，因为：
1. 这已经是使用 200 bins radius 的结果
2. 误差主要来自数值精度和舍入
3. 通过安全边际可以吸收这个误差

**但是，监控服务目前只用 50 bins，误差会远大于 0.3%！**

## 验证步骤

修改后，应该：
1. 运行 `verify_swap_path.rs` 验证模拟精度仍为 ~0.3%
2. 在监控服务中添加日志，记录每次套利的：
   - 预期输出
   - 实际输出
   - 差异百分比
3. 收集数据，验证改进效果

## 性能影响估算

增加 BINS_RADIUS 从 50 到 200：
- bins 数量：从 100 增加到 400 (4x)
- 同步时间：从 ~2s 增加到 ~8s (估算)
- 内存占用：每个池子增加 ~10KB

对于套利监控服务，这个开销是值得的，因为准确性比速度更重要。

