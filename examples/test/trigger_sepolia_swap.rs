/// 测试脚本：在 Mantle Sepolia 上手动触发交易来创建套利机会
///
/// 这个脚本用于测试套利监控和执行服务，通过在池子中执行交易来人为制造价格差异
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了：
///    - MANTLE_SEPOLIA_RPC_URL
///    - MANTLE_SEPOLIA_PRIVATE_KEY
///    - 测试账户需要有足够的 WMNT 和其他测试代币
///
/// 2. 运行脚本：
///    cargo run --example trigger_sepolia_swap
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Bytes, U160, U256};
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner, sol};
use eyre::Result;
use std::str::FromStr;
use tracing::{info, warn};

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
        function transfer(address to, uint256 amount) external returns (bool);
    }
}

sol! {
    #[sol(rpc)]
    interface IWMNT {
        function deposit() external payable;
        function withdraw(uint256 amount) external;
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
        target: "trigger",
        address = %from_address,
        "🚀 Starting swap trigger script on Mantle Sepolia"
    );

    // Mantle Sepolia 地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");
    let usdt = address!("EC8671327870A16B3CB857B7F477c13ba48fBD08");

    // 选择要交易的池子（从 poolLists_testnet.csv）
    // USDC-USDT pool with 100 bps fee
    let pool_address = address!("024013F09Dc065Ab88d4dFa81391d673975877f8");

    info!(
        target: "trigger",
        pool = %pool_address,
        "Selected pool: WMNT-USDT"
    );

    // 获取池子信息
    let pool = IAgniPool::new(pool_address, provider.clone());
    let token0 = pool.token0().call().await?.0;
    let token1 = pool.token1().call().await?.0;
    let slot0 = pool.slot0().call().await;

    info!(
        target: "trigger",
        token0 = %token0,
        token1 = %token1,
        sqrt_price = ?slot0.as_ref().map(|s| s.sqrtPriceX96),
        "Pool information"
    );

    // 检查余额
    let token0_contract = IERC20::new(token0.into(), provider.clone());
    let token1_contract = IERC20::new(token1.into(), provider.clone());

    let balance0 = token0_contract.balanceOf(from_address).call().await?;
    let balance1 = token1_contract.balanceOf(from_address).call().await?;

    info!(
        target: "trigger",
        token0_balance = %balance0,
        token1_balance = %balance1,
        "Current balances"
    );

    if balance0 == U256::ZERO && balance1 == U256::ZERO {
        warn!(
            target: "trigger",
            "⚠️  No token balance! You need to obtain test tokens first."
        );
        warn!(
            target: "trigger",
            "You can:"
        );
        warn!(target: "trigger", "1. Use a faucet to get WMNT");
        warn!(target: "trigger", "2. Swap WMNT for USDC/USDT on Agni Finance");
        return Ok(());
    }

    // 选择交易方向和金额
    let (swap_token_in, swap_token_out, swap_amount, zero_for_one) = if balance0 > U256::ZERO {
        // 有 token0，卖出 token0 买入 token1
        let amount = if balance0 > U256::from(10_000_000_000_000_000_000u64) {
            U256::from(5_000_000u64) // 5 USDC (假设 6 decimals)
        } else {
            balance0 / U256::from(2) // 卖一半
        };
        (token0, token1, amount, true)
    } else {
        // 有 token1，卖出 token1 买入 token0
        let amount = if balance1 > U256::from(10_000_000u64) {
            U256::from(5_000_000u64)
        } else {
            balance1 / U256::from(2)
        };
        (token1, token0, amount, false)
    };

    info!(
        target: "trigger",
        token_in = %swap_token_in,
        token_out = %swap_token_out,
        amount = %swap_amount,
        zero_for_one = zero_for_one,
        "Preparing swap"
    );

    // 授权池子使用代币（Agni V3 通过回调支付，需要授权）
    let token_in_contract = IERC20::new(swap_token_in.into(), provider.clone());

    info!(target: "trigger", "Approving pool to spend tokens...");
    let approve_tx = token_in_contract
        .approve(pool_address, swap_amount)
        .send()
        .await?;
    let approve_hash = approve_tx.watch().await?;
    info!(target: "trigger", tx = %approve_hash, "✅ Approval confirmed");

    // 执行交换
    info!(target: "trigger", "Executing swap to create arbitrage opportunity...");

    // sqrtPriceLimitX96: 使用极端值接受任何价格
    let sqrt_price_limit = if zero_for_one {
        U256::from(4295128739u64) // Min sqrt price
    } else {
        // Max sqrt price for uint160
        U256::from_str("1461446703485210103287273052203988822378723970342")?
    };

    let swap_call = pool.swap(
        from_address,
        zero_for_one,
        swap_amount.try_into()?,
        sqrt_price_limit.to::<U160>(),
        Bytes::new(),
    );

    let gas_limit = gas_limit_for_hops(1);
    info!(
        target: "trigger",
        gas_limit = gas_limit,
        hops = 1,
        "Using hop-based gas limit for swap"
    );

    let pending_tx = swap_call.gas(gas_limit).send().await?;

    info!(target: "trigger", "Transaction sent, waiting for confirmation...");
    let tx_hash = pending_tx.watch().await?;

    info!(
        target: "trigger",
        tx = %tx_hash,
        "✅ Swap executed successfully!"
    );

    // 检查新余额
    let new_balance0 = token0_contract.balanceOf(from_address).call().await?;
    let new_balance1 = token1_contract.balanceOf(from_address).call().await?;

    info!(
        target: "trigger",
        token0_balance = %new_balance0,
        token1_balance = %new_balance1,
        "New balances after swap"
    );

    // 检查池子新状态
    let new_slot0 = pool.slot0().call().await;
    info!(
        target: "trigger",
        sqrt_price = ?new_slot0.as_ref().map(|s| s.sqrtPriceX96),
        tick = ?new_slot0.as_ref().map(|s| s.tick),
        "Pool state after swap"
    );

    info!(
        target: "trigger",
        "🎯 Price changed! Arbitrage monitor should detect opportunity soon."
    );

    Ok(())
}
