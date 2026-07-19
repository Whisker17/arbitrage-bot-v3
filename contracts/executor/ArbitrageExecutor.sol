// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

/// @notice Uniswap V2 / MoeLP-style pair
interface IUniswapV2Pair {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function getReserves() external view returns (uint112, uint112, uint32);
    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata data) external;
}

/// @notice Moe Liquidity Book pair
interface IMoeLBPair {
    function getTokenX() external view returns (address);
    function getTokenY() external view returns (address);
    function getActiveId() external view returns (uint24);
    function getBinStep() external view returns (uint16);
    function swap(bool swapForY, address to) external returns (bytes32 amountsOut);
}

/// @notice Agni / Uniswap V3-style pool
interface IAgniPool {
    function token0() external view returns (address);
    function token1() external view returns (address);
    function fee() external view returns (uint24);
    function factory() external view returns (address);
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160 sqrtPriceLimitX96,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1);
}

/**
 * @title ArbitrageExecutor
 * @notice Hardened multi-protocol atomic arbitrage executor (V2 / Agni-V3 / Moe LB).
 * @dev Security model (WHI-501):
 *      - Cold admin manages trust, roles, unpause, withdrawals
 *      - Hot executors may only call executeArbitrage
 *      - Optional guardian may pause (not unpause / withdraw)
 *      - Canonical pool registry (allowlist); optional CREATE2 venue checks
 *      - Callbacks never trust caller token getters alone
 *      - Trade-local hop deltas; final minProfit + deadline
 *      - Safe ERC-20 transfers (true or empty returndata)
 */
contract ArbitrageExecutor {
    uint160 internal constant MIN_SQRT_RATIO = 4295128739;
    uint160 internal constant MAX_SQRT_RATIO = 1461446703485210103287273052203988822378723970342;

    uint8 public constant POOL_TYPE_V2 = 0;
    uint8 public constant POOL_TYPE_V3 = 1;
    uint8 public constant POOL_TYPE_MOE_LB = 2;

    address public immutable WMNT;

    address public admin;
    address public guardian;
    bool public paused;

    mapping(address => bool) public isHotExecutor;

    struct RegisteredPool {
        uint8 poolType;
        address token0;
        address token1;
        uint24 fee; // V3 only; 0 otherwise
        bool enabled;
    }

    mapping(address => RegisteredPool) public registeredPools;

    /// @notice Optional CREATE2 venue per pool type (factory + init code hash).
    struct Venue {
        address factory;
        bytes32 initCodeHash;
        bool enabled;
    }

    mapping(uint8 => Venue) public venues;

    event AdminTransferred(address indexed previousAdmin, address indexed newAdmin);
    event GuardianUpdated(address indexed guardian);
    event HotExecutorUpdated(address indexed executor, bool allowed);
    event Paused(address indexed account);
    event Unpaused(address indexed account);
    event PoolRegistered(address indexed pool, uint8 poolType, address token0, address token1, uint24 fee);
    event PoolDisabled(address indexed pool);
    event VenueUpdated(uint8 poolType, address factory, bytes32 initCodeHash, bool enabled);
    event ArbitrageExecuted(address indexed caller, uint256 amountIn, uint256 minProfit, uint256 profit);

    error NotAdmin();
    error NotHotExecutor();
    error NotGuardianOrAdmin();
    error PausedError();
    error NotPausedError();
    error ZeroAddress();
    error InvalidPath();
    error DeadlineExpired();
    error InsufficientBalance();
    error InsufficientProfit();
    error PoolNotRegistered();
    error PoolTypeMismatch();
    error TokenDirectionMismatch();
    error SettlementMismatch();
    error UnauthorizedCallback();
    error UnknownPoolType();
    error TransferFailed();
    error VenueMismatch();
    error ZeroAmount();

    modifier onlyAdmin() {
        if (msg.sender != admin) revert NotAdmin();
        _;
    }

    modifier onlyHotExecutor() {
        if (!isHotExecutor[msg.sender] && msg.sender != admin) revert NotHotExecutor();
        _;
    }

    modifier whenNotPaused() {
        if (paused) revert PausedError();
        _;
    }

    constructor(address wmnt_, address admin_) {
        if (wmnt_ == address(0) || admin_ == address(0)) revert ZeroAddress();
        WMNT = wmnt_;
        admin = admin_;
        emit AdminTransferred(address(0), admin_);
    }

    // -------------------------------------------------------------------------
    // Admin / roles
    // -------------------------------------------------------------------------

    function transferAdmin(address newAdmin) external onlyAdmin {
        if (newAdmin == address(0)) revert ZeroAddress();
        emit AdminTransferred(admin, newAdmin);
        admin = newAdmin;
    }

    function setGuardian(address guardian_) external onlyAdmin {
        guardian = guardian_;
        emit GuardianUpdated(guardian_);
    }

    function setHotExecutor(address executor, bool allowed) external onlyAdmin {
        if (executor == address(0)) revert ZeroAddress();
        isHotExecutor[executor] = allowed;
        emit HotExecutorUpdated(executor, allowed);
    }

    function pause() external {
        if (msg.sender != admin && msg.sender != guardian) revert NotGuardianOrAdmin();
        paused = true;
        emit Paused(msg.sender);
    }

    function unpause() external onlyAdmin {
        if (!paused) revert NotPausedError();
        paused = false;
        emit Unpaused(msg.sender);
    }

    function setVenue(uint8 poolType, address factory, bytes32 initCodeHash, bool enabled)
        external
        onlyAdmin
    {
        if (poolType > POOL_TYPE_MOE_LB) revert UnknownPoolType();
        venues[poolType] = Venue({factory: factory, initCodeHash: initCodeHash, enabled: enabled});
        emit VenueUpdated(poolType, factory, initCodeHash, enabled);
    }

    /**
     * @notice Register a canonical pool. Reads token metadata from the pool and optionally
     *         verifies CREATE2 against the configured venue for that pool type.
     */
    function registerPool(address pool, uint8 poolType) external onlyAdmin {
        if (pool == address(0)) revert ZeroAddress();
        if (poolType > POOL_TYPE_MOE_LB) revert UnknownPoolType();

        address t0;
        address t1;
        uint24 fee;

        if (poolType == POOL_TYPE_V2) {
            t0 = IUniswapV2Pair(pool).token0();
            t1 = IUniswapV2Pair(pool).token1();
            _maybeVerifyV2(pool, t0, t1);
        } else if (poolType == POOL_TYPE_V3) {
            t0 = IAgniPool(pool).token0();
            t1 = IAgniPool(pool).token1();
            fee = IAgniPool(pool).fee();
            _maybeVerifyV3(pool, t0, t1, fee);
        } else {
            t0 = IMoeLBPair(pool).getTokenX();
            t1 = IMoeLBPair(pool).getTokenY();
            // Moe LB pair addresses are not CREATE2(tokenX,tokenY)-simple; allowlist only.
        }

        registeredPools[pool] = RegisteredPool({
            poolType: poolType,
            token0: t0,
            token1: t1,
            fee: fee,
            enabled: true
        });
        emit PoolRegistered(pool, poolType, t0, t1, fee);
    }

    function disablePool(address pool) external onlyAdmin {
        registeredPools[pool].enabled = false;
        emit PoolDisabled(pool);
    }

    // -------------------------------------------------------------------------
    // Execution
    // -------------------------------------------------------------------------

    /**
     * @param amountIn Starting WMNT amount already held by this contract.
     * @param path Token path; must start and end with WMNT. Length = pools + 1.
     * @param pools Canonical registered pool addresses.
     * @param poolTypes Must match each registered pool's type.
     * @param amountsOut Per-hop venue-native outs (V2 uses these as amountOut args). Length = pools.
     * @param minProfit Required WMNT balance increase after the trade (can be 0).
     * @param deadline Inclusive block.timestamp deadline.
     */
    function executeArbitrage(
        uint256 amountIn,
        address[] calldata path,
        address[] calldata pools,
        uint8[] calldata poolTypes,
        uint256[] calldata amountsOut,
        uint256 minProfit,
        uint256 deadline
    ) external onlyHotExecutor whenNotPaused {
        if (block.timestamp > deadline) revert DeadlineExpired();
        uint256 n = pools.length;
        if (n == 0) revert InvalidPath();
        if (path.length != n + 1) revert InvalidPath();
        if (poolTypes.length != n || amountsOut.length != n) revert InvalidPath();
        if (path[0] != WMNT || path[n] != WMNT) revert SettlementMismatch();
        if (amountIn == 0) revert ZeroAmount();

        uint256 balanceBefore = _balanceOf(WMNT, address(this));
        if (balanceBefore < amountIn) revert InsufficientBalance();

        _validateRoute(path, pools, poolTypes);

        uint256 amount = amountIn;
        for (uint256 i = 0; i < n;) {
            address tokenIn = path[i];
            address tokenOut = path[i + 1];
            address pool = pools[i];
            uint8 poolType = poolTypes[i];

            uint256 outBefore = _balanceOf(tokenOut, address(this));
            _swap(pool, poolType, tokenIn, tokenOut, amount, amountsOut[i]);
            uint256 outAfter = _balanceOf(tokenOut, address(this));
            if (outAfter < outBefore) revert InsufficientBalance();
            amount = outAfter - outBefore;

            unchecked {
                ++i;
            }
        }

        uint256 balanceAfter = _balanceOf(WMNT, address(this));
        if (balanceAfter < balanceBefore + minProfit) revert InsufficientProfit();

        emit ArbitrageExecuted(msg.sender, amountIn, minProfit, balanceAfter - balanceBefore);
    }

    function _validateRoute(
        address[] calldata path,
        address[] calldata pools,
        uint8[] calldata poolTypes
    ) internal view {
        for (uint256 i = 0; i < pools.length;) {
            RegisteredPool memory rp = registeredPools[pools[i]];
            if (!rp.enabled) revert PoolNotRegistered();
            if (rp.poolType != poolTypes[i]) revert PoolTypeMismatch();

            // V2/V3/Moe all store ordered pair ends in token0/token1 (X/Y for Moe).
            address tokenIn = path[i];
            address tokenOut = path[i + 1];
            bool ok = (tokenIn == rp.token0 && tokenOut == rp.token1)
                || (tokenIn == rp.token1 && tokenOut == rp.token0);
            if (!ok) revert TokenDirectionMismatch();
            unchecked {
                ++i;
            }
        }
    }

    function _swap(
        address pool,
        uint8 poolType,
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint256 expectedOut
    ) internal {
        if (poolType == POOL_TYPE_V2) {
            _swapV2(pool, tokenIn, amountIn, expectedOut);
        } else if (poolType == POOL_TYPE_V3) {
            _swapV3(pool, tokenIn, amountIn);
        } else if (poolType == POOL_TYPE_MOE_LB) {
            _swapMoeLB(pool, tokenIn, tokenOut, amountIn);
        } else {
            revert UnknownPoolType();
        }
    }

    function _swapV2(address pool, address tokenIn, uint256 amountIn, uint256 expectedOut) internal {
        _safeTransfer(tokenIn, pool, amountIn);
        address token0 = registeredPools[pool].token0;
        bool zeroForOne = tokenIn == token0;
        IUniswapV2Pair(pool).swap(
            zeroForOne ? 0 : expectedOut,
            zeroForOne ? expectedOut : 0,
            address(this),
            new bytes(0)
        );
    }

    function _swapV3(address pool, address tokenIn, uint256 amountIn) internal {
        address token0 = registeredPools[pool].token0;
        bool zeroForOne = tokenIn == token0;
        IAgniPool(pool).swap(
            address(this),
            zeroForOne,
            int256(amountIn),
            zeroForOne ? MIN_SQRT_RATIO + 1 : MAX_SQRT_RATIO - 1,
            new bytes(0)
        );
    }

    function _swapMoeLB(address pool, address tokenIn, address tokenOut, uint256 amountIn) internal {
        _safeTransfer(tokenIn, pool, amountIn);
        address tokenX = registeredPools[pool].token0;
        address tokenY = registeredPools[pool].token1;
        bool swapForY;
        if (tokenIn == tokenX && tokenOut == tokenY) {
            swapForY = true;
        } else if (tokenIn == tokenY && tokenOut == tokenX) {
            swapForY = false;
        } else {
            revert TokenDirectionMismatch();
        }
        IMoeLBPair(pool).swap(swapForY, address(this));
    }

    // -------------------------------------------------------------------------
    // Callbacks
    // -------------------------------------------------------------------------

    function agniSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        RegisteredPool memory rp = registeredPools[msg.sender];
        if (!rp.enabled || rp.poolType != POOL_TYPE_V3) revert UnauthorizedCallback();

        if (amount0Delta > 0) {
            _safeTransfer(rp.token0, msg.sender, uint256(amount0Delta));
        }
        if (amount1Delta > 0) {
            _safeTransfer(rp.token1, msg.sender, uint256(amount1Delta));
        }
    }

    // -------------------------------------------------------------------------
    // Withdrawals (cold admin only)
    // -------------------------------------------------------------------------

    function withdraw(address token) external onlyAdmin {
        uint256 bal = _balanceOf(token, address(this));
        if (bal > 0) {
            _safeTransfer(token, admin, bal);
        }
    }

    function withdrawAmount(address token, uint256 amount) external onlyAdmin {
        if (amount == 0) revert ZeroAmount();
        _safeTransfer(token, admin, amount);
    }

    function withdrawNative(uint256 amount) external onlyAdmin {
        if (amount == 0) revert ZeroAmount();
        (bool ok,) = admin.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    function withdrawAllNative() external onlyAdmin {
        uint256 bal = address(this).balance;
        if (bal == 0) return;
        (bool ok,) = admin.call{value: bal}("");
        if (!ok) revert TransferFailed();
    }

    receive() external payable {}

    // -------------------------------------------------------------------------
    // Internals
    // -------------------------------------------------------------------------

    function _maybeVerifyV2(address pool, address token0, address token1) internal view {
        Venue memory v = venues[POOL_TYPE_V2];
        if (!v.enabled) return;
        address expected = _pairForV2(v.factory, token0, token1, v.initCodeHash);
        if (expected != pool) revert VenueMismatch();
    }

    function _maybeVerifyV3(address pool, address token0, address token1, uint24 fee) internal view {
        Venue memory v = venues[POOL_TYPE_V3];
        if (!v.enabled) return;
        address expected = _pairForV3(v.factory, token0, token1, fee, v.initCodeHash);
        if (expected != pool) revert VenueMismatch();
    }

    function _pairForV2(address factory, address tokenA, address tokenB, bytes32 initCodeHash)
        internal
        pure
        returns (address pair)
    {
        (address token0, address token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        pair = address(
            uint160(
                uint256(
                    keccak256(
                        abi.encodePacked(hex"ff", factory, keccak256(abi.encodePacked(token0, token1)), initCodeHash)
                    )
                )
            )
        );
    }

    function _pairForV3(address factory, address tokenA, address tokenB, uint24 fee, bytes32 initCodeHash)
        internal
        pure
        returns (address pool)
    {
        (address token0, address token1) = tokenA < tokenB ? (tokenA, tokenB) : (tokenB, tokenA);
        pool = address(
            uint160(
                uint256(
                    keccak256(abi.encodePacked(hex"ff", factory, keccak256(abi.encode(token0, token1, fee)), initCodeHash))
                )
            )
        );
    }

    function _balanceOf(address token, address account) internal view returns (uint256) {
        (bool ok, bytes memory data) =
            token.staticcall(abi.encodeWithSelector(0x70a08231, account)); // balanceOf(address)
        if (!ok || data.length < 32) return 0;
        return abi.decode(data, (uint256));
    }

    function _safeTransfer(address token, address to, uint256 amount) internal {
        (bool success, bytes memory data) =
            token.call(abi.encodeWithSelector(0xa9059cbb, to, amount)); // transfer(address,uint256)
        if (!success) revert TransferFailed();
        if (data.length > 0) {
            if (data.length < 32 || !abi.decode(data, (bool))) revert TransferFailed();
        }
    }
}
