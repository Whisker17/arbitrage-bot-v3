// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract GetMoeLBPairSlot0BatchRequest {
    struct Slot0Data {
        address tokenX;
        address tokenY;
        uint24 activeId;
        uint16 binStep;
        uint128 reserveX;
        uint128 reserveY;
        uint16 baseFactor;
        uint16 filterPeriod;
        uint16 decayPeriod;
        uint16 reductionFactor;
        uint24 variableFeeControl;
        uint16 protocolShare;
        uint24 maxVolatilityAccumulator;
        uint24 volatilityAccumulator;
        uint24 volatilityReference;
        uint24 idReference;
        uint32 timeOfLastUpdate;
    }

    constructor(address[] memory pairs) {
        Slot0Data[] memory allSlot0Data = new Slot0Data[](pairs.length);
        for (uint256 i = 0; i < pairs.length; ++i) {
            IMoeLBPair pair = IMoeLBPair(pairs[i]);
            Slot0Data memory data;
            if (pairs[i].code.length == 0) {
                // leave zeros
            } else {
                // Guard each external call to avoid bubbling up reverts
                try pair.getTokenX() returns (address tx) {
                    data.tokenX = tx;
                } catch {}
                try pair.getTokenY() returns (address ty) {
                    data.tokenY = ty;
                } catch {}
                try pair.getActiveId() returns (uint24 a) {
                    data.activeId = a;
                } catch {}
                try pair.getBinStep() returns (uint16 b) {
                    data.binStep = b;
                } catch {}
                try pair.getReserves() returns (uint128 rx, uint128 ry) {
                    data.reserveX = rx;
                    data.reserveY = ry;
                } catch {}
                try pair.getStaticFeeParameters() returns (
                    uint16 baseFactor,
                    uint16 filterPeriod,
                    uint16 decayPeriod,
                    uint16 reductionFactor,
                    uint24 variableFeeControl,
                    uint16 ps,
                    uint24 mva
                ) {
                    data.baseFactor = baseFactor;
                    data.filterPeriod = filterPeriod;
                    data.decayPeriod = decayPeriod;
                    data.reductionFactor = reductionFactor;
                    data.variableFeeControl = variableFeeControl;
                    data.protocolShare = ps;
                    data.maxVolatilityAccumulator = mva;
                } catch {}
                try pair.getVariableFeeParameters() returns (
                    uint24 volatilityAccumulator,
                    uint24 volatilityReference,
                    uint24 idReference,
                    uint40 timeOfLastUpdate
                ) {
                    data.volatilityAccumulator = volatilityAccumulator;
                    data.volatilityReference = volatilityReference;
                    data.idReference = idReference;
                    data.timeOfLastUpdate = uint32(timeOfLastUpdate);
                } catch {}
            }
            allSlot0Data[i] = data;
        }

        bytes memory encoded = abi.encode(allSlot0Data);
        assembly {
            let dataStart := add(encoded, 0x20)
            let dataLength := mload(encoded)
            return(dataStart, dataLength)
        }
    }
}

interface IMoeLBPair {
    function getTokenX() external view returns (address);

    function getTokenY() external view returns (address);

    function getReserves() external view returns (uint128 reserveX, uint128 reserveY);

    function getActiveId() external view returns (uint24);

    function getBinStep() external view returns (uint16);

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

    function getVariableFeeParameters()
        external
        view
        returns (uint24 volatilityAccumulator, uint24 volatilityReference, uint24 idReference, uint40 timeOfLastUpdate);
}
