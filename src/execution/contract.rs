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
        function swap(bool swapForY, address to) external returns (bytes32 amountsOut);
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

// OptimizedArbitrageExecutor interface matching contracts/ArbitrageExecutor.sol
sol! {
    #[sol(rpc)]
    interface IArbitrageExecutor {
        function executeArbitrage(
            uint256 _amountIn,
            address[] calldata _path,
            address[] calldata _pools,
            uint8[] calldata _poolTypes,
            uint256[] calldata _expectedStates,
            uint256[] calldata _amountsOut
        ) external;

        function withdraw(address _token) external;
        function withdrawAmount(address _token, uint256 _amount) external;
        function owner() external view returns (address);
    }
}

pub struct ContractsConfig {
    pub executor_address: Address,
    pub wmnt_address: Address,
}
