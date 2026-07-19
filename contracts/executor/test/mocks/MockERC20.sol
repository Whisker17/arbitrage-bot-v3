// SPDX-License-Identifier: MIT
pragma solidity ^0.8.18;

contract MockERC20 {
    string public name;
    string public symbol;
    uint8 public immutable decimals;
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    bool public returnFalse;
    bool public noReturn;
    bool public revertOnTransfer;

    constructor(string memory name_, string memory symbol_, uint8 decimals_) {
        name = name_;
        symbol = symbol_;
        decimals = decimals_;
    }

    function mint(address to, uint256 amount) external {
        balanceOf[to] += amount;
        totalSupply += amount;
    }

    function setReturnFalse(bool v) external {
        returnFalse = v;
    }

    function setNoReturn(bool v) external {
        noReturn = v;
    }

    function setRevertOnTransfer(bool v) external {
        revertOnTransfer = v;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        if (noReturn) {
            assembly {
                return(0, 0)
            }
        }
        if (returnFalse) return false;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            require(allowed >= amount, "ALLOWANCE");
            allowance[from][msg.sender] = allowed - amount;
        }
        _transfer(from, to, amount);
        if (noReturn) {
            assembly {
                return(0, 0)
            }
        }
        if (returnFalse) return false;
        return true;
    }

    function _transfer(address from, address to, uint256 amount) internal {
        if (revertOnTransfer) revert("REVERT_TRANSFER");
        require(balanceOf[from] >= amount, "BAL");
        unchecked {
            balanceOf[from] -= amount;
            balanceOf[to] += amount;
        }
    }
}
