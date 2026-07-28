// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import "./IFixtureERC20Transferable.sol";

/// @notice Real constant-product Uniswap-V2-style pair, deployed as a WHI-525 E2E
///         fixture only — never a production DEX contract.
/// @dev Fee is expressed in the same 1/100_000 unit as `amms::UniswapV2Pool::fee`
///      (300 == 0.30%) so the on-chain invariant matches the off-chain `get_amount_out`
///      exactly, keeping preflight predictions and real execution in agreement.
contract E2EFixturePoolV2 {
    uint256 internal constant FEE_DENOM = 100_000;

    address public immutable token0;
    address public immutable token1;
    uint256 public immutable fee;

    uint112 public reserve0;
    uint112 public reserve1;
    uint32 public blockTimestampLast;

    event Sync(uint112 reserve0, uint112 reserve1);
    event Swap(
        address indexed sender,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out,
        address indexed to
    );

    error InsufficientOutputAmount();
    error InsufficientLiquidity();
    error InvalidTo();
    error InsufficientInputAmount();
    error FlashSwapUnsupported();
    error InvariantViolated();

    constructor(address token0_, address token1_, uint256 fee_) {
        require(token0_ < token1_, "ORDER");
        token0 = token0_;
        token1 = token1_;
        fee = fee_;
    }

    /// @notice Seeds reserves from the pool's current token balances.
    /// @dev Fixture-only bootstrap: real Uniswap V2 pairs derive this via `mint()`
    ///      LP-share accounting; the harness never removes liquidity, so we skip it.
    ///      Transfer both tokens to this contract, then call `seed()`.
    function seed() external {
        reserve0 = uint112(IFixtureERC20Transferable(token0).balanceOf(address(this)));
        reserve1 = uint112(IFixtureERC20Transferable(token1).balanceOf(address(this)));
        blockTimestampLast = uint32(block.timestamp % 2 ** 32);
        emit Sync(reserve0, reserve1);
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, blockTimestampLast);
    }

    /// @dev Mirrors real UniswapV2Pair.swap(): caller transfers input in first, then
    ///      requests specific output amounts, and the pool enforces the fee-adjusted
    ///      constant-product invariant against actual post-transfer balances.
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external {
        if (amount0Out == 0 && amount1Out == 0) revert InsufficientOutputAmount();
        (uint112 _reserve0, uint112 _reserve1) = (reserve0, reserve1);
        if (amount0Out >= _reserve0 || amount1Out >= _reserve1) revert InsufficientLiquidity();
        if (to == token0 || to == token1) revert InvalidTo();
        if (data.length != 0) revert FlashSwapUnsupported();

        if (amount0Out > 0) IFixtureERC20Transferable(token0).transfer(to, amount0Out);
        if (amount1Out > 0) IFixtureERC20Transferable(token1).transfer(to, amount1Out);

        uint256 balance0 = IFixtureERC20Transferable(token0).balanceOf(address(this));
        uint256 balance1 = IFixtureERC20Transferable(token1).balanceOf(address(this));

        uint256 amount0In =
            balance0 > uint256(_reserve0) - amount0Out ? balance0 - (uint256(_reserve0) - amount0Out) : 0;
        uint256 amount1In =
            balance1 > uint256(_reserve1) - amount1Out ? balance1 - (uint256(_reserve1) - amount1Out) : 0;
        if (amount0In == 0 && amount1In == 0) revert InsufficientInputAmount();

        {
            uint256 balance0Adjusted = balance0 * FEE_DENOM - amount0In * fee;
            uint256 balance1Adjusted = balance1 * FEE_DENOM - amount1In * fee;
            if (
                balance0Adjusted * balance1Adjusted
                    < uint256(_reserve0) * uint256(_reserve1) * (FEE_DENOM * FEE_DENOM)
            ) {
                revert InvariantViolated();
            }
        }

        reserve0 = uint112(balance0);
        reserve1 = uint112(balance1);
        blockTimestampLast = uint32(block.timestamp % 2 ** 32);
        emit Sync(reserve0, reserve1);
        emit Swap(msg.sender, amount0In, amount1In, amount0Out, amount1Out, to);
    }
}
