// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IFixtureERC20Transferable {
    function transfer(address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}
