//SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {IUniswapV2Pair} from "../interfaces/IUniswapV2.sol";
import {IUniswapV2Factory} from "../interfaces/IUniswapV2.sol";
import {IUniswapV3Pool} from "../interfaces/IUniswapV3.sol";
import {IUniswapV3Factory} from "../interfaces/IUniswapV3.sol";
import {IERC20} from "../interfaces/Token.sol";
import {FixedPointMath} from "../UniswapV3/FixedPoint.sol";

// Removed: WethValueInPools.sol deprecated in favor of WmntValueInPools.sol
