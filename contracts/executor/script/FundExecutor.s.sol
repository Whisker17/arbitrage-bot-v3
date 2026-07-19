// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

import "forge-std/Script.sol";
import "../ArbitrageExecutor.sol";

interface IWMNT {
    function deposit() external payable;
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/**
 * @title FundExecutor
 * @notice 向 ArbitrageExecutor 合约注入 WMNT 的脚本
 * 
 * Usage:
 * 
 * 1. 设置环境变量 ARBITRAGE_EXECUTOR_ADDRESS（部署后的合约地址）
 * 
 * 2. 运行注资命令（注入 1 WMNT）：
 *    forge script script/FundExecutor.s.sol:FundExecutor \
 *      --rpc-url $MANTLE_MAINNET_RPC_URL \
 *      --private-key $MANTLE_MAINNET_PRIVATE_KEY \
 *      --broadcast \
 *      -vvvv
 * 
 * 3. 可以通过修改 FUNDING_AMOUNT 变量来调整注资金额
 */
contract FundExecutor is Script {
    // Mantle 主网 WMNT 地址
    address constant WMNT = 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8;
    
    // 注资金额（单位：wei）
    // 默认 1 WMNT = 1 ether
    uint256 constant FUNDING_AMOUNT = 1 ether;

    function run() external {
        // 从环境变量读取私钥和合约地址
        // Prefer chain-prefixed key name used in repo docs; fall back to PRIVATE_KEY.
        uint256 deployerPrivateKey = vm.envOr("MANTLE_MAINNET_PRIVATE_KEY", uint256(0));
        if (deployerPrivateKey == 0) {
            deployerPrivateKey = vm.envUint("PRIVATE_KEY");
        }
        address executorAddress = vm.envAddress("ARBITRAGE_EXECUTOR_ADDRESS");

        vm.startBroadcast(deployerPrivateKey);

        IWMNT wmnt = IWMNT(WMNT);
        
        // 检查当前余额
        uint256 balanceBefore = wmnt.balanceOf(executorAddress);
        console.log("================================================================================");
        console.log("Funding ArbitrageExecutor with WMNT");
        console.log("================================================================================");
        console.log("Executor Address:", executorAddress);
        console.log("WMNT Balance Before:", balanceBefore);
        console.log("Funding Amount:", FUNDING_AMOUNT);

        // 将 MNT 包装为 WMNT
        wmnt.deposit{value: FUNDING_AMOUNT}();
        
        // 将 WMNT 转账到执行器合约
        require(wmnt.transfer(executorAddress, FUNDING_AMOUNT), "Transfer failed");

        // 检查新余额
        uint256 balanceAfter = wmnt.balanceOf(executorAddress);
        console.log("WMNT Balance After:", balanceAfter);
        console.log("================================================================================");
        console.log("Funding Complete!");
        console.log("================================================================================");

        vm.stopBroadcast();
    }
}
