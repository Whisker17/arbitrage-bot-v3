//! Offline observed-arb coverage report (WHI-906).
//!
//! Intersect a frozen pool universe with real atomic-arbitrage paths and
//! rank candidate additions by marginal fully-executable gain. The arb
//! dataset and census stay external; only the script + its report are
//! committed in this repo.
//!
//! ```bash
//! cargo run --release --bin arb_coverage -- \
//!   --universe data/pool_universe.csv \
//!   --arbs /path/to/arbs_month.jsonl \
//!   --census /path/to/pool_census.json \
//!   --out evidence/coverage/month30.json \
//!   --write-meta-coverage
//! ```

use std::path::PathBuf;

use amms::service::{
    build_report, coverage_path_for, dataset_label, format_report_text, load_arbs_jsonl, load_census,
    load_held_pools_from_csv, load_unified_meta, write_report, write_unified_meta, ObservedArbCoverage,
};
use clap::Parser;
use eyre::{bail, Context, Result};

#[derive(Debug, Parser)]
#[command(
    name = "arb_coverage",
    about = "Observed on-chain arb coverage vs a frozen pool universe (WHI-906)"
)]
struct Args {
    /// Unified universe CSV (`data/pool_universe.csv`).
    #[arg(long, default_value = "data/pool_universe.csv", env = "BOT_POOL_UNIVERSE")]
    universe: PathBuf,

    /// Atomic arbs JSONL (each line: `{"path":["0x…",…],"block":…}`).
    #[arg(long, env = "ARB_COVERAGE_ARBS")]
    arbs: PathBuf,

    /// Pool census JSON (map address → `{kind,factory,s0,s1,…}`).
    #[arg(long, env = "ARB_COVERAGE_CENSUS")]
    census: PathBuf,

    /// Greedy top-N candidates (default 12, matching the WHI-906 headline table).
    #[arg(long, default_value_t = 12)]
    greedy_top: usize,

    /// Write full JSON report here. Default: `{universe_stem}.coverage.json`.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Also patch `observed_arb_coverage` into the universe meta sidecar
    /// (next to fingerprint). Does not require regenerating the CSV.
    #[arg(long, default_value_t = false)]
    write_meta_coverage: bool,

    /// Optional fingerprint hex to embed in the report (defaults to meta).
    #[arg(long)]
    fingerprint: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.arbs.exists() {
        bail!(
            "arbs file not found: {} (dataset stays external; pass an absolute path)",
            args.arbs.display()
        );
    }
    if !args.census.exists() {
        bail!("census file not found: {}", args.census.display());
    }
    if !args.universe.exists() {
        bail!("universe CSV not found: {}", args.universe.display());
    }

    let held = load_held_pools_from_csv(&args.universe)
        .with_context(|| format!("load universe {}", args.universe.display()))?;
    let arbs = load_arbs_jsonl(&args.arbs)
        .with_context(|| format!("load arbs {}", args.arbs.display()))?;
    let census = load_census(&args.census)
        .with_context(|| format!("load census {}", args.census.display()))?;

    let fingerprint = args.fingerprint.or_else(|| {
        load_unified_meta(&args.universe)
            .ok()
            .and_then(|m| m.fingerprint)
    });

    let report = build_report(
        &held,
        &arbs,
        &census,
        args.greedy_top,
        fingerprint,
        Some(dataset_label(&args.arbs)),
    );

    let out_path = args
        .out
        .unwrap_or_else(|| coverage_path_for(&args.universe));
    write_report(&out_path, &report)
        .with_context(|| format!("write report {}", out_path.display()))?;

    print!("{}", format_report_text(&report));
    println!("wrote report → {}", out_path.display());

    if args.write_meta_coverage {
        let mut meta = load_unified_meta(&args.universe)
            .with_context(|| format!("load meta for {}", args.universe.display()))?;
        let observed: ObservedArbCoverage = report
            .observed
            .clone()
            .expect("build_report always sets observed");
        // Keep fingerprint alignment: coverage is for this meta's fingerprint.
        if meta.fingerprint.is_none() {
            meta.fingerprint = report.universe_fingerprint.clone();
        }
        meta.observed_arb_coverage = Some(observed);
        write_unified_meta(&args.universe, &meta)
            .with_context(|| format!("write meta for {}", args.universe.display()))?;
        println!(
            "patched observed_arb_coverage into meta (fingerprint={:?})",
            meta.fingerprint
        );
    }

    Ok(())
}
