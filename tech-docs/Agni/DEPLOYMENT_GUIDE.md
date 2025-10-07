# Mantle Sepolia 套利系统部署和使用指南

本指南详细说明如何在 Mantle Sepolia 测试网上部署和运行套利监控与执行系统。

## 目录

1. [前置准备](#前置准备)
2. [部署合约](#部署合约)
3. [注资合约](#注资合约)
4. [运行监控服务](#运行监控服务)
5. [测试套利执行](#测试套利执行)
6. [故障排查](#故障排查)

---

## 前置准备

### 1. 环境要求

- Rust 1.70+ 和 Cargo
- Foundry (forge, cast)
- Node.js (可选，用于某些工具)

### 2. 配置环境变量

在项目根目录创建 `.env` 文件：

```bash
# Mantle Sepolia RPC 端点
MANTLE_SEPOLIA_RPC_URL=https://rpc.sepolia.mantle.xyz
MANTLE_SEPOLIA_RPC_WS_URL=wss://ws.sepolia.mantle.xyz

# 私钥（不要包含 0x 前缀）
MANTLE_SEPOLIA_PRIVATE_KEY=your_private_key_here

# 部署后会填写
ARBITRAGE_EXECUTOR_ADDRESS=
```

### 3. 获取测试代币

访问 Mantle Sepolia 水龙头获取测试 MNT：
- https://faucet.sepolia.mantle.xyz

然后在 Agni Finance 上将 MNT 包装为 WMNT 并兑换一些 USDC/USDT 用于测试。

---

## 部署合约

### 步骤 1：编译合约

```bash
cd contracts
forge build
```

### 步骤 2：部署 ArbitrageExecutor

```bash
forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast \
  -vvvv
```

**注意事项：**
- 部署成功后，记录输出的合约地址
- 将合约地址添加到 `.env` 文件的 `ARBITRAGE_EXECUTOR_ADDRESS` 变量中

**示例输出：**
```
================================================================================
ArbitrageExecutor Deployed Successfully!
================================================================================
Contract Address: 0x1234567890abcdef1234567890abcdef12345678
Owner: 0xYourAddress...
WMNT: 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
================================================================================
```

### 步骤 3：验证部署

```bash
# 检查合约 owner
cast call $ARBITRAGE_EXECUTOR_ADDRESS "owner()(address)" --rpc-url $MANTLE_SEPOLIA_RPC_URL

# 检查 WMNT 地址
cast call $ARBITRAGE_EXECUTOR_ADDRESS "WMNT()(address)" --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

---

## 注资合约

### 方法 1：使用 Forge 脚本（推荐）

```bash
cd contracts

# 注入 0.1 WMNT（可以修改 FundExecutor.s.sol 中的 FUNDING_AMOUNT）
forge script script/FundExecutor.s.sol:FundExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast \
  -vvvv
```

### 方法 2：手动转账

```bash
# 1. 将 MNT 包装为 WMNT
cast send 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF "deposit()" \
  --value 0.1ether \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL

# 2. 转账 WMNT 到执行器合约
cast send 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF "transfer(address,uint256)" \
  $ARBITRAGE_EXECUTOR_ADDRESS 100000000000000000 \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

### 验证余额

```bash
# 检查执行器的 WMNT 余额
cast call 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF "balanceOf(address)(uint256)" \
  $ARBITRAGE_EXECUTOR_ADDRESS \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

---

## 运行监控服务

### 启动套利监控和执行服务

```bash
# 回到项目根目录
cd ..

# 运行监控服务
cargo run --example execute_sepolia_arbitrage
```

**服务功能：**
- ✅ 实时监控 Mantle Sepolia 上的 Agni 池子
- ✅ 检测套利机会（基于实时池子状态）
- ✅ 自动计算最优输入金额
- ✅ 自动执行套利交易（通过 ArbitrageExecutor 合约）
- ✅ 记录日志到 `logs/sepolia_pool_updates.csv`

**示例输出：**
```
INFO monitor: Starting Mantle Sepolia arbitrage monitor
INFO monitor: Initialized 12 Agni pools
INFO monitor: ✅ Subscribed to blocks, monitoring for arbitrage opportunities...
INFO monitor.block: Processing block 1234567
INFO monitor.arb: 🎯 Profitable arbitrage opportunity detected!
INFO executor: 🚀 Attempting to execute arbitrage...
INFO executor: ✅ Arbitrage executed successfully!
```

### 后台运行

使用 `nohup` 或 `screen` 在后台运行：

```bash
# 使用 nohup
nohup cargo run --release --example execute_sepolia_arbitrage > logs/arbitrage.log 2>&1 &

# 或使用 screen
screen -S arbitrage
cargo run --release --example execute_sepolia_arbitrage
# 按 Ctrl+A 然后 D 来 detach
```

---

## 测试套利执行

为了测试系统是否正常工作，你可以手动在池子中执行交易来制造套利机会。

### 步骤 1：运行触发脚本

在另一个终端中运行：

```bash
cargo run --example trigger_sepolia_swap
```

**这个脚本会：**
1. 检查你的代币余额
2. 在 USDC-USDT 池子中执行一笔交易
3. 改变池子价格，创造套利机会

### 步骤 2：观察监控服务

切换回监控服务的终端，你应该能看到：
1. 池子状态更新的日志
2. 套利机会检测
3. 自动执行套利交易

### 完整测试流程示例

**终端 1：**
```bash
# 启动监控服务
cargo run --example execute_sepolia_arbitrage
```

**终端 2：**
```bash
# 等待几秒让监控服务完全启动
sleep 5

# 触发交易制造套利机会
cargo run --example trigger_sepolia_swap
```

**预期结果：**
- 终端 2 显示交易成功执行
- 终端 1 检测到套利机会并自动执行

---

## 故障排查

### 问题 1：合约部署失败

**症状：**
```
Error: insufficient funds for gas * price + value
```

**解决方法：**
- 确保你的账户有足够的测试 MNT
- 访问水龙头：https://faucet.sepolia.mantle.xyz

---

### 问题 2：监控服务连接失败

**症状：**
```
Error: failed to connect to WebSocket
```

**解决方法：**
1. 检查 WebSocket URL 是否正确
2. 尝试使用其他 RPC 提供商
3. 检查网络连接

---

### 问题 3：没有检测到套利机会

**可能原因：**
1. **池子流动性不足**
   - Sepolia 是测试网，流动性可能很低
   - 解决：降低 `MIN_INPUT` 或增加更多流动性

2. **Gas 成本太高**
   - 利润不足以覆盖 gas 成本
   - 解决：在 `execute_sepolia_arbitrage.rs` 中调整 `is_profitable_after_gas` 的安全边际（当前是 1.5，可以降低到 1.2 或 1.0）

3. **池子价格已经平衡**
   - 没有真实的套利机会
   - 解决：运行 `trigger_sepolia_swap` 来人为制造机会

---

### 问题 4：交易执行失败

**症状：**
```
WARN executor: ⚠️  Arbitrage execution failed (transaction reverted)
```

**可能原因：**
1. **池子状态已变化**
   - MEV 竞争或其他交易抢先
   - 这是正常现象，系统会继续监控新机会

2. **预检失败**
   - 链上状态与快照不匹配
   - `executeArbitrage` 合约会在预检阶段 revert

3. **合约余额不足**
   - 检查执行器合约的 WMNT 余额

**调试步骤：**
```bash
# 检查合约余额
cast call 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF \
  "balanceOf(address)(uint256)" \
  $ARBITRAGE_EXECUTOR_ADDRESS \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL

# 查看最近的交易
cast receipt <transaction_hash> --rpc-url $MANTLE_SEPOLIA_RPC_URL
```

---

### 问题 5：Gas 估算失败

**症状：**
```
WARN executor: Gas estimation failed, opportunity may have expired
```

**解决方法：**
- 这通常意味着机会已经消失或池子状态已变化
- 系统会继续监控，这是正常的 MEV 竞争现象
- 可以考虑使用更快的 RPC 端点或私有 RPC

---

## 性能优化建议

### 1. 使用私有 RPC

公共 RPC 可能有延迟，考虑使用：
- Alchemy
- QuickNode
- 自建节点

### 2. 调整参数

在 `execute_sepolia_arbitrage.rs` 中：

```rust
// 降低最小输入以适应测试网的低流动性
const MIN_INPUT: u128 = 100_000_000_000; // 10^11 (更小)

// 降低利润门槛
if gas_config.is_profitable_after_gas(profit_u256, num_hops, 1.0) {
    // 改为 1.0 以降低门槛
}
```

### 3. 增加监控的池子

编辑 `data/poolLists_testnet.csv`，添加更多池子。

---

## 日志和监控

### 查看日志

```bash
# 实时查看池子更新日志
tail -f logs/sepolia_pool_updates.csv

# 如果使用 nohup
tail -f logs/arbitrage.log
```

### 日志格式

**sepolia_pool_updates.csv：**
```csv
block_number,pool_address,event,sqrt_price_x96,liquidity,tick
1234567,0xabc...,swap,1234567890123456789,1000000,12345
```

---

## 资源链接

- **Mantle Sepolia 浏览器：** https://explorer.sepolia.mantle.xyz
- **Agni Finance：** https://agni.finance
- **测试网水龙头：** https://faucet.sepolia.mantle.xyz
- **Mantle 文档：** https://docs.mantle.xyz

---

## 高级配置

### 自定义套利路径

修改 `execute_sepolia_arbitrage.rs` 中的路径约束：

```rust
let constraints = PathConstraints {
    max_length: 3,  // 最大跳数
    required_start_token: Some(address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF")), // WMNT
    required_end_token: Some(address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF")),
    ..PathConstraints::default()
};
```

### Gas 价格优化

在交易构建时设置自定义 gas price：

```rust
call_builder
    .gas(gas_estimate + 100000)
    .gas_price(1000000000) // 1 Gwei
    .send()
    .await?
```

---

## 安全注意事项

⚠️ **测试网使用：**
- 本指南针对 Mantle Sepolia 测试网
- 私钥仅用于测试，不要使用主网私钥
- 测试网代币没有真实价值

⚠️ **主网部署前：**
1. 全面审计合约代码
2. 进行充分的测试
3. 实施访问控制
4. 设置利润提取机制
5. 监控和告警系统
6. 考虑 MEV 保护（如 Flashbots）

---

## 支持

如有问题或建议，请查看：
- 项目 README
- 技术文档：`tech-docs/Agni/`
- 合约设计文档：`contracts/EXECUTOR_DESIGN.md`

祝套利顺利！🚀
