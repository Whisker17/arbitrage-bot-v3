/// 检查账户余额的简单脚本
use alloy::network::EthereumWallet;
use alloy::primitives::{address, U256};
use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use eyre::Result;
use std::str::FromStr;
use tracing::info;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    #[sol(rpc)]
    interface IWMNT {
        function deposit() external payable;
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
        target: "balance_check",
        address = %from_address,
        "Checking account balances"
    );

    // 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdt = address!("cC4Ac915857532ADa58D69493554C6d869932Fe6");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 检查 MNT 余额
    let mnt_balance = provider.get_balance(from_address).await?;
    info!(
        target: "balance_check",
        mnt_balance = %mnt_balance,
        "MNT balance: {} MNT",
        format_ether(mnt_balance)
    );

    // 检查 WMNT 余额
    let wmnt_contract = IERC20::new(wmnt.into(), provider.clone());
    let wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;
    info!(
        target: "balance_check",
        wmnt_balance = %wmnt_balance,
        "WMNT balance: {} WMNT",
        format_ether(wmnt_balance)
    );

    // 检查 USDT 余额
    let usdt_contract = IERC20::new(usdt.into(), provider.clone());
    let usdt_balance = usdt_contract.balanceOf(from_address).call().await?;
    info!(
        target: "balance_check",
        usdt_balance = %usdt_balance,
        "USDT balance: {} USDT",
        format_ether(usdt_balance)
    );

    // 检查 USDC 余额
    let usdc_contract = IERC20::new(usdc.into(), provider.clone());
    let usdc_balance = usdc_contract.balanceOf(from_address).call().await?;
    info!(
        target: "balance_check",
        usdc_balance = %usdc_balance,
        "USDC balance: {} USDC",
        format_ether(usdc_balance)
    );

    // 如果 WMNT 余额不足，尝试包装一些
    if wmnt_balance < U256::from(2_000_000_000_000_000_000u64)
        && mnt_balance > U256::from(5_000_000_000_000_000_000u64)
    {
        info!(
            target: "balance_check",
            "WMNT balance insufficient, attempting to wrap 5 MNT to WMNT"
        );

        let wmnt_wrapper = IWMNT::new(wmnt.into(), provider.clone());
        let deposit_tx = wmnt_wrapper
            .deposit()
            .value(U256::from(5_000_000_000_000_000_000u64))
            .send()
            .await?;

        let tx_hash = deposit_tx.watch().await?;
        info!(
            target: "balance_check",
            tx = %tx_hash,
            "✅ Wrapped 5 MNT to WMNT"
        );

        // 重新检查 WMNT 余额
        let new_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;
        info!(
            target: "balance_check",
            new_wmnt_balance = %new_wmnt_balance,
            "New WMNT balance: {} WMNT",
            format_ether(new_wmnt_balance)
        );
    }

    Ok(())
}

fn format_ether(wei: alloy::primitives::U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}
