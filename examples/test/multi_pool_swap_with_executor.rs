/// 使用执行器的多池连环交易脚本
///
/// 实现 WMNT->USDC->USDT->WMNT 的连环交易
/// 使用 SwapExecutor 自动处理不同类型的池子
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了：
///    - MANTLE_SEPOLIA_RPC_URL
///    - MANTLE_SEPOLIA_PRIVATE_KEY
///    - 测试账户需要有足够的 WMNT
///
/// 2. 运行脚本：
///    cargo run --example multi_pool_swap_with_executor
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::{providers::ProviderBuilder, signers::local::PrivateKeySigner, sol};
use amms::execution::{gas_schedule::gas_limit_for_hops, IAgniPool, IERC20};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info, warn};

sol! {
    #[sol(rpc)]
    interface IOptimizedArbitrageExecutor {
        function executeArbitrage(
            uint256 _amountIn,
            address[] calldata _path,
            address[] calldata _pools,
            uint8[] calldata _poolTypes,
            uint256[] calldata _expectedStates,
            uint256[] calldata _amountsOut
        ) external;
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
        target: "multi_swap",
        address = %from_address,
        "🚀 Starting multi-pool swap with executor on Mantle Sepolia"
    );

    // Mantle Sepolia 代币地址
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    // 池子地址 - 复杂路径（V3）
    // WMNT/USDT
    let pool_wmnt_usdt = address!("eade7A0307466817a0635ebFD5b781cDf526EC4F");
    // USDT/USDe
    let pool_usdt_usde = address!("677C9F5B215b87A7D04Ff9B4CBEf6A7b7AD1A0ce");
    // USDe/USDC
    let pool_usde_usdc = address!("189ea4e01a4BF8eA82d1784933F64D012415f61F");
    // USDC/WMNT
    let pool_usdc_wmnt = address!("Ce8B3Bd008A7fFD1E53756a40ee81AC914a3831D");

    // 输入金额：0.15 WMNT
    let input_amount = U256::from(150_000_000_000_000_000u64);

    info!(
        target: "multi_swap",
        input_amount = %input_amount,
        "Input amount: {} WMNT",
        format_ether(input_amount)
    );

    // 检查初始余额
    let wmnt_contract = IERC20::new(wmnt, &provider);
    let initial_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;

    info!(
        target: "multi_swap",
        initial_balance = %initial_wmnt_balance,
        "Initial WMNT balance: {}",
        format_ether(initial_wmnt_balance)
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

    info!(
        target: "multi_swap",
        "Using path WMNT -> USDT -> USDe -> USDC -> WMNT"
    );

    // 构建合约调用参数
    let pool_addresses = vec![
        pool_wmnt_usdt,
        pool_usdt_usde,
        pool_usde_usdc,
        pool_usdc_wmnt,
    ];
    let pool_types = vec![1u8; pool_addresses.len()]; // 全部为 V3

    // 动态构建 token_path
    let mut token_path: Vec<Address> = Vec::with_capacity(pool_addresses.len() + 1);
    token_path.push(wmnt);

    // 依次解析每个池的另一侧代币
    for (i, pool_addr) in pool_addresses.iter().enumerate() {
        let pool = IAgniPool::new(*pool_addr, &provider);
        let t0 = pool.token0().call().await?;
        let t1 = pool.token1().call().await?;
        let current_in = token_path[i];
        let next = if current_in == Address::from(t0) {
            Address::from(t1)
        } else if current_in == Address::from(t1) {
            Address::from(t0)
        } else {
            error!(
                target: "multi_swap",
                pool = %pool_addr,
                current_in = %current_in,
                token0 = %Address::from(t0),
                token1 = %Address::from(t1),
                "❌ Current token not in pool"
            );
            return Ok(());
        };
        token_path.push(next);
    }

    // 校验闭环：最后一跳应回到 WMNT
    if *token_path.last().unwrap() != wmnt {
        warn!(
            target: "multi_swap",
            end_token = %token_path.last().unwrap(),
            "End token is not WMNT; continuing but final balance calc may not reflect WMNT"
        );
    }

    // 获取每个池的状态快照 (sqrtPriceX96, liquidity)
    let mut expected_states: Vec<U256> = Vec::with_capacity(pool_addresses.len() * 2);
    for pool_addr in &pool_addresses {
        let pool = IAgniPool::new(*pool_addr, &provider);
        let slot0 = pool.slot0().call().await?;
        let liquidity = pool.liquidity().call().await?;
        expected_states.push(U256::from(slot0.sqrtPriceX96));
        expected_states.push(U256::from(liquidity));
    }

    // 最小回款检查：此处设置为 0 以不限制
    let amounts_out = vec![U256::ZERO; pool_addresses.len()];

    // 确保执行器合约中有足够的 WMNT（请替换为你的执行器地址）
    let executor_address: Address = address!("0x26F34aD8edf6FCb4e44d1e21109FC33FF42406eA");
    let executor_balance = wmnt_contract.balanceOf(executor_address).call().await?;

    if executor_balance < input_amount {
        let deficit = input_amount - executor_balance;
        info!(
            target: "multi_swap",
            deficit = %deficit,
            "Funding executor contract with additional WMNT"
        );
        let fund_tx = wmnt_contract
            .transfer(executor_address, deficit)
            .send()
            .await?;
        let fund_receipt = fund_tx.watch().await?;
        info!(
            target: "multi_swap",
            tx = %fund_receipt,
            "✅ Executor funded"
        );
    }

    // 调用链上 ArbitrageExecutor 执行原子多池交换
    let executor = IOptimizedArbitrageExecutor::new(executor_address, &provider);
    let gas_limit = gas_limit_for_hops(pool_addresses.len());

    info!(
        target: "multi_swap",
        gas_limit = gas_limit,
        hops = pool_addresses.len(),
        "Invoking on-chain ArbitrageExecutor"
    );

    let pending_tx = executor
        .executeArbitrage(
            input_amount,
            token_path.clone(),
            pool_addresses.clone(),
            pool_types.clone(),
            expected_states.clone(),
            amounts_out.clone(),
        )
        .gas(gas_limit)
        .send()
        .await?;

    let tx_hash = pending_tx.watch().await?;
    info!(
        target: "multi_swap",
        tx = %tx_hash,
        "✅ Atomic multi-pool swap executed"
    );

    // 计算最终结果
    let final_wmnt_balance = wmnt_contract.balanceOf(from_address).call().await?;
    let profit = if final_wmnt_balance > initial_wmnt_balance {
        final_wmnt_balance - initial_wmnt_balance
    } else {
        U256::ZERO
    };
    let loss = if initial_wmnt_balance > final_wmnt_balance {
        initial_wmnt_balance - final_wmnt_balance
    } else {
        U256::ZERO
    };

    info!(
        target: "multi_swap",
        initial_balance = %initial_wmnt_balance,
        final_balance = %final_wmnt_balance,
        profit = %profit,
        loss = %loss,
        "Final results - Initial: {}, Final: {}",
        format_ether(initial_wmnt_balance),
        format_ether(final_wmnt_balance)
    );

    if profit > U256::ZERO {
        info!(
            target: "multi_swap",
            "💰 Profit: {} WMNT",
            format_ether(profit)
        );
    } else if loss > U256::ZERO {
        warn!(
            target: "multi_swap",
            "⚠️  Loss: {} WMNT",
            format_ether(loss)
        );
    } else {
        info!(target: "multi_swap", "Break even");
    }

    Ok(())
}

fn format_ether(wei: U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}
