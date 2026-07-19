// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import "forge-std/Script.sol";
import "../executor/ArbitrageExecutor.sol";

/**
 * @title DeployArbitrageExecutor (Sepolia)
 * @notice Deploy hardened executor. WHI-501 does not broadcast this script.
 *
 * Env:
 *   - MANTLE_SEPOLIA_PRIVATE_KEY
 *   - optional ADMIN, HOT_EXECUTOR
 */
contract DeployArbitrageExecutor is Script {
    address constant WMNT = 0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF;

    function run() external {
        uint256 deployerPrivateKey = vm.envUint("MANTLE_SEPOLIA_PRIVATE_KEY");
        address admin = vm.envOr("ADMIN", address(0));
        address hotExecutor = vm.envOr("HOT_EXECUTOR", address(0));

        vm.startBroadcast(deployerPrivateKey);
        address deployer = vm.addr(deployerPrivateKey);
        if (admin == address(0)) {
            admin = deployer;
        }

        ArbitrageExecutor executor = new ArbitrageExecutor(WMNT, admin);
        if (hotExecutor != address(0)) {
            // only admin can set; if admin==deployer this works in same tx context
            executor.setHotExecutor(hotExecutor, true);
        }

        console.log("ArbitrageExecutor:", address(executor));
        console.log("admin:", executor.admin());
        console.log("WMNT:", executor.WMNT());
        vm.stopBroadcast();
    }
}
