// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract GetMoeLBPairBinDataBatchRequest {
    struct BinDataRequest {
        address pair;
        uint24[] ids;
    }

    struct BinData {
        uint128 reserveX;
        uint128 reserveY;
    }

    constructor(BinDataRequest[] memory requests) {
        BinData[][] memory allBinData = new BinData[][](requests.length);
        for (uint256 i = 0; i < requests.length; ++i) {
            IMoeLBPair pair = IMoeLBPair(requests[i].pair);
            uint24[] memory ids = requests[i].ids;
            BinData[] memory data = new BinData[](ids.length);
            for (uint256 j = 0; j < ids.length; ++j) {
                (uint128 reserveX, uint128 reserveY) = pair.getBin(ids[j]);
                data[j] = BinData({reserveX: reserveX, reserveY: reserveY});
            }
            allBinData[i] = data;
        }

        bytes memory abiEncodedData = abi.encode(allBinData);
        assembly {
            let dataStart := add(abiEncodedData, 0x20)
            let dataLength := mload(abiEncodedData)
            return(dataStart, dataLength)
        }
    }
}

interface IMoeLBPair {
    function getBin(uint24 id) external view returns (uint128 reserveX, uint128 reserveY);
}
