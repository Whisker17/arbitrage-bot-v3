//SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

/**
 * @dev This contract is not meant to be deployed. Instead, use a static call with the
 *       deployment bytecode as payload.
 */
contract GetAgniPoolTickBitmapBatchRequest {
    struct TickBitmapInfo {
        address pool;
        int16 minWord;
        int16 maxWord;
    }

    constructor(TickBitmapInfo[] memory allPoolInfo) {
        uint256[][] memory allTickBitmaps = new uint256[][](allPoolInfo.length);

        for (uint256 i = 0; i < allPoolInfo.length; ++i) {
            TickBitmapInfo memory info = allPoolInfo[i];
            IAgniPoolState pool = IAgniPoolState(info.pool);

            uint256[] memory tickBitmaps = new uint256[](uint16(info.maxWord - info.minWord) + 1);

            uint256 wordIdx = 0;
            for (int16 j = info.minWord; j <= info.maxWord; ++j) {
                uint256 tickBitmap = pool.tickBitmap(j);

                if (tickBitmap == 0) {
                    continue;
                }

                tickBitmaps[wordIdx] = uint256(int256(j));
                ++wordIdx;

                tickBitmaps[wordIdx] = tickBitmap;
                ++wordIdx;
            }

            assembly {
                mstore(tickBitmaps, wordIdx)
            }

            allTickBitmaps[i] = tickBitmaps;
        }

        // ensure abi encoding, not needed here but increase reusability for different return types
        // note: abi.encode add a first 32 bytes word with the address of the original data
        bytes memory abiEncodedData = abi.encode(allTickBitmaps);

        assembly {
            // Return from the start of the data (discarding the original data address)
            // up to the end of the memory used
            let dataStart := add(abiEncodedData, 0x20)
            return(dataStart, sub(msize(), dataStart))
        }
    }
}

/// @title Pool state that can change (Agni)
interface IAgniPoolState {
    function tickBitmap(int16 wordPosition) external view returns (uint256);
    function tickSpacing() external view returns (int24);
}
