// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import {Test} from "forge-std/Test.sol";
import {ArbitrageExecutor} from "../ArbitrageExecutor.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockV2Pair, MockV3Pool, MockMoeLBPair} from "./mocks/MockPools.sol";

/// @notice Hash-pinned WHI-501 gas measurements for M0-9 / WHI-546.
/// @dev Mock pools (not Mantle state). Deep tick/bin crossings cannot be produced
///      here; those route classes stay explicit Unsupported until a state-fork suite.
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
        wmnt.mint(address(exec), 10_000 ether);
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

    function _measure(string memory key, uint256 amountIn, address[] memory path, address[] memory pools, uint8[] memory types, uint256[] memory amountsOut)
        internal
        returns (uint256 gasUsed)
    {
        uint256 gasBefore = gasleft();
        vm.prank(hot);
        exec.executeArbitrage(amountIn, path, pools, types, amountsOut, 0, block.timestamp + 1);
        gasUsed = gasBefore - gasleft();
        // forge test -vv: parse lines starting with GAS_PROFILE_MEASURE
        emit log_named_uint(string.concat("GAS_PROFILE_MEASURE|", key), gasUsed);
    }

    function test_measure_v2_v2() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 500 ether, 2000 ether);
        uint256 amountIn = 10 ether;
        uint256 out1 = p1.getAmountOut(amountIn, address(wmnt) == p1.token0());
        uint256 out2 = p2.getAmountOut(out1, address(tka) == p2.token0());

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

    /// @dev 3-hop constant-product mocks are hard to keep profitable under minProfit=0;
    /// multi-hop classes stay explicit Unsupported in the profile artifact until a
    /// dedicated fixture is added. Keep this test as documentation of the gap.
    function test_measure_v2_v2_v2_documented_gap() public pure {
        // no-op: 3+ hop mock fixture pending; route class remains Unsupported.
    }

    function test_measure_v2_moe() public {
        MockV2Pair v2 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockMoeLBPair moe = _registerMoe(address(tka), address(wmnt), 12 ether);
        uint256 amountIn = 10 ether;
        uint256 out1 = v2.getAmountOut(amountIn, address(wmnt) == v2.token0());

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

    function test_measure_moe_v2() public {
        MockMoeLBPair moe = _registerMoe(address(wmnt), address(tka), 11 ether);
        MockV2Pair v2 = _registerV2(address(tka), address(wmnt), 1000 ether, 2000 ether);
        uint256 out2 = v2.getAmountOut(11 ether, address(tka) == v2.token0());

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
        _measure("moe+v2:bins=0", 10 ether, path, pools, types, amountsOut);
    }

    function test_measure_v3_v2() public {
        MockV3Pool v3 = _registerV3(address(wmnt), address(tka), 11 ether);
        MockV2Pair v2 = _registerV2(address(tka), address(wmnt), 1000 ether, 2000 ether);
        uint256 amountIn = 10 ether;
        uint256 out2 = v2.getAmountOut(11 ether, address(tka) == v2.token0());

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

    function test_measure_v2_v3() public {
        MockV2Pair v2 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV3Pool v3 = _registerV3(address(tka), address(wmnt), 12 ether);
        uint256 amountIn = 10 ether;
        uint256 out1 = v2.getAmountOut(amountIn, address(wmnt) == v2.token0());

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

    function test_measure_moe_moe() public {
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 11 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 12 ether);

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
        _measure("moe+moe:bins=0", 10 ether, path, pools, types, amountsOut);
    }

    function test_measure_v3_v3() public {
        MockV3Pool a = _registerV3(address(wmnt), address(tka), 11 ether);
        MockV3Pool b = _registerV3(address(tka), address(wmnt), 12 ether);

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
        _measure("v3+v3:ticks=0", 10 ether, path, pools, types, amountsOut);
    }
}
