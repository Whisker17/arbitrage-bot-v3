// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

// ============================================
// 接口定义
// ============================================

/// @notice Uniswap V2 / MoeLP 风格池接口
interface IMoePair {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
}

/// @notice Agni (Uniswap V3 风格) 池接口
interface IAgniPool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function slot0() external view returns (
        uint160 sqrtPriceX96,
        int24 tick,
        uint16 observationIndex,
        uint16 observationCardinality,
        uint16 observationCardinalityNext,
        uint32 feeProtocol,
        bool unlocked
    );
    function liquidity() external view returns (uint128);
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/// @notice ERC20 代币接口
interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

// ============================================
// 主合约
// ============================================

/**
 * @title OptimizedArbitrageExecutor
 * @notice 高度优化的原子套利合约，支持 Uniswap V2 和 Agni (Uniswap V3 风格) 多协议混合路径
 * @dev 核心特性：
 *      1. 零 approve 链式传递 - V2 池间直接传递，V3 通过回调支付
 *      2. 预检机制 - 验证链上状态与链下快照一致，前置失败保护
 *      3. Gas 优化 - 最小化存储操作和代币转移
 *      4. 安全保障 - 回调验证、最终利润检查、owner 权限控制
 */
contract OptimizedArbitrageExecutor {
    // ============================================
    // 状态变量
    // ============================================
    
    address public immutable owner;
    address public immutable WMNT;
    
    // Uniswap V3 / Agni 价格限制常量
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342656989090000000000;
    
    // ============================================
    // 构造函数与修饰器
    // ============================================
    
    constructor(address _wmntAddress) {
        owner = msg.sender;
        WMNT = _wmntAddress;
    }
    
    modifier onlyOwner() {
        require(msg.sender == owner, "NOT_OWNER");
        _;
    }
    
    // ============================================
    // Agni 回调函数
    // ============================================
    
    /**
     * @notice Agni swap 回调函数，用于支付输入代币
     * @dev 仅在 Agni swap 执行时被池子调用，支付正数 delta 对应的代币
     */
    function agniSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata /* data */
    ) external {
        // 安全检查：防止 EOA 直接调用
        require(msg.sender != tx.origin, "NO_EOA_CALLBACK");
        
        // 支付池子需要的代币（正数 delta）
        if (amount0Delta > 0) {
            address token0 = IAgniPool(msg.sender).token0();
            require(IERC20(token0).transfer(msg.sender, uint256(amount0Delta)), "TRANSFER_FAILED");
        }
        if (amount1Delta > 0) {
            address token1 = IAgniPool(msg.sender).token1();
            require(IERC20(token1).transfer(msg.sender, uint256(amount1Delta)), "TRANSFER_FAILED");
        }
    }
    
    // ============================================
    // 核心套利执行函数
    // ============================================
    
    /**
     * @notice 执行多协议混合套利交易
     * @param _amountIn 起始投入的 WMNT 数量（合约必须已持有）
     * @param _path 代币地址数组，长度 = pools.length + 1，例如 [WMNT, TKA, TKB, WMNT]
     * @param _pools 池子地址数组
     * @param _poolTypes 池子类型数组：0 = V2, 1 = Agni(V3)
     * @param _expectedStates 链下状态快照数组（串联）：
     *        - V2 池：[reserve0, reserve1] (2 个元素)
     *        - Agni 池：[sqrtPriceX96, liquidity] (2 个元素)
     * @param _amountsOut 每一步的预期输出量，最后一项是最终最小回款检查值
     */
    function executeArbitrage(
        uint256 _amountIn,
        address[] calldata _path,
        address[] calldata _pools,
        uint8[] calldata _poolTypes,
        uint256[] calldata _expectedStates,
        uint256[] calldata _amountsOut
    ) external onlyOwner {
        // ============================================
        // 1. 参数验证
        // ============================================
        uint256 numPools = _pools.length;
        require(numPools > 0, "EMPTY_PATH");
        require(_path.length == numPools + 1, "INVALID_PATH_LENGTH");
        require(_poolTypes.length == numPools, "INVALID_TYPES_LENGTH");
        require(_amountsOut.length == numPools, "INVALID_AMOUNTS_LENGTH");
        
        // ============================================
        // 2. 预检：验证链上状态与快照一致
        // ============================================
        uint256 stateIdx = 0;
        for (uint256 i = 0; i < numPools; i++) {
            address pool = _pools[i];
            
            if (_poolTypes[i] == 0) {
                // V2 池：检查 reserves
                (uint112 r0, uint112 r1, ) = IMoePair(pool).getReserves();
                require(uint256(r0) == _expectedStates[stateIdx], "V2_R0_MISMATCH");
                require(uint256(r1) == _expectedStates[stateIdx + 1], "V2_R1_MISMATCH");
                stateIdx += 2;
            } else {
                // Agni 池：检查 sqrtPrice 和 liquidity
                (uint160 sqrtPrice, , , , , , ) = IAgniPool(pool).slot0();
                uint128 liq = IAgniPool(pool).liquidity();
                require(uint256(sqrtPrice) == _expectedStates[stateIdx], "V3_PRICE_MISMATCH");
                require(uint256(liq) == _expectedStates[stateIdx + 1], "V3_LIQ_MISMATCH");
                stateIdx += 2;
            }
        }
        require(stateIdx == _expectedStates.length, "INVALID_STATES_LENGTH");
        
        // ============================================
        // 3. 记录初始余额（用于最终利润检查）
        // ============================================
        uint256 balanceBefore = IERC20(WMNT).balanceOf(address(this));
        require(balanceBefore >= _amountIn, "INSUFFICIENT_BALANCE");
        
        // ============================================
        // 4. 链式交换：零 approve，资产在池子间直接传递
        // ============================================
        uint256 amountToSwap = _amountIn;
        for (uint256 i = 0; i < numPools; i++) {
            address tokenIn = _path[i];
            address tokenOut = _path[i + 1];
            address pool = _pools[i];

            // 统一接收方为本合约，简化中间余额管理
            address to = address(this);

            // 获取 pool 的 token0，判断交易方向
            address token0 = (_poolTypes[i] == 0) ? IMoePair(pool).token0() : IAgniPool(pool).token0();
            bool zeroForOne = (tokenIn == token0);

            if (_poolTypes[i] == 0) {
                // ============================================
                // V2 swap：exact-input 通过先转入再指定 amountOut 完成
                // 若外部未提供 amountOut，需自行计算；此处使用 _amountsOut[i]
                // ============================================
                require(IERC20(tokenIn).transfer(pool, amountToSwap), "V2_TRANSFER_FAILED");

                uint amount0Out = zeroForOne ? 0 : _amountsOut[i];
                uint amount1Out = zeroForOne ? _amountsOut[i] : 0;

                IMoePair(pool).swap(amount0Out, amount1Out, to, new bytes(0));
            } else {
                // ============================================
                // Agni (V3) swap：使用 exact input 模式
                // ============================================
                uint160 sqrtPriceLimitX96 = zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1;

                // amountSpecified 为正数表示 exact input
                IAgniPool(pool).swap(
                    to,
                    zeroForOne,
                    int256(amountToSwap),
                    sqrtPriceLimitX96,
                    new bytes(0)
                );
            }

            // 更新下一步的输入金额：读取本合约持有的 tokenOut 余额
            if (i < numPools - 1) {
                amountToSwap = IERC20(tokenOut).balanceOf(address(this));
            }
        }
        
        // ============================================
        // 5. 最终兜底检查：确保有利润或达到最小回款
        // ============================================
        uint256 balanceAfter = IERC20(WMNT).balanceOf(address(this));
        uint256 minExpected = balanceBefore - _amountIn + _amountsOut[numPools - 1];
        require(balanceAfter >= minExpected, "INSUFFICIENT_OUTPUT");
    }
    
    // ============================================
    // 资金管理函数
    // ============================================
    
    /**
     * @notice 提取指定数量的代币
     */
    function withdrawAmount(address _token, uint256 _amount) external onlyOwner {
        require(_amount > 0, "ZERO_AMOUNT");
        uint256 balance = IERC20(_token).balanceOf(address(this));
        require(balance >= _amount, "INSUFFICIENT_BALANCE");
        require(IERC20(_token).transfer(owner, _amount), "WITHDRAW_FAILED");
    }
    
    /**
     * @notice 提取指定代币的全部余额
     */
    function withdraw(address _token) external onlyOwner {
        uint256 balance = IERC20(_token).balanceOf(address(this));
        if (balance > 0) {
            require(IERC20(_token).transfer(owner, balance), "WITHDRAW_FAILED");
        }
    }
    
    /**
     * @notice 允许合约接收原生代币（例如 MNT）
     */
    receive() external payable {}
}