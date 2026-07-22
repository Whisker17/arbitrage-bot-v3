//! Derive and export the immutable-patched `ArbitrageExecutor` runtime identity
//! (WHI-551).
//!
//! ```bash
//! (cd contracts/executor && scripts/export_artifacts.sh)
//! cargo run --example derive_runtime_identity -- \
//!   --artifact contracts/executor/artifacts/ \
//!   --wmnt 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8 --chain-id 5000 \
//!   --out config/executor_identity.json
//! ```
//!
//! This is a pure, offline derivation from committed build evidence — it makes no
//! RPC calls and does not verify against a live deployment (that's
//! `amms::execution::runtime_identity::verify_deployed_runtime`, used at runtime by
//! callers with a real provider).

use alloy::primitives::Address;
use amms::execution::runtime_identity::{build_export, resolve_immutable_plan, BuildEvidence, ImmutableInputs};
use clap::Parser;
use eyre::{Context, Result};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(about = "Derive the immutable-patched ArbitrageExecutor runtime identity")]
struct Args {
    /// Directory containing ArbitrageExecutor.full.json (forge build evidence).
    #[arg(long, default_value = "contracts/executor/artifacts")]
    artifact: PathBuf,

    /// WMNT address to patch into the runtime immutable.
    #[arg(long)]
    wmnt: Address,

    /// Chain id the plan is bound to (e.g. 5000 for Mantle mainnet).
    #[arg(long)]
    chain_id: u64,

    /// Output path for the exported executor_identity.json.
    #[arg(long, default_value = "config/executor_identity.json")]
    out: PathBuf,
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("derive_runtime_identity failed: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let args = Args::parse();

    let evidence = BuildEvidence::load(&args.artifact)
        .with_context(|| format!("load build evidence from {}", args.artifact.display()))?;
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt: args.wmnt }, args.chain_id)
        .context("resolve immutable plan")?;
    let export = build_export(&evidence, &plan, args.wmnt);

    let encoded = serde_json::to_vec_pretty(&export).context("encode executor_identity.json")?;
    if let Some(parent) = args.out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(&args.out, &encoded)
        .with_context(|| format!("write {}", args.out.display()))?;

    println!("wrote {}", args.out.display());
    println!("chain_id={}", export.chain_id);
    println!("template_hash={}", export.template_hash);
    println!("patched_runtime_hash={}", export.patched_runtime_hash);
    println!("wmnt={}", export.wmnt);
    println!("immutable_values_digest={}", export.immutable_values_digest);
    println!("compiler_config_digest={}", export.compiler_config_digest);
    println!("build_info_digest={}", export.build_info_digest);
    println!("storage_layout_digest={}", export.storage_layout_digest);
    println!("plan_digest={}", export.plan_digest);
    println!("identity_digest={}", export.identity_digest);

    Ok(())
}
