/// 在 Mantle 主网上包装 MNT 为 WMNT，然后在 Agni V3 池中将 WMNT 兑换为 USDC
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了：
///    - MANTLE_RPC_URL
///    - PRIVATE_KEY
///    - 账户需要有足够的 MNT
///
/// 2. 运行脚本：
///    cargo run --example wrap_and_swap_wmnt_to_usdc
use alloy::network::EthereumWallet;
use alloy::primitives::{address, U160, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info};

sol! {
    #[sol(rpc)]
    interface IAgniPool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function fee() external view returns (uint24);
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint32 feeProtocol,
            bool unlocked
        );
        function swap(
            address recipient,
            bool zeroForOne,
            int256 amountSpecified,
            uint160 sqrtPriceLimitX96,
            bytes calldata data
        ) external returns (int256 amount0, int256 amount1);
    }
}

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
        
        function exactInputSingle(ExactInputSingleParams calldata params) 
            external payable returns (uint256 amountOut);
    }
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function transfer(address to, uint256 amount) external returns (bool);
        function decimals() external view returns (uint8);
    }
}

sol! {
    #[sol(rpc)]
    interface IWMNT {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 加载环境变量
    dotenv::dotenv().ok();

    let rpc_url =
        std::env::var("RPC_URL").expect("MANTLE_RPC_URL must be set");
    
    let private_key = std::env::var("PRIVATE_KEY")
        .expect("PRIVATE_KEY must be set");

    // 创建带签名的 provider
    let signer = PrivateKeySigner::from_str(&private_key)?;
    let wallet = EthereumWallet::from(signer.clone());
    let from_address = signer.address();

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    info!(
        target: "wrap_and_swap",
        address = %from_address,
        "🚀 Starting wrap and swap script on Mantle Mainnet"
    );

    // Mantle 主网地址
    let wmnt = address!("78c1b0C915C4FAA5FFFa6CAbf0219DA63d7f4cb8");
    let usdc = address!("09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9");
    
    // Agni V3 池子地址：WMNT-USDC (主网)
    // 需要您提供正确的池子地址
    let pool_address = address!("8E2C009E45420D2B36bC15315F9de8CeCa2cc724");
    
    // Agni SwapRouter 地址 (主网)
    let router_address = address!("319B69888b0d11cEC22caA5034e25FfFBDc88421");

    // 要包装的 MNT 数量：1 MNT
    let wrap_amount = U256::from(1_000_000_000_000_000_000u64); // 1 MNT = 1e18 wei

    info!(
        target: "wrap_and_swap",
        "Step 1: Checking initial balances"
    );

    // 检查 MNT 余额
    let mnt_balance = provider.get_balance(from_address).await?;
    info!(
        target: "wrap_and_swap",
        mnt_balance = %mnt_balance,
        "MNT balance: {} MNT",
        format_ether(mnt_balance)
    );

    if mnt_balance < wrap_amount {
        error!(
            target: "wrap_and_swap",
            required = %wrap_amount,
            available = %mnt_balance,
            "❌ Insufficient MNT balance! Need at least 1 MNT"
        );
        return Ok(());
    }

    // 检查初始 WMNT 和 USDC 余额
    let wmnt_contract = IWMNT::new(wmnt.into(), provider.clone());
    let usdc_contract = IERC20::new(usdc.into(), provider.clone());

    let initial_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    let initial_usdc = usdc_contract.balanceOf(from_address).call().await?;

    info!(
        target: "wrap_and_swap",
        initial_wmnt = %initial_wmnt,
        initial_usdc = %initial_usdc,
        "Initial balances - WMNT: {} WMNT, USDC: {} USDC",
        format_ether(initial_wmnt),
        format_ether(initial_usdc)
    );

    // ========================================
    // Step 1: 包装 MNT 为 WMNT
    // ========================================
    info!(
        target: "wrap_and_swap",
        "Step 2: Wrapping {} MNT to WMNT...",
        format_ether(wrap_amount)
    );

    let deposit_tx = wmnt_contract
        .deposit()
        .value(wrap_amount)
        .send()
        .await?;

    let deposit_hash = deposit_tx.watch().await?;
    info!(
        target: "wrap_and_swap",
        tx = %deposit_hash,
        "✅ Wrapped 1 MNT to WMNT"
    );

    // 检查包装后的 WMNT 余额
    let wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;
    info!(
        target: "wrap_and_swap",
        wmnt_balance = %wmnt_balance,
        "WMNT balance after wrapping: {} WMNT",
        format_ether(wmnt_balance)
    );

    // ========================================
    // Step 2: 获取池子信息
    // ========================================
    info!(
        target: "wrap_and_swap",
        "Step 3: Fetching pool information..."
    );

    let pool = IAgniPool::new(pool_address, provider.clone());
    let token0 = pool.token0().call().await?.0;
    let token1 = pool.token1().call().await?.0;
    let slot0 = pool.slot0().call().await?;
    
    info!(
        target: "wrap_and_swap",
        pool = %pool_address,
        token0 = %token0,
        token1 = %token1,
        sqrt_price = %slot0.sqrtPriceX96,
        "Pool information"
    );

    // 确定交易方向
    let zero_for_one = wmnt.0 == token0.0;
    let token_out = if zero_for_one { token1 } else { token0 };

    if token_out.0 != usdc.0 {
        error!(
            target: "wrap_and_swap",
            expected = %usdc,
            actual = %token_out,
            "❌ Pool does not contain USDC as expected!"
        );
        return Ok(());
    }

    info!(
        target: "wrap_and_swap",
        zero_for_one = zero_for_one,
        token_in = %wmnt,
        token_out = %usdc,
        "Swap direction determined"
    );

    // 获取池子的 fee
    let fee = pool.fee().call().await?;
    
    info!(
        target: "wrap_and_swap",
        fee = %fee,
        "Pool fee: {} bps",
        fee
    );

    // ========================================
    // Step 3: 授权 Router 使用 WMNT
    // ========================================
    info!(
        target: "wrap_and_swap",
        "Step 4: Approving Router to spend WMNT..."
    );

    let approve_tx = wmnt_contract
        .approve(router_address, wrap_amount)
        .send()
        .await?;
    let approve_hash = approve_tx.watch().await?;
    info!(
        target: "wrap_and_swap",
        tx = %approve_hash,
        "✅ Approval confirmed"
    );

    // ========================================
    // Step 4: 通过 Router 执行交换
    // ========================================
    info!(
        target: "wrap_and_swap",
        "Step 5: Swapping {} WMNT to USDC via Router...",
        format_ether(wrap_amount)
    );

    let router = IAgniSwapRouter::new(router_address, provider.clone());
    
    let params = IAgniSwapRouter::ExactInputSingleParams {
        tokenIn: wmnt,
        tokenOut: usdc,
        fee,
        recipient: from_address,
        deadline: U256::MAX,
        amountIn: wrap_amount,
        amountOutMinimum: U256::ZERO,
        sqrtPriceLimitX96: U160::from(0u64),
    };

    let swap_call = router.exactInputSingle(params);

    // Router 调用需要更多 gas，使用 500M gas limit
    let gas_limit = 800_000_000u64;
    info!(
        target: "wrap_and_swap",
        gas_limit = gas_limit,
        "Using increased gas limit for Router swap"
    );

    let pending_tx = swap_call.gas(gas_limit).send().await?;

    info!(
        target: "wrap_and_swap",
        "Transaction sent, waiting for confirmation..."
    );
    let tx_hash = pending_tx.watch().await?;

    info!(
        target: "wrap_and_swap",
        tx = %tx_hash,
        "✅ Swap executed successfully!"
    );

    // ========================================
    // Step 5: 检查最终余额
    // ========================================
    info!(
        target: "wrap_and_swap",
        "Step 6: Checking final balances..."
    );

    let final_mnt = provider.get_balance(from_address).await?;
    let final_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    let final_usdc = usdc_contract.balanceOf(from_address).call().await?;

    info!(
        target: "wrap_and_swap",
        final_mnt = %final_mnt,
        final_wmnt = %final_wmnt,
        final_usdc = %final_usdc,
        "Final balances - MNT: {} MNT, WMNT: {} WMNT, USDC: {} USDC",
        format_ether(final_mnt),
        format_ether(final_wmnt),
        format_ether(final_usdc)
    );

    // 计算变化
    let mnt_used = mnt_balance.saturating_sub(final_mnt);
    let wmnt_change = final_wmnt.saturating_sub(initial_wmnt);
    let usdc_received = final_usdc.saturating_sub(initial_usdc);

    info!(
        target: "wrap_and_swap",
        "📊 Summary:"
    );
    info!(
        target: "wrap_and_swap",
        mnt_used = %mnt_used,
        "  - MNT used: {} MNT",
        format_ether(mnt_used)
    );
    info!(
        target: "wrap_and_swap",
        wmnt_change = %wmnt_change,
        "  - WMNT net change: {} WMNT",
        format_ether(wmnt_change)
    );
    info!(
        target: "wrap_and_swap",
        usdc_received = %usdc_received,
        "  - USDC received: {} USDC",
        format_ether(usdc_received)
    );

    // 检查池子新状态
    let new_slot0 = pool.slot0().call().await?;
    info!(
        target: "wrap_and_swap",
        sqrt_price = %new_slot0.sqrtPriceX96,
        "Pool state after swap"
    );

    info!(
        target: "wrap_and_swap",
        "🎉 Wrap and swap completed successfully!"
    );

    Ok(())
}

fn format_ether(wei: alloy::primitives::U256) -> String {
    let eth = wei.to_string().parse::<f64>().unwrap_or(0.0) / 1e18;
    format!("{:.6}", eth)
}

