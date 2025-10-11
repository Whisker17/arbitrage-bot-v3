// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

// ERC20 代币接口
interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
}

// Agni Pool (Uniswap V3 风格) 接口
interface IAgniPool {
    // slot0 返回当前池子状态
    function slot0() external view returns (
        uint160 sqrtPriceX96,
        int24 tick,
        uint16 observationIndex,
        uint16 observationCardinality,
        uint16 observationCardinalityNext,
        uint32 feeProtocol,
        bool unlocked
    );
    
    // liquidity 返回当前活跃流动性
    function liquidity() external view returns (uint128);
    
    // V3 风格的 swap 接口
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
    
    function token0() external view returns (address);
    function token1() external view returns (address);
}

// Uniswap V3 swap 回调接口
interface IUniswapV3SwapCallback {
    function uniswapV3SwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

// Agni swap 回调接口（与 Uniswap V3 相同）
interface IAgniSwapCallback {
    function agniSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external;
}

/**
 * @title AgniArbitrageExecutor
 * @notice 专为 Agni (V3 风格) 池设计的原子套利合约
 * @dev 1. 本合约必须预先充值起始资金 (WMNT)。Owner (Bot) 只需要调用执行函数。
 * @dev 2. 安全性依赖于链下传入的精确 slot0 快照，实现前置失败保护。
 * @dev 3. 使用 V3 的 callback 机制来支付代币
 */
contract AgniArbitrageExecutor is IAgniSwapCallback {
    address public immutable owner;
    address public immutable WMNT;
    
    // 用于跟踪当前正在执行的套利
    bool private locked;
    address private expectedPayer;

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

    struct Slot0Data {
        uint160 sqrtPriceX96;
        int24 tick;
        uint128 liquidity;
    }

    /**
     * @notice 执行一个多跳的 Agni 套利交易，包含预检安全机制
     * @param _amountIn 起始投入的WMNT数量。合约必须已持有此数量。
     * @param _path 交易路径上的代币地址数组 (例如 [WMNT, TKA, TKB, WMNT])
     * @param _pools 交易路径上的池子地址数组
     * @param _expectedSlot0 链下Bot看到的 slot0 快照数组 [(sqrtPrice, tick, liquidity), ...]
     * @param _minFinalAmount 期望的最小最终回款金额 (用于滑点保护)
     */
    function executeArbitrage(
        uint256 _amountIn,
        address[] calldata _path,
        address[] calldata _pools,
        Slot0Data[] calldata _expectedSlot0,
        uint256 _minFinalAmount
    ) external onlyOwner nonReentrant {
        require(_path.length >= 2, "Invalid path length");
        require(_pools.length == _path.length - 1, "Pools length mismatch");
        require(_expectedSlot0.length == _pools.length, "Slot0 length mismatch");

        // --- 1. 预检 (Pre-Flight Safety Check) ---
        // 验证所有池子的状态与 Bot 看到的快照一致
        for (uint i = 0; i < _pools.length; i++) {
            IAgniPool pool = IAgniPool(_pools[i]);
            (uint160 sqrtPrice, int24 tick, , , , , ) = pool.slot0();
            uint128 liquidity = pool.liquidity();
            
            // 确保链上状态与 Bot 看到的状态完全一致
            require(
                sqrtPrice == _expectedSlot0[i].sqrtPriceX96 &&
                tick == _expectedSlot0[i].tick &&
                liquidity == _expectedSlot0[i].liquidity,
                "SLOT0_MISMATCH"
            );
        }

        // --- 2. 执行链式交换 (Daisy-Chain Swaps) ---
        // 在 V3 中，所有的输入和输出都必须经过合约，不能直接在池子间传递
        uint256 currentAmount = _amountIn;
        
        for (uint i = 0; i < _pools.length; i++) {
            address tokenIn = _path[i];
            address tokenOut = _path[i + 1];
            IAgniPool pool = IAgniPool(_pools[i]);
            
            // 确定交易方向
            bool zeroForOne = tokenIn < tokenOut;
            
            // 设置价格限制（使用极限值以接受任何价格）
            uint160 sqrtPriceLimitX96 = zeroForOne ? 
                MIN_SQRT_RATIO + 1 : 
                MAX_SQRT_RATIO - 1;
            
            // V3 的接收地址必须是合约自己，因为 callback 机制需要从合约支付
            address recipient = address(this);
            
            // 设置 callback 的预期调用者（池子地址）
            expectedPayer = _pools[i];
            
            // 执行 swap（amountSpecified 为正表示精确输入）
            (int256 amount0, int256 amount1) = pool.swap(
                recipient,
                zeroForOne,
                int256(currentAmount),
                sqrtPriceLimitX96,
                abi.encode(tokenIn)
            );
            
            // 计算实际输出金额（输出是负数）
            currentAmount = uint256(-(zeroForOne ? amount1 : amount0));
            
            // 清除预期调用者
            expectedPayer = address(0);
        }

        // --- 3. 最终兜底安全检查 (Post-Flight Check) ---
        require(
            IERC20(WMNT).balanceOf(address(this)) >= _minFinalAmount,
            "SLIPPAGE_PROTECTION"
        );
    }

    /**
     * @notice Agni swap 回调函数
     * @dev 在 swap 过程中被池子调用，用于接收代币支付
     * @dev Agni 使用与 Uniswap V3 相同的 callback 机制
     */
    function agniSwapCallback(
        int256 amount0Delta,
        int256 amount1Delta,
        bytes calldata data
    ) external override {
        require(locked, "Not in transaction");
        require(msg.sender == expectedPayer, "Unauthorized callback");
        
        // 解码数据获取输入代币地址
        address tokenIn = abi.decode(data, (address));
        
        // 确定需要支付的金额（正数 delta 表示需要支付）
        uint256 amountToPay = amount0Delta > 0 ? 
            uint256(amount0Delta) : 
            uint256(amount1Delta);
        
        require(amountToPay > 0, "Invalid amount");
        
        // 支付代币给池子
        require(IERC20(tokenIn).transfer(msg.sender, amountToPay), "Transfer failed");
    }

    // --- 资金管理 ---
    
    /**
     * @notice 提取指定数量的代币
     * @param _token 要提取的代币地址
     * @param _amount 要提取的数量 (wei)
     */
    function withdrawAmount(address _token, uint256 _amount) external onlyOwner {
        require(_amount > 0, "Amount must be greater than 0");
        uint256 balance = IERC20(_token).balanceOf(address(this));
        require(balance >= _amount, "Insufficient contract balance");
        
        require(IERC20(_token).transfer(owner, _amount), "Transfer failed");
    }
    
    /**
     * @notice 提取指定代币的全部余额
     * @param _token 要提取的代币地址
     */
    function withdraw(address _token) external onlyOwner {
        uint256 balance = IERC20(_token).balanceOf(address(this));
        if (balance > 0) {
            require(IERC20(_token).transfer(owner, balance), "Transfer failed");
        }
    }
    
    /**
     * @notice 充值代币到合约（任何人都可以调用）
     * @param _token 代币地址
     * @param _amount 充值数量
     * @dev 调用者需要先 approve 本合约
     */
    function deposit(address _token, uint256 _amount) external {
        require(_amount > 0, "Amount must be greater than 0");
        // 注意：调用者需要提前 approve
        require(IERC20(_token).transferFrom(msg.sender, address(this), _amount), "Transfer failed");
    }
    
    // 允许合约接收原生 MNT
    receive() external payable {}
}