// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import "./IFixtureERC20Transferable.sol";

interface IAgniSwapCallback {
    function agniSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external;
}

/// @notice Full-range virtual-reserve Agni/Uniswap-V3-style pool, deployed as a
///         WHI-525 E2E fixture only — never a production DEX contract.
/// @dev Full-range V3 liquidity is exactly a constant-product AMM on virtual
///      reserves `x = L*2^96/sqrtP`, `y = L*sqrtP/2^96` (invariant `x*y = L^2`),
///      so genuine swap math needs no `TickMath`/`SqrtPriceMath` port. `tickSpacing`
///      is deliberately huge (200_000, far under `MAX_TICK`'s ~8.3e6 magnitude) so
///      that any realistic in-test price movement still divides down to the same
///      word as the hardcoded `tick = 0` reported by `slot0()`. The tick/bitmap/
///      ticks() fields exist only to satisfy the batch-request ABI shape the Rust
///      reader expects; they are never load-bearing for this fixture's own swap
///      math, since liquidity here is never tick-ranged.
contract E2EFixturePoolAgniV3 {
    uint256 internal constant Q96 = 0x1000000000000000000000000;
    uint256 internal constant FEE_DENOM = 1_000_000;
    int24 public constant tickSpacing = 200_000;

    address public immutable token0;
    address public immutable token1;
    uint24 public immutable fee;

    uint160 public sqrtPriceX96;
    uint128 public liquidity;

    event Swap(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity
    );

    error AlreadySeeded();
    error NotSeeded();
    error ZeroLiquidity();
    error ZeroAmountSpecified();
    error NegativeAmountUnsupported();
    error InsufficientInputPaid();

    struct TickInfo {
        uint128 liquidityGross;
        int128 liquidityNet;
        uint256 feeGrowthOutside0X128;
        uint256 feeGrowthOutside1X128;
        int56 tickCumulativeOutside;
        uint160 secondsPerLiquidityOutsideX128;
        uint32 secondsOutside;
        bool initialized;
    }

    constructor(address token0_, address token1_, uint24 fee_) {
        require(token0_ < token1_, "ORDER");
        token0 = token0_;
        token1 = token1_;
        fee = fee_;
    }

    /// @notice One-time bootstrap: the pool must already hold enough of both
    ///         tokens to back the requested full-range liquidity at `initialSqrtPriceX96`.
    function seed(uint160 initialSqrtPriceX96, uint128 initialLiquidity) external {
        if (sqrtPriceX96 != 0) revert AlreadySeeded();
        if (initialLiquidity == 0) revert ZeroLiquidity();
        sqrtPriceX96 = initialSqrtPriceX96;
        liquidity = initialLiquidity;
    }

    function slot0()
        external
        view
        returns (
            uint160 sqrtPriceX96_,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        )
    {
        return (sqrtPriceX96, 0, 0, 0, 0, 0, true);
    }

    function tickBitmap(int16) external pure returns (uint256) {
        return 0;
    }

    function ticks(int24) external pure returns (TickInfo memory) {
        return TickInfo(0, 0, 0, 0, 0, 0, 0, false);
    }

    /// @dev Mirrors real Agni/UniswapV3 pools: compute the output from current
    ///      state first, invoke the callback so the caller pays the input, verify
    ///      it actually arrived, then pay the output last.
    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1) {
        if (sqrtPriceX96 == 0) revert NotSeeded();
        if (amountSpecified == 0) revert ZeroAmountSpecified();
        if (amountSpecified < 0) revert NegativeAmountUnsupported();

        uint256 amountIn = uint256(amountSpecified);
        uint256 amountInAfterFee = (amountIn * (FEE_DENOM - fee)) / FEE_DENOM;

        uint256 l = uint256(liquidity);
        uint256 sqrtP = uint256(sqrtPriceX96);
        uint256 amountOut;
        uint256 newSqrtP;

        if (zeroForOne) {
            uint256 x = (l * Q96) / sqrtP;
            uint256 y = (l * sqrtP) / Q96;
            uint256 newX = x + amountInAfterFee;
            newSqrtP = (l * Q96) / newX;
            uint256 newY = (l * newSqrtP) / Q96;
            amountOut = y > newY ? y - newY : 0;
            amount0 = amountSpecified;
            amount1 = -int256(amountOut);
        } else {
            uint256 x = (l * Q96) / sqrtP;
            uint256 y = (l * sqrtP) / Q96;
            uint256 newY = y + amountInAfterFee;
            newSqrtP = (newY * Q96) / l;
            uint256 newX = (l * Q96) / newSqrtP;
            amountOut = x > newX ? x - newX : 0;
            amount0 = -int256(amountOut);
            amount1 = amountSpecified;
        }

        address tokenIn = zeroForOne ? token0 : token1;
        address tokenOut = zeroForOne ? token1 : token0;
        uint256 balanceInBefore = IFixtureERC20Transferable(tokenIn).balanceOf(address(this));

        IAgniSwapCallback(msg.sender).agniSwapCallback(amount0, amount1, data);

        uint256 balanceInAfter = IFixtureERC20Transferable(tokenIn).balanceOf(address(this));
        if (balanceInAfter < balanceInBefore + amountIn) revert InsufficientInputPaid();

        sqrtPriceX96 = uint160(newSqrtP);
        IFixtureERC20Transferable(tokenOut).transfer(recipient, amountOut);

        emit Swap(msg.sender, recipient, amount0, amount1, sqrtPriceX96, liquidity);
    }
}
