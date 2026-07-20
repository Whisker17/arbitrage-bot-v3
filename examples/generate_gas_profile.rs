//! Deterministic Mantle gas-profile generator (WHI-546 / M0-9).
//!
//! ```bash
//! # Regenerate the checked-in artifact from pinned inputs
//! cargo run --example generate_gas_profile
//!
//! # Explicit paths
//! cargo run --example generate_gas_profile -- \
//!   --config config/gas_profiles/pinned/generator_config.json \
//!   --samples config/gas_profiles/pinned/samples.jsonl \
//!   --out config/gas_profiles/mantle_mainnet_v1.json
//! ```
//!
//! Identical inputs produce an identical `content_digest`. Production
//! qualification samples must use the WHI-501 optimized runtime codehash.

use amms::execution::gas_profile::{
    generate_artifact, load_generator_config, load_samples_jsonl, write_artifact, ProfileStatus,
    GAS_PROFILE_TOOL_VERSION, WHI501_EXECUTOR_CODEHASH,
};
use clap::Parser;
use eyre::{bail, Context, Result};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(about = "Generate a versioned Mantle gas-profile artifact from pinned samples")]
struct Args {
    /// Pinned generator config (schema, route classes, fee analysis, margin policy).
    #[arg(long, default_value = "config/gas_profiles/pinned/generator_config.json")]
    config: PathBuf,

    /// JSONL gas samples (fork_replay + optional research_* lines).
    #[arg(long, default_value = "config/gas_profiles/pinned/samples.jsonl")]
    samples: PathBuf,

    /// Output artifact path.
    #[arg(long, default_value = "config/gas_profiles/mantle_mainnet_v1.json")]
    out: PathBuf,

    /// Also print the content digest on stdout (does not suppress writing).
    #[arg(long, default_value_t = false)]
    print_digest: bool,

    /// Compute and print digest without writing the output file.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

fn main() -> ExitCode {
    if let Err(e) = run() {
        eprintln!("generate_gas_profile failed: {e:#}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<()> {
    let args = Args::parse();

    let config = load_generator_config(&args.config)
        .with_context(|| format!("load config {}", args.config.display()))?;
    let samples = load_samples_jsonl(&args.samples)
        .with_context(|| format!("load samples {}", args.samples.display()))?;

    if config.tool_version != GAS_PROFILE_TOOL_VERSION {
        eprintln!(
            "warning: config tool_version {} != library {}",
            config.tool_version, GAS_PROFILE_TOOL_VERSION
        );
    }
    if config.executor_code_hash.to_ascii_lowercase()
        != WHI501_EXECUTOR_CODEHASH.to_ascii_lowercase()
    {
        eprintln!(
            "warning: config executor_code_hash {} != WHI501 pin {}",
            config.executor_code_hash, WHI501_EXECUTOR_CODEHASH
        );
    }

    let artifact = generate_artifact(&config, &samples).context("generate artifact")?;

    let approved = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Approved)
        .count();
    let unsupported = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Unsupported)
        .count();

    if args.print_digest || args.dry_run {
        println!("{}", artifact.content_digest);
    }

    println!(
        "gas profile: chain={} schema={} tool={} profiles={} approved={} unsupported={} digest={}",
        artifact.chain_id,
        artifact.schema_version,
        artifact.tool_version,
        artifact.profiles.len(),
        approved,
        unsupported,
        artifact.content_digest
    );
    println!(
        "executor_code_hash={} abi_digest={}",
        artifact.executor_code_hash, artifact.executor_abi_digest
    );
    println!(
        "fee window blocks {}..{} min_block_gas_limit={} (not a permanent constant)",
        artifact.fee_analysis.start_block,
        artifact.fee_analysis.end_block,
        artifact.fee_analysis.min_block_gas_limit
    );
    println!(
        "v3: {}",
        artifact.crossing_bucket_evidence.v3_tick_conclusion
    );
    println!(
        "moe: {}",
        artifact.crossing_bucket_evidence.moe_bin_conclusion
    );

    if approved == 0 {
        bail!("no approved profiles; refusing to write a fully unsupported artifact");
    }

    if !args.dry_run {
        write_artifact(&args.out, &artifact)
            .with_context(|| format!("write {}", args.out.display()))?;
        println!("wrote {}", args.out.display());
    }

    Ok(())
}
