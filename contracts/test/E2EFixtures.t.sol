// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import "forge-std/Test.sol";
import "../src/fixtures/FixtureERC20.sol";
import "../src/fixtures/E2EFixturePoolV2.sol";
import "../src/fixtures/E2EFixturePoolAgniV3.sol";

contract E2EFixturesTest is Test, IAgniSwapCallback {
    uint256 internal constant Q96 = 0x1000000000000000000000000;
    uint256 internal constant SEED_AMOUNT = 1_000_000 ether;

    FixtureERC20 internal tokenA;
    FixtureERC20 internal tokenB;
    address internal token0;
    address internal token1;

    E2EFixturePoolV2 internal v2Pool;
    E2EFixturePoolAgniV3 internal v3Pool;

    address internal pendingPayToken;
    address internal pendingPayPool;

    function setUp() public {
        tokenA = new FixtureERC20("Token A", "TKA", 18);
        tokenB = new FixtureERC20("Token B", "TKB", 18);
        (token0, token1) = address(tokenA) < address(tokenB)
            ? (address(tokenA), address(tokenB))
            : (address(tokenB), address(tokenA));

        v2Pool = new E2EFixturePoolV2(token0, token1, 300);
        v3Pool = new E2EFixturePoolAgniV3(token0, token1, 3000);

        FixtureERC20(token0).mint(address(v2Pool), SEED_AMOUNT);
        FixtureERC20(token1).mint(address(v2Pool), SEED_AMOUNT);
        v2Pool.seed();

        FixtureERC20(token0).mint(address(v3Pool), SEED_AMOUNT);
        FixtureERC20(token1).mint(address(v3Pool), SEED_AMOUNT);
        v3Pool.seed(uint160(Q96), uint128(SEED_AMOUNT));
    }

    function test_v2SeedReadsBackReserves() public view {
        (uint112 r0, uint112 r1,) = v2Pool.getReserves();
        assertEq(uint256(r0), SEED_AMOUNT);
        assertEq(uint256(r1), SEED_AMOUNT);
        assertEq(v2Pool.token0(), token0);
        assertEq(v2Pool.token1(), token1);
    }

    function test_v2SwapRespectsInvariant() public {
        uint256 amountIn = 1_000 ether;
        FixtureERC20(token0).mint(address(this), amountIn);
        FixtureERC20(token0).transfer(address(v2Pool), amountIn);

        uint256 amountInWithFee = amountIn * 99_700;
        uint256 amountOut = (amountInWithFee * SEED_AMOUNT) / (SEED_AMOUNT * 100_000 + amountInWithFee);

        v2Pool.swap(0, amountOut, address(this), "");

        assertEq(FixtureERC20(token1).balanceOf(address(this)), amountOut);
        (uint112 r0, uint112 r1,) = v2Pool.getReserves();
        assertEq(uint256(r0), SEED_AMOUNT + amountIn);
        assertEq(uint256(r1), SEED_AMOUNT - amountOut);
    }

    function test_v2SwapRevertsWhenOutputTooHigh() public {
        uint256 amountIn = 1_000 ether;
        FixtureERC20(token0).mint(address(this), amountIn);
        FixtureERC20(token0).transfer(address(v2Pool), amountIn);

        uint256 tooMuchOut = 2_000 ether;
        vm.expectRevert(E2EFixturePoolV2.InvariantViolated.selector);
        v2Pool.swap(0, tooMuchOut, address(this), "");
    }

    function test_v3SeedReadsBackSlot0() public view {
        (uint160 sqrtPriceX96, int24 tick,,,,, bool unlocked) = v3Pool.slot0();
        assertEq(sqrtPriceX96, uint160(Q96));
        assertEq(tick, int24(0));
        assertTrue(unlocked);
        assertEq(v3Pool.liquidity(), uint128(SEED_AMOUNT));
        assertEq(v3Pool.tickBitmap(0), 0);
    }

    function test_v3SwapZeroForOneMovesPriceDown() public {
        (uint160 priceBefore,,,,,,) = v3Pool.slot0();

        pendingPayToken = token0;
        pendingPayPool = address(v3Pool);
        uint256 amountIn = 1_000 ether;
        FixtureERC20(token0).mint(address(this), amountIn);

        (int256 amount0, int256 amount1) = v3Pool.swap(address(this), true, int256(amountIn), 0, "");

        assertEq(amount0, int256(amountIn));
        assertTrue(amount1 < 0);
        assertEq(FixtureERC20(token1).balanceOf(address(this)), uint256(-amount1));

        (uint160 priceAfter,,,,,,) = v3Pool.slot0();
        assertTrue(priceAfter < priceBefore);
    }

    function test_v3SwapRevertsWhenCallbackUnderpays() public {
        pendingPayToken = address(0);
        pendingPayPool = address(v3Pool);
        uint256 amountIn = 1_000 ether;

        vm.expectRevert(E2EFixturePoolAgniV3.InsufficientInputPaid.selector);
        v3Pool.swap(address(this), true, int256(amountIn), 0, "");
    }

    function agniSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        require(msg.sender == pendingPayPool, "unexpected caller");
        if (pendingPayToken == address(0)) {
            return;
        }
        int256 delta = pendingPayToken == token0 ? amount0Delta : amount1Delta;
        if (delta > 0) {
            FixtureERC20(pendingPayToken).transfer(msg.sender, uint256(delta));
        }
    }
}
