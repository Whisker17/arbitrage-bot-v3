/// 检查池子信息的脚本
use alloy::network::EthereumWallet;
use alloy::primitives::address;
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner, sol};
use eyre::Result;
use std::str::FromStr;
use tracing::info;

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
    }
}

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
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
        target: "pool_info",
        address = %from_address,
        "🔍 Checking pool information on Mantle Sepolia"
    );

    // 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdt = address!("cC4Ac915857532ADa58D69493554C6d869932Fe6");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 池子地址
    let pools = vec![
        (
            "WMNT/USDT",
            address!("eade7A0307466817a0635ebFD5b781cDf526EC4F"),
        ),
        (
            "USDT/USDC",
            address!("b525072a02668781b1e49f9d101e856B2a3791CE"),
        ),
        (
            "USDC/WMNT",
            address!("Ce8B3Bd008A7fFD1E53756a40ee81AC914a3831D"),
        ),
    ];

    for (name, pool_address) in pools {
        info!(
            target: "pool_info",
            "=========================================="
        );
        info!(
            target: "pool_info",
            pool_name = name,
            pool_address = %pool_address,
            "Checking pool: {}",
            name
        );

        let pool = IAgniPool::new(pool_address.into(), provider.clone());

        // 获取基本信息
        let token0 = pool.token0().call().await?.0;
        let token1 = pool.token1().call().await?.0;
        let fee = pool.fee().call().await?;
        let liquidity = pool.liquidity().call().await?;

        info!(
            target: "pool_info",
            token0 = %token0,
            token1 = %token1,
            fee = %fee,
            liquidity = %liquidity,
            "Basic pool info"
        );

        // 获取价格信息
        let slot0 = pool.slot0().call().await?;
        info!(
            target: "pool_info",
            sqrt_price = %slot0.sqrtPriceX96,
            tick = %slot0.tick,
            unlocked = %slot0.unlocked,
            "Price info"
        );

        // 检查代币余额
        let token0_contract = IERC20::new(token0.into(), provider.clone());
        let token1_contract = IERC20::new(token1.into(), provider.clone());

        let token0_balance = token0_contract
            .balanceOf(pool_address.into())
            .call()
            .await?;
        let token1_balance = token1_contract
            .balanceOf(pool_address.into())
            .call()
            .await?;

        info!(
            target: "pool_info",
            token0_balance = %token0_balance,
            token1_balance = %token1_balance,
            "Pool token balances"
        );

        // 检查代币精度
        let token0_decimals = token0_contract.decimals().call().await?;
        let token1_decimals = token1_contract.decimals().call().await?;

        info!(
            target: "pool_info",
            token0_decimals = %token0_decimals,
            token1_decimals = %token1_decimals,
            "Token decimals"
        );

        // 计算价格
        if slot0.sqrtPriceX96 > alloy::primitives::Uint::from(0u64) {
            let price = calculate_price_from_sqrt_price(
                slot0.sqrtPriceX96,
                token0_decimals,
                token1_decimals,
            );
            info!(
                target: "pool_info",
                price = %price,
                "Calculated price (token1/token0)"
            );
        }

        info!(
            target: "pool_info",
            "=========================================="
        );
    }

    Ok(())
}

fn calculate_price_from_sqrt_price(
    sqrt_price: alloy::primitives::Uint<160, 3>,
    decimals0: u8,
    decimals1: u8,
) -> f64 {
    // 使用字符串转换避免溢出
    let sqrt_price_str = format!("{}", sqrt_price);
    let sqrt_price_f64 = sqrt_price_str.parse::<f64>().unwrap_or(0.0);

    let price = (sqrt_price_f64 / (2.0_f64.powi(96))).powi(2);

    // 调整精度差异
    let decimals_diff = (decimals1 as i32) - (decimals0 as i32);
    let adjusted_price = price * 10.0_f64.powi(-decimals_diff);

    adjusted_price
}
