// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import "forge-std/Script.sol";
import "../ArbitrageExecutor.sol";

/**
 * @title DeployArbitrageExecutor
 * @notice Deploy the hardened ArbitrageExecutor (do not broadcast from WHI-501).
 *
 * Env (must match docs):
 *   - MANTLE_MAINNET_PRIVATE_KEY or PRIVATE_KEY (uint)
 *   - optional ADMIN address (defaults to broadcaster)
 *   - optional HOT_EXECUTOR address to grant immediately
 *
 * Example (no broadcast):
 *   forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
 *     --rpc-url $MANTLE_MAINNET_RPC_URL -vvvv
 */
contract DeployArbitrageExecutor is Script {
    address constant WMNT = 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8;

    function run() external {
        uint256 deployerPrivateKey = vm.envOr("MANTLE_MAINNET_PRIVATE_KEY", uint256(0));
        if (deployerPrivateKey == 0) {
            deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        }
        address admin = vm.envOr("ADMIN", address(0));
        address hotExecutor = vm.envOr("HOT_EXECUTOR", address(0));

        vm.startBroadcast(deployerPrivateKey);
        address deployer = vm.addr(deployerPrivateKey);
        if (admin == address(0)) {
            admin = deployer;
        }

        ArbitrageExecutor executor = new ArbitrageExecutor(WMNT, admin);
        if (hotExecutor != address(0)) {
            executor.setHotExecutor(hotExecutor, true);
        }

        console.log("ArbitrageExecutor deployed:", address(executor));
        console.log("admin:", executor.admin());
        console.log("WMNT:", executor.WMNT());
        console.log("Set ARBITRAGE_EXECUTOR_ADDRESS after verification (not in WHI-501).");
        vm.stopBroadcast();
    }
}
