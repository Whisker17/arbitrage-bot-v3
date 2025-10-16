// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

interface IMoeLBToken {
    function totalSupply(uint256 id) external view returns (uint256);

    function balanceOf(address account, uint256 id) external view returns (uint256);

    function isApprovedForAll(address owner, address spender) external view returns (bool);
}

interface IMoeLBPair {
    function getTokenX() external view returns (address);

    function getTokenY() external view returns (address);

    function getReserves() external view returns (uint128 reserveX, uint128 reserveY);

    function getBinStep() external view returns (uint16);

    function getActiveId() external view returns (uint24);

    function getBin(uint24 id) external view returns (uint128 reserveX, uint128 reserveY);

    function getPriceFromId(uint24 id) external view returns (uint256);

    function getNextNonEmptyBin(bool swapForY, uint24 id) external view returns (uint24);

    function getStaticFeeParameters()
        external
        view
        returns (
            uint16 baseFactor,
            uint16 filterPeriod,
            uint16 decayPeriod,
            uint16 reductionFactor,
            uint24 variableFeeControl,
            uint16 protocolShare,
            uint24 maxVolatilityAccumulator
        );
}

interface IMoeLBFactory {
    event LBPairCreated(address indexed tokenX, address indexed tokenY, uint16 binStep, address lbPair, uint256 pid);

    function getMinBinStep() external view returns (uint256);

    function getLBPairImplementation() external view returns (address);

    function getAllLBPairs(address tokenX, address tokenY)
        external
        view
        returns (address[] memory lbPairs);

    function getQuoteAssetAtIndex(uint256 index) external view returns (address);

    function getNumberOfQuoteAssets() external view returns (uint256);
}
