/// 多池连环交易脚本
///
/// 实现 WMNT->USDT->USDC->WMNT 的连环交易
/// 池子地址：
/// - WMNT->USDT: 0xeade7A0307466817a0635ebFD5b781cDf526EC4F
/// - USDT->USDC: 0xb525072a02668781b1e49f9d101e856B2a3791CE  
/// - USDC->WMNT: 0xCe8B3Bd008A7fFD1E53756a40ee81AC914a3831D
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了：
///    - MANTLE_SEPOLIA_RPC_URL
///    - MANTLE_SEPOLIA_PRIVATE_KEY
///    - 测试账户需要有足够的 WMNT 和其他代币
///
/// 2. 运行脚本：
///    cargo run --example multi_pool_swap
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, Bytes, U160, U256};
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner, sol};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info, warn};

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
        function allowance(address owner, address spender) external view returns (uint256);
    }
}

#[derive(Debug, Clone)]
struct SwapStep {
    pool_address: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    expected_amount_out: Option<U256>,
}

#[derive(Debug)]
struct SwapResult {
    step: usize,
    pool_address: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    amount_out: U256,
    tx_hash: alloy::primitives::TxHash,
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
        target: "multi_swap",
        address = %from_address,
        "🚀 Starting multi-pool swap on Mantle Sepolia"
    );

    // Mantle Sepolia 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdt = address!("cC4Ac915857532ADa58D69493554C6d869932Fe6");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 池子地址
    let pool_wmnt_usdt = address!("eade7A0307466817a0635ebFD5b781cDf526EC4F");
    let pool_usdt_usdc = address!("b525072a02668781b1e49f9d101e856B2a3791CE");
    let pool_usdc_wmnt = address!("Ce8B3Bd008A7fFD1E53756a40ee81AC914a3831D");

    // 输入金额：2 WMNT
    let input_amount = U256::from(100_000_000_000_000_000u64); // 2 WMNT (18 decimals)

    info!(
        target: "multi_swap",
        input_amount = %input_amount,
        "Input amount: 2 WMNT"
    );

    // 构建交易步骤
    let swap_steps = vec![
        SwapStep {
            pool_address: pool_wmnt_usdt,
            token_in: wmnt,
            token_out: usdt,
            amount_in: input_amount,
            expected_amount_out: None,
        },
        SwapStep {
            pool_address: pool_usdt_usdc,
            token_in: usdt,
            token_out: usdc,
            amount_in: U256::ZERO, // 将在执行时更新
            expected_amount_out: None,
        },
        SwapStep {
            pool_address: pool_usdc_wmnt,
            token_in: usdc,
            token_out: wmnt,
            amount_in: U256::ZERO, // 将在执行时更新
            expected_amount_out: None,
        },
    ];

    // 检查初始余额
    let wmnt_contract = IERC20::new(wmnt.into(), provider.clone());
    let initial_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;

    info!(
        target: "multi_swap",
        initial_balance = %initial_wmnt_balance,
        "Initial WMNT balance"
    );

    if initial_wmnt_balance < input_amount {
        error!(
            target: "multi_swap",
            required = %input_amount,
            available = %initial_wmnt_balance,
            "❌ Insufficient WMNT balance!"
        );
        return Ok(());
    }

    // 执行连环交易
    let mut current_amount = input_amount;
    let mut results = Vec::new();

    for (step_index, mut swap_step) in swap_steps.into_iter().enumerate() {
        info!(
            target: "multi_swap",
            step = step_index + 1,
            pool = %swap_step.pool_address,
            token_in = %swap_step.token_in,
            token_out = %swap_step.token_out,
            amount_in = %current_amount,
            "Executing swap step {}",
            step_index + 1
        );

        // 更新当前步骤的输入金额
        swap_step.amount_in = current_amount;

        // 执行单个交换
        match execute_single_swap(&swap_step, from_address, &provider).await {
            Ok(result) => {
                info!(
                    target: "multi_swap",
                    step = step_index + 1,
                    tx_hash = %result.tx_hash,
                    amount_out = %result.amount_out,
                    "✅ Swap step {} completed successfully",
                    step_index + 1
                );

                current_amount = result.amount_out;
                results.push(result);
            }
            Err(e) => {
                error!(
                    target: "multi_swap",
                    step = step_index + 1,
                    error = ?e,
                    "❌ Swap step {} failed",
                    step_index + 1
                );
                return Err(e);
            }
        }
    }

    // 检查最终余额
    let final_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;
    let profit = if final_wmnt_balance > initial_wmnt_balance {
        final_wmnt_balance - initial_wmnt_balance
    } else {
        U256::ZERO
    };

    info!(
        target: "multi_swap",
        "🎉 Multi-pool swap completed!"
    );
    info!(
        target: "multi_swap",
        initial_balance = %initial_wmnt_balance,
        final_balance = %final_wmnt_balance,
        profit = %profit,
        "Final results"
    );

    if profit > U256::ZERO {
        info!(
            target: "multi_swap",
            "💰 Profit detected: {} WMNT",
            format_ether(profit)
        );
    } else {
        warn!(
            target: "multi_swap",
            "⚠️  No profit or loss detected"
        );
    }

    Ok(())
}

async fn execute_single_swap<P: alloy::providers::Provider + Clone>(
    swap_step: &SwapStep,
    from_address: alloy::primitives::Address,
    provider: &P,
) -> Result<SwapResult> {
    let pool = IAgniPool::new(swap_step.pool_address.into(), provider.clone());

    // 获取池子信息
    let token0 = pool.token0().call().await?.0;
    let token1 = pool.token1().call().await?.0;

    // 确定交易方向
    let zero_for_one = swap_step.token_in == Address::from(token0);

    info!(
        target: "single_swap",
        pool = %swap_step.pool_address,
        token0 = %token0,
        token1 = %token1,
        token_in = %swap_step.token_in,
        zero_for_one = zero_for_one,
        "Pool information"
    );

    // 授权代币（如果需要）
    let token_in_contract = IERC20::new(swap_step.token_in.into(), provider.clone());

    // 检查当前授权额度
    let current_allowance = token_in_contract
        .allowance(from_address, swap_step.pool_address.into())
        .call()
        .await?;

    if current_allowance < swap_step.amount_in {
        info!(
            target: "single_swap",
            current_allowance = %current_allowance,
            required = %swap_step.amount_in,
            "Approving tokens for pool"
        );

        let approve_tx = token_in_contract
            .approve(swap_step.pool_address.into(), swap_step.amount_in)
            .send()
            .await?;
        let approve_hash = approve_tx.watch().await?;

        info!(
            target: "single_swap",
            tx = %approve_hash,
            "✅ Approval confirmed"
        );
    }

    // 设置价格限制 - 使用更宽松的限制
    let sqrt_price_limit = if zero_for_one {
        U160::from(4295128739u64) // Min sqrt price
    } else {
        U160::from_str("1461446703485210103287273052203988822378723970342")? // Max sqrt price
    };

    // 执行交换
    let swap_call = pool.swap(
        from_address,
        zero_for_one,
        swap_step.amount_in.try_into()?,
        sqrt_price_limit,
        Bytes::new(),
    );

    let gas_limit = gas_limit_for_hops(1);
    info!(
        target: "single_swap",
        gas_limit = gas_limit,
        hops = 1,
        "Using hop-based gas limit for swap"
    );

    // 发送交易
    let pending_tx = swap_call.gas(gas_limit).send().await?;

    info!(
        target: "single_swap",
        "Transaction sent, waiting for confirmation..."
    );

    let tx_hash = pending_tx.watch().await?;

    // 获取输出金额（通过检查代币余额变化）
    let token_out_contract = IERC20::new(swap_step.token_out.into(), provider.clone());
    let amount_out = token_out_contract.balanceOf(from_address).call().await?;

    Ok(SwapResult {
        step: 0, // 将在调用处设置
        pool_address: swap_step.pool_address,
        token_in: swap_step.token_in,
        token_out: swap_step.token_out,
        amount_in: swap_step.amount_in,
        amount_out,
        tx_hash,
    })
}

fn format_ether(wei: U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}
