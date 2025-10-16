//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAgniPoolTickDataBatchRequest {
    struct TickDataInfo {
        address pool;
        int24[] ticks;
    }

    struct Info {
        bool initialized;
        uint128 liquidityGross;
        int128 liquidityNet;
    }

    constructor(TickDataInfo[] memory allPoolInfo) {
        Info[][] memory tickInfoReturn = new Info[][](allPoolInfo.length);

        for (uint256 i = 0; i < allPoolInfo.length; ++i) {
            Info[] memory tickInfo = new Info[](allPoolInfo[i].ticks.length);
            for (uint256 j = 0; j < allPoolInfo[i].ticks.length; ++j) {
                IAgniPoolState.Info memory tick = IAgniPoolState(
                    allPoolInfo[i].pool
                ).ticks(allPoolInfo[i].ticks[j]);

                tickInfo[j] = Info({
                    liquidityGross: tick.liquidityGross,
                    liquidityNet: tick.liquidityNet,
                    initialized: tick.initialized
                });
            }
            tickInfoReturn[i] = tickInfo;
        }

        // ensure abi encoding, not needed here but increase reusability for different return types
        // note: abi.encode add a first 32 bytes word with the address of the original data
        bytes memory abiEncodedData = abi.encode(tickInfoReturn);

        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            let dataLength := mload(abiEncodedData)
            return(dataStart, dataLength)
        }
    }
}

/// @title Pool state that can change (Agni)
interface IAgniPoolState {
    struct Info {
        uint128 liquidityGross;
        int128 liquidityNet;
        uint256 feeGrowthOutside0X128;
        uint256 feeGrowthOutside1X128;
        int56 tickCumulativeOutside;
        uint160 secondsPerLiquidityOutsideX128;
        uint32 secondsOutside;
        bool initialized;
    }

    function tickBitmap(int16 wordPosition) external view returns (uint256);
    function ticks(int24 tick) external view returns (Info memory);
}
