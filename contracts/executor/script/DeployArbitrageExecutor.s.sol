// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import "forge-std/Script.sol";
import "../ArbitrageExecutor.sol";

/**
 * @title DeployArbitrageExecutor
 * @notice 部署 ArbitrageExecutor 合约的脚本
 * 
 * Usage:
 * 
 * 1. 确保 .env 文件中有以下变量：
 *    - MANTLE_MAINNET_RPC_URL
 *    - MANTLE_MAINNET_PRIVATE_KEY
 * 
 * 2. 运行部署命令：
 *    forge script script/DeployArbitrageExecutor.s.sol:DeployArbitrageExecutor \
 *      --rpc-url $MANTLE_MAINNET_RPC_URL \
 *      --private-key $MANTLE_MAINNET_PRIVATE_KEY \
 *      --broadcast \
 *      --verify \
 *      -vvvv
 * 
 * 3. 部署后记录合约地址，用于后续注资和交易执行
 */
contract DeployArbitrageExecutor is Script {
    // Mantle 主网 WMNT 地址
    address constant WMNT = 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8;

    function run() external {
        // 从环境变量读取私钥
        uint256 deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        
        vm.startBroadcast(deployerPrivateKey);

        // 部署合约
        OptimizedArbitrageExecutor executor = new OptimizedArbitrageExecutor(WMNT);

        console.log("================================================================================");
        console.log("ArbitrageExecutor Deployed Successfully!");
        console.log("================================================================================");
        console.log("Contract Address:", address(executor));
        console.log("Owner:", executor.owner());
        console.log("WMNT:", executor.WMNT());
        console.log("================================================================================");
        console.log("");
        console.log("Next Steps:");
        console.log("1. Fund the contract with WMNT for arbitrage");
        console.log("2. Update the contract address in your .env file:");
        console.log("   ARBITRAGE_EXECUTOR_ADDRESS=%s", address(executor));
        console.log("3. Run the arbitrage monitor service");
        console.log("================================================================================");

        vm.stopBroadcast();
    }
}
