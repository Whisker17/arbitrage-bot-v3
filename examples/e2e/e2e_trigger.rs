//! WHI-525 M5: repeatable price-imbalance trigger for the fixture V2 pool.
//!
//! Reads the deployment manifest written by `e2e_bootstrap` (M4), reads the
//! fixture V2 pool's current reserves, and sends a WMNT-in / fixture-token-out
//! swap sized off those live reserves — a real constant-product swap against
//! the real fixture contract, not a simulated one. This shifts the V2 pool's
//! price away from the (untouched) Agni V3 fixture pool's price, creating the
//! arb opportunity `e2e_run` (M6) exploits. Never redeploys or reconfigures
//! anything: safe to run repeatedly against the same manifest.
//!
//! No Sepolia private key or credential ever reaches this file directly: the
//! signer lives only inside [`E2eBootstrapAuthority`]'s private transport,
//! reached through [`validate_e2e_startup`] (WHI-555). This example only
//! mints and consumes one-shot trigger permits via [`VerifiedE2eManifest`].
//!
//! Usage:
//! ```text
//! MANTLE_SEPOLIA_E2E_RPC_URL=... \
//! MANTLE_SEPOLIA_E2E_PRIVATE_KEY=... \
//! MANTLE_SEPOLIA_E2E_EXECUTOR_ADDRESS=... \
//!   cargo run --example e2e_trigger -- --wmnt-in-wei 500000000000000000
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use alloy::network::TransactionBuilder;
use alloy::primitives::{Address, Bytes, B256, U256};
use alloy::providers::utils::Eip1559Estimation;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol;
use clap::Parser;
use eyre::{bail, Context, Result};
use tokio::time::sleep;

use amms::execution::contract::IWMNT;
use amms::execution::e2e::{
    load_deployment_manifest, load_harness_config, validate_e2e_startup, E2eBootstrapAuthority,
    EnvSource, ProcessEnvSource, VerifiedE2eManifest, ENV_E2E_RPC_URL,
};
use amms::execution::pipeline::NoopDurableHook;

const FEE_DENOM: u64 = 100_000;
const FIXTURE_V2_FEE_BPS: u64 = 300;

const GAS_HEADROOM_NUMERATOR: u64 = 120;
const GAS_HEADROOM_DENOMINATOR: u64 = 100;

const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const RECEIPT_POLL_ATTEMPTS: u32 = 100;

sol! {
    #[sol(rpc)]
    interface IFixturePoolV2Trigger {
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
        function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
    }
}

/// Standard Uniswap-V2 `getAmountOut`: floor division here always leaves the
/// post-swap balances satisfying `E2EFixturePoolV2.swap`'s own constant-product
/// invariant check (`fee` is charged against the input side, matching the
/// contract's `balanceAdjusted = balance * FEE_DENOM - amountIn * fee`).
fn get_amount_out(amount_in: U256, reserve_in: U256, reserve_out: U256) -> U256 {
    let amount_in_with_fee = amount_in * U256::from(FEE_DENOM - FIXTURE_V2_FEE_BPS);
    let numerator = amount_in_with_fee * reserve_out;
    let denominator = reserve_in * U256::from(FEE_DENOM) + amount_in_with_fee;
    numerator / denominator
}

#[derive(Debug, Parser)]
#[command(about = "WHI-525 M5: trigger a repeatable price imbalance in the fixture V2 pool")]
struct Args {
    #[arg(long, default_value = "config/e2e_sepolia.json")]
    config: PathBuf,
    /// Amount of WMNT (in wei) to swap into the fixture V2 pool for fixture
    /// tokens. Defaults to 0.5 WMNT — large enough to move the fixture V2
    /// pool's price measurably away from the untouched Agni V3 fixture pool
    /// without exhausting the pool's seeded fixture-token liquidity.
    #[arg(long, default_value_t = 500_000_000_000_000_000u128)]
    wmnt_in_wei: u128,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("e2e_trigger failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let env = ProcessEnvSource;
    let startup = validate_e2e_startup(&env).context("validating E2E startup env")?;

    // `validate_e2e_startup` validates the RPC URL's shape and then discards
    // it: re-read only to build the provider. Never attach the raw URL to an
    // error chain (userinfo/query tokens must not hit stderr).
    let rpc_url = env
        .get(ENV_E2E_RPC_URL)
        .ok_or_else(|| eyre::eyre!("{ENV_E2E_RPC_URL} unexpectedly absent after validation"))?;
    let rpc_http = rpc_url
        .parse()
        .map_err(|_| eyre::eyre!("invalid E2E RPC URL shape after validation"))?;
    let provider = ProviderBuilder::new().connect_http(rpc_http).erased();

    let authority = E2eBootstrapAuthority::establish(startup, provider.clone())
        .await
        .context("establishing E2E bootstrap authority")?;

    let harness_config =
        load_harness_config(&args.config).context("loading committed E2E harness config")?;
    if harness_config.chain_id != authority.chain_id() {
        bail!(
            "harness config chain id {} does not match live provider chain id {}",
            harness_config.chain_id,
            authority.chain_id()
        );
    }

    let manifest_path = PathBuf::from(&harness_config.manifest_path);
    if !manifest_path.exists() {
        bail!(
            "no deployment manifest at {} — run `cargo run --example e2e_bootstrap` first",
            manifest_path.display()
        );
    }
    let deployment =
        load_deployment_manifest(&manifest_path).context("loading recorded deployment manifest")?;

    let signer = authority.signer_address();
    let chain_id = authority.chain_id();
    let env_executor = authority.executor_address();
    let manifest = authority.finalize();

    let wmnt = deployment
        .wmnt
        .parse::<Address>()
        .context("parsing manifest WMNT address")?;
    let fixture_token = deployment
        .fixture_token
        .parse::<Address>()
        .context("parsing manifest fixture token address")?;
    let pool_v2 = deployment
        .fixture_pool_v2
        .parse::<Address>()
        .context("parsing manifest fixture V2 pool address")?;
    let deployment_executor = deployment
        .executor_address
        .parse::<Address>()
        .context("parsing manifest executor address")?;
    if env_executor != deployment_executor {
        bail!(
            "MANTLE_SEPOLIA_E2E_EXECUTOR_ADDRESS ({env_executor}) does not match the \
             deployment manifest executor ({deployment_executor}) — refuse to run against a \
             mismatched identity"
        );
    }

    let pool = IFixturePoolV2Trigger::new(pool_v2, &provider);
    let reserves = pool
        .getReserves()
        .call()
        .await
        .context("reading fixture V2 pool reserves")?;

    let wmnt_is_token0 = wmnt < fixture_token;
    let (reserve_wmnt, reserve_fixture) = if wmnt_is_token0 {
        (reserves.reserve0.to::<u128>(), reserves.reserve1.to::<u128>())
    } else {
        (reserves.reserve1.to::<u128>(), reserves.reserve0.to::<u128>())
    };

    let amount_in = U256::from(args.wmnt_in_wei);
    let amount_out = get_amount_out(
        amount_in,
        U256::from(reserve_wmnt),
        U256::from(reserve_fixture),
    );
    if amount_out.is_zero() {
        bail!("computed a zero output amount for the requested --wmnt-in-wei — increase it");
    }

    let (amount0_out, amount1_out) = if wmnt_is_token0 {
        (U256::ZERO, amount_out)
    } else {
        (amount_out, U256::ZERO)
    };

    let fees = provider
        .estimate_eip1559_fees()
        .await
        .context("estimating EIP-1559 fees")?;
    let mut nonce = provider
        .get_transaction_count(signer)
        .await
        .context("fetching signer's starting nonce")?;

    let wmnt_contract = IWMNT::new(wmnt, &provider);

    let deposit_calldata = wmnt_contract.deposit().calldata().clone();
    let deposit_receipt = send_trigger_tx(
        &manifest,
        &provider,
        "trigger_deposit_wmnt",
        chain_id,
        signer,
        wmnt,
        amount_in,
        deposit_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let transfer_calldata = wmnt_contract
        .transfer(pool_v2, amount_in)
        .calldata()
        .clone();
    let transfer_receipt = send_trigger_tx(
        &manifest,
        &provider,
        "trigger_transfer_wmnt_to_pool",
        chain_id,
        signer,
        wmnt,
        U256::ZERO,
        transfer_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let swap_calldata = pool
        .swap(amount0_out, amount1_out, signer, Bytes::new())
        .calldata()
        .clone();
    let swap_receipt = send_trigger_tx(
        &manifest,
        &provider,
        "trigger_swap",
        chain_id,
        signer,
        pool_v2,
        U256::ZERO,
        swap_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;

    // Print every trigger tx hash so operators can feed them into
    // `e2e_run --trigger-tx-hash` for the evidence bundle.
    println!(
        "trigger complete: pool_v2={pool_v2} wmnt_in={amount_in} fixture_token_out={amount_out}"
    );
    println!(
        "trigger_tx_hash deposit={} transfer={} swap={}",
        deposit_receipt.transaction_hash,
        transfer_receipt.transaction_hash,
        swap_receipt.transaction_hash
    );
    println!(
        "e2e_run --trigger-tx-hash {} --trigger-tx-hash {} --trigger-tx-hash {}",
        deposit_receipt.transaction_hash,
        transfer_receipt.transaction_hash,
        swap_receipt.transaction_hash
    );

    Ok(())
}

/// Build, gas-estimate (with a 20% headroom), mint, sign, broadcast, and wait
/// for a canonical receipt for one trigger transaction. Bails if the receipt
/// reports a reverted status.
#[allow(clippy::too_many_arguments)]
async fn send_trigger_tx(
    manifest: &VerifiedE2eManifest,
    provider: &impl Provider,
    label: &str,
    chain_id: u64,
    from: Address,
    to: Address,
    value: U256,
    input: Vec<u8>,
    nonce: u64,
    fees: Eip1559Estimation,
) -> Result<TransactionReceipt> {
    let mut tx = TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(from)
        .with_to(to)
        .with_nonce(nonce)
        .with_value(value)
        .with_input(Bytes::from(input))
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);

    let estimated_gas = provider
        .estimate_gas(tx.clone())
        .await
        .with_context(|| format!("estimating gas for {label}"))?;
    let gas_limit = estimated_gas * GAS_HEADROOM_NUMERATOR / GAS_HEADROOM_DENOMINATOR;
    tx = tx.with_gas_limit(gas_limit);

    let permit = manifest
        .mint_trigger_permit(tx)
        .with_context(|| format!("minting trigger permit for {label}"))?;
    let submission = manifest
        .sign(permit, &NoopDurableHook)
        .await
        .with_context(|| format!("signing trigger tx for {label}"))?;
    let tx_hash = submission.tx_hash();
    manifest
        .broadcast(submission)
        .await
        .with_context(|| format!("broadcasting {label}"))?;

    let receipt = wait_for_receipt(provider, tx_hash)
        .await
        .with_context(|| format!("waiting for {label} receipt"))?;
    if !receipt.status() {
        bail!("{label} transaction {tx_hash} reverted on-chain");
    }

    Ok(receipt)
}

async fn wait_for_receipt<P: Provider>(provider: &P, tx_hash: B256) -> Result<TransactionReceipt> {
    for _ in 0..RECEIPT_POLL_ATTEMPTS {
        if let Some(receipt) = provider
            .get_transaction_receipt(tx_hash)
            .await
            .context("polling for transaction receipt")?
        {
            return Ok(receipt);
        }
        sleep(RECEIPT_POLL_INTERVAL).await;
    }
    bail!("transaction {tx_hash} did not confirm after {RECEIPT_POLL_ATTEMPTS} poll attempts")
}
