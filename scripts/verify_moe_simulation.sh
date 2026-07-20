#!/bin/bash
# Moe 链下模拟精度验证脚本
# 
# 用途：验证修复后的模拟精度是否达到预期的 ~0.3% 误差
# 
# 使用方法：
#   ./scripts/verify_moe_simulation.sh

set -e

echo "======================================"
echo "  Moe 链下模拟精度验证"
echo "======================================"
echo ""

# 检查环境变量
if [ -z "$MANTLE_HTTP_URL" ]; then
    echo "⚠️  警告: MANTLE_HTTP_URL 未设置，将使用默认 RPC"
    export MANTLE_HTTP_URL="https://rpc.mantle.xyz"
fi

echo "✓ RPC URL: $MANTLE_HTTP_URL"
echo ""

# 运行验证脚本
echo "▶ 运行 verify_swap_path.rs..."
echo "   (这将需要 10-30 秒，取决于 RPC 响应速度)"
echo ""

cargo run --release --example verify_swap_path 2>&1 | tee /tmp/moe_verify_output.txt

echo ""
echo "======================================"
echo "  验证结果分析"
echo "======================================"
echo ""

# 提取关键指标
if grep -q "MISMATCH" /tmp/moe_verify_output.txt; then
    echo "⚠️  检测到输出差异"
    echo ""
    
    # 提取误差百分比
    ERRORS=$(grep -o "[0-9]\+\.[0-9]\+%" /tmp/moe_verify_output.txt | tail -3)
    
    if [ ! -z "$ERRORS" ]; then
        echo "各跳误差："
        echo "$ERRORS" | while read err; do
            PCT=$(echo $err | tr -d '%')
            if (( $(echo "$PCT < 1.0" | bc -l) )); then
                echo "  ✓ $err - 可接受"
            elif (( $(echo "$PCT < 5.0" | bc -l) )); then
                echo "  ⚠️  $err - 较高，但可能可接受"
            else
                echo "  ❌ $err - 过高，需要优化"
            fi
        done
    fi
    
    echo ""
    
    # 检查是否有 bins 缺失
    if grep -q "LOCAL MISSING" /tmp/moe_verify_output.txt; then
        echo "❌ 严重问题: 检测到 bins 数据缺失"
        echo "   建议: 增加 BINS_RADIUS 或检查同步逻辑"
        exit 1
    elif grep -q "bins coverage insufficient" /tmp/moe_verify_output.txt; then
        echo "⚠️  警告: Bins 覆盖范围不足"
        echo "   建议: 考虑增加 BINS_RADIUS"
    fi
else
    echo "✓ 所有跳的输出完全匹配！"
fi

echo ""

# 检查 bins 验证结果
if grep -q "All pools have sufficient bins coverage" /tmp/moe_verify_output.txt; then
    echo "✓ Bins 覆盖范围充足"
elif grep -q "pools have insufficient bins coverage" /tmp/moe_verify_output.txt; then
    echo "⚠️  部分池子 bins 覆盖不足"
else
    echo "ℹ️  未检测到 bins 覆盖验证（可能使用旧版本脚本）"
fi

echo ""
echo "======================================"
echo "  建议"
echo "======================================"
echo ""

# 基于结果给出建议
if grep -q "MISMATCH" /tmp/moe_verify_output.txt; then
    MAX_ERROR=$(grep -o "[0-9]\+\.[0-9]\+%" /tmp/moe_verify_output.txt | \
                sort -rn | head -1 | tr -d '%')
    
    if (( $(echo "$MAX_ERROR < 1.0" | bc -l) )); then
        echo "✓ 模拟精度良好 (误差 < 1%)"
        echo ""
        echo "建议配置："
        echo "  - MIN_PROFIT_FLOOR_WEI: 0.2-0.3 MNT"
        echo "  - BINS_RADIUS: 200 (当前设置)"
        echo "  - 可以开始运行监控服务"
    elif (( $(echo "$MAX_ERROR < 5.0" | bc -l) )); then
        echo "⚠️  模拟精度一般 (误差 1-5%)"
        echo ""
        echo "建议："
        echo "  1. 增加 BINS_RADIUS 到 300-400"
        echo "  2. 提高 MIN_PROFIT_FLOOR_WEI 到 0.5 MNT"
        echo "  3. 谨慎运行监控服务，密切观察执行结果"
    else
        echo "❌ 模拟精度较差 (误差 > 5%)"
        echo ""
        echo "需要立即修复："
        echo "  1. 检查 BINS_RADIUS 设置（应该 >= 200）"
        echo "  2. 验证 bins 同步逻辑是否正确"
        echo "  3. 检查是否有 RPC 限流或数据不完整"
        echo "  4. 暂时不要运行监控服务"
    fi
else
    echo "✓ 完美匹配！但这可能不太现实..."
    echo ""
    echo "请检查："
    echo "  - 测试的交易金额是否足够大"
    echo "  - 是否覆盖了多个 bins"
fi

echo ""
echo "完整输出已保存到: /tmp/moe_verify_output.txt"
echo ""
echo "下一步："
echo "  1. 保持 live sender 停用；运行: cargo run --locked --example verify_gas_profile_runtime"
echo "  2. 只有完成动态 gas 入口迁移和人工复核后，才恢复 live execution"
echo "  3. 根据 canonical receipt 继续更新 profile 资格"
echo ""
