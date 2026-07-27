//! WHI-525 M4: idempotent bootstrap for the Mantle Sepolia E2E harness.
//!
//! On first run (no manifest on disk at `harness_config.manifest_path`):
//! deploys the three fixture contracts (`FixtureERC20`, `E2EFixturePoolV2`,
//! `E2EFixturePoolAgniV3`) and `ArbitrageExecutor` via the credential-isolated
//! [`amms::execution::e2e`] capability layer (WHI-555), registers both pools
//! and the signer as hot executor, seeds each pool with fixture liquidity,
//! resolves + verifies the deployed executor's runtime identity (WHI-551),
//! and writes a [`DeploymentManifest`] (WHI-521/M3) to disk.
//!
//! On a later run (manifest already present): re-derives every
//! live-comparable field from current chain state and on-disk artifacts,
//! diffs against the recorded manifest via [`diff_against_chain`], and exits
//! 0 ("reused") on an exact match or fails closed with a structured diff on
//! drift. It never overwrites or redeploys on drift — that is an operator
//! decision.
//!
//! No Sepolia private key or credential ever reaches this file directly: the
//! signer lives only inside [`E2eBootstrapAuthority`]'s private transport,
//! reached through [`validate_e2e_startup`] (WHI-555). This example only
//! mints and consumes one-shot bootstrap permits.
//!
//! Usage:
//! ```text
//! MANTLE_SEPOLIA_E2E_RPC_URL=... \
//! MANTLE_SEPOLIA_E2E_PRIVATE_KEY=... \
//! MANTLE_SEPOLIA_E2E_EXECUTOR_ADDRESS=... \
//!   cargo run --example e2e_bootstrap
//! ```

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use alloy::network::{ReceiptResponse, TransactionBuilder};
use alloy::primitives::aliases::U24;
use alloy::primitives::{keccak256, Address, Bytes, TxKind, B256, U160, U256};
use alloy::providers::utils::Eip1559Estimation;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{TransactionReceipt, TransactionRequest};
use alloy::sol_types::SolValue;
use clap::Parser;
use eyre::{bail, Context, Result};
use tokio::time::sleep;

use amms::execution::contract::{IArbitrageExecutor, IWMNT};
use amms::execution::e2e::{
    arbitrage_executor_init_code, diff_against_chain, fixture_erc20_init_code,
    fixture_pool_agni_v3_init_code, fixture_pool_v2_init_code, load_creation_bytecode,
    load_deployment_manifest, load_harness_config, validate_e2e_startup,
    write_deployment_manifest, BootstrapAction, DeploymentManifest, E2eBootstrapAuthority,
    EnvSource, HarnessConfig, IFixtureErc20, IFixturePoolAgniV3Seed, IFixturePoolV2Seed,
    ProcessEnvSource, RoleHolders, TxRecord, VenueProvenance, ENV_E2E_RPC_URL,
};
use amms::execution::gas_profile::load_artifact;
use amms::execution::runtime_identity::{
    build_export, resolve_immutable_plan, verify_deployed_runtime, BuildEvidence,
    ExecutorIdentityExport, ImmutableInputs,
};
use amms::signing::canonical::canonicalize_value;

const FIXTURE_TOKEN_NAME: &str = "E2E Fixture Token";
const FIXTURE_TOKEN_SYMBOL: &str = "eTOK";
const FIXTURE_TOKEN_DECIMALS: u8 = 18;

const FIXTURE_V2_FEE_BPS: u64 = 300;
const FIXTURE_AGNI_V3_FEE: u32 = 3000;

const POOL_TYPE_V2: u8 = 0;
const POOL_TYPE_AGNI_V3: u8 = 1;

const ETHER: u128 = 1_000_000_000_000_000_000;
const FIXTURE_TOKEN_MINT_PER_POOL_WEI: u128 = 1_000_000 * ETHER;
const WMNT_PER_POOL_WEI: u128 = ETHER;

const GAS_HEADROOM_NUMERATOR: u64 = 120;
const GAS_HEADROOM_DENOMINATOR: u64 = 100;

const RECEIPT_POLL_INTERVAL: Duration = Duration::from_secs(3);
const RECEIPT_POLL_ATTEMPTS: u32 = 100;

fn ether(n: u64) -> U256 {
    U256::from(n) * U256::from(ETHER)
}

/// Approximate `sqrtPriceX96` from two wei reserves. Precision here only
/// needs to be "good enough to seed a fixture pool that isn't already at a
/// degenerate price" — the E2E trigger step (M5) is what actually creates the
/// price imbalance a real arb cycle exploits.
fn approx_sqrt_price_x96(reserve0_wei: u128, reserve1_wei: u128) -> U160 {
    let ratio = reserve1_wei as f64 / reserve0_wei as f64;
    let sqrt_price = (ratio.sqrt() * 2f64.powi(96)) as u128;
    U160::from(sqrt_price)
}

fn approx_liquidity(reserve0_wei: u128, reserve1_wei: u128) -> u128 {
    ((reserve0_wei as f64) * (reserve1_wei as f64)).sqrt() as u128
}

#[derive(Debug, Parser)]
#[command(about = "WHI-525 M4: idempotent Mantle Sepolia E2E fixture bootstrap")]
struct Args {
    #[arg(long, default_value = "config/e2e_sepolia.json")]
    config: PathBuf,
    #[arg(long, default_value = "contracts/fixtures/artifacts")]
    fixture_artifacts: PathBuf,
    #[arg(long, default_value = "contracts/executor/artifacts")]
    executor_artifacts: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("e2e_bootstrap failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    let env = ProcessEnvSource;
    let startup = validate_e2e_startup(&env).context("validating E2E startup env")?;

    // `validate_e2e_startup` validates the RPC URL's shape and then discards
    // it (see `env_guard.rs`'s doc comment): this is the one place that reads
    // it again, only to build the provider, and never retains it past that.
    let rpc_url = env
        .get(ENV_E2E_RPC_URL)
        .ok_or_else(|| eyre::eyre!("{ENV_E2E_RPC_URL} unexpectedly absent after validation"))?;
    let provider = ProviderBuilder::new()
        .connect_http(rpc_url.parse().context("parsing E2E RPC URL")?)
        .erased();

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
    if manifest_path.exists() {
        reconcile_existing_manifest(&args, &harness_config, &manifest_path, &provider).await
    } else {
        bootstrap_fresh_deployment(&args, &harness_config, &manifest_path, &authority, &provider)
            .await
    }
}

async fn bootstrap_fresh_deployment(
    args: &Args,
    harness_config: &HarnessConfig,
    manifest_path: &Path,
    authority: &E2eBootstrapAuthority,
    provider: &impl Provider,
) -> Result<()> {
    let chain_id = authority.chain_id();
    let signer = authority.signer_address();
    let wmnt = harness_config
        .wmnt
        .parse::<Address>()
        .context("parsing harness config WMNT address")?;

    let fixture_token_bytecode = load_creation_bytecode(
        &args.fixture_artifacts.join("FixtureERC20.full.json"),
    )
    .context("loading FixtureERC20 creation bytecode")?;
    let pool_v2_bytecode = load_creation_bytecode(
        &args.fixture_artifacts.join("E2EFixturePoolV2.full.json"),
    )
    .context("loading E2EFixturePoolV2 creation bytecode")?;
    let pool_agni_v3_bytecode = load_creation_bytecode(
        &args
            .fixture_artifacts
            .join("E2EFixturePoolAgniV3.full.json"),
    )
    .context("loading E2EFixturePoolAgniV3 creation bytecode")?;
    let executor_bytecode = load_creation_bytecode(
        &args.executor_artifacts.join("ArbitrageExecutor.full.json"),
    )
    .context("loading ArbitrageExecutor creation bytecode")?;

    let starting_nonce = provider
        .get_transaction_count(signer)
        .await
        .context("fetching signer's starting nonce")?;

    let predicted_fixture_token = signer.create(starting_nonce);
    let predicted_pool_v2 = signer.create(starting_nonce + 1);
    let predicted_pool_agni_v3 = signer.create(starting_nonce + 2);
    let predicted_executor = signer.create(starting_nonce + 3);

    if predicted_executor != authority.executor_address() {
        bail!(
            "predicted executor address {predicted_executor} (at nonce {}) does not match the \
             configured executor address {} — refusing to deploy against a mismatched signer/nonce",
            starting_nonce + 3,
            authority.executor_address()
        );
    }

    let (token0, token1) = if predicted_fixture_token < wmnt {
        (predicted_fixture_token, wmnt)
    } else {
        (wmnt, predicted_fixture_token)
    };

    let fees = provider
        .estimate_eip1559_fees()
        .await
        .context("estimating EIP-1559 fees")?;

    let mut nonce = starting_nonce;

    let fixture_token_init_code = fixture_erc20_init_code(
        &fixture_token_bytecode,
        FIXTURE_TOKEN_NAME,
        FIXTURE_TOKEN_SYMBOL,
        FIXTURE_TOKEN_DECIMALS,
    );
    let (fixture_token_record, fixture_token_receipt) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Deploy,
        "deploy_fixture_token",
        chain_id,
        signer,
        None,
        U256::ZERO,
        fixture_token_init_code,
        nonce,
        fees,
    )
    .await?;
    verify_predicted_deploy_address(
        "fixture token",
        &fixture_token_receipt,
        predicted_fixture_token,
    )?;
    nonce += 1;

    let pool_v2_init_code = fixture_pool_v2_init_code(
        &pool_v2_bytecode,
        token0,
        token1,
        U256::from(FIXTURE_V2_FEE_BPS),
    );
    let (pool_v2_record, pool_v2_receipt) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Deploy,
        "deploy_pool_v2",
        chain_id,
        signer,
        None,
        U256::ZERO,
        pool_v2_init_code,
        nonce,
        fees,
    )
    .await?;
    verify_predicted_deploy_address("fixture V2 pool", &pool_v2_receipt, predicted_pool_v2)?;
    nonce += 1;

    let pool_agni_v3_init_code = fixture_pool_agni_v3_init_code(
        &pool_agni_v3_bytecode,
        token0,
        token1,
        U24::from(FIXTURE_AGNI_V3_FEE),
    );
    let (pool_agni_v3_record, pool_agni_v3_receipt) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Deploy,
        "deploy_pool_agni_v3",
        chain_id,
        signer,
        None,
        U256::ZERO,
        pool_agni_v3_init_code,
        nonce,
        fees,
    )
    .await?;
    verify_predicted_deploy_address(
        "fixture Agni V3 pool",
        &pool_agni_v3_receipt,
        predicted_pool_agni_v3,
    )?;
    nonce += 1;

    let executor_init_code = arbitrage_executor_init_code(&executor_bytecode, wmnt, signer);
    let (executor_record, executor_receipt) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Deploy,
        "deploy_executor",
        chain_id,
        signer,
        None,
        U256::ZERO,
        executor_init_code,
        nonce,
        fees,
    )
    .await?;
    verify_predicted_deploy_address("executor", &executor_receipt, predicted_executor)?;
    nonce += 1;

    let executor = IArbitrageExecutor::new(predicted_executor, provider);

    let register_v2_calldata = executor
        .registerPool(predicted_pool_v2, POOL_TYPE_V2)
        .calldata()
        .clone();
    let (register_v2_record, _) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Config,
        "register_pool_v2",
        chain_id,
        signer,
        Some(predicted_executor),
        U256::ZERO,
        register_v2_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let register_agni_v3_calldata = executor
        .registerPool(predicted_pool_agni_v3, POOL_TYPE_AGNI_V3)
        .calldata()
        .clone();
    let (register_agni_v3_record, _) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Config,
        "register_pool_agni_v3",
        chain_id,
        signer,
        Some(predicted_executor),
        U256::ZERO,
        register_agni_v3_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let set_hot_executor_calldata = executor
        .setHotExecutor(signer, true)
        .calldata()
        .clone();
    let (set_hot_executor_record, _) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::Config,
        "set_hot_executor",
        chain_id,
        signer,
        Some(predicted_executor),
        U256::ZERO,
        set_hot_executor_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let config_txs = vec![register_v2_record, register_agni_v3_record, set_hot_executor_record];

    let wmnt_contract = IWMNT::new(wmnt, provider);
    let deposit_calldata = wmnt_contract.deposit().calldata().clone();
    let (deposit_record, _) = send_bootstrap_tx(
        authority,
        provider,
        BootstrapAction::InitialSeed,
        "deposit_wmnt",
        chain_id,
        signer,
        Some(wmnt),
        ether(2),
        deposit_calldata.to_vec(),
        nonce,
        fees,
    )
    .await?;
    nonce += 1;

    let fixture_token_contract = IFixtureErc20::new(predicted_fixture_token, provider);
    let mut seed_txs = vec![deposit_record];

    for (label_prefix, pool_address, is_v2) in [
        ("v2", predicted_pool_v2, true),
        ("agni_v3", predicted_pool_agni_v3, false),
    ] {
        let mint_calldata = fixture_token_contract
            .mint(pool_address, U256::from(FIXTURE_TOKEN_MINT_PER_POOL_WEI))
            .calldata()
            .clone();
        let (mint_record, _) = send_bootstrap_tx(
            authority,
            provider,
            BootstrapAction::InitialSeed,
            &format!("mint_fixture_token_{label_prefix}"),
            chain_id,
            signer,
            Some(predicted_fixture_token),
            U256::ZERO,
            mint_calldata.to_vec(),
            nonce,
            fees,
        )
        .await?;
        nonce += 1;
        seed_txs.push(mint_record);

        let transfer_calldata = wmnt_contract
            .transfer(pool_address, U256::from(WMNT_PER_POOL_WEI))
            .calldata()
            .clone();
        let (transfer_record, _) = send_bootstrap_tx(
            authority,
            provider,
            BootstrapAction::InitialSeed,
            &format!("transfer_wmnt_{label_prefix}"),
            chain_id,
            signer,
            Some(wmnt),
            U256::ZERO,
            transfer_calldata.to_vec(),
            nonce,
            fees,
        )
        .await?;
        nonce += 1;
        seed_txs.push(transfer_record);

        let seed_calldata = if is_v2 {
            IFixturePoolV2Seed::new(pool_address, provider)
                .seed()
                .calldata()
                .clone()
        } else {
            IFixturePoolAgniV3Seed::new(pool_address, provider)
                .seed(
                    approx_sqrt_price_x96(FIXTURE_TOKEN_MINT_PER_POOL_WEI, WMNT_PER_POOL_WEI),
                    approx_liquidity(FIXTURE_TOKEN_MINT_PER_POOL_WEI, WMNT_PER_POOL_WEI),
                )
                .calldata()
                .clone()
        };
        let (seed_record, _) = send_bootstrap_tx(
            authority,
            provider,
            BootstrapAction::InitialSeed,
            &format!("seed_pool_{label_prefix}"),
            chain_id,
            signer,
            Some(pool_address),
            U256::ZERO,
            seed_calldata.to_vec(),
            nonce,
            fees,
        )
        .await?;
        nonce += 1;
        seed_txs.push(seed_record);
    }

    let evidence = BuildEvidence::load(&args.executor_artifacts)
        .context("loading executor build evidence")?;
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt }, chain_id)
        .context("resolving immutable plan")?;
    let on_chain_code = provider
        .get_code_at(predicted_executor)
        .await
        .context("reading deployed executor code")?;
    verify_deployed_runtime(&on_chain_code, &plan).context("verifying deployed runtime identity")?;
    let identity_export = build_export(&plan);

    if let Some(pinned) = load_pinned_identity(&harness_config.executor_identity_path)? {
        if pinned.identity_digest != identity_export.identity_digest {
            bail!(
                "deployed executor identity_digest {} does not match the pinned {} at {}",
                identity_export.identity_digest,
                pinned.identity_digest,
                harness_config.executor_identity_path
            );
        }
    }

    let gas_profile = load_artifact(Path::new(&harness_config.gas_profile_path))
        .context("loading gas profile artifact")?;
    let e2e_config_digest = keccak256(
        canonicalize_value(&serde_json::to_value(harness_config)?)
            .context("canonicalizing harness config for digest")?,
    );
    let constructor_args_digest = keccak256((wmnt, signer).abi_encode_params());

    let manifest = DeploymentManifest {
        schema_version: amms::execution::e2e::DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
        chain_id,
        wmnt: wmnt.to_string(),
        executor_address: predicted_executor.to_string(),
        fixture_token: predicted_fixture_token.to_string(),
        fixture_pool_v2: predicted_pool_v2.to_string(),
        fixture_pool_agni_v3: predicted_pool_agni_v3.to_string(),
        venue_provenance: VenueProvenance::Fixture,
        roles: RoleHolders {
            admin: signer.to_string(),
            hot_executor: signer.to_string(),
        },
        template_hash: identity_export.template_hash.clone(),
        patched_runtime_hash: identity_export.patched_runtime_hash.clone(),
        immutable_values_digest: identity_export.immutable_values_digest.clone(),
        compiler_config_digest: identity_export.compiler_config_digest.clone(),
        build_info_digest: identity_export.build_info_digest.clone(),
        storage_layout_digest: identity_export.storage_layout_digest.clone(),
        plan_digest: identity_export.plan_digest.clone(),
        identity_digest: identity_export.identity_digest.clone(),
        constructor_args_digest: constructor_args_digest.to_string(),
        gas_profile_content_digest: gas_profile.content_digest.clone(),
        e2e_config_digest: e2e_config_digest.to_string(),
        deploy_txs: vec![
            fixture_token_record,
            pool_v2_record,
            pool_agni_v3_record,
            executor_record,
        ],
        config_txs,
        seed_txs,
    };

    write_deployment_manifest(manifest_path, &manifest).context("writing deployment manifest")?;

    println!(
        "bootstrap complete: executor={} fixture_token={} pool_v2={} pool_agni_v3={}",
        predicted_executor, predicted_fixture_token, predicted_pool_v2, predicted_pool_agni_v3
    );

    Ok(())
}

fn load_pinned_identity(path: &str) -> Result<Option<ExecutorIdentityExport>> {
    let path = Path::new(path);
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading pinned executor identity at {}", path.display()))?;
    let parsed: ExecutorIdentityExport =
        serde_json::from_str(&raw).context("parsing pinned executor identity")?;
    Ok(Some(parsed))
}

async fn reconcile_existing_manifest(
    args: &Args,
    harness_config: &HarnessConfig,
    manifest_path: &Path,
    provider: &impl Provider,
) -> Result<()> {
    let recorded =
        load_deployment_manifest(manifest_path).context("loading recorded deployment manifest")?;

    let executor_address = recorded
        .executor_address
        .parse::<Address>()
        .context("parsing recorded executor address")?;
    let wmnt = recorded
        .wmnt
        .parse::<Address>()
        .context("parsing recorded WMNT address")?;

    let executor = IArbitrageExecutor::new(executor_address, provider);
    let admin = executor
        .admin()
        .call()
        .await
        .context("reading executor admin from chain")?;
    let is_hot_executor = executor
        .isHotExecutor(admin)
        .call()
        .await
        .context("reading hot-executor status from chain")?;
    if !is_hot_executor {
        bail!(
            "recorded admin {admin} is not a registered hot executor on-chain — the recorded \
             manifest no longer reflects live executor state"
        );
    }

    let evidence = BuildEvidence::load(&args.executor_artifacts)
        .context("loading executor build evidence")?;
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt }, recorded.chain_id)
        .context("resolving immutable plan")?;
    let on_chain_code = provider
        .get_code_at(executor_address)
        .await
        .context("reading deployed executor code")?;
    verify_deployed_runtime(&on_chain_code, &plan).context("verifying deployed runtime identity")?;
    let identity_export = build_export(&plan);

    let gas_profile = load_artifact(Path::new(&harness_config.gas_profile_path))
        .context("loading gas profile artifact")?;
    let e2e_config_digest = keccak256(
        canonicalize_value(&serde_json::to_value(harness_config)?)
            .context("canonicalizing harness config for digest")?,
    );
    let constructor_args_digest = keccak256((wmnt, admin).abi_encode_params());

    let observed = DeploymentManifest {
        schema_version: recorded.schema_version,
        chain_id: recorded.chain_id,
        wmnt: recorded.wmnt.clone(),
        executor_address: recorded.executor_address.clone(),
        fixture_token: recorded.fixture_token.clone(),
        fixture_pool_v2: recorded.fixture_pool_v2.clone(),
        fixture_pool_agni_v3: recorded.fixture_pool_agni_v3.clone(),
        venue_provenance: recorded.venue_provenance,
        roles: RoleHolders {
            admin: admin.to_string(),
            hot_executor: admin.to_string(),
        },
        template_hash: identity_export.template_hash,
        patched_runtime_hash: identity_export.patched_runtime_hash,
        immutable_values_digest: identity_export.immutable_values_digest,
        compiler_config_digest: identity_export.compiler_config_digest,
        build_info_digest: identity_export.build_info_digest,
        storage_layout_digest: identity_export.storage_layout_digest,
        plan_digest: identity_export.plan_digest,
        identity_digest: identity_export.identity_digest,
        constructor_args_digest: constructor_args_digest.to_string(),
        gas_profile_content_digest: gas_profile.content_digest,
        e2e_config_digest: e2e_config_digest.to_string(),
        deploy_txs: recorded.deploy_txs.clone(),
        config_txs: recorded.config_txs.clone(),
        seed_txs: recorded.seed_txs.clone(),
    };

    let drift = diff_against_chain(&recorded, &observed);
    if drift.is_empty() {
        println!(
            "reused existing manifest at {}: executor={}",
            manifest_path.display(),
            recorded.executor_address
        );
        Ok(())
    } else {
        eprintln!("manifest drift detected against live chain state:");
        for d in &drift {
            eprintln!("  {}: recorded={} observed={}", d.field, d.recorded, d.observed);
        }
        bail!(
            "{} field(s) drifted from the recorded manifest at {} — refusing to overwrite; \
             resolve manually",
            drift.len(),
            manifest_path.display()
        );
    }
}

/// Build, gas-estimate (with a 20% headroom), mint, sign, broadcast, and wait
/// for a canonical receipt for one bootstrap transaction. Bails if the
/// receipt reports a reverted status.
#[allow(clippy::too_many_arguments)]
async fn send_bootstrap_tx(
    authority: &E2eBootstrapAuthority,
    provider: &impl Provider,
    action: BootstrapAction,
    label: &str,
    chain_id: u64,
    from: Address,
    to: Option<Address>,
    value: U256,
    input: Vec<u8>,
    nonce: u64,
    fees: Eip1559Estimation,
) -> Result<(TxRecord, TransactionReceipt)> {
    let mut tx = TransactionRequest::default()
        .with_chain_id(chain_id)
        .with_from(from)
        .with_nonce(nonce)
        .with_value(value)
        .with_input(Bytes::from(input))
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    tx = match to {
        Some(addr) => tx.with_to(addr),
        None => tx.with_kind(TxKind::Create),
    };

    let estimated_gas = provider
        .estimate_gas(tx.clone())
        .await
        .with_context(|| format!("estimating gas for {label}"))?;
    let gas_limit = estimated_gas * GAS_HEADROOM_NUMERATOR / GAS_HEADROOM_DENOMINATOR;
    tx = tx.with_gas_limit(gas_limit);

    let permit = authority
        .mint_bootstrap_permit(action, tx)
        .with_context(|| format!("minting bootstrap permit for {label}"))?;
    let submission = authority
        .sign_bootstrap(permit)
        .await
        .with_context(|| format!("signing bootstrap tx for {label}"))?;
    let tx_hash = submission.tx_hash();
    authority
        .broadcast_bootstrap(submission)
        .await
        .with_context(|| format!("broadcasting {label}"))?;

    let receipt = wait_for_receipt(provider, tx_hash)
        .await
        .with_context(|| format!("waiting for {label} receipt"))?;
    if !receipt.status() {
        bail!("{label} transaction {tx_hash} reverted on-chain");
    }

    let record = TxRecord {
        label: label.to_string(),
        tx_hash: tx_hash.to_string(),
        block_number: receipt
            .block_number()
            .ok_or_else(|| eyre::eyre!("{label} receipt missing block_number"))?,
        block_hash: receipt
            .block_hash()
            .ok_or_else(|| eyre::eyre!("{label} receipt missing block_hash"))?
            .to_string(),
        gas_used: receipt.gas_used(),
    };

    Ok((record, receipt))
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

fn verify_predicted_deploy_address(
    label: &str,
    receipt: &TransactionReceipt,
    predicted: Address,
) -> Result<()> {
    match receipt.contract_address() {
        Some(actual) if actual == predicted => Ok(()),
        Some(actual) => bail!(
            "{label} deployed to {actual}, but the predicted CREATE address was {predicted}"
        ),
        None => bail!("{label}'s deploy receipt has no contract_address"),
    }
}
