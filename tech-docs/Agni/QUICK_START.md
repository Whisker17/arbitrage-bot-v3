# Mantle Sepolia 套利系统快速启动指南

> 5 分钟快速启动 Mantle Sepolia 套利监控和执行系统

## 🚀 快速开始

### 1. 配置环境变量

```bash
# 复制环境变量模板
cp env.sepolia.example .env

# 编辑 .env 文件，填写你的私钥
# MANTLE_SEPOLIA_PRIVATE_KEY=your_private_key_here
```

### 2. 获取测试代币

访问 [Mantle Sepolia 水龙头](https://faucet.sepolia.mantle.xyz) 获取测试 MNT

### 3. 部署和注资（自动）

```bash
# 一键部署和注资
./scripts/deploy_and_fund.sh
```

或者手动执行：

```bash
# 部署合约
cd contracts
forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast

# 将合约地址添加到 .env 中的 ARBITRAGE_EXECUTOR_ADDRESS

# 注资
forge script script/FundExecutor.s.sol:FundExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast

cd ..
```

### 4. 启动监控服务

```bash
# 启动套利监控和执行服务
cargo run --release --example execute_sepolia_arbitrage
```

### 5. 测试执行（可选）

在另一个终端中：

```bash
# 手动触发交易制造套利机会
cargo run --example trigger_sepolia_swap
```

---

## 📁 项目结构

```
arbitrage-bot-v3/
├── contracts/
│   ├── src/
│   │   └── ArbitrageExecutor.sol          # 套利执行合约
│   └── script/
│       ├── DeployArbitrageExecutor.s.sol  # 部署脚本
│       └── FundExecutor.s.sol             # 注资脚本
│
├── examples/test/
│   ├── execute_sepolia_arbitrage.rs       # 监控和执行服务 ⭐
│   ├── trigger_sepolia_swap.rs            # 测试触发脚本
│   └── monitor_pools.rs                   # 原始监控服务（参考）
│
├── data/
│   └── poolLists_testnet.csv              # Sepolia 池子列表
│
├── tech-docs/Agni/
│   ├── QUICK_START.md                     # 本文档
│   ├── DEPLOYMENT_GUIDE.md                # 详细部署指南
│   └── Mantle_Sepolia_TEST.md             # 原始设计文档
│
└── scripts/
    └── deploy_and_fund.sh                 # 一键部署脚本
```

---

## 🔍 关键组件说明

### ArbitrageExecutor 合约

位置：`contracts/src/ArbitrageExecutor.sol`

**功能：**
- 执行多协议混合套利（支持 Uniswap V2 和 Agni/V3）
- 零 approve 链式传递
- 预检机制（验证链上状态）
- Gas 优化

**关键函数：**
```solidity
function executeArbitrage(
    uint256 amountIn,
    address[] calldata path,
    address[] calldata pools,
    uint8[] calldata poolTypes,
    uint256[] calldata expectedStates,
    uint256[] calldata amountsOut
) external onlyOwner;
```

### 监控和执行服务

位置：`examples/test/execute_sepolia_arbitrage.rs`

**功能：**
- 监控 Agni 池子的实时状态变化
- 检测套利机会（基于图算法的路径查找）
- 计算最优输入金额
- 自动调用合约执行套利
- 防重复执行（同一路径不会连续执行）

**流程：**
```
新区块 → 获取日志 → 更新池子状态 → 构建图
         ↓
    查找套利路径 → 模拟计算利润 → 检查 gas 成本
         ↓
    构建交易参数 → 调用合约 → 等待确认
```

---

## 📊 监控日志

### 日志类型

1. **池子更新日志**
   - 位置：`logs/sepolia_pool_updates.csv`
   - 记录每次 Swap/Mint/Burn 事件

2. **控制台日志**
   - 实时显示系统状态
   - 套利机会检测
   - 交易执行结果

### 日志示例

```
INFO monitor: Initialized 12 Agni pools
INFO monitor.block: Processing block 1234567
INFO monitor.arb: 🎯 Profitable arbitrage opportunity detected!
  profit = 12345678901234567
  roi = 5.2345%
  input = 100000000000000000
INFO executor: 🚀 Attempting to execute arbitrage...
INFO executor: ✅ Arbitrage executed successfully!
```

---

## ⚙️ 配置调整

### 调整利润门槛

在 `execute_sepolia_arbitrage.rs` 中：

```rust
// 行 ~730：降低利润门槛以适应测试网
if gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.0) {
    // 1.5 → 1.0 可以降低门槛
}
```

### 调整输入范围

```rust
// 行 ~720：调整最小/最大输入金额
const MIN_INPUT: u128 = 1_000_000_000_000; // 10^12
const MAX_INPUT: u128 = 100_000_000_000_000_000; // 0.1 WMNT
```

### 修改监控的池子

编辑 `data/poolLists_testnet.csv`，添加或删除池子。

---

## 🐛 常见问题

### Q1: 为什么没有检测到套利机会？

**A:** 可能的原因：
1. 测试网流动性低，真实套利机会少
2. 利润门槛太高（调低 `is_profitable_after_gas` 的系数）
3. 池子价格已平衡

**解决：** 运行 `trigger_sepolia_swap` 人为制造机会

### Q2: 交易执行失败？

**A:** 常见原因：
1. 池子状态已变化（MEV 竞争，正常现象）
2. 合约余额不足
3. Gas 估算失败（机会已消失）

### Q3: 如何查看合约余额？

```bash
cast call 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF \
  "balanceOf(address)(uint256)" \
  $ARBITRAGE_EXECUTOR_ADDRESS \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

### Q4: 如何提取合约中的利润？

```bash
# 提取所有 WMNT
cast send $ARBITRAGE_EXECUTOR_ADDRESS \
  "withdraw(address)" \
  0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

---

## 📚 更多资源

- **详细部署指南：** [DEPLOYMENT_GUIDE.md](./DEPLOYMENT_GUIDE.md)
- **原始设计文档：** [Mantle_Sepolia_TEST.md](./Mantle_Sepolia_TEST.md)
- **合约设计文档：** [../../contracts/EXECUTOR_DESIGN.md](../../contracts/EXECUTOR_DESIGN.md)

---

## 🔐 安全提示

⚠️ **仅用于测试网**
- 不要在主网使用测试私钥
- 测试网代币无真实价值
- 主网部署需要全面审计

---

## 🎯 下一步

完成基本测试后，你可以：

1. **优化参数**
   - 调整利润门槛
   - 修改输入范围
   - 增加更多池子

2. **扩展功能**
   - 支持更多协议（Moe LB Pair）
   - 实现更复杂的路径策略
   - 添加监控告警

3. **准备主网**
   - 全面测试
   - 合约审计
   - 实施安全措施

---

祝套利成功！🚀

有问题？查看详细指南或检查日志文件。
