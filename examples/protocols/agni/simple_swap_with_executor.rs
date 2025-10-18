/// 使用执行器的简单交换测试脚本
/// 演示如何使用 SwapExecutor 执行单个池子的交换
use alloy::network::EthereumWallet;
use alloy::primitives::{address, U256};
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner};
use amms::execution::{ExecutorConfig, SwapExecutor, IERC20};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info};

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
        "🚀 Starting simple swap test with executor on Mantle Sepolia"
    );

    // 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 池子地址 - WMNT/USDC (V3)
    let pool_address = address!("37d045fF40d7f9c2E4C66c3F31551FF22Eb6C267");

    // 测试金额：0.1 WMNT
    let swap_amount = U256::from(1_000_000_000_000_000_000u64); // 0.1 WMNT

    info!(
        target: "simple_swap",
        pool = %pool_address,
        amount = %swap_amount,
        "Testing swap: {} WMNT -> USDC",
        format_ether(swap_amount)
    );

    // 检查余额
    let wmnt_contract = IERC20::new(wmnt, &provider);
    let usdc_contract = IERC20::new(usdc, &provider);

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

    // 构建交换步骤
    let swap_step =
        SwapExecutor::build_swap_step(&provider, pool_address, wmnt, usdc, swap_amount).await?;

    info!(
        target: "simple_swap",
        pool_type = ?swap_step.pool_type,
        "Built swap step"
    );

    // 执行交换
    let mut config = ExecutorConfig::default();
    config.v3_router_address = Some(address!("e38cfa32cCd918d94E2e20230dFaD1A4Fd8aEF16"));

    match SwapExecutor::execute_swap(&provider, &swap_step, from_address, &config).await {
        Ok(amount_out) => {
            info!(
                target: "simple_swap",
                amount_out = %amount_out,
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

            let usdc_change = if final_usdc > initial_usdc {
                final_usdc - initial_usdc
            } else {
                initial_usdc - final_usdc
            };

            info!(
                target: "simple_swap",
                final_wmnt = %final_wmnt,
                final_usdc = %final_usdc,
                wmnt_change = %wmnt_change,
                usdc_change = %usdc_change,
                "Final results - WMNT: {} (change: {}), USDC: {} (change: {})",
                format_ether(final_wmnt),
                format_ether(wmnt_change),
                format_ether(final_usdc),
                format_ether(usdc_change)
            );
        }
        Err(e) => {
            error!(
                target: "simple_swap",
                error = ?e,
                "❌ Swap failed"
            );
        }
    }

    Ok(())
}

fn format_ether(wei: U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}
