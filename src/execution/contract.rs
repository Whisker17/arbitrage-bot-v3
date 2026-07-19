use alloy::primitives::Address;
use alloy::sol;

// IMoePair minimal interface for reserves and swapping metadata (Uni V2 style)
sol! {
    #[sol(rpc)]
    interface IMoePair {
        function getReserves() external view returns (uint112, uint112, uint32);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function swap(
            uint amount0Out,
            uint amount1Out,
            address to,
            bytes calldata data
        ) external;
    }
}

// IMoeLBPair interface for Moe Liquidity Book pairs
sol! {
    #[sol(rpc)]
    interface IMoeLBPair {
        function getTokenX() external view returns (address);
        function getTokenY() external view returns (address);
        function getBinStep() external view returns (uint16);
        function getReserves() external view returns (uint128 reserveX, uint128 reserveY);
        function getActiveId() external view returns (uint24);
        function getSwapOut(uint128 amountIn, bool swapForY)
            external
            view
            returns (uint128 amountInLeft, uint128 amountOut, uint128 fee);
        function swap(bool swapForY, address to) external returns (bytes32 amountsOut);
        function getPriceFromId(uint24 id) external view returns (uint256 price);
        function getBin(uint24 id) external view returns (uint128 binReserveX, uint128 binReserveY);
    }
}

// IWMNT interface for wrapped native token
sol! {
    #[sol(rpc)]
    interface IWMNT {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

// ILBRouter interface for Moe Liquidity Book Router
sol! {
    #[sol(rpc)]
    interface ILBRouter {
        #[derive(Debug)]
        struct Path {
            uint256[] pairBinSteps;
            uint8[] versions;  // Version enum: V1=0, V2=1, V2_1=2
            address[] tokenPath;
        }

        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Path memory path,
            address to,
            uint256 deadline
        ) external returns (uint256 amountOut);

        function getWNATIVE() external view returns (address);
    }
}

// IAgniPool interface for Uniswap V3 style pools
sol! {
    #[sol(rpc)]
    interface IAgniPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function swap(
            address recipient,
            bool zeroForOne,
            int256 amountSpecified,
            uint160 sqrtPriceLimitX96,
            bytes calldata data
        ) external returns (int256 amount0, int256 amount1);
    }
}

// Minimal ERC20 interface
sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }
}

// Agni SwapRouter interface (Uniswap V3 style)
sol! {
    #[sol(rpc)]
    interface IAgniSwapRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            uint24 fee;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }

        function exactInputSingle(ExactInputSingleParams calldata params) external payable returns (uint256 amountOut);
    }
}

// Hardened ArbitrageExecutor ABI (WHI-501) — contracts/executor/ArbitrageExecutor.sol
sol! {
    #[sol(rpc)]
    interface IArbitrageExecutor {
        function executeArbitrage(
            uint256 amountIn,
            address[] calldata path,
            address[] calldata pools,
            uint8[] calldata poolTypes,
            uint256[] calldata amountsOut,
            uint256 minProfit,
            uint256 deadline
        ) external;

        function withdraw(address token) external;
        function withdrawAmount(address token, uint256 amount) external;
        function withdrawNative(uint256 amount) external;
        function withdrawAllNative() external;
        function admin() external view returns (address);
        function WMNT() external view returns (address);
        function paused() external view returns (bool);
        function isHotExecutor(address account) external view returns (bool);
        function setHotExecutor(address executor, bool allowed) external;
        function registerPool(address pool, uint8 poolType) external;
        function pause() external;
        function unpause() external;
    }
}

/// Validate settlement cycle and hop count before encoding calldata.
pub fn validate_execute_path(wmnt: Address, path: &[Address], pools: &[Address]) -> Result<(), String> {
    if pools.is_empty() {
        return Err("empty pools".into());
    }
    if path.len() != pools.len() + 1 {
        return Err("path length must be pools+1".into());
    }
    if path[0] != wmnt || path[path.len() - 1] != wmnt {
        return Err("path must start and end with WMNT".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn rejects_non_wmnt_cycle() {
        let wmnt = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let pool = address!("0x0000000000000000000000000000000000000003");
        assert!(validate_execute_path(wmnt, &[other, wmnt], &[pool]).is_err());
        assert!(validate_execute_path(wmnt, &[wmnt, other], &[pool]).is_err());
        assert!(validate_execute_path(wmnt, &[wmnt, other, wmnt], &[pool]).is_ok());
        assert!(validate_execute_path(wmnt, &[wmnt, other, wmnt], &[]).is_err());
    }
}

pub struct ContractsConfig {
    pub executor_address: Address,
    pub wmnt_address: Address,
}
