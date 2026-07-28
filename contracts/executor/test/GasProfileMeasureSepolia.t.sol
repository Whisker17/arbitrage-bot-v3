// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import {Test} from "forge-std/Test.sol";
import {ArbitrageExecutor} from "../ArbitrageExecutor.sol";
import {FixtureERC20} from "fixtures/FixtureERC20.sol";
import {E2EFixturePoolV2} from "fixtures/E2EFixturePoolV2.sol";
import {E2EFixturePoolAgniV3} from "fixtures/E2EFixturePoolAgniV3.sol";

interface IWMNTLike {
    function deposit() external payable;
    function balanceOf(address) external view returns (uint256);
}

/// @notice WHI-525 M2 gas measurements for the Mantle Sepolia E2E fixture venues.
/// @dev Forks Mantle Sepolia (read-only; no private key, no broadcast) so the real
///      WMNT contract's bytecode backs every measurement, then deploys the real
///      `ArbitrageExecutor` and the real M1 fixture pools (`E2EFixturePoolV2`,
///      `E2EFixturePoolAgniV3`) fresh inside the forked EVM — a real executeArbitrage
///      cycle, not a mock-pool stand-in. Only two route classes exist with one V2 +
///      one V3 fixture (v2/v3, v3/v2); deeper route classes stay explicit Unsupported
///      in the generator config, same as mainnet's mock-pool gap.
///
/// Emit lines: `GAS_PROFILE_MEASURE|<route_key>|<amountIn>:<gasUsed>` (same convention
/// as `GasProfileMeasure.t.sol`; `forge test -vvv` to observe them).
contract GasProfileMeasureSepoliaTest is Test {
    uint256 internal constant Q96 = 0x1000000000000000000000000;
    uint256 internal constant SKEW_BPS = 500; // 5%, comfortably over the 2*0.30% round-trip fee drag.
    address internal constant WMNT = 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF;

    ArbitrageExecutor internal exec;
    FixtureERC20 internal token;

    address internal admin = address(0xA11CE);
    address internal hot = address(0xB0B);

    function setUp() public {
        vm.createSelectFork(vm.envString("MANTLE_SEPOLIA_RPC_URL"));

        exec = new ArbitrageExecutor(WMNT, admin);
        vm.prank(admin);
        exec.setHotExecutor(hot, true);

        token = new FixtureERC20("E2E Gas Profile Token", "eGAS", 18);

        // vm.deal fakes native balance in the local fork only; no real funds involved.
        // Large enough to fund the executor's starting balance plus both fixture pools'
        // WMNT-side reserves (up to 10_000_000 ether for the skewed V3 side) from this
        // test contract's own wallet.
        vm.deal(address(this), 20_000_000 ether);
        IWMNTLike(WMNT).deposit{value: 20_000_000 ether}();
        IFixtureERC20Like(WMNT).transfer(address(exec), 1_000 ether);
    }

    function _ordered(address a, address b) internal pure returns (address t0, address t1) {
        return a < b ? (a, b) : (b, a);
    }

    /// @dev Fair 1:1 V2 pool (real invariant, real fee) pairing WMNT against the fixture token.
    function _deployFairV2() internal returns (E2EFixturePoolV2 pool) {
        (address t0, address t1) = _ordered(WMNT, address(token));
        pool = new E2EFixturePoolV2(t0, t1, 300);
        _mint(t0, address(pool), 1_000_000 ether);
        _mint(t1, address(pool), 1_000_000 ether);
        pool.seed();
        vm.prank(admin);
        exec.registerPool(address(pool), 0);
    }

    /// @dev V3 pool skewed so that selling `tokenIn` for `tokenOut` yields more `tokenOut`
    ///      than a fair 1:1 price would — enough to clear the round-trip fee drag against
    ///      a fair-priced V2 leg on the other hop. Virtual reserves only (this fixture's
    ///      `seed()` does not read balances), so real balances are funded generously above
    ///      whatever the skewed reserves imply.
    function _deploySkewedV3(address tokenIn) internal returns (E2EFixturePoolAgniV3 pool) {
        (address t0, address t1) = _ordered(WMNT, address(token));
        pool = new E2EFixturePoolAgniV3(t0, t1, 3000);
        _mint(t0, address(pool), 10_000_000 ether);
        _mint(t1, address(pool), 10_000_000 ether);

        uint256 base = 1_000_000 ether;
        uint256 boosted = (base * (10_000 + SKEW_BPS)) / 10_000;
        (uint256 reserve0, uint256 reserve1) =
            tokenIn == t0 ? (base, boosted) : (boosted, base);
        // (reserve1 * Q96 * Q96) / reserve0 overflows uint256 at these reserve magnitudes
        // (~1e24 * 2^192 ~= 6.6e81 > ~1.16e77 max) — divide by reserve0 before the second
        // multiply-by-Q96 to keep every intermediate product in range.
        uint256 ratioQ96 = (reserve1 * Q96) / reserve0;
        uint160 sqrtPriceX96 = uint160(_sqrt(ratioQ96 * Q96));
        uint128 liquidity = uint128(_sqrt(reserve0 * reserve1));
        pool.seed(sqrtPriceX96, liquidity);

        vm.prank(admin);
        exec.registerPool(address(pool), 1);
        // Sanity: the skew must favor tokenIn -> tokenOut, not the reverse.
        assertTrue(_v3AmountOut(pool, tokenIn, 1 ether) > (1 ether * (10_000 + SKEW_BPS / 2)) / 10_000);
    }

    function _mint(address t, address to, uint256 amount) internal {
        if (t == WMNT) {
            IFixtureERC20Like(WMNT).transfer(to, amount);
        } else {
            FixtureERC20(t).mint(to, amount);
        }
    }

    function _v2AmountOut(E2EFixturePoolV2 pool, address tokenIn, uint256 amountIn)
        internal
        view
        returns (uint256)
    {
        (uint112 r0, uint112 r1,) = pool.getReserves();
        (uint256 reserveIn, uint256 reserveOut) = tokenIn == pool.token0() ? (r0, r1) : (r1, r0);
        uint256 amountInWithFee = amountIn * (100_000 - pool.fee());
        return (amountInWithFee * reserveOut) / (reserveIn * 100_000 + amountInWithFee);
    }

    /// @dev Mirrors `E2EFixturePoolAgniV3.swap`'s exact virtual-reserve formula so the
    ///      predicted amountOut matches the real fixture's output bit-for-bit.
    function _v3AmountOut(E2EFixturePoolAgniV3 pool, address tokenIn, uint256 amountIn)
        internal
        view
        returns (uint256 amountOut)
    {
        bool zeroForOne = tokenIn == pool.token0();
        uint256 amountInAfterFee = (amountIn * (1_000_000 - pool.fee())) / 1_000_000;
        uint256 l = uint256(pool.liquidity());
        uint256 sqrtP = uint256(pool.sqrtPriceX96());

        if (zeroForOne) {
            uint256 x = (l * Q96) / sqrtP;
            uint256 y = (l * sqrtP) / Q96;
            uint256 newX = x + amountInAfterFee;
            uint256 newSqrtP = (l * Q96) / newX;
            uint256 newY = (l * newSqrtP) / Q96;
            amountOut = y > newY ? y - newY : 0;
        } else {
            uint256 x = (l * Q96) / sqrtP;
            uint256 y = (l * sqrtP) / Q96;
            uint256 newY = y + amountInAfterFee;
            uint256 newSqrtP = (newY * Q96) / l;
            uint256 newX = (l * Q96) / newSqrtP;
            amountOut = x > newX ? x - newX : 0;
        }
    }

    function _measure(
        string memory key,
        uint256 amountIn,
        address[] memory path,
        address[] memory pools,
        uint8[] memory types,
        uint256[] memory amountsOut
    ) internal returns (uint256 gasUsed) {
        uint256 gasBefore = gasleft();
        vm.prank(hot);
        exec.executeArbitrage(amountIn, path, pools, types, amountsOut, 0, block.timestamp + 1);
        gasUsed = gasBefore - gasleft();
        // forge test -vvv: parse lines starting with GAS_PROFILE_MEASURE
        emit log_named_uint(string.concat("GAS_PROFILE_MEASURE|", key, "|", vm.toString(amountIn)), gasUsed);
    }

    /// @dev Same grid as the mainnet mock-pool suite: 12 real gasleft() measurements per route.
    function _amountGrid() internal pure returns (uint256[12] memory a) {
        a[0] = 1 ether;
        a[1] = 2 ether;
        a[2] = 3 ether;
        a[3] = 4 ether;
        a[4] = 5 ether;
        a[5] = 6 ether;
        a[6] = 7 ether;
        a[7] = 8 ether;
        a[8] = 9 ether;
        a[9] = 10 ether;
        a[10] = 12 ether;
        a[11] = 15 ether;
    }

    function test_measure_v2_v3_ticks0_grid() public {
        E2EFixturePoolV2 v2 = _deployFairV2();
        E2EFixturePoolAgniV3 v3 = _deploySkewedV3(address(token));

        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            uint256 out1 = _v2AmountOut(v2, WMNT, amountIn);
            uint256 out2 = _v3AmountOut(v3, address(token), out1);
            require(out2 >= amountIn, "v2v3 not profitable");

            address[] memory path = new address[](3);
            path[0] = WMNT;
            path[1] = address(token);
            path[2] = WMNT;
            address[] memory pools = new address[](2);
            pools[0] = address(v2);
            pools[1] = address(v3);
            uint8[] memory types = new uint8[](2);
            types[0] = 0;
            types[1] = 1;
            uint256[] memory amountsOut = new uint256[](2);
            amountsOut[0] = out1;
            amountsOut[1] = 0;
            _measure("v2+v3:ticks=0", amountIn, path, pools, types, amountsOut);
        }
    }

    function test_measure_v3_v2_ticks0_grid() public {
        E2EFixturePoolAgniV3 v3 = _deploySkewedV3(WMNT);
        E2EFixturePoolV2 v2 = _deployFairV2();

        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            uint256 out1 = _v3AmountOut(v3, WMNT, amountIn);
            uint256 out2 = _v2AmountOut(v2, address(token), out1);
            require(out2 >= amountIn, "v3v2 not profitable");

            address[] memory path = new address[](3);
            path[0] = WMNT;
            path[1] = address(token);
            path[2] = WMNT;
            address[] memory pools = new address[](2);
            pools[0] = address(v3);
            pools[1] = address(v2);
            uint8[] memory types = new uint8[](2);
            types[0] = 1;
            types[1] = 0;
            uint256[] memory amountsOut = new uint256[](2);
            amountsOut[0] = 0;
            amountsOut[1] = out2;
            _measure("v3+v2:ticks=0", amountIn, path, pools, types, amountsOut);
        }
    }

    function _sqrt(uint256 x) internal pure returns (uint256 y) {
        if (x == 0) return 0;
        uint256 z = (x + 1) / 2;
        y = x;
        while (z < y) {
            y = z;
            z = (x / z + z) / 2;
        }
    }
}

interface IFixtureERC20Like {
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}
