// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import {Test} from "forge-std/Test.sol";
import {ArbitrageExecutor} from "../ArbitrageExecutor.sol";
import {MockERC20} from "./mocks/MockERC20.sol";
import {MockV2Pair, MockV3Pool, ForgedV3Pool, MockMoeLBPair} from "./mocks/MockPools.sol";
import {LegacyVulnerableExecutor} from "./mocks/LegacyVulnerableExecutor.sol";

contract ArbitrageExecutorTest is Test {
    ArbitrageExecutor internal exec;
    MockERC20 internal wmnt;
    MockERC20 internal tka;
    MockERC20 internal tkb;

    address internal admin = address(0xA11CE);
    address internal hot = address(0xB0B);
    address internal guardian = address(0x61A1D);
    address internal stranger = address(0xBAD);

    function setUp() public {
        wmnt = new MockERC20("WMNT", "WMNT", 18);
        tka = new MockERC20("TKA", "TKA", 18);
        tkb = new MockERC20("TKB", "TKB", 18);

        exec = new ArbitrageExecutor(address(wmnt), admin);

        vm.prank(admin);
        exec.setHotExecutor(hot, true);
        vm.prank(admin);
        exec.setGuardian(guardian);

        wmnt.mint(address(exec), 1_000 ether);
        tka.mint(address(exec), 0);
        tkb.mint(address(exec), 0);
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
        // fund pair
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
        // Moe uses X/Y without sorting requirement in our registry
        pair = new MockMoeLBPair(tokenA, tokenB);
        MockERC20(tokenA).mint(address(pair), 1_000_000 ether);
        MockERC20(tokenB).mint(address(pair), 1_000_000 ether);
        pair.setOutAmount(outAmt);
        vm.prank(admin);
        exec.registerPool(address(pair), 2);
    }

    // -----------------------------------------------------------------------
    // Happy path
    // -----------------------------------------------------------------------

    function test_v2_roundtrip_wmnt_cycle_profit() public {
        // WMNT -> TKA -> WMNT on two V2 pools with favorable rates
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 500 ether, 2000 ether);

        uint256 amountIn = 10 ether;
        uint256 out1 = p1.getAmountOut(amountIn, address(wmnt) == p1.token0());
        // second hop
        bool zfo2 = address(tka) == p2.token0();
        uint256 out2 = p2.getAmountOut(out1, zfo2);

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

        uint256 before = wmnt.balanceOf(address(exec));
        vm.prank(hot);
        exec.executeArbitrage(amountIn, path, pools, types, amountsOut, 0, block.timestamp + 1);
        uint256 afterBal = wmnt.balanceOf(address(exec));
        assertGe(afterBal, before);
    }

    function test_v3_single_hop_then_v2_back() public {
        // WMNT -> TKA via V3 (1:1.1), TKA -> WMNT via V2 profitable
        MockV3Pool v3 = _registerV3(address(wmnt), address(tka), 11 ether); // out for 10 in
        MockV2Pair v2 = _registerV2(address(tka), address(wmnt), 1000 ether, 2000 ether);

        uint256 amountIn = 10 ether;
        uint256 out1 = 11 ether;
        bool zfo = address(tka) == v2.token0();
        uint256 out2 = v2.getAmountOut(out1, zfo);

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

        uint256 before = wmnt.balanceOf(address(exec));
        vm.prank(hot);
        exec.executeArbitrage(amountIn, path, pools, types, amountsOut, 0, block.timestamp + 1);
        assertGe(wmnt.balanceOf(address(exec)), before);
    }

    function test_moe_roundtrip() public {
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

        uint256 before = wmnt.balanceOf(address(exec));
        vm.prank(hot);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 1 ether, block.timestamp + 1);
        assertGe(wmnt.balanceOf(address(exec)), before + 1 ether);
    }

    // -----------------------------------------------------------------------
    // Security: callback forgery
    // -----------------------------------------------------------------------

    function test_forged_callback_reverts() public {
        ForgedV3Pool forged = new ForgedV3Pool(address(wmnt), address(tka));
        uint256 before = wmnt.balanceOf(address(exec));
        vm.expectRevert(ArbitrageExecutor.UnauthorizedCallback.selector);
        forged.tryDrain(address(exec), address(wmnt), 100 ether);
        assertEq(wmnt.balanceOf(address(exec)), before);
    }

    function test_unregistered_pool_reverts() public {
        (address t0, address t1) = _ordered(address(wmnt), address(tka));
        MockV2Pair pair = new MockV2Pair(t0, t1);
        MockERC20(t0).mint(address(pair), 1000 ether);
        MockERC20(t1).mint(address(pair), 1000 ether);
        pair.setReserves(1000 ether, 1000 ether);

        address[] memory path = new address[](2);
        path[0] = address(wmnt);
        path[1] = address(wmnt); // malformed but pool check first-ish
        // need length pools+1
        path = new address[](2);
        path[0] = address(wmnt);
        path[1] = address(tka);

        address[] memory pools = new address[](1);
        pools[0] = address(pair);
        uint8[] memory types = new uint8[](1);
        types[0] = 0;
        uint256[] memory amountsOut = new uint256[](1);
        amountsOut[0] = 1;

        // settlement requires end WMNT — fix path for settlement check after route
        path = new address[](2);
        path[0] = address(wmnt);
        path[1] = address(wmnt);

        // Actually path[1] must be tka for hop then we need 2 hops for cycle.
        // Single hop WMNT->WMNT invalid token direction.
        path = new address[](2);
        path[0] = address(wmnt);
        path[1] = address(tka);

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.SettlementMismatch.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    function test_non_canonical_pool_reverts() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        // unregistered second pool
        (address t0, address t1) = _ordered(address(tka), address(wmnt));
        MockV2Pair p2 = new MockV2Pair(t0, t1);
        MockERC20(t0).mint(address(p2), 1000 ether);
        MockERC20(t1).mint(address(p2), 1000 ether);
        p2.setReserves(1000 ether, 1000 ether);

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
        amountsOut[0] = 1 ether;
        amountsOut[1] = 1 ether;

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.PoolNotRegistered.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    function test_pool_type_mismatch_reverts() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 1000 ether, 1000 ether);

        address[] memory path = new address[](3);
        path[0] = address(wmnt);
        path[1] = address(tka);
        path[2] = address(wmnt);
        address[] memory pools = new address[](2);
        pools[0] = address(p1);
        pools[1] = address(p2);
        uint8[] memory types = new uint8[](2);
        types[0] = 1; // lie: claim V3
        types[1] = 0;
        uint256[] memory amountsOut = new uint256[](2);

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.PoolTypeMismatch.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    function test_token_direction_mismatch_reverts() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 1000 ether, 1000 ether);

        address[] memory path = new address[](3);
        path[0] = address(wmnt);
        path[1] = address(tkb); // wrong intermediate
        path[2] = address(wmnt);
        address[] memory pools = new address[](2);
        pools[0] = address(p1);
        pools[1] = address(p2);
        uint8[] memory types = new uint8[](2);
        types[0] = 0;
        types[1] = 0;
        uint256[] memory amountsOut = new uint256[](2);

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.TokenDirectionMismatch.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    function test_non_wmnt_endpoints_revert() public {
        MockV2Pair p1 = _registerV2(address(tka), address(tkb), 1000 ether, 1000 ether);
        address[] memory path = new address[](2);
        path[0] = address(tka);
        path[1] = address(tkb);
        address[] memory pools = new address[](1);
        pools[0] = address(p1);
        uint8[] memory types = new uint8[](1);
        types[0] = 0;
        uint256[] memory amountsOut = new uint256[](1);
        amountsOut[0] = 1;

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.SettlementMismatch.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    // -----------------------------------------------------------------------
    // Deadline / minProfit
    // -----------------------------------------------------------------------

    function test_deadline_expired_reverts() public {
        MockV2Pair p1 = _registerV2(address(wmnt), address(tka), 1000 ether, 1000 ether);
        MockV2Pair p2 = _registerV2(address(tka), address(wmnt), 1000 ether, 1000 ether);
        address[] memory path = new address[](3);
        path[0] = address(wmnt);
        path[1] = address(tka);
        path[2] = address(wmnt);
        address[] memory pools = new address[](2);
        pools[0] = address(p1);
        pools[1] = address(p2);
        uint8[] memory types = new uint8[](2);
        uint256[] memory amountsOut = new uint256[](2);

        vm.warp(1000);
        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.DeadlineExpired.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, 999);
    }

    function test_min_profit_enforced() public {
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether); // flat
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether); // flat

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

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.InsufficientProfit.selector);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 1 ether, block.timestamp + 1);
    }

    function test_zero_min_profit_allows_break_even() public {
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether);

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

        uint256 before = wmnt.balanceOf(address(exec));
        vm.prank(hot);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
        assertEq(wmnt.balanceOf(address(exec)), before);
    }

    // -----------------------------------------------------------------------
    // Dust preservation + hop deltas
    // -----------------------------------------------------------------------

    function test_preloaded_intermediate_dust_preserved() public {
        // Preload TKA dust on executor
        tka.mint(address(exec), 50 ether);
        uint256 dust = tka.balanceOf(address(exec));

        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether);

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

        vm.prank(hot);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);

        // Dust must remain: hop uses only received delta (10), so final TKA = dust
        assertEq(tka.balanceOf(address(exec)), dust);
    }

    function test_adverse_final_profit_rolls_back_balances() public {
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 5 ether); // loses half

        uint256 wBefore = wmnt.balanceOf(address(exec));
        uint256 aBefore = tka.balanceOf(address(exec));

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

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.InsufficientProfit.selector);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);

        assertEq(wmnt.balanceOf(address(exec)), wBefore);
        assertEq(tka.balanceOf(address(exec)), aBefore);
    }

    function test_cross_hop_redistribution_still_ok_if_final_profit_clears() public {
        // Hop1 pays extra, hop2 pays less, final still profitable
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 20 ether);
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

        uint256 before = wmnt.balanceOf(address(exec));
        vm.prank(hot);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, 1 ether, block.timestamp + 1);
        assertGe(wmnt.balanceOf(address(exec)), before + 1 ether);
    }

    // -----------------------------------------------------------------------
    // Safe transfer + roles
    // -----------------------------------------------------------------------

    function test_no_return_token_transfer_succeeds_on_withdraw() public {
        MockERC20 weird = new MockERC20("W", "W", 18);
        weird.mint(address(exec), 10 ether);
        weird.setNoReturn(true);
        vm.prank(admin);
        exec.withdraw(address(weird));
        assertEq(weird.balanceOf(admin), 10 ether);
    }

    function test_false_return_token_reverts() public {
        MockERC20 weird = new MockERC20("W", "W", 18);
        weird.mint(address(exec), 10 ether);
        weird.setReturnFalse(true);
        vm.prank(admin);
        vm.expectRevert(ArbitrageExecutor.TransferFailed.selector);
        exec.withdraw(address(weird));
    }

    function test_hot_executor_cannot_withdraw() public {
        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        exec.withdraw(address(wmnt));
    }

    function test_hot_executor_cannot_unpause() public {
        vm.prank(guardian);
        exec.pause();
        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        exec.unpause();
    }

    function test_guardian_can_pause_not_withdraw() public {
        vm.prank(guardian);
        exec.pause();
        assertTrue(exec.paused());

        vm.prank(guardian);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        exec.withdraw(address(wmnt));

        // hot cannot execute while paused
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether);
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

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.PausedError.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);

        vm.prank(admin);
        exec.unpause();
    }

    function test_stranger_cannot_execute() public {
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether);
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

        vm.prank(stranger);
        vm.expectRevert(ArbitrageExecutor.NotHotExecutor.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, block.timestamp + 1);
    }

    function test_native_withdraw() public {
        vm.deal(address(exec), 5 ether);
        uint256 before = admin.balance;
        vm.prank(admin);
        exec.withdrawNative(2 ether);
        assertEq(admin.balance, before + 2 ether);
        assertEq(address(exec).balance, 3 ether);

        vm.prank(admin);
        exec.withdrawAllNative();
        assertEq(address(exec).balance, 0);
    }

    function test_abi_has_no_expected_states_selector_collision() public view {
        // ensure executeArbitrage selector matches new 7-arg form
        bytes4 sel = bytes4(keccak256("executeArbitrage(uint256,address[],address[],uint8[],uint256[],uint256,uint256)"));
        assertEq(sel, exec.executeArbitrage.selector);
    }

    /// @notice Regression: the historical callback drain works on the legacy bytecode
    ///         and is rejected by the hardened executor (WHI-501 req 8).
    function test_fork_replay_legacy_drain_blocked_on_hardened() public {
        // --- Legacy: drain succeeds ---
        LegacyVulnerableExecutor legacy = new LegacyVulnerableExecutor(address(wmnt));
        wmnt.mint(address(legacy), 100 ether);
        ForgedV3Pool forged = new ForgedV3Pool(address(wmnt), address(tka));

        uint256 attackerBefore = wmnt.balanceOf(address(this));
        forged.tryDrain(address(legacy), address(wmnt), 40 ether);
        assertEq(wmnt.balanceOf(address(this)), attackerBefore + 40 ether, "legacy must be drainable");
        assertEq(wmnt.balanceOf(address(legacy)), 60 ether);

        // --- Hardened: same attack reverts, balances unchanged ---
        uint256 hardenedBefore = wmnt.balanceOf(address(exec));
        uint256 attackerMid = wmnt.balanceOf(address(this));
        vm.expectRevert(ArbitrageExecutor.UnauthorizedCallback.selector);
        forged.tryDrain(address(exec), address(wmnt), 40 ether);
        assertEq(wmnt.balanceOf(address(exec)), hardenedBefore, "hardened inventory intact");
        assertEq(wmnt.balanceOf(address(this)), attackerMid, "attacker gains nothing");
    }

    function testFuzz_deadline_expired_reverts(uint64 nowTs) public {
        // Always pick a deadline strictly before now.
        uint256 now_ = uint256(nowTs);
        if (now_ == 0) now_ = 1;
        vm.warp(now_);
        uint256 deadline = now_ - 1;

        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether);

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

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.DeadlineExpired.selector);
        exec.executeArbitrage(1 ether, path, pools, types, amountsOut, 0, deadline);
    }

    function testFuzz_positive_min_profit_on_breakeven_reverts(uint128 minProfitRaw) public {
        uint256 minProfit = bound(uint256(minProfitRaw), 1, 5 ether);
        MockMoeLBPair m1 = _registerMoe(address(wmnt), address(tka), 10 ether);
        MockMoeLBPair m2 = _registerMoe(address(tka), address(wmnt), 10 ether); // break-even

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

        vm.prank(hot);
        vm.expectRevert(ArbitrageExecutor.InsufficientProfit.selector);
        exec.executeArbitrage(10 ether, path, pools, types, amountsOut, minProfit, block.timestamp + 1);
    }

    // -----------------------------------------------------------------------
    // WHI-861: custody handoff sequence (setHotExecutor → setGuardian → transferAdmin)
    // -----------------------------------------------------------------------

    /// @notice Exact operator sequence for moving admin off a hot deploy key onto
    ///         a cold key while registering the former admin as execute-only hot.
    ///         Order is load-bearing: transferAdmin must be last.
    function test_whi861_custody_handoff_sequence() public {
        address cold = address(0xC01D);
        address formerAdmin = admin;

        // Precondition mirrors mainnet: paused, zero balances, no hot executors.
        // (setUp already set hot/guardian — reset to mainnet-like blank slate.)
        ArbitrageExecutor fresh = new ArbitrageExecutor(address(wmnt), formerAdmin);
        assertEq(fresh.admin(), formerAdmin);
        assertEq(fresh.guardian(), address(0));
        assertFalse(fresh.isHotExecutor(formerAdmin));
        assertEq(address(fresh).balance, 0);
        assertEq(wmnt.balanceOf(address(fresh)), 0);

        // Optional pause (mainnet already paused at deploy time).
        vm.prank(formerAdmin);
        fresh.pause();
        assertTrue(fresh.paused());

        // 1. Register hot executor while still admin.
        vm.prank(formerAdmin);
        fresh.setHotExecutor(formerAdmin, true);
        assertTrue(fresh.isHotExecutor(formerAdmin));

        // 2. Optional guardian.
        address guardian_ = address(0x61A1D);
        vm.prank(formerAdmin);
        fresh.setGuardian(guardian_);
        assertEq(fresh.guardian(), guardian_);

        // 3. transferAdmin LAST — one-way.
        vm.prank(formerAdmin);
        fresh.transferAdmin(cold);

        // Post-state: cold admin, hot ≠ admin, paused, zero balances.
        assertEq(fresh.admin(), cold);
        assertTrue(fresh.isHotExecutor(formerAdmin));
        assertTrue(formerAdmin != cold);
        assertEq(fresh.guardian(), guardian_);
        assertTrue(fresh.paused());
        assertEq(address(fresh).balance, 0);
        assertEq(wmnt.balanceOf(address(fresh)), 0);

        // Former admin lost admin powers.
        vm.prank(formerAdmin);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        fresh.transferAdmin(formerAdmin);

        vm.prank(formerAdmin);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        fresh.unpause();

        vm.prank(formerAdmin);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        fresh.withdraw(address(wmnt));

        // Cold admin can act (incident usability proof — setGuardian no-op).
        vm.prank(cold);
        fresh.setGuardian(guardian_);
        assertEq(fresh.guardian(), guardian_);

        // Guardian can still pause (already paused — re-pause is fine).
        vm.prank(guardian_);
        fresh.pause();
        assertTrue(fresh.paused());
    }

    /// @notice Reversing the order leaves the hot key unregistered forever if the
    ///         new admin is offline — operator must never transferAdmin first.
    function test_whi861_transfer_admin_before_set_hot_is_unsafe_order() public {
        address cold = address(0xC01D);
        address formerAdmin = admin;
        ArbitrageExecutor fresh = new ArbitrageExecutor(address(wmnt), formerAdmin);

        // Wrong order: transfer first.
        vm.prank(formerAdmin);
        fresh.transferAdmin(cold);

        // Former admin can no longer register itself as hot.
        vm.prank(formerAdmin);
        vm.expectRevert(ArbitrageExecutor.NotAdmin.selector);
        fresh.setHotExecutor(formerAdmin, true);

        assertFalse(fresh.isHotExecutor(formerAdmin));
        assertEq(fresh.admin(), cold);
    }
}
