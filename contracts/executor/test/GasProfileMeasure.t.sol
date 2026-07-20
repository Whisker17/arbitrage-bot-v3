// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import {Test} from "forge-std/Test.sol";
import {ArbitrageExecutor} from "../ArbitrageExecutor.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockV2Pair, MockV3Pool, MockMoeLBPair} from "./mocks/MockPools.sol";

/// @notice Hash-pinned WHI-501 gas measurements for M0-9 / WHI-546.
/// @dev Mock pools (not Mantle state). Deep tick/bin crossings cannot be produced
///      here; those route classes stay explicit Unsupported until a state-fork suite.
///
/// Emit lines: `GAS_PROFILE_MEASURE|<route_key>|<amountIn>:<gasUsed>`
contract GasProfileMeasureTest is Test {
    ArbitrageExecutor internal exec;
    MockERC20 internal wmnt;
    MockERC20 internal tka;
    MockERC20 internal tkb;

    address internal admin = address(0xA11CE);
    address internal hot = address(0xB0B);

    function setUp() public {
        wmnt = new MockERC20("WMNT", "WMNT", 18);
        tka = new MockERC20("TKA", "TKA", 18);
        tkb = new MockERC20("TKB", "TKB", 18);
        exec = new ArbitrageExecutor(address(wmnt), admin);
        vm.prank(admin);
        exec.setHotExecutor(hot, true);
        wmnt.mint(address(exec), 100_000 ether);
    }

    function _ordered(address a, address b) internal pure returns (address t0, address t1) {
        return a < b ? (a, b) : (b, a);
    }

    function _registerV2(address tokenA, address tokenB, uint112 r0, uint112 r1)
        internal
        returns (MockV2Pair pair)
    {
        (address t0, address t1) = _ordered(tokenA, tokenB);
        pair = new MockV2Pair(t0, t1);
        MockERC20(t0).mint(address(pair), r0);
        MockERC20(t1).mint(address(pair), r1);
        pair.setReserves(r0, r1);
        vm.prank(admin);
        exec.registerPool(address(pair), 0);
    }

    function _registerV3(address tokenA, address tokenB, uint256 outAmt)
        internal
        returns (MockV3Pool pool)
    {
        (address t0, address t1) = _ordered(tokenA, tokenB);
        pool = new MockV3Pool(t0, t1, 3000);
        MockERC20(t0).mint(address(pool), 1_000_000 ether);
        MockERC20(t1).mint(address(pool), 1_000_000 ether);
        pool.setOutAmount(outAmt);
        vm.prank(admin);
        exec.registerPool(address(pool), 1);
    }

    function _registerMoe(address tokenA, address tokenB, uint256 outAmt)
        internal
        returns (MockMoeLBPair pair)
    {
        pair = new MockMoeLBPair(tokenA, tokenB);
        MockERC20(tokenA).mint(address(pair), 1_000_000 ether);
        MockERC20(tokenB).mint(address(pair), 1_000_000 ether);
        pair.setOutAmount(outAmt);
        vm.prank(admin);
        exec.registerPool(address(pair), 2);
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
        emit log_named_uint(
            string.concat("GAS_PROFILE_MEASURE|", key, "|", vm.toString(amountIn)),
            gasUsed
        );
    }

    /// @dev 12 distinct amountIns → 12 real gasleft measurements per route (not synthetic ramps).
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

    function test_measure_v2_v2_grid() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 50_000 ether, 50_000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 10_000 ether, 80_000 ether);
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            uint256 out1 = p1.getAmountOut(amountIn, address(wmnt) == p1.token0());
            uint256 out2 = p2.getAmountOut(out1, address(tka) == p2.token0());
            require(out2 >= amountIn, "v2v2 not profitable");

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
            address[] memory pools = new address[](2);
            pools[0] = address(p1);
            pools[1] = address(p2);
            uint8[] memory types = new uint8[](2);
            types[0] = 0;
            types[1] = 0;
            uint256[] memory amountsOut = new uint256[](2);
            amountsOut[0] = out1;
            amountsOut[1] = out2;
            _measure("v2+v2", amountIn, path, pools, types, amountsOut);
        }
    }

    function test_measure_v3_v3_ticks0_grid() public {
        // Fixed mock out amounts scale with input for profitability.
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            // Fresh pools each run so setOutAmount is independent.
            MockV3Pool a = _registerV3(address(wmnt), address(tka), (amountIn * 110) / 100);
            MockV3Pool b = _registerV3(address(tka), address(wmnt), (amountIn * 120) / 100);

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
            address[] memory pools = new address[](2);
            pools[0] = address(a);
            pools[1] = address(b);
            uint8[] memory types = new uint8[](2);
            types[0] = 1;
            types[1] = 1;
            uint256[] memory amountsOut = new uint256[](2);
            _measure("v3+v3:ticks=0", amountIn, path, pools, types, amountsOut);
        }
    }

    function test_measure_v3_v2_ticks0_grid() public {
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            MockV3Pool v3 = _registerV3(address(wmnt), address(tka), (amountIn * 110) / 100);
            MockV2Pair v2 = _registerV2(address(tka), address(wmnt), 50_000 ether, 100_000 ether);
            uint256 out1 = (amountIn * 110) / 100;
            uint256 out2 = v2.getAmountOut(out1, address(tka) == v2.token0());
            require(out2 >= amountIn, "v3v2 not profitable");

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
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

    function test_measure_v2_v3_ticks0_grid() public {
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            MockV2Pair v2 = _registerV2(address(wmnt), address(tka), 50_000 ether, 50_000 ether);
            uint256 out1 = v2.getAmountOut(amountIn, address(wmnt) == v2.token0());
            MockV3Pool v3 = _registerV3(address(tka), address(wmnt), (out1 * 120) / 100);

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
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

    function test_measure_moe_moe_bins0_grid() public {
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), (amountIn * 110) / 100);
            MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), (amountIn * 120) / 100);

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
            address[] memory pools = new address[](2);
            pools[0] = address(m1);
            pools[1] = address(m2);
            uint8[] memory types = new uint8[](2);
            types[0] = 2;
            types[1] = 2;
            uint256[] memory amountsOut = new uint256[](2);
            _measure("moe+moe:bins=0", amountIn, path, pools, types, amountsOut);
        }
    }

    function test_measure_v2_moe_bins0_grid() public {
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            MockV2Pair v2 = _registerV2(address(wmnt), address(tka), 50_000 ether, 50_000 ether);
            uint256 out1 = v2.getAmountOut(amountIn, address(wmnt) == v2.token0());
            MockMoeLBPair moe = _registerMoe(address(tka), address(wmnt), (out1 * 120) / 100);

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
            address[] memory pools = new address[](2);
            pools[0] = address(v2);
            pools[1] = address(moe);
            uint8[] memory types = new uint8[](2);
            types[0] = 0;
            types[1] = 2;
            uint256[] memory amountsOut = new uint256[](2);
            amountsOut[0] = out1;
            amountsOut[1] = 0;
            _measure("v2+moe:bins=0", amountIn, path, pools, types, amountsOut);
        }
    }

    function test_measure_moe_v2_bins0_grid() public {
        uint256[12] memory amounts = _amountGrid();
        for (uint256 i = 0; i < amounts.length; i++) {
            uint256 amountIn = amounts[i];
            MockMoeLBPair moe = _registerMoe(address(wmnt), address(tka), (amountIn * 110) / 100);
            MockV2Pair v2 = _registerV2(address(tka), address(wmnt), 50_000 ether, 100_000 ether);
            uint256 out1 = (amountIn * 110) / 100;
            uint256 out2 = v2.getAmountOut(out1, address(tka) == v2.token0());
            require(out2 >= amountIn, "moev2 not profitable");

            address[] memory path = new address[](3);
            path[0] = address(wmnt);
            path[1] = address(tka);
            path[2] = address(wmnt);
            address[] memory pools = new address[](2);
            pools[0] = address(moe);
            pools[1] = address(v2);
            uint8[] memory types = new uint8[](2);
            types[0] = 2;
            types[1] = 0;
            uint256[] memory amountsOut = new uint256[](2);
            amountsOut[0] = 0;
            amountsOut[1] = out2;
            _measure("moe+v2:bins=0", amountIn, path, pools, types, amountsOut);
        }
    }

    /// Multi-hop mock fixtures remain a documented gap (route class Unsupported).
    function test_measure_v2_v2_v2_documented_gap() public pure {}
}
