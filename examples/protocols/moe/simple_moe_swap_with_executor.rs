/// 使用执行器的 Moe LBPair 交换测试脚本
/// 演示如何使用 SwapExecutor 执行 Moe LBPair 的交换
use alloy::network::EthereumWallet;
use alloy::primitives::{address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;
use amms::execution::{ExecutorConfig, SwapExecutor, IERC20, IWMNT};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 加载环境变量
    dotenv::dotenv().ok();

    // 使用 Mantle 主网
    let rpc_url = std::env::var("MANTLE_HTTP_URL")
        .or_else(|_| std::env::var("RPC_HTTP_URL"))
        .unwrap_or_else(|_| "https://rpc.mantle.xyz".to_string());

    let private_key = std::env::var("MANTLE_MAINNET_PRIVATE_KEY")
        .or_else(|_| std::env::var("PRIVATE_KEY"))
        .expect("MANTLE_MAINNET_PRIVATE_KEY or PRIVATE_KEY must be set");

    // 创建带签名的 provider
    let signer = PrivateKeySigner::from_str(&private_key)?;
    let wallet = EthereumWallet::from(signer.clone());
    let from_address = signer.address();

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    info!(
        target: "moe_swap",
        address = %from_address,
        "🚀 Starting Moe LBPair swap test with executor on Mantle Mainnet"
    );

    // 代币地址（Mantle 主网）
    let wmnt = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
    let usdt = address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE");

    // Moe LBPair 池子地址 - WMNT/USDT (bin step 15)
    let pool_address = address!("f6C9020c9E915808481757779EDB53DACEaE2415");

    // 测试金额：0.1 WMNT
    let swap_amount = U256::from(100_000_000_000_000_000u64); // 0.1 WMNT

    info!(
        target: "moe_swap",
        pool = %pool_address,
        amount = %swap_amount,
        "Testing Moe swap: {} WMNT -> USDT",
        format_ether(swap_amount)
    );

    // 创建合约实例
    let wmnt_contract = IWMNT::new(wmnt, &provider);
    let usdt_contract = IERC20::new(usdt, &provider);

    // 检查初始余额
    let initial_mnt = provider.get_balance(from_address).await?;
    let initial_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    let initial_usdt = usdt_contract.balanceOf(from_address).call().await?;

    info!(
        target: "moe_swap",
        initial_mnt = %initial_mnt,
        initial_wmnt = %initial_wmnt,
        initial_usdt = %initial_usdt,
        "Initial balances - MNT: {}, WMNT: {}, USDT: {}",
        format_ether(initial_mnt),
        format_ether(initial_wmnt),
        format_usdt(initial_usdt)
    );

    // Step 1: 如果 WMNT 余额不足，先 deposit MNT 到 WMNT
    if initial_wmnt < swap_amount {
        let deposit_amount = swap_amount - initial_wmnt + U256::from(10_000_000_000_000_000u64); // 额外存一点

        if initial_mnt < deposit_amount {
            error!(
                target: "moe_swap",
                required = %deposit_amount,
                available = %initial_mnt,
                "❌ Insufficient MNT balance!"
            );
            return Ok(());
        }

        info!(
            target: "moe_swap",
            amount = %deposit_amount,
            "Depositing MNT to WMNT: {}",
            format_ether(deposit_amount)
        );

        let deposit_call = wmnt_contract.deposit();
        let deposit_tx = deposit_call.value(deposit_amount).send().await?;
        let deposit_hash = deposit_tx.watch().await?;

        let new_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;

        info!(
            target: "moe_swap",
            tx = %deposit_hash,
            new_balance = %new_wmnt_balance,
            "✅ Deposit completed. New WMNT balance: {}",
            format_ether(new_wmnt_balance)
        );
    }

    // 再次检查 WMNT 余额
    let current_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
    if current_wmnt < swap_amount {
        error!(
            target: "moe_swap",
            required = %swap_amount,
            available = %current_wmnt,
            "❌ Still insufficient WMNT balance after deposit!"
        );
        return Ok(());
    }

    // Step 2: 构建交换步骤（自动检测池子类型和交换方向）
    info!(
        target: "moe_swap",
        "Building swap step for Moe LBPair"
    );

    let swap_step =
        SwapExecutor::build_swap_step(&provider, pool_address, wmnt, usdt, swap_amount).await?;

    info!(
        target: "moe_swap",
        pool_type = ?swap_step.pool_type,
        swap_for_y = ?swap_step.swap_for_y,
        "Built swap step"
    );

    // Step 3: 执行交换
    let mut config = ExecutorConfig::default();
    // 设置 Moe Router 地址（Mantle 主网）
    config.moe_router_address = Some(address!("013e138EF6008ae5FDFDE29700e3f2Bc61d21E3a"));

    match SwapExecutor::execute_swap(&provider, &swap_step, from_address, &config).await {
        Ok(amount_out) => {
            info!(
                target: "moe_swap",
                amount_out = %amount_out,
                "✅ Swap completed successfully!"
            );

            // 检查最终余额
            let final_wmnt = wmnt_contract.balanceOf(from_address).call().await?;
            let final_usdt = usdt_contract.balanceOf(from_address).call().await?;

            let wmnt_spent = if initial_wmnt > final_wmnt {
                initial_wmnt - final_wmnt
            } else {
                U256::ZERO
            };

            let usdt_received = if final_usdt > initial_usdt {
                final_usdt - initial_usdt
            } else {
                U256::ZERO
            };

            info!(
                target: "moe_swap",
                final_wmnt = %final_wmnt,
                final_usdt = %final_usdt,
                wmnt_spent = %wmnt_spent,
                usdt_received = %usdt_received,
                "Final results - WMNT: {} (spent: {}), USDT: {} (received: {})",
                format_ether(final_wmnt),
                format_ether(wmnt_spent),
                format_usdt(final_usdt),
                format_usdt(usdt_received)
            );

            if usdt_received > U256::ZERO {
                info!(
                    target: "moe_swap",
                    "📊 Trade summary: {} WMNT -> {} USDT",
                    format_ether(wmnt_spent),
                    format_usdt(usdt_received)
                );
            }
        }
        Err(e) => {
            error!(
                target: "moe_swap",
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

fn format_usdt(raw: U256) -> String {
    // USDT on Mantle has 6 decimals
    let usdt = raw.to::<u128>() as f64 / 1e6;
    format!("{:.6}", usdt)
}
