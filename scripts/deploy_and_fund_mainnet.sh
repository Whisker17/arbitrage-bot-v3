#!/bin/bash

# Mantle 主网部署和注资脚本
# 使用方法：
#   1. 确保已配置 .env 文件
#   2. 运行：./scripts/deploy_and_fund_mainnet.sh

set -e

echo "=================================="
echo "Mantle Mainnet 部署脚本"
echo "=================================="

# 检查环境变量
if [ ! -f .env ]; then
    echo "❌ 错误：.env 文件不存在"
    echo "请复制 env.mainnet.example 为 .env 并填写配置"
    exit 1
fi

# 加载环境变量
source .env

if [ -z "$PRIVATE_KEY" ]; then
    echo "❌ 错误：PRIVATE_KEY 未设置"
    exit 1
fi

if [ -z "$RPC_URL" ]; then
    echo "❌ 错误：RPC_URL 未设置"
    exit 1
fi

echo ""
echo "📋 配置信息："
echo "  RPC URL: $RPC_URL"
echo ""

# 避免环境中隐式开启 fork 导致重复传参
unset FOUNDRY_FORK_URL || true
unset FORK_URL || true

# 步骤 1：部署合约
echo "=================================="
echo "步骤 1/3：部署 ArbitrageExecutor"
echo "=================================="

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EXECUTOR_DIR="$REPO_ROOT/contracts/executor"

if [ ! -d "$EXECUTOR_DIR" ]; then
    echo "❌ 错误：未找到目录 $EXECUTOR_DIR"
    exit 1
fi

pushd "$EXECUTOR_DIR" >/dev/null

# 导出环境变量给 forge
export PRIVATE_KEY
export RPC_URL

FOUNDRY_PROFILE=deploy forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
  --rpc-url $RPC_URL \
  --private-key $PRIVATE_KEY \
  --broadcast \
  --skip-simulation \
  -vvvv

echo ""
echo "⚠️  请将部署的合约地址添加到 .env 文件："
echo "    ARBITRAGE_EXECUTOR_ADDRESS=<合约地址>"
echo ""
read -p "按 Enter 继续，或 Ctrl+C 取消..."

popd >/dev/null

# 重新加载环境变量（包含新的合约地址）
source .env

if [ -z "$ARBITRAGE_EXECUTOR_ADDRESS" ]; then
    echo "❌ 错误：ARBITRAGE_EXECUTOR_ADDRESS 未设置"
    echo "请在 .env 中设置合约地址后重新运行"
    exit 1
fi

# 步骤 2：注资合约
echo ""
echo "=================================="
echo "步骤 2/3：为合约注入 1 WMNT"
echo "=================================="

# 导出环境变量给 forge
export ARBITRAGE_EXECUTOR_ADDRESS

pushd "$EXECUTOR_DIR" >/dev/null

FOUNDRY_PROFILE=deploy forge script script/FundExecutor.s.sol:FundExecutor \
  --rpc-url $RPC_URL \
  --private-key $PRIVATE_KEY \
  --broadcast \
  --skip-simulation \
  -vvvv

popd >/dev/null

# 步骤 3：验证
echo ""
echo "=================================="
echo "步骤 3/3：验证部署"
echo "=================================="

WMNT_ADDRESS="0x78C1b0C915C4fAA5fFfA6cABf0219DA63d7F4Cb8"
BALANCE=$(cast call $WMNT_ADDRESS "balanceOf(address)(uint256)" $ARBITRAGE_EXECUTOR_ADDRESS --rpc-url $RPC_URL)

echo "✅ 合约 WMNT 余额：$BALANCE"

if [ "$BALANCE" = "0" ]; then
    echo "⚠️  警告：合约余额为 0，请先注资"
else
    echo "✅ 余额正常"
fi

echo ""
echo "=================================="
echo "🎉 部署完成！"
echo "=================================="
echo ""
echo "下一步："
echo "1. 启动监控服务："
echo "   cargo run --release --example execute_mainnet_arbitrage"
echo ""
echo "2. 在另一个终端触发测试交易："
echo "   cargo run --example trigger_mainnet_swap"
echo ""
echo "详细使用指南请参考："
echo "   tech-docs/Agni/DEPLOYMENT_GUIDE.md"
echo ""

