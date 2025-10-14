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
///    cargo run --example multi_pool_swap_with_executor [-- --mode agni|uni_v2|both]
///    或设置环境变量 SWAP_MODE=agni/uni_v2/both
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use amms::execution::{gas_schedule::gas_limit_for_hops, IAgniPool, IMoePair, IERC20};
use eyre::Result;
use std::{fmt, str::FromStr};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwapMode {
    Agni,
    UniV2,
    Both,
}

impl SwapMode {
    fn from_str(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "agni" | "uni_v3" | "v3" => Some(Self::Agni),
            "uni_v2" | "univ2" | "v2" => Some(Self::UniV2),
            "both" | "all" | "mixed" | "dual" => Some(Self::Both),
            _ => None,
        }
    }
}

impl fmt::Display for SwapMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwapMode::Agni => write!(f, "agni"),
            SwapMode::UniV2 => write!(f, "uni_v2"),
            SwapMode::Both => write!(f, "both"),
        }
    }
}

fn resolve_swap_mode() -> SwapMode {
    let args: Vec<String> = std::env::args().collect();
    let mut mode_candidate: Option<String> = None;

    let mut index = 1;
    while index < args.len() {
        if let Some(value) = args[index].strip_prefix("--mode=") {
            mode_candidate = Some(value.to_string());
            break;
        }

        if args[index] == "--mode" {
            if let Some(value) = args.get(index + 1) {
                mode_candidate = Some(value.clone());
            }
            break;
        }

        index += 1;
    }

    if mode_candidate.is_none() {
        if let Ok(value) = std::env::var("SWAP_MODE") {
            mode_candidate = Some(value);
        }
    }

    if let Some(candidate) = mode_candidate {
        if let Some(mode) = SwapMode::from_str(&candidate) {
            return mode;
        }
        warn!(
            target: "multi_swap",
            mode_value = %candidate,
            "Unknown swap mode specified, defaulting to Both"
        );
    }

    SwapMode::Both
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

    let swap_mode = resolve_swap_mode();
    info!(
        target: "multi_swap",
        mode = %swap_mode,
        "Configured swap mode"
    );

    // Common configuration
    let wmnt = address!("67A1f4A939b477A6b7c5BF94D97E45dE87E608eF");
    let executor_address: Address = address!("0xe3Fe72b3286BA305571de96631120A4046EbF97C");

    match swap_mode {
        SwapMode::Agni => run_agni_path(&provider, from_address, executor_address, wmnt).await?,
        SwapMode::UniV2 => run_uni_v2_path(&provider, from_address, executor_address, wmnt).await?,
        SwapMode::Both => {
            run_agni_path(&provider, from_address, executor_address, wmnt).await?;
            run_uni_v2_path(&provider, from_address, executor_address, wmnt).await?;
        }
    }

    Ok(())
}

async fn run_agni_path<P: Provider>(
    provider: &P,
    from_address: Address,
    executor_address: Address,
    wmnt: Address,
) -> Result<()> {
    let usdc = address!("Acab8129E2cE587fD203FD770ec9ECAFA2C88080");

    let pool_wmnt_usdt = address!("eade7A0307466817a0635ebFD5b781cDf526EC4F");
    let pool_usdt_usde = address!("677C9F5B215b87A7D04Ff9B4CBEf6A7b7AD1A0ce");
    let pool_usde_usdc = address!("189ea4e01a4BF8eA82d1784933F64D012415f61F");
    let pool_usdc_wmnt = address!("Ce8B3Bd008A7fFD1E53756a40ee81AC914a3831D");

    let input_amount = U256::from(150_000_000_000_000_000u64); // 0.15 WMNT

    info!(
        target: "multi_swap",
        "Using Agni (Uni V3) executor path WMNT -> USDT -> USDe -> USDC -> WMNT"
    );
    info!(
        target: "multi_swap",
        input_amount = %input_amount,
        "Input amount: {} WMNT",
        format_ether(input_amount)
    );

    let wmnt_contract = IERC20::new(wmnt, provider);
    let executor_initial_balance = wmnt_contract.balanceOf(executor_address).call().await?;
    info!(
        target: "multi_swap",
        executor_balance = %executor_initial_balance,
        "Executor WMNT balance (before Agni): {}",
        format_ether(executor_initial_balance)
    );

    if executor_initial_balance < input_amount {
        error!(
            target: "multi_swap",
            required = %input_amount,
            available = %executor_initial_balance,
            "❌ Executor contract WMNT balance insufficient for Agni path!"
        );
        return Ok(());
    }

    let pool_addresses = vec![
        pool_wmnt_usdt,
        pool_usdt_usde,
        pool_usde_usdc,
        pool_usdc_wmnt,
    ];
    let pool_types = vec![1u8; pool_addresses.len()];

    let mut token_path: Vec<Address> = Vec::with_capacity(pool_addresses.len() + 1);
    token_path.push(wmnt);

    for (i, pool_addr) in pool_addresses.iter().enumerate() {
        let pool = IAgniPool::new(*pool_addr, provider);
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
                "❌ Current token not in Agni pool"
            );
            return Ok(());
        };
        token_path.push(next);
    }

    if *token_path.last().unwrap() != wmnt {
        warn!(
            target: "multi_swap",
            end_token = %token_path.last().unwrap(),
            "End token is not WMNT; continuing but final balance may not reflect WMNT"
        );
    }

    let mut expected_states: Vec<U256> = Vec::with_capacity(pool_addresses.len() * 2);
    for pool_addr in &pool_addresses {
        let pool = IAgniPool::new(*pool_addr, provider);
        let slot0 = pool.slot0().call().await?;
        let liquidity = pool.liquidity().call().await?;
        expected_states.push(U256::from(slot0.sqrtPriceX96));
        expected_states.push(U256::from(liquidity));
    }

    let amounts_out = vec![U256::ZERO; pool_addresses.len()];

    let executor = IOptimizedArbitrageExecutor::new(executor_address, provider);
    let gas_limit = gas_limit_for_hops(pool_addresses.len());
    info!(
        target: "multi_swap",
        gas_limit = gas_limit,
        hops = pool_addresses.len(),
        "Invoking on-chain ArbitrageExecutor (Agni path)"
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
        "✅ Atomic Agni multi-pool swap executed"
    );

    let executor_final_balance = wmnt_contract.balanceOf(executor_address).call().await?;
    let profit = if executor_final_balance > executor_initial_balance {
        executor_final_balance - executor_initial_balance
    } else {
        U256::ZERO
    };
    let loss = if executor_initial_balance > executor_final_balance {
        executor_initial_balance - executor_final_balance
    } else {
        U256::ZERO
    };

    info!(
        target: "multi_swap",
        path = "Agni",
        initial_balance = %executor_initial_balance,
        final_balance = %executor_final_balance,
        profit = %profit,
        loss = %loss,
        "Final results (Agni) - Executor Initial: {}, Executor Final: {}",
        format_ether(executor_initial_balance),
        format_ether(executor_final_balance)
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

async fn run_uni_v2_path<P: Provider>(
    provider: &P,
    from_address: Address,
    executor_address: Address,
    wmnt: Address,
) -> Result<()> {
    let pool_wmnt_fff1 = address!("0x553143BA3352B88d88727bF3A06707bD9dAd7945");
    let pool_fff1_fff2 = address!("0x61979bBd99CCbc2E584788C28812E7A7aaF441DC");
    let pool_fff2_wmnt = address!("0x3CFe2054d996Ea9Fc2840908feb046564cf5768b");

    let input_amount = U256::from(100_000_000_000_000_000u64); // 0.1 WMNT

    info!(
        target: "multi_swap",
        "Using Uni V2 executor path WMNT -> FFF1 -> FFF2 -> WMNT"
    );
    info!(
        target: "multi_swap",
        input_amount = %input_amount,
        "Input amount: {} WMNT",
        format_ether(input_amount)
    );

    let wmnt_contract = IERC20::new(wmnt, provider);
    let executor_initial_balance = wmnt_contract.balanceOf(executor_address).call().await?;
    info!(
        target: "multi_swap",
        executor_balance = %executor_initial_balance,
        "Executor WMNT balance (before Uni V2): {}",
        format_ether(executor_initial_balance)
    );

    if executor_initial_balance < input_amount {
        error!(
            target: "multi_swap",
            required = %input_amount,
            available = %executor_initial_balance,
            "❌ Executor contract WMNT balance insufficient for Uni V2 path!"
        );
        return Ok(());
    }

    let pool_addresses = vec![pool_wmnt_fff1, pool_fff1_fff2, pool_fff2_wmnt];
    let pool_types = vec![0u8; pool_addresses.len()];

    let mut token_path: Vec<Address> = Vec::with_capacity(pool_addresses.len() + 1);
    token_path.push(wmnt);

    let mut expected_states: Vec<U256> = Vec::with_capacity(pool_addresses.len() * 2);
    let mut amounts_out: Vec<U256> = Vec::with_capacity(pool_addresses.len());
    let mut current_amount = input_amount;

    for (i, pool_addr) in pool_addresses.iter().enumerate() {
        let pair = IMoePair::new(*pool_addr, provider);
        let token0 = pair.token0().call().await?;
        let token1 = pair.token1().call().await?;
        let token0_addr = Address::from(token0);
        let token1_addr = Address::from(token1);
        let current_in = token_path[i];

        let next_token = if current_in == token0_addr {
            token1_addr
        } else if current_in == token1_addr {
            token0_addr
        } else {
            error!(
                target: "multi_swap",
                pool = %pool_addr,
                current_in = %current_in,
                token0 = %token0_addr,
                token1 = %token1_addr,
                "❌ Current token not in Uni V2 pool"
            );
            return Ok(());
        };
        token_path.push(next_token);

        let reserves = pair.getReserves().call().await?;
        let reserve0 = U256::from(reserves._0);
        let reserve1 = U256::from(reserves._1);
        expected_states.push(reserve0);
        expected_states.push(reserve1);

        let (reserve_in, reserve_out) = if current_in == token0_addr {
            (reserve0, reserve1)
        } else {
            (reserve1, reserve0)
        };

        let numerator = current_amount * U256::from(997u64) * reserve_out;
        let denominator = reserve_in * U256::from(1000u64) + current_amount * U256::from(997u64);
        let amount_out = if denominator.is_zero() {
            U256::ZERO
        } else {
            numerator / denominator
        };

        info!(
            target: "multi_swap",
            step = i + 1,
            pool = %pool_addr,
            amount_in = %current_amount,
            reserve_in = %reserve_in,
            reserve_out = %reserve_out,
            expected_out = %amount_out,
            "Computed Uni V2 hop expectations"
        );

        amounts_out.push(amount_out);
        current_amount = amount_out;
    }

    if *token_path.last().unwrap() != wmnt {
        warn!(
            target: "multi_swap",
            end_token = %token_path.last().unwrap(),
            "End token is not WMNT for Uni V2 path; continuing"
        );
    }

    let executor = IOptimizedArbitrageExecutor::new(executor_address, provider);
    let gas_limit = gas_limit_for_hops(pool_addresses.len());
    info!(
        target: "multi_swap",
        gas_limit = gas_limit,
        hops = pool_addresses.len(),
        "Invoking on-chain ArbitrageExecutor (Uni V2 path)"
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
        "✅ Atomic Uni V2 multi-pool swap executed"
    );

    let executor_final_balance = wmnt_contract.balanceOf(executor_address).call().await?;
    let profit = if executor_final_balance > executor_initial_balance {
        executor_final_balance - executor_initial_balance
    } else {
        U256::ZERO
    };
    let loss = if executor_initial_balance > executor_final_balance {
        executor_initial_balance - executor_final_balance
    } else {
        U256::ZERO
    };

    info!(
        target: "multi_swap",
        path = "UniV2",
        initial_balance = %executor_initial_balance,
        final_balance = %executor_final_balance,
        profit = %profit,
        loss = %loss,
        "Final results (Uni V2) - Executor Initial: {}, Executor Final: {}",
        format_ether(executor_initial_balance),
        format_ether(executor_final_balance)
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
