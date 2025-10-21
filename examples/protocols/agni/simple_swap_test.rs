/// 简单的单池交换测试脚本
/// 用于测试基本的交换功能
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, Bytes, U160, U256};
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner, sol};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info};

use amms::execution::gas_schedule::gas_limit_for_hops;

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
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function approve(address spender, uint256 amount) external returns (bool);
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 加载环境变量
    dotenv::dotenv().ok();

    let rpc_url =
        std::env::var("MANTLE_SEPOLIA_RPC_URL").expect("MANTLE_SEPOLIA_RPC_URL must be set");
    let private_key = std::env::var("MANTLE_SEPOLIA_PRIVATE_KEY")
        .expect("MANTLE_SEPOLIA_PRIVATE_KEY must be set");

    // 创建带签名的 provider
    let signer = PrivateKeySigner::from_str(&private_key)?;
    let wallet = EthereumWallet::from(signer.clone());
    let from_address = signer.address();

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    info!(
        target: "simple_swap",
        address = %from_address,
        "🚀 Starting simple swap test on Mantle Sepolia"
    );

    // 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 池子地址 - WMNT/USDT
    let pool_address = address!("37d045fF40d7f9c2E4C66c3F31551FF22Eb6C267");

    // 测试金额：0.1 WMNT
    let swap_amount = U256::from(100_000_000_000_000_000u64); // 0.1 WMNT

    info!(
        target: "simple_swap",
        pool = %pool_address,
        amount = %swap_amount,
        "Testing swap: {} WMNT -> USDC",
        format_ether(swap_amount)
    );

    // 检查余额
    let wmnt_contract = IERC20::new(wmnt.into(), provider.clone());
    let usdc_contract = IERC20::new(usdc.into(), provider.clone());

    let initial_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    let initial_usdc = usdc_contract.balanceOf(from_address).call().await?;

    info!(
        target: "simple_swap",
        initial_wmnt = %initial_wmnt,
        initial_usdc = %initial_usdc,
        "Initial balances - WMNT: {}, USDC: {}",
        format_ether(initial_wmnt),
        format_ether(initial_usdc)
    );

    if initial_wmnt < swap_amount {
        error!(
            target: "simple_swap",
            required = %swap_amount,
            available = %initial_wmnt,
            "❌ Insufficient WMNT balance! WMNT: {}, USDC: {}",
            format_ether(initial_wmnt),
            format_ether(initial_usdc)
        );
        return Ok(());
    }

    // 获取池子信息
    let pool = IAgniPool::new(pool_address.into(), provider.clone());
    let token0 = pool.token0().call().await?.0;
    let token1 = pool.token1().call().await?.0;

    info!(
        target: "simple_swap",
        token0 = %token0,
        token1 = %token1,
        "Pool tokens"
    );

    // 确定交易方向
    let zero_for_one = wmnt == Address::from(token0);

    info!(
        target: "simple_swap",
        zero_for_one = zero_for_one,
        "Swap direction: {} -> {}",
        if zero_for_one { "token0" } else { "token1" },
        if zero_for_one { "token1" } else { "token0" }
    );

    // 获取当前价格
    let slot0 = pool.slot0().call().await?;
    info!(
        target: "simple_swap",
        sqrt_price = %slot0.sqrtPriceX96,
        tick = %slot0.tick,
        "Current pool state"
    );

    // 授权代币
    let current_allowance = wmnt_contract
        .allowance(from_address, pool_address.into())
        .call()
        .await?;

    if current_allowance < swap_amount {
        info!(
            target: "simple_swap",
            current_allowance = %current_allowance,
            required = %swap_amount,
            "Approving WMNT for pool"
        );

        let approve_tx = wmnt_contract
            .approve(pool_address.into(), swap_amount)
            .send()
            .await?;
        let approve_hash = approve_tx.watch().await?;

        info!(
            target: "simple_swap",
            tx = %approve_hash,
            "✅ Approval confirmed"
        );
    }

    // 设置价格限制 - 使用更宽松的限制
    let sqrt_price_limit = if zero_for_one {
        // 允许价格下跌 50%
        let current_price = slot0.sqrtPriceX96;
        let min_price = current_price * U160::from(50u64) / U160::from(100u64);
        min_price.max(U160::from(4295128739u64)) // 不低于最小价格
    } else {
        // 允许价格上涨 50%
        let current_price = slot0.sqrtPriceX96;
        let max_price = current_price * U160::from(150u64) / U160::from(100u64);
        max_price.min(U160::from_str(
            "1461446703485210103287273052203988822378723970342",
        )?) // 不高于最大价格
    };

    info!(
        target: "simple_swap",
        sqrt_price_limit = %sqrt_price_limit,
        "Price limit set"
    );

    // 执行交换
    let swap_call = pool.swap(
        from_address,
        zero_for_one,
        swap_amount.try_into()?,
        sqrt_price_limit,
        Bytes::new(),
    );

    let gas_limit = gas_limit_for_hops(1);
    info!(
        target: "simple_swap",
        gas_limit = gas_limit,
        hops = 1,
        "Using hop-based gas limit for swap"
    );

    // 发送交易
    let pending_tx = swap_call.gas(gas_limit).send().await?;

    info!(
        target: "simple_swap",
        "Transaction sent, waiting for confirmation..."
    );

    let tx_hash = pending_tx.watch().await?;

    info!(
        target: "simple_swap",
        tx = %tx_hash,
        "✅ Swap completed successfully!"
    );

    // 检查最终余额
    let final_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    let final_usdc = usdc_contract.balanceOf(from_address).call().await?;

    let wmnt_change = if final_wmnt > initial_wmnt {
        final_wmnt - initial_wmnt
    } else {
        initial_wmnt - final_wmnt
    };

    let usdt_change = if final_usdc > initial_usdc {
        final_usdc - initial_usdc
    } else {
        initial_usdc - final_usdc
    };

    info!(
        target: "simple_swap",
        final_wmnt = %final_wmnt,
        final_usdc = %final_usdc,
        wmnt_change = %wmnt_change,
        usdt_change = %usdt_change,
        "Final results - WMNT: {} (change: {}), USDC: {} (change: {})",
        format_ether(final_wmnt),
        format_ether(wmnt_change),
        format_ether(final_usdc),
        format_ether(usdt_change)
    );

    Ok(())
}

fn format_ether(wei: U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}
