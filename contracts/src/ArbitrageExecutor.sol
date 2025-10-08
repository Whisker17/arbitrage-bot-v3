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
    uint160 internal constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;
    
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
        require(_pools.length > 0, "EMPTY_PATH");
        require(_path.length == _pools.length + 1, "INVALID_PATH_LENGTH");
        require(_poolTypes.length == _pools.length, "INVALID_TYPES_LENGTH");
        require(_amountsOut.length == _pools.length, "INVALID_AMOUNTS_LENGTH");
        
        // 预检状态
        _validateStates(_pools, _poolTypes, _expectedStates);
        
        // 记录初始余额
        uint256 balanceBefore = IERC20(WMNT).balanceOf(address(this));
        require(balanceBefore >= _amountIn, "INSUFFICIENT_BALANCE");
        
        // 执行交换
        _executeSwaps(_amountIn, _path, _pools, _poolTypes, _amountsOut);
        
        // 最终检查
        uint256 balanceAfter = IERC20(WMNT).balanceOf(address(this));
        uint256 minExpected = balanceBefore - _amountIn + _amountsOut[_pools.length - 1];
        require(balanceAfter >= minExpected, "INSUFFICIENT_OUTPUT");
    }
    
    // ============================================
    // 内部函数
    // ============================================
    
    function _validateStates(
        address[] calldata pools,
        uint8[] calldata poolTypes,
        uint256[] calldata expectedStates
    ) internal view {
        uint256 stateIdx = 0;
        for (uint256 i = 0; i < pools.length; ) {
            if (poolTypes[i] == 0) {
                (uint112 r0, uint112 r1, ) = IMoePair(pools[i]).getReserves();
                require(uint256(r0) == expectedStates[stateIdx], "V2_R0_MISMATCH");
                require(uint256(r1) == expectedStates[stateIdx + 1], "V2_R1_MISMATCH");
                stateIdx += 2;
            } else {
                (uint160 sqrtPrice, , , , , , ) = IAgniPool(pools[i]).slot0();
                uint128 liq = IAgniPool(pools[i]).liquidity();
                require(uint256(sqrtPrice) == expectedStates[stateIdx], "V3_PRICE_MISMATCH");
                require(uint256(liq) == expectedStates[stateIdx + 1], "V3_LIQ_MISMATCH");
                stateIdx += 2;
            }
            unchecked { ++i; }
        }
        require(stateIdx == expectedStates.length, "INVALID_STATES_LENGTH");
    }
    
    function _executeSwaps(
        uint256 amountIn,
        address[] calldata path,
        address[] calldata pools,
        uint8[] calldata poolTypes,
        uint256[] calldata amountsOut
    ) internal {
        uint256 amountToSwap = amountIn;
        for (uint256 i = 0; i < pools.length; ) {
            address nextPool = (i < pools.length - 1) ? pools[i + 1] : address(0);
            uint8 nextPoolType = (i < pools.length - 1) ? poolTypes[i + 1] : 1;
            
            _doSwap(
                amountToSwap,
                path[i],
                pools[i],
                poolTypes[i],
                nextPool,
                nextPoolType,
                amountsOut[i]
            );
            if (i < pools.length - 1) {
                amountToSwap = IERC20(path[i + 1]).balanceOf(address(this));
            }
            unchecked { ++i; }
        }
    }
    
    function _doSwap(
        uint256 amountIn,
        address tokenIn,
        address pool,
        uint8 poolType,
        address nextPool,
        uint8 nextPoolType,
        uint256 expectedOut
    ) internal {
        address to = address(this);
        if (nextPool != address(0) && nextPoolType == 0) {
            to = nextPool;
        }
        
        if (poolType == 0) {
            _swapV2(pool, tokenIn, amountIn, expectedOut, to);
        } else {
            _swapV3(pool, tokenIn, amountIn, to);
        }
    }
    
    function _swapV2(
        address pool,
        address tokenIn,
        uint256 amountIn,
        uint256 expectedOut,
        address to
    ) internal {
        require(IERC20(tokenIn).transfer(pool, amountIn), "V2_TRANSFER_FAILED");
        address token0 = IMoePair(pool).token0();
        bool zeroForOne = (tokenIn == token0);
        IMoePair(pool).swap(
            zeroForOne ? 0 : expectedOut,
            zeroForOne ? expectedOut : 0,
            to,
            new bytes(0)
        );
    }
    
    function _swapV3(
        address pool,
        address tokenIn,
        uint256 amountIn,
        address to
    ) internal {
        address token0 = IAgniPool(pool).token0();
        bool zeroForOne = (tokenIn == token0);
        IAgniPool(pool).swap(
            to,
            zeroForOne,
            int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
            new bytes(0)
        );
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