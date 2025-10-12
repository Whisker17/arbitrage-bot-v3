#!/bin/bash

# Mantle Sepolia 使用 cast 的一键部署与注资脚本
# 使用方法：
#   1) 确保已配置 .env，包含：
#      - MANTLE_SEPOLIA_RPC_URL
#      - MANTLE_SEPOLIA_PRIVATE_KEY
#   2) 运行：./scripts/deploy_and_fund_cast.sh

set -e

echo "=================================="
echo "Mantle Sepolia 部署脚本 (cast)"
echo "=================================="

if [ ! -f .env ]; then
    echo "❌ 错误：.env 文件不存在"
    echo "请复制 env.sepolia.example 为 .env 并填写配置"
    exit 1
fi

# 加载环境变量
source .env

if [ -z "$MANTLE_SEPOLIA_PRIVATE_KEY" ]; then
    echo "❌ 错误：MANTLE_SEPOLIA_PRIVATE_KEY 未设置"
    exit 1
fi

if [ -z "$MANTLE_SEPOLIA_RPC_URL" ]; then
    echo "❌ 错误：MANTLE_SEPOLIA_RPC_URL 未设置"
    exit 1
fi

export RPC_URL="$MANTLE_SEPOLIA_RPC_URL"
export PK="$MANTLE_SEPOLIA_PRIVATE_KEY"

echo ""
echo "📋 配置信息："
echo "  RPC URL: $RPC_URL"
echo ""

# 准备 artifact 与构造参数
ARTIFACT="contracts/out/ArbitrageExecutor.sol/OptimizedArbitrageExecutor.json"
if [ ! -f "$ARTIFACT" ]; then
  echo "❌ 错误：未找到编译产物 $ARTIFACT"
  echo "请先在 contracts 目录执行：forge build"
  exit 1
fi

WMNT_ADDRESS="0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF"

echo "=================================="
echo "步骤 1/3：部署 ArbitrageExecutor (cast)"
echo "=================================="

# 读取 bytecode 并编码构造参数
BYTECODE=$(jq -r '.bytecode.object' "$ARTIFACT")
if [ -z "$BYTECODE" ] || [ "$BYTECODE" = "null" ]; then
  echo "❌ 错误：从 $ARTIFACT 读取 bytecode 失败"
  exit 1
fi

ARGS=$(cast abi-encode "constructor(address)" "$WMNT_ADDRESS")
DATA="${BYTECODE}${ARGS:2}"

TX_HASH=$(cast send --create "$DATA" --rpc-url "$RPC_URL" --private-key "$PK")
echo "部署交易哈希：$TX_HASH"

ADDR=$(cast receipt "$TX_HASH" --rpc-url "$RPC_URL" --json | jq -r '.contractAddress')
if [ -z "$ADDR" ] || [ "$ADDR" = "null" ]; then
  echo "❌ 错误：获取合约地址失败"
  exit 1
fi

echo "✅ 合约地址：$ADDR"

# 更新 .env
if grep -q '^ARBITRAGE_EXECUTOR_ADDRESS=' .env; then
  # 覆盖原值
  sed -i '' "s/^ARBITRAGE_EXECUTOR_ADDRESS=.*/ARBITRAGE_EXECUTOR_ADDRESS=$ADDR/" .env
else
  echo "ARBITRAGE_EXECUTOR_ADDRESS=$ADDR" >> .env
fi

echo ""
echo "⚠️  合约地址已写入 .env：ARBITRAGE_EXECUTOR_ADDRESS=$ADDR"

echo "=================================="
echo "步骤 2/3：为合约注入 WMNT (cast)"
echo "=================================="

source .env
EXECUTOR_ADDRESS="$ARBITRAGE_EXECUTOR_ADDRESS"

if [ -z "$EXECUTOR_ADDRESS" ]; then
  echo "❌ 错误：ARBITRAGE_EXECUTOR_ADDRESS 未设置"
  exit 1
fi

# 注资金额（默认 20 WMNT，可改）
FUND_AMOUNT_WEI=$(cast --to-wei 20 ether)

echo "将向 EOA 包装 20 MNT -> WMNT..."
cast send "$WMNT_ADDRESS" "deposit()" --value "$FUND_AMOUNT_WEI" --rpc-url "$RPC_URL" --private-key "$PK"

echo "将 WMNT 转给执行器合约..."
cast send "$WMNT_ADDRESS" "transfer(address,uint256)(bool)" "$EXECUTOR_ADDRESS" "$FUND_AMOUNT_WEI" --rpc-url "$RPC_URL" --private-key "$PK"

echo ""
echo "=================================="
echo "步骤 3/3：验证部署"
echo "=================================="

BALANCE=$(cast call "$WMNT_ADDRESS" "balanceOf(address)(uint256)" "$EXECUTOR_ADDRESS" --rpc-url "$RPC_URL")
echo "✅ 合约 WMNT 余额：$BALANCE"

if [ "$BALANCE" = "0" ]; then
  echo "⚠️  警告：合约余额为 0，请先注资"
else
  echo "✅ 余额正常"
fi

echo ""
echo "=================================="
echo "🎉 部署完成！(cast)"
echo "=================================="
echo ""
echo "下一步："
echo "1. 启动监控服务："
echo "   cargo run --release --example execute_sepolia_arbitrage"
echo ""
echo "2. 在另一个终端触发测试交易："
echo "   cargo run --example trigger_sepolia_swap"
echo ""
echo "详细使用指南请参考："
echo "   tech-docs/Agni/DEPLOYMENT_GUIDE.md"
echo ""


