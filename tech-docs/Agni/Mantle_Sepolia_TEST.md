# 在 Mantle Sepolia 上完成交易执行合约的测试

## Prerequisite

- Agni 合约信息
    - Factory: 0xA9AcD50B042A72c33d05fDcC8ad209d3aD361762
    - PoolDepolyer: 0x6C53C6cC7c10B389c5680458Fc0C4079f3F012b4
    - WMNT: 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF
    - SwapRouter: 0xe38cfa32cCd918d94E2e20230dFaD1A4Fd8aEF16
    - Quoter: 0xA82F8dC4704d3512b120de70480219761F24B6Eb
    - QuoterV2: 0x9Da17239a4170f50A5A2c11813BD0C601b5c9693

- 套利池子监控：
    - /Users/whisker/Work/src/Whisker17/arbitrage/arbitrage-bot-v3/data/poolLists_testnet.csv

- 必要的信息
    - 存在于 .env 中，包括：
        - MANTLE_SEPOLIA_RPC_URL 用于交易构造和执行
        - MANTLE_SEPOLIA_RPC_WS_URL 用于新区块监控
        - MANTLE_SEPOLIA_PRIVATE_KEY 

## 步骤

### 1. 部署 `ArbitrageExecutor` 合约 ✅

**脚本位置：** `contracts/script/DeployArbitrageExecutor.s.sol`

**使用方法：**
```bash
cd contracts
forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast
```

### 2. 注入 WMNT 到合约 ✅

**脚本位置：** `contracts/script/FundExecutor.s.sol`

**使用方法：**
```bash
forge script script/FundExecutor.s.sol:FundExecutor \
  --rpc-url $MANTLE_SEPOLIA_RPC_URL \
  --private-key $MANTLE_SEPOLIA_PRIVATE_KEY \
  --broadcast
```

**一键部署脚本：** `scripts/deploy_and_fund.sh`

### 3. 套利监控和执行服务 ✅

**服务位置：** `examples/test/execute_sepolia_arbitrage.rs`

**功能：**
- 实时监控 Agni 池子的状态变化
- 检测套利机会（基于图算法路径查找）
- 计算最优输入金额和预期利润
- 自动调用 `ArbitrageExecutor` 合约执行套利
- 防重复执行（同一路径不会连续执行）
- 记录详细日志

**使用方法：**
```bash
cargo run --release --example execute_sepolia_arbitrage
```

### 4. 测试脚本：手动触发套利机会 ✅

**脚本位置：** `examples/test/trigger_sepolia_swap.rs`

**功能：**
- 在指定池子中执行交易
- 改变池子价格
- 人为制造套利机会
- 用于测试监控服务

**使用方法：**
```bash
cargo run --example trigger_sepolia_swap
```

---

## 快速启动

详细指南请参考：
- **快速启动：** [QUICK_START.md](./QUICK_START.md) - 5 分钟快速上手
- **详细指南：** [DEPLOYMENT_GUIDE.md](./DEPLOYMENT_GUIDE.md) - 完整的部署和配置说明

**一键启动：**
```bash
# 1. 配置环境变量
cp env.sepolia.example .env
# 编辑 .env 填写私钥

# 2. 部署和注资
./scripts/deploy_and_fund.sh

# 3. 启动监控服务
cargo run --release --example execute_sepolia_arbitrage
```