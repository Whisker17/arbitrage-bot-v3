// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

/// @dev Minimal reproduction of the pre-WHI-501 callback hole for fork-style regression tests.
///      `agniSwapCallback` only rejects EOAs and trusts caller token getters.
contract LegacyVulnerableExecutor {
    address public immutable WMNT;
    address public immutable owner;

    constructor(address wmnt_) {
        WMNT = wmnt_;
        owner = msg.sender;
    }

    function agniSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata) external {
        require(msg.sender != tx.origin, "NO_EOA_CALLBACK");
        if (amount0Delta > 0) {
            address token0 = ITokenGetters(msg.sender).token0();
            require(IERC20Like(token0).transfer(msg.sender, uint256(amount0Delta)), "T0");
        }
        if (amount1Delta > 0) {
            address token1 = ITokenGetters(msg.sender).token1();
            require(IERC20Like(token1).transfer(msg.sender, uint256(amount1Delta)), "T1");
        }
    }

    receive() external payable {}
}

interface ITokenGetters {
    function token0() external view returns (address);
    function token1() external view returns (address);
}

interface IERC20Like {
    function transfer(address to, uint256 amount) external returns (bool);
}
