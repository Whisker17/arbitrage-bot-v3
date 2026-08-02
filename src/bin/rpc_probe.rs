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

use amms::rpc_probe::{
    run_probe, ProbeConfig, DEFAULT_ADDRESS_MULTIPLIER, DEFAULT_BLOCKS, DEFAULT_DURATION_SECS,
    DEFAULT_LOGS_BLOCK_WINDOW,
};
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
    #[arg(long, default_value_t = DEFAULT_LOGS_BLOCK_WINDOW)]
    logs_window: u64,

    /// Multiply the merged pool address set size to probe provider headroom.
    #[arg(long, default_value_t = DEFAULT_ADDRESS_MULTIPLIER)]
    address_multiplier: f64,

    /// Unified multi-protocol pool universe (same default as `bot`, WHI-793).
    #[arg(
        long = "pool-universe",
        env = "BOT_POOL_UNIVERSE",
        default_value = amms::service::DEFAULT_POOL_UNIVERSE_REL
    )]
    pool_universe: PathBuf,
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

    // Refuse to run when the unified universe is missing (live width is part of the gate).
    if !args.pool_universe.exists() {
        bail!(
            "pool_universe_load_failed: unified pool universe missing at {}",
            args.pool_universe.display()
        );
    }

    let config = ProbeConfig {
        http_url: http,
        ws_url: ws,
        out: args.out,
        blocks: args.blocks,
        duration_secs: args.duration,
        logs_block_window: args.logs_window,
        address_multiplier: args.address_multiplier,
        pool_universe: args.pool_universe,
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
