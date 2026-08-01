//! Mantle RPC endpoint qualification probe (WHI-744).
//!
//! ```text
//! cargo run --bin rpc_probe -- --http <url> --ws <url> --out /tmp/r.json
//! ```
//!
//! Exit 0 only when the report's top-level `qualified` is true. Endpoint URLs
//! and API keys are never written to the report or stdout — only fingerprints.

use std::path::PathBuf;
use std::process::ExitCode;

use amms::rpc_probe::{run_probe, ProbeConfig, DEFAULT_BLOCKS, DEFAULT_DURATION_SECS};
use clap::Parser;
use eyre::{bail, Result};
use tracing::error;

/// Qualify a Mantle HTTP+WS RPC pair against the merged multi-protocol bot needs.
#[derive(Debug, Parser)]
#[command(
    name = "rpc_probe",
    about = "Qualify Mantle RPC endpoints for multi-protocol arbitrage (WHI-744)"
)]
struct Args {
    /// HTTP JSON-RPC URL (also `MANTLE_RPC_URL`).
    #[arg(long, env = "MANTLE_RPC_URL")]
    http: Option<String>,

    /// WebSocket JSON-RPC URL (also `MANTLE_RPC_WS_URL`).
    #[arg(long, env = "MANTLE_RPC_WS_URL")]
    ws: Option<String>,

    /// Write the machine-readable JSON report here.
    #[arg(long, default_value = "rpc_probe_report.json")]
    out: PathBuf,

    /// Number of consecutive blocks to sample (continuity / headers / receipts).
    #[arg(long, default_value_t = DEFAULT_BLOCKS)]
    blocks: u64,

    /// Sustained WS subscription duration in seconds (Check E).
    #[arg(long, default_value_t = DEFAULT_DURATION_SECS)]
    duration: u64,

    /// Recent-block window for multi-address eth_getLogs (Check A).
    #[arg(long, default_value_t = 8)]
    logs_window: u64,

    /// Multiply the merged pool address set size to probe provider headroom.
    #[arg(long, default_value_t = 1.0)]
    address_multiplier: f64,

    /// CSV pool list for Agni-V2 (same default as `bot`).
    #[arg(long, env = "BOT_V2_POOL_LIST", default_value = "data/poolLists.csv")]
    v2_pool_list: PathBuf,

    /// CSV pool list for Agni-V3 (same default as `bot`).
    #[arg(long, env = "BOT_V3_POOL_LIST", default_value = "data/poolLists.csv")]
    v3_pool_list: PathBuf,

    /// CSV pool list for Moe (same default as `bot`).
    #[arg(long, env = "BOT_MOE_POOL_LIST", default_value = "data/poolLists_moe.csv")]
    moe_pool_list: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            // Never print raw endpoint URLs or embedded secrets.
            let safe = amms::rpc_probe::sanitize_error(&format!("{e:#}"));
            error!(target: "rpc_probe", error = %safe, "rpc_probe failed");
            eprintln!("rpc_probe error: {safe}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> Result<bool> {
    let args = Args::parse();
    let http = args
        .http
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            eyre::eyre!(
                "missing_endpoint: provide --http or set MANTLE_RPC_URL (probe refuses to no-op)"
            )
        })?;
    let ws = args.ws.filter(|s| !s.trim().is_empty()).ok_or_else(|| {
        eyre::eyre!(
            "missing_endpoint: provide --ws or set MANTLE_RPC_WS_URL (probe refuses to no-op)"
        )
    })?;

    // Refuse to run when pool lists are missing (live width is part of the gate).
    for (label, path) in [
        ("v2", &args.v2_pool_list),
        ("v3", &args.v3_pool_list),
        ("moe", &args.moe_pool_list),
    ] {
        if !path.exists() {
            bail!(
                "pool_universe_load_failed: {label} pool list missing at {}",
                path.display()
            );
        }
    }

    let config = ProbeConfig {
        http_url: http,
        ws_url: ws,
        out: args.out,
        blocks: args.blocks,
        duration_secs: args.duration,
        logs_block_window: args.logs_window,
        address_multiplier: args.address_multiplier,
        v2_pool_list: args.v2_pool_list,
        v3_pool_list: args.v3_pool_list,
        moe_pool_list: args.moe_pool_list,
    };

    let (report, ok) = run_probe(config).await?;
    if !ok {
        eprintln!(
            "rpc_probe: NOT QUALIFIED (see report; top-level qualified=false)"
        );
        // Print distinct failed reasons without secrets.
        for (id, check) in &report.checks {
            if !check.passed {
                eprintln!(
                    "  fail {id}: {}",
                    check.failure_reason.as_deref().unwrap_or("unknown")
                );
            }
        }
    }
    Ok(ok)
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}
