use alloy::network::EthereumWallet;
use alloy::primitives::{address, Address, U256};
use alloy::{
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use amms::execution::{gas_schedule::gas_limit_for_hops, IAgniPool, IERC20};
use eyre::{bail, Result};
use std::str::FromStr;
use tracing::{error, info, warn};

sol! {
    #[sol(rpc)]
    interface IArbitrageExecutor {
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

/// 解析套利数据行，提取起始Token、池子地址列表和输入金额。
///
/// # Arguments
/// * `data_row` - 格式为 "...,...,token_in->token_out@pool|...,amount_in,..." 的字符串
///
/// # Returns
/// A tuple containing: (start_token, pool_addresses, input_amount)
fn parse_arbitrage_data(data_row: &str) -> Result<(Address, Vec<Address>, U256)> {
    let parts: Vec<&str> = data_row.split(',').collect();

    // 动态定位包含路径的字段（包含"->" 和 "@"）
    let path_index = parts
        .iter()
        .position(|part| {
            let trimmed = part.trim();
            trimmed.contains("->") && trimmed.contains('@')
        })
        .ok_or_else(|| eyre::eyre!("Missing path data in row"))?;

    let path_str = parts[path_index].trim();

    let hops: Vec<&str> = path_str
        .split('|')
        .map(|hop| hop.trim())
        .filter(|hop| !hop.is_empty())
        .collect();
    if hops.is_empty() {
        bail!("Path string does not contain any hops");
    }

    // 如果下一字段为 hop 数量，则跳过
    let recorded_hops = parts
        .get(path_index + 1)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|count| *count == hops.len());

    // 输入金额字段在路径之后（可选 hop 字段之后）
    let amount_index = path_index + if recorded_hops.is_some() { 2 } else { 1 };
    let amount_str = parts
        .get(amount_index)
        .ok_or_else(|| eyre::eyre!("Missing amount data in row"))?
        .trim();

    // 1. 提取起始 Token
    let first_hop_parts: Vec<&str> = hops[0].split("->").map(|segment| segment.trim()).collect();
    let start_token_str = first_hop_parts
        .first()
        .ok_or_else(|| eyre::eyre!("Invalid first hop format"))?;
    let start_token = Address::from_str(start_token_str)?;

    // 2. 提取所有池子地址
    let mut pool_addresses = Vec::new();
    for hop in hops {
        let at_split: Vec<&str> = hop.split('@').map(|segment| segment.trim()).collect();
        let pool_part = at_split
            .get(1)
            .ok_or_else(|| eyre::eyre!("Hop is missing pool address: {}", hop))?;
        // Pool 地址在 '@' 之后，'(' 之前
        let pool_str = pool_part
            .split('(')
            .next()
            .ok_or_else(|| eyre::eyre!("Invalid pool address format"))?
            .trim();
        pool_addresses.push(Address::from_str(pool_str)?);
    }

    // 3. 提取输入金额
    let input_amount = U256::from_str(amount_str)?;

    Ok((start_token, pool_addresses, input_amount))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    dotenv::dotenv().ok();

    // --- 配置 ---
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
    let private_key = std::env::var("PRIVATE_KEY").expect("PRIVATE_KEY must be set");
    let executor_address: Address = address!("0xe3Fe72b3286BA305571de96631120A4046EbF97C");

    // --- 用户提供的数据行 ---
    // let data_row = "86108827,1316,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500)|0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0xbcf99c834e65e8a58090e20edc058279317865bd(fee_bps=100)|0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500)|0xcda86a272531e8640cd7f1a92c01839911b90bb0->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0x4b96994181cb694f506bdf24a218fe7af64147cb(fee_bps=2500),50000000000000000,543773731814576813,129099994556066087,31.1329,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500) | 0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0xbcf99c834e65e8a58090e20edc058279317865bd(fee_bps=100) | 0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500) | 0xcda86a272531e8640cd7f1a92c01839911b90bb0->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0x4b96994181cb694f506bdf24a218fe7af64147cb(fee_bps=2500)";

    // 构造一笔 WMNT->USDC->WMNT 的多跳 swap
    //let data_row = "86108827,1316,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500)|0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0xbcf99c834e65e8a58090e20edc058279317865bd(fee_bps=100)|0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500)|0xcda86a272531e8640cd7f1a92c01839911b90bb0->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0x4b96994181cb694f506bdf24a218fe7af64147cb(fee_bps=2500),50000000000000000,543773731814576813,129099994556066087,31.1329,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500) | 0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9@0xbcf99c834e65e8a58090e20edc058279317865bd(fee_bps=100) | 0x09bc4e0d864854c6afb6eb9a9cdf58ac190d0df9->0xcda86a272531e8640cd7f1a92c01839911b90bb0@0xc81f612980db7a9e5e16c52450f391698f6584cc(fee_bps=500) | 0xcda86a272531e8640cd7f1a92c01839911b90bb0->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0x4b96994181cb694f506bdf24a218fe7af64147cb(fee_bps=2500)";

    let data_row = "86150063,171,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500)|0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x201eba5cc46d216ce6dc03f6a759e8e766e956ae@0x36a7aff497eef6a9cd7d0e7bc243793fcb3e57e2(fee_bps=100)|0x201eba5cc46d216ce6dc03f6a759e8e766e956ae->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0xcb893a28933a89b5c4ee3d02ca37524d3d0bfc97(fee_bps=10000),4879665618368645170,4901034702819166033,21369084450520863,0.4379,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8->0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34@0xeafc4d6d4c3391cd4fc10c85d2f5f972d58c0dd5(fee_bps=2500) | 0x5d3a1ff2b6bab83b63cd9ad0787074081a52ef34->0x201eba5cc46d216ce6dc03f6a759e8e766e956ae@0x36a7aff497eef6a9cd7d0e7bc243793fcb3e57e2(fee_bps=100) | 0x201eba5cc46d216ce6dc03f6a759e8e766e956ae->0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8@0xcb893a28933a89b5c4ee3d02ca37524d3d0bfc97(fee_bps=10000)";

    // --- 解析数据 ---
    let (start_token, pool_addresses, input_amount) = parse_arbitrage_data(data_row)?;

    // --- 设置 Provider 和 Signer ---
    let signer = PrivateKeySigner::from_str(&private_key)?;
    let wallet = EthereumWallet::from(signer.clone());
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect_http(rpc_url.parse()?);

    info!(
        target: "multi_swap",
        address = %signer.address(),
        "🚀 Starting Agni multi-pool swap with executor on Mantle Sepolia"
    );

    // --- 执行交易 ---
    run_agni_swap(
        &provider,
        executor_address,
        start_token,
        pool_addresses,
        input_amount,
    )
    .await?;

    Ok(())
}

async fn run_agni_swap<P: Provider>(
    provider: &P,
    executor_address: Address,
    start_token: Address,
    pool_addresses: Vec<Address>,
    input_amount: U256,
) -> Result<()> {
    info!(target: "multi_swap", "Executing Agni (Uni V3 style) swap path");
    info!(target: "multi_swap", input_amount = %input_amount, "Input amount: {} tokens", format_units(input_amount, 18));
    info!(target: "multi_swap", start_token = %start_token, "Start token");
    info!(target: "multi_swap", "Pools: {:?}", pool_addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>());

    // --- 检查执行器合约余额 ---
    let token_contract = IERC20::new(start_token, provider);
    let executor_initial_balance = token_contract.balanceOf(executor_address).call().await?;
    info!(
        target: "multi_swap",
        executor_balance = %executor_initial_balance,
        "Executor start token balance (before): {}",
        format_units(executor_initial_balance, 18)
    );

    if executor_initial_balance < input_amount {
        error!(
            target: "multi_swap",
            required = %input_amount,
            available = %executor_initial_balance,
            "❌ Executor contract balance insufficient for this swap!"
        );
        return Ok(());
    }

    // --- 构建交易参数 ---
    // Agni (V3) 池类型为 1
    let pool_types = vec![1u8; pool_addresses.len()];

    // 动态推导完整的 Token 路径
    let mut token_path: Vec<Address> = Vec::with_capacity(pool_addresses.len() + 1);
    token_path.push(start_token);

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
                "❌ Current token not found in Agni pool"
            );
            return Ok(());
        };
        token_path.push(next);
    }
    info!(target: "multi_swap", "Full token path: {:?}", token_path.iter().map(|a| a.to_string()).collect::<Vec<_>>());

    // 检查最终 Token 是否与起始 Token 相同（套利回路）
    if *token_path.last().unwrap() != start_token {
        warn!(
            target: "multi_swap",
            end_token = %token_path.last().unwrap(),
            "End token is not the same as start token; this is not a closed arbitrage loop."
        );
    }

    // 获取池子的当前状态 (sqrtPriceX96 and liquidity)
    let mut expected_states: Vec<U256> = Vec::with_capacity(pool_addresses.len() * 2);
    for pool_addr in &pool_addresses {
        let pool = IAgniPool::new(*pool_addr, provider);
        let slot0 = pool.slot0().call().await?;
        let liquidity = pool.liquidity().call().await?;
        expected_states.push(U256::from(slot0.sqrtPriceX96));
        expected_states.push(U256::from(liquidity));
    }

    // 对于 V3 路径，我们让合约计算输出，所以这里传 0
    let amounts_out = vec![U256::ZERO; pool_addresses.len()];

    // --- 发送交易 ---
    let executor = IArbitrageExecutor::new(executor_address, provider);
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
            amounts_out.clone(),
            alloy::primitives::U256::ZERO,
            alloy::primitives::U256::from(u64::MAX),
        )
        .gas(gas_limit)
        .send()
        .await?;

    let tx_hash = pending_tx.watch().await?;
    info!(
        target: "multi_swap",
        tx = %tx_hash,
        "✅ Atomic Agni multi-pool swap executed successfully"
    );

    // --- 结果分析 ---
    let executor_final_balance = token_contract.balanceOf(executor_address).call().await?;
    let profit = executor_final_balance.saturating_sub(executor_initial_balance);
    let loss = executor_initial_balance.saturating_sub(executor_final_balance);

    info!(
        target: "multi_swap",
        initial_balance = %executor_initial_balance,
        final_balance = %executor_final_balance,
        "Final results - Executor Initial: {}, Executor Final: {}",
        format_units(executor_initial_balance, 18),
        format_units(executor_final_balance, 18)
    );

    if profit > U256::ZERO {
        info!(
            target: "multi_swap",
            "💰 Profit: {} tokens",
            format_units(profit, 18)
        );
    } else if loss > U256::ZERO {
        warn!(
            target: "multi_swap",
            "⚠️  Loss: {} tokens",
            format_units(loss, 18)
        );
    } else {
        info!(target: "multi_swap", "🔄 Break even or no change detected.");
    }

    Ok(())
}

/// 格式化 wei 值为更易读的单位 (例如, ether)
fn format_units(value: U256, decimals: u32) -> String {
    let divisor = U256::from(10).pow(U256::from(decimals));
    let int_part = value / divisor;
    let frac_part = value % divisor;

    // 为了避免浮点数精度问题，我们手动处理小数部分
    // 只显示前6位小数
    let frac_str = format!(
        "{:0>width$}",
        frac_part.to_string(),
        width = decimals as usize
    );
    let display_frac = &frac_str[..std::cmp::min(6, frac_str.len())];

    format!("{}.{}", int_part, display_frac)
}
