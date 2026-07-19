// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import {MockERC20} from "./MockERC20.sol";

interface IExecutorCallback {
    function agniSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

/// @dev Simple constant-product V2-style pair for unit tests.
contract MockV2Pair {
    address public token0;
    address public token1;
    uint112 public reserve0;
    uint112 public reserve1;

    /// @notice If set, swap pays `drainToken` amount of the executor via callback-style pull is N/A;
    /// instead we just transfer from executor if approved. For V2 we receive tokens first.
    uint256 public outOverride; // 0 = use x*y math
    bool public useOutOverride;

    constructor(address t0, address t1) {
        require(t0 < t1, "ORDER");
        token0 = t0;
        token1 = t1;
    }

    function setReserves(uint112 r0, uint112 r1) external {
        reserve0 = r0;
        reserve1 = r1;
    }

    function setOutOverride(uint256 amount, bool enabled) external {
        outOverride = amount;
        useOutOverride = enabled;
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, 0);
    }

    function swap(uint amount0Out, uint amount1Out, address to, bytes calldata) external {
        require(amount0Out > 0 || amount1Out > 0, "IOA");
        if (amount0Out > 0) {
            MockERC20(token0).transfer(to, amount0Out);
            reserve0 = uint112(uint256(reserve0) - amount0Out);
        }
        if (amount1Out > 0) {
            MockERC20(token1).transfer(to, amount1Out);
            reserve1 = uint112(uint256(reserve1) - amount1Out);
        }
        // absorb input sitting on pair
        uint256 bal0 = MockERC20(token0).balanceOf(address(this));
        uint256 bal1 = MockERC20(token1).balanceOf(address(this));
        reserve0 = uint112(bal0);
        reserve1 = uint112(bal1);
    }

    /// @dev Helper for tests: compute amountOut with 0.3% fee style.
    function getAmountOut(uint256 amountIn, bool zeroForOne) external view returns (uint256) {
        if (useOutOverride) return outOverride;
        (uint256 rIn, uint256 rOut) = zeroForOne ? (uint256(reserve0), uint256(reserve1)) : (uint256(reserve1), uint256(reserve0));
        uint256 amountInWithFee = amountIn * 997;
        return (amountInWithFee * rOut) / (rIn * 1000 + amountInWithFee);
    }
}

/// @dev V3-style pool that pulls input via agniSwapCallback and pays output.
contract MockV3Pool {
    address public token0;
    address public token1;
    uint24 public fee;
    address public factory;
    uint256 public outAmount;
    bool public adverse; // if true, pay less than configured on purpose mid-path tests

    constructor(address t0, address t1, uint24 fee_) {
        require(t0 < t1, "ORDER");
        token0 = t0;
        token1 = t1;
        fee = fee_;
        factory = msg.sender;
        outAmount = 0;
    }

    function setOutAmount(uint256 amount) external {
        outAmount = amount;
    }

    function mintLiquidity(address token, uint256 amount) external {
        MockERC20(token).mint(address(this), amount);
    }

    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1) {
        require(amountSpecified > 0, "EXACT_IN");
        uint256 amountIn = uint256(amountSpecified);
        uint256 amountOut = outAmount;
        if (amountOut == 0) {
            // default 1:1 for simple tests
            amountOut = amountIn;
        }

        if (zeroForOne) {
            amount0 = int256(amountIn);
            amount1 = -int256(amountOut);
            IExecutorCallback(msg.sender).agniSwapCallback(amount0, amount1, data);
            MockERC20(token1).transfer(recipient, amountOut);
        } else {
            amount0 = -int256(amountOut);
            amount1 = int256(amountIn);
            IExecutorCallback(msg.sender).agniSwapCallback(amount0, amount1, data);
            MockERC20(token0).transfer(recipient, amountOut);
        }
    }
}

/// @dev Forged pool that exposes token0/token1 but is not registered.
contract ForgedV3Pool {
    address public token0;
    address public token1;
    uint24 public fee = 3000;

    constructor(address t0, address t1) {
        token0 = t0;
        token1 = t1;
    }

    function tryDrain(address executor, address stealToken, uint256 amount) external {
        // Attempt the historical attack: call callback as if we were a pool.
        IExecutorCallback(executor).agniSwapCallback(int256(amount), 0, "");
        // If callback transferred stealToken to us, forward to attacker.
        uint256 bal = MockERC20(stealToken).balanceOf(address(this));
        if (bal > 0) {
            MockERC20(stealToken).transfer(msg.sender, bal);
        }
    }
}

/// @dev Moe LB-style pair: pull is pre-funded transfer; swap sends the other token.
contract MockMoeLBPair {
    address public tokenX;
    address public tokenY;
    uint256 public outAmount;

    constructor(address x, address y) {
        tokenX = x;
        tokenY = y;
        outAmount = 0;
    }

    function getTokenX() external view returns (address) {
        return tokenX;
    }

    function getTokenY() external view returns (address) {
        return tokenY;
    }

    function getActiveId() external pure returns (uint24) {
        return 8388608;
    }

    function getBinStep() external pure returns (uint16) {
        return 25;
    }

    function setOutAmount(uint256 amount) external {
        outAmount = amount;
    }

    function mintLiquidity(address token, uint256 amount) external {
        MockERC20(token).mint(address(this), amount);
    }

    function swap(bool swapForY, address to) external returns (bytes32) {
        address tokenIn = swapForY ? tokenX : tokenY;
        address tokenOut = swapForY ? tokenY : tokenX;
        uint256 amountIn = MockERC20(tokenIn).balanceOf(address(this));
        // leave nothing of tokenIn on pair for simplicity after "swap"
        // (in real LB the reserves update; here we burn input)
        MockERC20(tokenIn).transfer(address(0xdead), amountIn);

        uint256 amountOut = outAmount == 0 ? amountIn : outAmount;
        MockERC20(tokenOut).transfer(to, amountOut);
        return bytes32(0);
    }
}
