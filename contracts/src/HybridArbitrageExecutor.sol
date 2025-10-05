// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

// ERC20 代币接口
interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

// Uniswap V2 / MoeLP 池的接口
interface IUniswapV2Pair {
    function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
}

// Agni Pool (Uniswap V3 风格) 接口
interface IAgniPool {
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

// Agni swap 回调接口
interface IAgniSwapCallback {
    function agniSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

/**
 * @title HybridArbitrageExecutor
 * @notice 可同时支持 Uniswap V2 和 Agni (V3) 风格池子的混合套利合约
 * @dev 通过池子类型标识符来区分不同的 DEX 协议，执行相应的交换逻辑
 */
contract HybridArbitrageExecutor is IAgniSwapCallback {
    address public immutable owner;
    address public immutable WMNT;
    
    // 重入锁和 V3 callback 验证
    bool private locked;
    address private expectedCaller;
    uint256 private currentHopIndex;  // 当前执行到第几跳

    // 池子类型枚举
    enum PoolType {
        UNISWAP_V2,  // Uniswap V2 / MoeLP 风格
        AGNI_V3      // Agni / Uniswap V3 风格
    }

    // V2 池子的状态快照
    struct V2PoolData {
        uint112 reserve0;
        uint112 reserve1;
    }

    // V3 池子的状态快照
    struct V3PoolData {
        uint160 sqrtPriceX96;
        int24 tick;
        uint128 liquidity;
    }

    // 常量：用于 V3 价格限制
    uint160 private constant MIN_SQRT_RATIO = 4295128739;
    uint160 private constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;

    constructor(address _wmntAddress) {
        owner = msg.sender;
        WMNT = _wmntAddress;
    }

    modifier onlyOwner() {
        require(msg.sender == owner, "Caller is not the owner");
        _;
    }

    modifier nonReentrant() {
        require(!locked, "Reentrant call");
        locked = true;
        _;
        locked = false;
    }

    /**
     * @notice 执行混合路径套利交易
     * @param _amountIn 起始投入金额
     * @param _path 代币路径 [token0, token1, token2, ...]
     * @param _pools 池子地址数组
     * @param _poolTypes 池子类型数组 (0=V2, 1=V3)
     * @param _v2PoolData V2 池子的 reserves 快照数组（只包含 V2 池子的数据，按照它们在 pools 中的顺序）
     * @param _v3PoolData V3 池子的 slot0 快照数组（只包含 V3 池子的数据，按照它们在 pools 中的顺序）
     * @param _expectedAmountsOut 每一跳的预期输出金额（用于 V2，V3 会忽略此参数）
     * @param _minFinalAmount 最小最终回款金额
     */
    function executeArbitrage(
        uint256 _amountIn,
        address[] calldata _path,
        address[] calldata _pools,
        PoolType[] calldata _poolTypes,
        V2PoolData[] calldata _v2PoolData,
        V3PoolData[] calldata _v3PoolData,
        uint256[] calldata _expectedAmountsOut,
        uint256 _minFinalAmount
    ) external onlyOwner nonReentrant {
        require(_path.length >= 2, "Invalid path length");
        require(_pools.length == _path.length - 1, "Pools length mismatch");
        require(_poolTypes.length == _pools.length, "PoolTypes length mismatch");
        require(_expectedAmountsOut.length == _pools.length, "AmountsOut length mismatch");

        // --- 1. 预检 (Pre-Flight Safety Check) ---
        uint256 v2Index = 0;
        uint256 v3Index = 0;
        
        for (uint i = 0; i < _pools.length; i++) {
            if (_poolTypes[i] == PoolType.UNISWAP_V2) {
                // V2 预检：验证 reserves
                require(v2Index < _v2PoolData.length, "V2 data index out of bounds");
                (uint112 r0, uint112 r1, ) = IUniswapV2Pair(_pools[i]).getReserves();
                require(
                    r0 == _v2PoolData[v2Index].reserve0 && 
                    r1 == _v2PoolData[v2Index].reserve1,
                    "V2_RESERVE_MISMATCH"
                );
                v2Index++;
            } else if (_poolTypes[i] == PoolType.AGNI_V3) {
                // V3 预检：验证 slot0 和 liquidity
                require(v3Index < _v3PoolData.length, "V3 data index out of bounds");
                IAgniPool pool = IAgniPool(_pools[i]);
                (uint160 sqrtPrice, int24 tick, , , , , ) = pool.slot0();
                uint128 liquidity = pool.liquidity();
                require(
                    sqrtPrice == _v3PoolData[v3Index].sqrtPriceX96 &&
                    tick == _v3PoolData[v3Index].tick &&
                    liquidity == _v3PoolData[v3Index].liquidity,
                    "V3_SLOT0_MISMATCH"
                );
                v3Index++;
            }
        }

        // --- 2. 执行链式交换 ---
        uint256 currentAmount = _amountIn;
        
        for (uint i = 0; i < _pools.length; i++) {
            currentHopIndex = i;
            address tokenIn = _path[i];
            address tokenOut = _path[i + 1];
            address pool = _pools[i];
            
            if (_poolTypes[i] == PoolType.UNISWAP_V2) {
                // V2 风格交换
                address recipient = (i == _pools.length - 1) ? address(this) : _pools[i + 1];
                currentAmount = _executeV2Swap(
                    pool,
                    tokenIn,
                    tokenOut,
                    currentAmount,
                    _expectedAmountsOut[i],
                    recipient
                );
            } else if (_poolTypes[i] == PoolType.AGNI_V3) {
                // V3 风格交换
                currentAmount = _executeV3Swap(
                    pool,
                    tokenIn,
                    tokenOut,
                    currentAmount
                );
            }
        }

        // --- 3. 最终检查 ---
        require(
            IERC20(WMNT).balanceOf(address(this)) >= _minFinalAmount,
            "SLIPPAGE_PROTECTION"
        );
    }

    /**
     * @dev 执行 V2 风格的交换
     */
    function _executeV2Swap(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 expectedAmountOut,
        address recipient
    ) private returns (uint256) {
        // V2 需要预先转账
        require(IERC20(tokenIn).transfer(pool, amountIn), "V2 transfer failed");
        
        // 确定输出方向和金额
        bool zeroForOne = tokenIn < tokenOut;
        (uint amount0Out, uint amount1Out) = zeroForOne ? 
            (uint(0), expectedAmountOut) : 
            (expectedAmountOut, uint(0));
        
        // 执行 swap
        IUniswapV2Pair(pool).swap(amount0Out, amount1Out, recipient, new bytes(0));
        
        // 返回预期输出金额（实际金额由池子保证）
        return expectedAmountOut;
    }

    /**
     * @dev 执行 V3 风格的交换
     */
    function _executeV3Swap(
        address pool,
        address tokenIn,
        address tokenOut,
        uint256 amountIn
    ) private returns (uint256) {
        bool zeroForOne = tokenIn < tokenOut;
        
        // 设置价格限制
        uint160 sqrtPriceLimitX96 = zeroForOne ? 
            MIN_SQRT_RATIO + 1 : 
            MAX_SQRT_RATIO - 1;
        
        // 设置预期的 callback 调用者
        expectedCaller = pool;
        
        // 执行 swap
        (int256 amount0, int256 amount1) = IAgniPool(pool).swap(
            address(this),  // recipient
            zeroForOne,
            int256(amountIn),
            sqrtPriceLimitX96,
            abi.encode(tokenIn)
        );
        
        // 清除预期调用者
        expectedCaller = address(0);
        
        // 计算输出金额（输出是负数）
        uint256 amountOut = uint256(-(zeroForOne ? amount1 : amount0));
        return amountOut;
    }

    /**
     * @notice Agni V3 swap 回调函数
     */
    function agniSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        require(locked, "Not in transaction");
        require(msg.sender == expectedCaller, "Unauthorized callback");
        
        // 解码输入代币
        address tokenIn = abi.decode(data, (address));
        
        // 确定需要支付的金额
        uint256 amountToPay = amount0Delta > 0 ? 
            uint256(amount0Delta) : 
            uint256(amount1Delta);
        
        require(amountToPay > 0, "Invalid callback amount");
        
        // 支付代币给池子
        require(IERC20(tokenIn).transfer(msg.sender, amountToPay), "Callback payment failed");
    }

    // --- 资金管理 ---
    
    /**
     * @notice 提取指定数量的代币
     */
    function withdrawAmount(address _token, uint256 _amount) external onlyOwner {
        require(_amount > 0, "Amount must be greater than 0");
        uint256 balance = IERC20(_token).balanceOf(address(this));
        require(balance >= _amount, "Insufficient balance");
        require(IERC20(_token).transfer(owner, _amount), "Transfer failed");
    }
    
    /**
     * @notice 提取指定代币的全部余额
     */
    function withdraw(address _token) external onlyOwner {
        uint256 balance = IERC20(_token).balanceOf(address(this));
        if (balance > 0) {
            require(IERC20(_token).transfer(owner, balance), "Transfer failed");
        }
    }
    
    /**
     * @notice 批量提取多个代币
     */
    function withdrawMultiple(address[] calldata _tokens) external onlyOwner {
        for (uint i = 0; i < _tokens.length; i++) {
            uint256 balance = IERC20(_tokens[i]).balanceOf(address(this));
            if (balance > 0) {
                require(IERC20(_tokens[i]).transfer(owner, balance), "Transfer failed");
            }
        }
    }
    
    // 允许合约接收原生 MNT
    receive() external payable {}
}

