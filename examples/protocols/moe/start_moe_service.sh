#!/bin/bash
# Moe LBT 套利服务启动脚本
#
# 使用方法：
#   ./start_moe_service.sh          # 前台运行
#   ./start_moe_service.sh --daemon # 后台运行

set -e

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 打印带颜色的消息
info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# 检查是否在项目根目录
if [ ! -f "Cargo.toml" ]; then
    error "Please run this script from the project root directory"
    exit 1
fi

# 检查环境变量
check_env() {
    local var_name=$1
    local var_value=$(eval echo \$$var_name)
    
    if [ -z "$var_value" ]; then
        error "Environment variable $var_name is not set"
        return 1
    fi
    
    success "✓ $var_name is set"
    return 0
}

info "Checking environment variables..."

# 加载 .env 文件
if [ -f ".env" ]; then
    info "Loading .env file..."
    export $(cat .env | grep -v '^#' | xargs)
else
    warn ".env file not found, using existing environment variables"
fi

# 检查必需的环境变量
REQUIRED_VARS=(
    "RPC_WS_URL"
    "RPC_HTTP_URL"
    "ARBITRAGE_EXECUTOR_ADDRESS"
)

OPTIONAL_VARS=(
    "EXECUTION_PRIVATE_KEY"
    "PRIVATE_KEY"
)

all_ok=true
for var in "${REQUIRED_VARS[@]}"; do
    if ! check_env "$var"; then
        all_ok=false
    fi
done

# 检查至少有一个私钥变量
if [ -z "$EXECUTION_PRIVATE_KEY" ] && [ -z "$PRIVATE_KEY" ]; then
    error "Either EXECUTION_PRIVATE_KEY or PRIVATE_KEY must be set"
    all_ok=false
else
    success "✓ Private key is set"
fi

if [ "$all_ok" = false ]; then
    error "Environment check failed. Please set the required variables."
    exit 1
fi

# 显示配置
info "Configuration:"
echo "  RPC_WS_URL: $RPC_WS_URL"
echo "  RPC_HTTP_URL: $RPC_HTTP_URL"
echo "  EXECUTOR_ADDRESS: $ARBITRAGE_EXECUTOR_ADDRESS"
echo "  MIN_GROSS_PROFIT: ${MIN_GROSS_PROFIT_WEI:-10000000000000000} wei"
echo "  MIN_NET_PROFIT: ${MIN_NET_PROFIT_WEI:-10000000000000000} wei"
echo "  SLIPPAGE: ${EXECUTION_SLIPPAGE_BPS:-30} bps"
echo "  COOLDOWN: ${EXECUTION_BLOCK_COOLDOWN:-1} blocks"
echo "  LOG_LEVEL: ${RUST_LOG:-info}"

# 创建日志目录
info "Creating logs directory..."
mkdir -p logs

# 检查执行器余额
info "Checking executor balance..."
EXECUTOR_BALANCE=$(cast call $ARBITRAGE_EXECUTOR_ADDRESS \
    "balanceOf(address)(uint256)" \
    $ARBITRAGE_EXECUTOR_ADDRESS \
    --rpc-url $RPC_HTTP_URL 2>/dev/null || echo "0")

if [ "$EXECUTOR_BALANCE" = "0" ]; then
    warn "Executor balance is 0 or could not be checked"
    warn "Make sure the executor has sufficient WMNT balance"
else
    # 转换为 ether (假设 18 decimals)
    BALANCE_ETHER=$(echo "scale=6; $EXECUTOR_BALANCE / 1000000000000000000" | bc)
    success "Executor balance: $BALANCE_ETHER WMNT"
fi

# 编译服务
info "Building service..."
if cargo build --release --example moe_monitor_executor_service; then
    success "Build completed"
else
    error "Build failed"
    exit 1
fi

# 检查是否以 daemon 模式运行
DAEMON_MODE=false
if [ "$1" = "--daemon" ] || [ "$1" = "-d" ]; then
    DAEMON_MODE=true
fi

# 启动服务
if [ "$DAEMON_MODE" = true ]; then
    info "Starting service in daemon mode..."
    
    # 检查是否已经在运行
    if pgrep -f "moe_monitor_executor_service" > /dev/null; then
        warn "Service is already running!"
        echo "PID: $(pgrep -f moe_monitor_executor_service)"
        exit 1
    fi
    
    # 后台启动
    nohup ./target/release/examples/moe_monitor_executor_service \
        > logs/moe_service.log 2>&1 &
    
    PID=$!
    sleep 2
    
    # 检查是否成功启动
    if kill -0 $PID 2>/dev/null; then
        success "Service started successfully!"
        echo "  PID: $PID"
        echo "  Log: logs/moe_service.log"
        echo ""
        echo "To view logs:"
        echo "  tail -f logs/moe_service.log"
        echo ""
        echo "To stop the service:"
        echo "  kill $PID"
        echo "  # or"
        echo "  pkill -f moe_monitor_executor_service"
    else
        error "Service failed to start"
        echo "Check logs/moe_service.log for details"
        exit 1
    fi
else
    info "Starting service in foreground mode..."
    info "Press Ctrl+C to stop"
    echo ""
    
    # 前台启动
    ./target/release/examples/moe_monitor_executor_service
fi

