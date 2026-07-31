//! Offline known-bot benchmark comparator (WHI-715).
//!
//! Cross-references continuous shadow ledgers against a list of known Mantle
//! arbitrage bot transactions and prints a 3-bucket report.
//!
//! ```bash
//! cargo run --example shadow_bot_benchmark -- compare \
//!   --ledger evidence/shadow/continuous/v2/ledger.jsonl \
//!   --ledger evidence/shadow/continuous/v3/ledger.jsonl \
//!   --known-bots config/shadow/known_bots.example.json \
//!   --json-out evidence/shadow/continuous/benchmark_report.json \
//!   --md-out evidence/shadow/continuous/benchmark_report.md
//! ```
//!
//! No RPC and no signing keys are required. Input ledgers must declare
//! `send_capability=no_send` (enforced).

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use amms::execution::shadow_bot_benchmark::{
    compare, load_known_bot_events, render_markdown_report, LedgerBytes,
};
use clap::{Parser, Subcommand};
use eyre::{Context, Result};

#[derive(Parser, Debug)]
#[command(name = "shadow_bot_benchmark")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Classify known-bot events against one or more shadow ledgers.
    Compare {
        /// Repeatable: path to a service ledger JSONL.
        #[arg(long = "ledger", required = true)]
        ledgers: Vec<PathBuf>,
        /// Path to known-bot events JSON (array, or `{ "events": [...] }`).
        #[arg(long = "known-bots")]
        known_bots: PathBuf,
        /// Optional JSON report output path.
        #[arg(long = "json-out")]
        json_out: Option<PathBuf>,
        /// Optional markdown report output path.
        #[arg(long = "md-out")]
        md_out: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Compare {
            ledgers,
            known_bots,
            json_out,
            md_out,
        } => {
            let mut ledger_bytes = Vec::with_capacity(ledgers.len());
            for path in &ledgers {
                ledger_bytes.push(
                    LedgerBytes::load(path)
                        .with_context(|| format!("load ledger {}", path.display()))?,
                );
            }
            let events = load_known_bot_events(&known_bots)
                .with_context(|| format!("load known bots {}", known_bots.display()))?;
            let report = compare(&ledger_bytes, &events).context("compare")?;

            let json = serde_json::to_string_pretty(&report).context("serialize report")?;
            if let Some(path) = json_out {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create dir {}", parent.display()))?;
                }
                fs::write(&path, format!("{json}\n"))
                    .with_context(|| format!("write {}", path.display()))?;
                eprintln!("wrote {}", path.display());
            }

            let md = render_markdown_report(&report);
            if let Some(path) = md_out {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)
                        .with_context(|| format!("create dir {}", parent.display()))?;
                }
                fs::write(&path, md.as_bytes())
                    .with_context(|| format!("write {}", path.display()))?;
                eprintln!("wrote {}", path.display());
            } else {
                // Default: print markdown to stdout for human review.
                print!("{md}");
            }

            // Nonzero exit when any missed detection exists — highest-priority signal.
            if report.bucket_counts.missed_detection > 0 {
                eprintln!(
                    "warning: {} missed_detection event(s)",
                    report.bucket_counts.missed_detection
                );
            }
            Ok(())
        }
    }
}
