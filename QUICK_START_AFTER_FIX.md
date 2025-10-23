# Moe 模拟精度修复 - 快速开始指南

## ✅ 已完成的修复

### 1. 关键参数调整

```rust
// examples/protocols/moe/moe_monitor_executor_service.rs

// BINS_RADIUS: 50 → 200 (4倍增加)
const BINS_RADIUS: u32 = 200;

// MIN_PROFIT_FLOOR_WEI: 0.1 MNT → 0.3 MNT (3倍增加)
const MIN_PROFIT_FLOOR_WEI: &str = "300000000000000000";
```

### 2. 新增功能

- ✅ Bins 覆盖范围验证
- ✅ 增强的执行日志（记录预测指标）
- ✅ 详细的注释说明

## 🚀 使用步骤

### 步骤 1: 验证模拟精度（推荐）

```bash
# 运行验证脚本，确认误差约 0.3%
cargo run --release --example verify_swap_path
```

**预期结果：**
- 每跳的误差应该 < 1%
- 总体误差约 0.3%
- 无 "LOCAL MISSING" 或 "bins coverage insufficient" 警告

### 步骤 2: 配置环境变量

```bash
# 设置 RPC 端点
export MANTLE_HTTP_URL="https://rpc.mantle.xyz"
export MANTLE_WS_URL="wss://mantle.publicnode.com"

# 设置执行账户私钥
export PRIVATE_KEY="your_private_key_here"

# 设置执行器合约地址
export ARBITRAGE_EXECUTOR_ADDRESS="0x..."

# 设置利润门槛（可选，已有默认值）
export MIN_GROSS_PROFIT_WEI=300000000000000000  # 0.3 MNT
export MIN_NET_PROFIT_WEI=300000000000000000     # 0.3 MNT

# 设置日志级别
export RUST_LOG=info,moe=debug
```

### 步骤 3: 运行监控服务

```bash
cargo run --release --example moe_monitor_executor_service
```

### 步骤 4: 监控执行结果

观察日志中的关键信息：

```
✅ All pools have sufficient bins coverage (BINS_RADIUS=200)
```
→ Bins 覆盖充足，可以继续

```
✅ Execution confirmed - predicted metrics logged
  predicted_input: 16.297609
  predicted_output: 16.468269
  predicted_profit: 0.170660
  predicted_net_profit: 0.100660
  predicted_roi: 1.05
```
→ 记录这些预测值，后续对比实际结果

## 📊 验证实际效果

### 方法 1: 查看区块链浏览器

1. 复制交易哈希
2. 在 Mantle 浏览器查看：https://explorer.mantle.xyz/tx/{hash}
3. 检查是否实际盈利

### 方法 2: 分析日志

```bash
# 查看所有执行的交易
grep "Execution confirmed" logs/arbitrage.log

# 查看失败的交易
grep "Execution attempt failed" logs/arbitrage.log
```

### 方法 3: 对比预测 vs 实际

创建一个表格记录：

| 区块 | 预测利润 | 实际利润 | 误差 % | 备注 |
|------|----------|----------|--------|------|
| 86500540 | 0.17 MNT | ? | ? | 待验证 |

## ⚠️ 注意事项

### 1. 初始同步时间增加

由于 BINS_RADIUS 增加到 200，初始同步需要更长时间：
- 之前：~2-3 秒/池
- 现在：~8-10 秒/池

**这是正常的，为了精度值得等待。**

### 2. 套利机会可能减少

由于利润门槛从 0.1 MNT 提高到 0.3 MNT：
- 过滤掉了更多小额套利
- 但保留的机会更可靠

**这是好事，避免了亏损交易。**

### 3. 如果仍然发现亏损

如果运行一段时间后仍然发现亏损交易：

**立即停止服务，进行诊断：**

```bash
# 1. 重新运行验证脚本
cargo run --release --example verify_swap_path

# 2. 检查误差是否仍然 < 1%
# 3. 如果误差变大，考虑：
#    - 增加 BINS_RADIUS 到 300
#    - 提高 MIN_PROFIT_FLOOR_WEI 到 0.5 MNT
#    - 检查池子流动性是否发生重大变化
```

## 🔧 参数调优指南

### 如果套利机会太少

```rust
// 适当降低利润门槛（但不要低于 0.2 MNT）
const MIN_PROFIT_FLOOR_WEI: &str = "200000000000000000"; // 0.2 MNT
```

### 如果仍有较大误差

```rust
// 进一步增加 BINS_RADIUS
const BINS_RADIUS: u32 = 300;  // 或 400
```

### 如果同步太慢

```rust
// 减少 BINS_RADIUS（但不要低于 150）
const BINS_RADIUS: u32 = 150;  // 最小推荐值
```

## 📚 相关文档

- **技术分析**：`SIMULATION_ACCURACY_ANALYSIS.md`
  - 深入解释问题根源
  - 技术细节和数学原理

- **完整指南**：`MOE_SIMULATION_FIX_SUMMARY.md`
  - 详细的修复说明
  - FAQ 和常见问题
  - 中长期优化建议

- **验证脚本**：`scripts/verify_moe_simulation.sh`
  - 自动化验证工具
  - 一键检查模拟精度

## ❓ 常见问题

### Q: 为什么要先运行 verify_swap_path？

A: 这是一个"质量检查"，确保修复有效。如果验证都不通过，运行监控服务只会浪费 gas。

### Q: 0.3 MNT 的门槛会不会太高？

A: 考虑到 ~0.3% 的模拟误差，这是必要的安全边际。可以根据实际执行结果调整。

### Q: 编译时有很多 warning，正常吗？

A: 是的，这些是未使用的函数和变量的警告，不影响功能。可以忽略。

### Q: 如何知道修复是否成功？

A: 
1. ✅ verify_swap_path 误差 < 1%
2. ✅ 监控服务启动时无 bins coverage 警告
3. ✅ 执行的交易实际盈利（在区块链浏览器验证）

## 🎯 成功标准

修复成功的标志：
- ✅ 模拟误差 < 1%
- ✅ 执行的交易 80%+ 实际盈利
- ✅ 平均实际利润 ≥ 预测利润的 95%

如果达到这些标准，说明修复成功！

---

**祝你套利顺利！记得持续监控和优化。** 🚀

