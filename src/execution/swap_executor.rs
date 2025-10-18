use alloy::primitives::aliases::U24;
/// Simplified swap executor for direct pool interactions
/// Supports both Uni V2 and Agni (V3) style swaps
use alloy::primitives::{Address, Bytes, U160, U256};
use alloy::providers::Provider;
use eyre::{eyre, Result};
use tracing::{error, info};

use super::contract::{IAgniPool, IAgniSwapRouter, ILBRouter, IMoeLBPair, IMoePair, IERC20};
use super::gas_schedule::gas_limit_for_hops;
use super::types::{ExecutorConfig, PoolType, SwapStep};

pub struct SwapExecutor;

impl SwapExecutor {
    /// Execute a single swap step
    pub async fn execute_swap<P: Provider>(
        provider: &P,
        swap_step: &SwapStep,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<U256> {
        match swap_step.pool_type {
            PoolType::UniV2 => Self::execute_v2_swap(provider, swap_step, from_address).await,
            PoolType::UniV3 => {
                Self::execute_v3_swap(provider, swap_step, from_address, config).await
            }
            PoolType::MoeLB => Self::execute_moe_lb_swap(provider, swap_step, from_address, config).await,
        }
    }

    /// Execute a Uniswap V2 style swap
    async fn execute_v2_swap<P: Provider>(
        provider: &P,
        swap_step: &SwapStep,
        from_address: Address,
    ) -> Result<U256> {
        let pool = IMoePair::new(swap_step.pool_address, provider);

        // Get pool information
        let token0 = pool.token0().call().await?;
        let reserves = pool.getReserves().call().await?;

        // Determine swap direction
        let zero_for_one = swap_step.token_in == token0;
        let (reserve_in, reserve_out) = if zero_for_one {
            (U256::from(reserves._0), U256::from(reserves._1))
        } else {
            (U256::from(reserves._1), U256::from(reserves._0))
        };

        // Calculate expected output using Uniswap V2 formula
        // out = (in * 997 * Rout) / (Rin * 1000 + in * 997)
        let numerator = swap_step.amount_in * U256::from(997u64) * reserve_out;
        let denominator =
            reserve_in * U256::from(1000u64) + swap_step.amount_in * U256::from(997u64);
        let expected_out = if denominator.is_zero() {
            U256::ZERO
        } else {
            numerator / denominator
        };

        info!(
            target: "swap_executor",
            pool = %swap_step.pool_address,
            pool_type = "UniV2",
            token_in = %swap_step.token_in,
            token_out = %swap_step.token_out,
            amount_in = %swap_step.amount_in,
            expected_out = %expected_out,
            zero_for_one = zero_for_one,
            "Executing V2 swap"
        );

        // Approve token for pool
        Self::ensure_approval(
            provider,
            swap_step.token_in,
            swap_step.pool_address,
            swap_step.amount_in,
            from_address,
        )
        .await?;

        // Transfer tokens to pool
        let token_in_contract = IERC20::new(swap_step.token_in, provider);
        let transfer_tx = token_in_contract
            .transfer(swap_step.pool_address, swap_step.amount_in)
            .send()
            .await?;
        let _transfer_hash = transfer_tx.watch().await?;

        // Execute swap
        let (amount0_out, amount1_out) = if zero_for_one {
            (U256::ZERO, expected_out)
        } else {
            (expected_out, U256::ZERO)
        };

        let swap_call = pool.swap(amount0_out, amount1_out, from_address, Bytes::new());

        let gas_limit = gas_limit_for_hops(1);
        info!(
            target: "swap_executor",
            gas_limit = gas_limit,
            hops = 1,
            "Using hop-based gas limit for V2 swap"
        );

        let pending_tx = swap_call.gas(gas_limit).send().await?;

        let tx_hash = pending_tx.watch().await?;

        info!(
            target: "swap_executor",
            tx = %tx_hash,
            amount_out = %expected_out,
            "✅ V2 swap completed"
        );

        Ok(expected_out)
    }

    /// Execute a Uniswap V3 style swap (Agni)
    async fn execute_v3_swap<P: Provider>(
        provider: &P,
        swap_step: &SwapStep,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<U256> {
        let router_address = swap_step
            .router_address
            .or(config.v3_router_address)
            .ok_or_else(|| eyre!("No router address configured for V3 swap"))?;
        let router = IAgniSwapRouter::new(router_address, provider);

        // Determine fee tier using provided config or pool query
        let fee: U24 = if let Some(fee) = swap_step.fee.map(U24::from) {
            fee
        } else {
            let pool = IAgniPool::new(swap_step.pool_address, provider);
            match pool.fee().call().await {
                Ok(fee) => fee,
                Err(e) => {
                    error!(
                        target: "swap_executor",
                        pool = %swap_step.pool_address,
                        error = ?e,
                        "Failed to fetch fee tier, defaulting to 3000"
                    );
                    U24::from(3000_u32)
                }
            }
        };
        let fee_u32: u32 = fee.to::<u32>();

        info!(
            target: "swap_executor",
            pool = %swap_step.pool_address,
            pool_type = "UniV3",
            token_in = %swap_step.token_in,
            token_out = %swap_step.token_out,
            amount_in = %swap_step.amount_in,
            router = %router_address,
            fee_bps = fee_u32,
            "Executing V3 swap via router"
        );

        // Approve router to spend tokens
        Self::ensure_approval(
            provider,
            swap_step.token_in,
            router_address,
            swap_step.amount_in,
            from_address,
        )
        .await?;

        let params = IAgniSwapRouter::ExactInputSingleParams {
            tokenIn: swap_step.token_in,
            tokenOut: swap_step.token_out,
            fee,
            recipient: from_address,
            deadline: U256::MAX,
            amountIn: swap_step.amount_in,
            amountOutMinimum: U256::ZERO,
            sqrtPriceLimitX96: swap_step.sqrt_price_limit.unwrap_or(U160::from(0u64)),
        };

        let swap_call = router.exactInputSingle(params);

        let gas_limit = gas_limit_for_hops(1);
        info!(
            target: "swap_executor",
            gas_limit = gas_limit,
            hops = 1,
            "Using hop-based gas limit for V3 swap via router"
        );

        let pending_tx = swap_call.gas(gas_limit).send().await?;
        let tx_hash = *pending_tx.tx_hash();
        pending_tx.watch().await?;

        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| eyre!("Router swap transaction missing receipt after inclusion"))?;

        if !receipt.status() {
            error!(
                target: "swap_executor",
                tx = %receipt.transaction_hash,
                "❌ V3 swap via router reverted"
            );
            return Err(eyre!("Router swap transaction reverted"));
        }

        let token_out_contract = IERC20::new(swap_step.token_out, provider);
        let amount_out = token_out_contract.balanceOf(from_address).call().await?;

        info!(
            target: "swap_executor",
            tx = %receipt.transaction_hash,
            amount_out = %amount_out,
            "✅ V3 swap via router completed"
        );

        Ok(amount_out)
    }

    /// Execute a Moe Liquidity Book style swap via router
    async fn execute_moe_lb_swap<P: Provider>(
        provider: &P,
        swap_step: &SwapStep,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<U256> {
        // Get router address from config or swap_step
        let router_address = swap_step
            .router_address
            .or(config.moe_router_address)
            .ok_or_else(|| eyre!("No Moe router address configured for Moe LB swap"))?;

        let router = ILBRouter::new(router_address, provider);

        // Get bin step
        let bin_step = swap_step
            .bin_step
            .ok_or_else(|| eyre!("Bin step not provided for Moe LB swap"))?;

        info!(
            target: "swap_executor",
            pool = %swap_step.pool_address,
            pool_type = "MoeLB",
            token_in = %swap_step.token_in,
            token_out = %swap_step.token_out,
            amount_in = %swap_step.amount_in,
            bin_step = bin_step,
            router = %router_address,
            "Executing Moe LB swap via router"
        );

        // Approve router to spend tokens
        Self::ensure_approval(
            provider,
            swap_step.token_in,
            router_address,
            swap_step.amount_in,
            from_address,
        )
        .await?;

        // Build Path structure for router
        // Version: V2_1 = 2 for current Moe LB
        let path = ILBRouter::Path {
            pairBinSteps: vec![U256::from(bin_step)],
            versions: vec![2u8], // V2_1
            tokenPath: vec![swap_step.token_in, swap_step.token_out],
        };

        // Execute swap via router
        let swap_call = router.swapExactTokensForTokens(
            swap_step.amount_in,
            U256::ZERO, // amountOutMin - we'll check slippage separately
            path,
            from_address,
            U256::MAX, // deadline - effectively no deadline for now
        );

        let gas_limit = gas_limit_for_hops(1);
        info!(
            target: "swap_executor",
            gas_limit = gas_limit,
            hops = 1,
            "Using hop-based gas limit for Moe LB swap via router"
        );

        let pending_tx = swap_call.gas(gas_limit).send().await?;
        let tx_hash = *pending_tx.tx_hash();
        pending_tx.watch().await?;

        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| eyre!("Moe LB swap via router transaction missing receipt"))?;

        if !receipt.status() {
            error!(
                target: "swap_executor",
                tx = %receipt.transaction_hash,
                "❌ Moe LB swap via router reverted"
            );
            return Err(eyre!("Moe LB swap via router transaction reverted"));
        }

        // Get amount out from token balance
        let token_out_contract = IERC20::new(swap_step.token_out, provider);
        let amount_out = token_out_contract.balanceOf(from_address).call().await?;

        info!(
            target: "swap_executor",
            tx = %receipt.transaction_hash,
            amount_out = %amount_out,
            "✅ Moe LB swap via router completed"
        );

        Ok(amount_out)
    }

    /// Execute multiple swap steps in sequence
    pub async fn execute_multi_swap<P: Provider>(
        provider: &P,
        swap_steps: Vec<SwapStep>,
        from_address: Address,
        config: &ExecutorConfig,
    ) -> Result<Vec<U256>> {
        let mut results = Vec::new();
        let mut current_amount = swap_steps
            .first()
            .map(|s| s.amount_in)
            .unwrap_or(U256::ZERO);

        for (i, mut step) in swap_steps.into_iter().enumerate() {
            info!(
                target: "swap_executor",
                step = i + 1,
                pool = %step.pool_address,
                amount_in = %current_amount,
                "Executing swap step {}/{}",
                i + 1,
                results.len() + 1
            );

            // Update amount for this step
            step.amount_in = current_amount;

            // Execute swap
            match Self::execute_swap(provider, &step, from_address, config).await {
                Ok(amount_out) => {
                    info!(
                        target: "swap_executor",
                        step = i + 1,
                        amount_out = %amount_out,
                        "✅ Step {} completed",
                        i + 1
                    );
                    current_amount = amount_out;
                    results.push(amount_out);
                }
                Err(e) => {
                    error!(
                        target: "swap_executor",
                        step = i + 1,
                        error = ?e,
                        "❌ Step {} failed",
                        i + 1
                    );
                    return Err(e);
                }
            }
        }

        Ok(results)
    }

    /// Ensure token approval for a spender
    async fn ensure_approval<P: Provider>(
        provider: &P,
        token: Address,
        spender: Address,
        amount: U256,
        owner: Address,
    ) -> Result<()> {
        let token_contract = IERC20::new(token, provider);

        // Check current allowance
        let current_allowance = token_contract.allowance(owner, spender).call().await?;

        if current_allowance < amount {
            info!(
                target: "swap_executor",
                token = %token,
                spender = %spender,
                current_allowance = %current_allowance,
                required = %amount,
                "Approving tokens"
            );

            let approve_tx = token_contract.approve(spender, amount).send().await?;
            let approve_hash = approve_tx.watch().await?;

            info!(
                target: "swap_executor",
                tx = %approve_hash,
                "✅ Approval confirmed"
            );
        }

        Ok(())
    }

    /// Build a swap step with automatic pool type detection
    pub async fn build_swap_step<P: Provider>(
        provider: &P,
        pool_address: Address,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<SwapStep> {
        // Try to detect pool type by checking pool-specific functions
        
        // First, try Moe LBPair (check for getTokenX)
        let pool_moe_lb = IMoeLBPair::new(pool_address, provider);
        if let Ok(token_x) = pool_moe_lb.getTokenX().call().await {
            let token_y = pool_moe_lb.getTokenY().call().await?;
            let bin_step = pool_moe_lb.getBinStep().call().await?;
            
            // Determine swap direction
            let swap_for_y = if token_in == token_x && token_out == token_y {
                Some(true)
            } else if token_in == token_y && token_out == token_x {
                Some(false)
            } else {
                error!(
                    target: "swap_executor",
                    pool = %pool_address,
                    token_in = %token_in,
                    token_out = %token_out,
                    token_x = %token_x,
                    token_y = %token_y,
                    "Token mismatch in Moe LBPair"
                );
                return Err(eyre!("Token mismatch in Moe LBPair"));
            };
            
            info!(
                target: "swap_executor",
                pool = %pool_address,
                detected_type = "MoeLB",
                swap_for_y = ?swap_for_y,
                bin_step = bin_step,
                "Detected Moe LBPair"
            );
            
            return Ok(SwapStep {
                pool_address,
                pool_type: PoolType::MoeLB,
                token_in,
                token_out,
                amount_in,
                expected_amount_out: None,
                sqrt_price_limit: None,
                zero_for_one: None,
                fee: None,
                router_address: None,
                swap_for_y,
                bin_step: Some(bin_step),
            });
        }
        
        // Try to detect V3 pool (check for liquidity function)
        let pool_v3 = IAgniPool::new(pool_address, provider);
        let pool_type = match pool_v3.liquidity().call().await {
            Ok(_) => PoolType::UniV3,
            Err(_) => PoolType::UniV2,
        };

        let fee = if pool_type == PoolType::UniV3 {
            match pool_v3.fee().call().await {
                Ok(fee) => Some(fee.to::<u32>()),
                Err(e) => {
                    error!(
                        target: "swap_executor",
                        pool = %pool_address,
                        error = ?e,
                        "Failed to fetch V3 fee tier"
                    );
                    None
                }
            }
        } else {
            None
        };

        info!(
            target: "swap_executor",
            pool = %pool_address,
            detected_type = ?pool_type,
            "Detected pool type"
        );

        Ok(SwapStep {
            pool_address,
            pool_type,
            token_in,
            token_out,
            amount_in,
            expected_amount_out: None,
            sqrt_price_limit: None,
            zero_for_one: None,
            fee,
            router_address: None,
            swap_for_y: None,
            bin_step: None,
        })
    }
}
