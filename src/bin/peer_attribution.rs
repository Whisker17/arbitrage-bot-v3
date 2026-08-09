//! Peer-arb attribution CLI (WHI-957).
//!
//! Attribute every WHI-956 ground-truth event to exactly one cause against a
//! frozen universe and optional concurrent shadow ledger + block discovery view.
//!
//! ```bash
//! # Offline universe-side pass (no concurrent ledger)
//! cargo run --release --bin peer_attribution -- \
//!   --events /path/to/ground_truth_events.jsonl \
//!   --universe data/pool_universe.csv \
//!   --json-out evidence/peer-attribution/offline_universe.json \
//!   --md-out evidence/peer-attribution/offline_universe.md
//!
//! # Concurrent shadow window
//! cargo run --release --bin peer_attribution -- \
//!   --events /path/to/events_for_window.jsonl \
//!   --universe data/pool_universe.csv \
//!   --ledger evidence/shadow/.../ledger.jsonl \
//!   --block-views /path/to/block_discovery.jsonl \
//!   --json-out evidence/peer-attribution/shadow_window.json \
//!   --md-out evidence/peer-attribution/shadow_window.md
//! ```
//!
//! Real event JSONL stays external (same contract as WHI-906 / WHI-956).

use std::collections::BTreeSet;
use std::path::PathBuf;

use amms::execution::{
    attach_benchmark_buckets, attribute_all, load_block_views, load_events, load_events_jsonl,
    load_observed_blocks_from_ledger, render_peer_attribution_markdown,
    write_peer_attribution_report, AttributionInputs, LedgerBytes, ShadowLedgerIndex,
    UniverseContext,
};
// attach_benchmark_buckets used below
use clap::Parser;
use eyre::{bail, Context, Result};

#[derive(Debug, Parser)]
#[command(
    name = "peer_attribution",
    about = "WHI-957: attribute every ground-truth arb to exactly one cause"
)]
struct Args {
    /// Known-bot events JSON (`{events:[…]}`) or JSONL (KnownBotEvent per line).
    #[arg(long)]
    events: PathBuf,
    /// Frozen universe CSV (`data/pool_universe.csv`).
    #[arg(long, default_value = "data/pool_universe.csv")]
    universe: PathBuf,
    /// Optional shadow ledger JSONL (repeatable).
    #[arg(long = "ledger")]
    ledgers: Vec<PathBuf>,
    /// Optional per-block discovery view JSONL (`block_number`, `dirty_pools`, `skipped`).
    #[arg(long)]
    block_views: Option<PathBuf>,
    /// When set, events outside the ledger observation window are BlockSkipped
    /// instead of Unattributable.
    #[arg(long, default_value_t = false)]
    outside_window_is_skip: bool,
    /// Optional adapter-required factory addresses (repeatable, lower-case hex).
    #[arg(long = "adapter-factory")]
    adapter_factories: Vec<String>,
    #[arg(long)]
    json_out: Option<PathBuf>,
    #[arg(long)]
    md_out: Option<PathBuf>,
    /// Also run WHI-715 bucket compare when ledgers are present.
    #[arg(long, default_value_t = true)]
    with_benchmark_buckets: bool,
    /// Omit per-event rows from JSON output (for committed aggregate reports).
    #[arg(long, default_value_t = false)]
    summary_only: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !args.events.exists() {
        bail!(
            "events not found: {} (dataset stays external)",
            args.events.display()
        );
    }
    if !args.universe.exists() {
        bail!("universe not found: {}", args.universe.display());
    }

    let events = if args
        .events
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("jsonl"))
        .unwrap_or(false)
    {
        load_events_jsonl(&args.events)
            .with_context(|| format!("load events jsonl {}", args.events.display()))?
    } else {
        load_events(&args.events)
            .with_context(|| format!("load events {}", args.events.display()))?
    };

    let mut universe = UniverseContext::from_pool_csv(&args.universe)
        .with_context(|| format!("load universe {}", args.universe.display()))?;
    if !args.adapter_factories.is_empty() {
        universe = universe.with_adapter_factories(args.adapter_factories.clone());
    }

    let mut ledger_bytes = Vec::new();
    for path in &args.ledgers {
        ledger_bytes.push(
            LedgerBytes::load(path).with_context(|| format!("load ledger {}", path.display()))?,
        );
    }
    let ledger_index = if ledger_bytes.is_empty() {
        None
    } else {
        Some(
            ShadowLedgerIndex::from_ledgers(&ledger_bytes)
                .map_err(|e| eyre::eyre!(e.to_string()))
                .context("index ledgers")?,
        )
    };

    let mut observed: BTreeSet<u64> = BTreeSet::new();
    for lb in &ledger_bytes {
        let blocks = load_observed_blocks_from_ledger(lb)
            .with_context(|| format!("observations {}", lb.label))?;
        observed.extend(blocks);
    }
    let observed_ref = if observed.is_empty() {
        None
    } else {
        Some(&observed)
    };

    let views = match args.block_views {
        Some(ref p) => Some(
            load_block_views(p).with_context(|| format!("load block views {}", p.display()))?,
        ),
        None => None,
    };

    let inputs = AttributionInputs {
        events: &events,
        universe: &universe,
        ledger_index: ledger_index.as_ref(),
        observed_blocks: observed_ref,
        block_views: views.as_ref(),
        outside_window_is_unattributable: !args.outside_window_is_skip,
    };

    let mut report = attribute_all(&inputs).context("attribute")?;
    if args.with_benchmark_buckets && !ledger_bytes.is_empty() {
        attach_benchmark_buckets(&mut report, &events, &ledger_bytes)
            .context("benchmark buckets")?;
    }
    if args.summary_only {
        amms::execution::peer_attribution::strip_events(&mut report);
    }

    println!(
        "events={} attributed={} dirty_cycle_filter_skipped={} (is_zero={}) not_in_universe={} block_skipped={} unattributable={} oos={} aggregator={}",
        report.event_count,
        report.attributed_count,
        report.cause_counts.dirty_cycle_filter_skipped,
        report.dirty_cycle_filter_skipped_is_zero,
        report.cause_counts.not_in_universe,
        report.cause_counts.block_skipped,
        report.cause_counts.unattributable,
        report.cause_counts.out_of_scope_total(),
        report.cause_counts.aggregator_misclass,
    );
    println!(
        "reachable_miss_rate={:.2}% out_of_scope_rate={:.2}%",
        report.reachable_miss_rate * 100.0,
        report.out_of_scope_rate * 100.0
    );

    if let Some(p) = args.json_out {
        write_peer_attribution_report(&p, &report)
            .with_context(|| format!("write {}", p.display()))?;
        println!("wrote {}", p.display());
    }
    if let Some(p) = args.md_out {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::write(&p, render_peer_attribution_markdown(&report))
            .with_context(|| format!("write {}", p.display()))?;
        println!("wrote {}", p.display());
    }

    Ok(())
}
