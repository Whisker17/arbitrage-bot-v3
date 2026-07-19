/// 使用执行器的 Moe LBT 多池连环交易脚本
///
/// 实现 WMNT->USDT->USDE->WMNT 的连环交易
/// 使用 OptimizedArbitrageExecutor 自动处理 Moe LBT 池子
///
/// 使用方法：
/// 1. 确保 .env 文件中配置了：
///    - MANTLE_HTTP_URL (或 RPC_HTTP_URL)
///    - MANTLE_MAINNET_PRIVATE_KEY (或 PRIVATE_KEY)
///    - 执行器合约需要有足够的 WMNT
///
/// 2. 运行脚本：
///    cargo run --example multi_pool_moe_swap_with_executor
use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::{
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
    sol,
};
use amms::execution::{gas_limit_for_hops, IERC20, IMoeLBPair};
use eyre::Result;
use std::str::FromStr;
use tracing::{error, info, warn};

sol! {
    #[sol(rpc)]
    interface IOptimizedArbitrageExecutor {
        function executeArbitrage(
            uint256 amountIn,
            address[] calldata path,
            address[] calldata pools,
            uint8[] calldata poolTypes,
            uint256[] calldata amountsOut,
            uint256 minProfit,
            uint256 deadline
        ) external;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // 加载环境变量
    dotenv::dotenv().ok();

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
        target: "moe_multi_swap",
        address = %from_address,
        "🚀 Starting Moe LBT multi-pool swap with executor on Mantle Mainnet"
    );

    // 代币地址（Mantle 主网）
    let wmnt = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");

    // Moe LBPair 池子地址
    // WMNT/USDT (bin step 15)
    let pool_wmnt_usdt = address!("f6C9020c9E915808481757779EDB53DACEaE2415");
    // USDT/USDE (bin step 100)
    let pool_usdt_usde = address!("3eb7e346f050c6c42bf94a69e63eaee28a602cfd");
    // USDE/WMNT (bin step 100)
    let pool_usde_wmnt = address!("d4a1f03c2c4981f7b84b9de1af936bf34a20fa3a");

    // 执行器合约地址
    let executor_address: Address = address!("5714c6bca610c16594c8ab3f812e42ebbb062077");

    let input_amount = U256::from(10_000_000_000_000_000u64); // 0.01 WMNT

    info!(
        target: "moe_multi_swap",
        "Using Moe LBT executor path: WMNT -> USDT -> USDE -> WMNT"
    );
    info!(
        target: "moe_multi_swap",
        input_amount = %input_amount,
        "Input amount: {} WMNT",
        format_ether(input_amount)
    );

    let wmnt_contract = IERC20::new(wmnt, &provider);
    let executor_initial_balance = wmnt_contract.balanceOf(executor_address).call().await?;
    info!(
        target: "moe_multi_swap",
        executor_balance = %executor_initial_balance,
        "Executor WMNT balance (before swap): {}",
        format_ether(executor_initial_balance)
    );

    if executor_initial_balance < input_amount {
        error!(
            target: "moe_multi_swap",
            required = %input_amount,
            available = %executor_initial_balance,
            "❌ Executor contract WMNT balance insufficient!"
        );
        return Ok(());
    }

    // 构建池子数组
    let pool_addresses = vec![pool_wmnt_usdt, pool_usdt_usde, pool_usde_wmnt];
    
    // 所有池子都是 Moe LBT 类型 (poolType = 2)
    let pool_types = vec![2u8; pool_addresses.len()];

    // 构建代币路径
    let mut token_path: Vec<Address> = Vec::with_capacity(pool_addresses.len() + 1);
    token_path.push(wmnt);

    info!(
        target: "moe_multi_swap",
        "Building token path by querying pool tokens..."
    );

    // 为每个池子确定输出代币
    for (i, pool_addr) in pool_addresses.iter().enumerate() {
        let pool = IMoeLBPair::new(*pool_addr, &provider);
        let token_x = pool.getTokenX().call().await?;
        let token_y = pool.getTokenY().call().await?;
        let token_x_addr = Address::from(token_x);
        let token_y_addr = Address::from(token_y);
        let current_in = token_path[i];

        let next_token = if current_in == token_x_addr {
            token_y_addr
        } else if current_in == token_y_addr {
            token_x_addr
        } else {
            error!(
                target: "moe_multi_swap",
                pool = %pool_addr,
                current_in = %current_in,
                token_x = %token_x_addr,
                token_y = %token_y_addr,
                "❌ Current token not in Moe LBT pool"
            );
            return Ok(());
        };
        
        token_path.push(next_token);
        
        info!(
            target: "moe_multi_swap",
            step = i + 1,
            pool = %pool_addr,
            token_in = %current_in,
            token_out = %next_token,
            "Pool {} token mapping",
            i + 1
        );
    }

    // 验证路径是否回到 WMNT
    if *token_path.last().unwrap() != wmnt {
        warn!(
            target: "moe_multi_swap",
            end_token = %token_path.last().unwrap(),
            "⚠️  End token is not WMNT; path may not be circular"
        );
    } else {
        info!(
            target: "moe_multi_swap",
            "✅ Token path verified: circular path back to WMNT"
        );
    }

    // 对于 Moe LBT，我们使用 0 作为 amountsOut（合约内部会处理）
    let amounts_out = vec![U256::ZERO; pool_addresses.len()];

    let executor = IOptimizedArbitrageExecutor::new(executor_address, &provider);
    let gas_limit = gas_limit_for_hops(pool_addresses.len());
    
    // 重试机制：最多尝试 3 次
    let max_retries = 3;
    #[allow(unused_assignments)]
    let mut last_error: Option<String> = None;
    let mut success = false;
    
    for attempt in 1..=max_retries {
        info!(
            target: "moe_multi_swap",
            attempt = attempt,
            max_retries = max_retries,
            "Attempt {} of {} - Fetching fresh pool states...",
            attempt,
            max_retries
        );

        // 每次尝试都重新获取最新的池子状态
        let mut expected_states: Vec<U256> = Vec::with_capacity(pool_addresses.len() * 2);
        
        for (i, pool_addr) in pool_addresses.iter().enumerate() {
            let pool = IMoeLBPair::new(*pool_addr, &provider);
            let active_id = pool.getActiveId().call().await?;
            let bin_step = pool.getBinStep().call().await?;
            
            info!(
                target: "moe_multi_swap",
                step = i + 1,
                pool = %pool_addr,
                active_id = %active_id,
                bin_step = %bin_step,
                "Pool {} state (attempt {})",
                i + 1,
                attempt
            );
            
            expected_states.push(U256::from(active_id));
            expected_states.push(U256::from(bin_step));
        }

        info!(
            target: "moe_multi_swap",
            gas_limit = gas_limit,
            hops = pool_addresses.len(),
            "Invoking on-chain ArbitrageExecutor (Moe LBT path)"
        );

        if attempt == 1 {
            info!(
                target: "moe_multi_swap",
                "Transaction parameters:"
            );
            info!(
                target: "moe_multi_swap",
                "  - amountIn: {}",
                input_amount
            );
            info!(
                target: "moe_multi_swap",
                "  - path: {:?}",
                token_path
            );
            info!(
                target: "moe_multi_swap",
                "  - pools: {:?}",
                pool_addresses
            );
            info!(
                target: "moe_multi_swap",
                "  - poolTypes: {:?}",
                pool_types
            );
        }
        
        info!(
            target: "moe_multi_swap",
            "  - expectedStates: {:?}",
            expected_states
        );

        // 尝试发送交易
        match executor
            .executeArbitrage(
                input_amount,
                token_path.clone(),
                pool_addresses.clone(),
                pool_types.clone(),
            amounts_out.clone(),
            alloy::primitives::U256::ZERO,
            alloy::primitives::U256::from(u64::MAX),
            )
            .gas(gas_limit)
            .send()
            .await
        {
            Ok(pending_tx) => {
                info!(
                    target: "moe_multi_swap",
                    "Transaction sent, waiting for confirmation..."
                );

                match pending_tx.watch().await {
                    Ok(tx_hash) => {
                        info!(
                            target: "moe_multi_swap",
                            tx = %tx_hash,
                            attempt = attempt,
                            "✅ Atomic Moe LBT multi-pool swap executed successfully!"
                        );
                        // 成功，跳出循环
                        success = true;
                        break;
                    }
                    Err(e) => {
                        last_error = Some(format!("Transaction failed: {:?}", e));
                        warn!(
                            target: "moe_multi_swap",
                            attempt = attempt,
                            error = ?e,
                            "⚠️  Transaction failed, will retry if attempts remain"
                        );
                        
                        // 等待一小段时间再重试
                        if attempt < max_retries {
                            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                        }
                    }
                }
            }
            Err(e) => {
                last_error = Some(format!("Failed to send transaction: {:?}", e));
                warn!(
                    target: "moe_multi_swap",
                    attempt = attempt,
                    error = ?e,
                    "⚠️  Failed to send transaction, will retry if attempts remain"
                );
                
                // 等待一小段时间再重试
                if attempt < max_retries {
                    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
                }
            }
        }
        
        // 如果是最后一次尝试且失败了
        if attempt == max_retries && !success {
            if let Some(err) = &last_error {
                error!(
                    target: "moe_multi_swap",
                    "❌ All {} attempts failed. Last error: {}",
                    max_retries,
                    err
                );
            } else {
                error!(
                    target: "moe_multi_swap",
                    "❌ All {} attempts failed with unknown error",
                    max_retries
                );
            }
            return Ok(());
        }
    }
    
    // 如果所有尝试都失败了，不继续执行
    if !success {
        return Ok(());
    }

    // 检查最终余额
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
        target: "moe_multi_swap",
        path = "Moe LBT",
        initial_balance = %executor_initial_balance,
        final_balance = %executor_final_balance,
        profit = %profit,
        loss = %loss,
        "Final results (Moe LBT) - Executor Initial: {}, Executor Final: {}",
        format_ether(executor_initial_balance),
        format_ether(executor_final_balance)
    );

    if profit > U256::ZERO {
        info!(
            target: "moe_multi_swap",
            "💰 Profit: {} WMNT",
            format_ether(profit)
        );
    } else if loss > U256::ZERO {
        warn!(
            target: "moe_multi_swap",
            "⚠️  Loss: {} WMNT",
            format_ether(loss)
        );
    } else {
        info!(target: "moe_multi_swap", "Break even");
    }

    Ok(())
}

fn format_ether(wei: U256) -> String {
    let ether = wei.to::<u128>() as f64 / 1e18;
    format!("{:.6}", ether)
}

