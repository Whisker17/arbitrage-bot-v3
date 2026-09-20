//! Mantle arb-bot ground-truth collector (WHI-956).
//!
//! Re-runnable offline pipeline: take a Dune CSV export or pre-extracted arbs
//! JSONL over a caller-supplied block range, apply the explicit acceptance
//! heuristic + misclassification exclusions, and emit:
//!
//! * comparator-schema known-bots JSON (`--known-bots-out`)
//! * events JSONL with WHI-957 fields (`--events-out`) — **keep external**
//! * aggregate report JSON + Markdown (`--report-out` / `--md-out`)
//!
//! Real event datasets are **not** committed (same contract as `arb_coverage`).
//!
//! ```bash
//! # From WHI-906-style arbs JSONL (external) + optional pool census for venues
//! cargo run --release --bin ground_truth_collector -- collect \
//!   --input /path/to/arbs_month.jsonl \
//!   --from-block 96806569 --to-block 98098684 \
//!   --census /path/to/pool_census.json \
//!   --known-bots-out /tmp/known_bots.json \
//!   --events-out /tmp/ground_truth_events.jsonl \
//!   --report-out evidence/ground-truth/baseline_report.json \
//!   --md-out evidence/ground-truth/baseline_report.md
//!
//! # From a Dune CSV export of scripts/dunesql/02_arb_detail_feed.sql (WHI-1406)
//! # (rename `executor_address` -> `to` for collector alias matching)
//! cargo run --release --bin ground_truth_collector -- collect \
//!   --input /path/to/dune_export.csv \
//!   --from-block 98100000 --to-block 98200000 \
//!   --known-bots-out /tmp/known_bots.json \
//!   --report-out /tmp/report.json
//!
//! # Attach a manual / Blockscout verification sample
//! cargo run --release --bin ground_truth_collector -- verify-sample \
//!   --events /tmp/ground_truth_events.jsonl \
//!   --labels /path/to/verification_labels.json \
//!   --report-in evidence/ground-truth/baseline_report.json \
//!   --report-out evidence/ground-truth/baseline_report.json \
//!   --md-out evidence/ground-truth/baseline_report.md
//!
//! # Emit a deterministic sample list for Blockscout review
//! cargo run --release --bin ground_truth_collector -- sample \
//!   --events /tmp/ground_truth_events.jsonl \
//!   --sample-size 40 \
//!   --out /tmp/sample_tx_hashes.txt
//! ```

use std::path::PathBuf;

use amms::execution::load_known_bot_events;
use amms::service::{
    collect_from_candidates_allow_empty, collect_from_path, load_candidates_dune_csv,
    load_candidates_jsonl, load_census, load_events_jsonl, load_verification_labels,
    render_report_markdown, sample_tx_hashes, score_blockscout_tx, verification_from_labels,
    write_events_jsonl, write_ground_truth_report, write_known_bots_json, BlockRange,
    GroundTruthReport, VerificationLabel, DEFAULT_BLOCKSCOUT_BASE,
};
use clap::{Parser, Subcommand};
use eyre::{bail, Context, Result};

#[derive(Debug, Parser)]
#[command(
    name = "ground_truth_collector",
    about = "WHI-956: re-runnable Mantle arb-bot ground-truth collector"
)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Collect accepted events over a block range from JSONL or Dune CSV.
    Collect {
        /// Candidates input: `.jsonl` (arbs / discovery rows) or `.csv` (Dune export).
        #[arg(long)]
        input: PathBuf,
        /// Inclusive start block.
        #[arg(long)]
        from_block: u64,
        /// Inclusive end block.
        #[arg(long)]
        to_block: u64,
        /// Optional pool census JSON for `venues` (factory) resolution.
        #[arg(long, env = "ARB_COVERAGE_CENSUS")]
        census: Option<PathBuf>,
        /// Write comparator-schema known-bots JSON here (external path recommended).
        #[arg(long)]
        known_bots_out: Option<PathBuf>,
        /// Write full events JSONL here (external — do not commit).
        #[arg(long)]
        events_out: Option<PathBuf>,
        /// Aggregate report JSON (safe to commit when events stay external).
        #[arg(long)]
        report_out: Option<PathBuf>,
        /// Aggregate report Markdown.
        #[arg(long)]
        md_out: Option<PathBuf>,
        /// Extra note lines embedded in the report.
        #[arg(long = "note")]
        notes: Vec<String>,
        /// Allow empty accepted set (writes report with zero events; default fails).
        #[arg(long, default_value_t = false)]
        allow_empty: bool,
    },
    /// Deterministic sample of tx hashes for Blockscout / manual review.
    Sample {
        /// Events JSONL produced by `collect`.
        #[arg(long)]
        events: PathBuf,
        #[arg(long, default_value_t = 40)]
        sample_size: usize,
        /// One hash per line.
        #[arg(long)]
        out: PathBuf,
        /// Optional Blockscout base URL printed as review links.
        #[arg(long, default_value = DEFAULT_BLOCKSCOUT_BASE)]
        blockscout_base: String,
    },
    /// Attach verification labels (manual or Blockscout-scored) to a report.
    VerifySample {
        /// Existing report JSON to update.
        #[arg(long)]
        report_in: PathBuf,
        /// Verification labels JSON array / JSONL (`tx_hash`, `verdict`, optional `note`).
        #[arg(long)]
        labels: Option<PathBuf>,
        /// Directory of Blockscout/fixture JSON files named `{tx_hash}.json`.
        #[arg(long)]
        blockscout_dir: Option<PathBuf>,
        /// Sample hashes file (one per line) when scoring a directory.
        #[arg(long)]
        sample_hashes: Option<PathBuf>,
        #[arg(long)]
        report_out: PathBuf,
        #[arg(long)]
        md_out: Option<PathBuf>,
        #[arg(long, default_value = "manual+blockscout")]
        method: String,
        #[arg(long, default_value = "")]
        notes: String,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.cmd {
        Cmd::Collect {
            input,
            from_block,
            to_block,
            census,
            known_bots_out,
            events_out,
            report_out,
            md_out,
            notes,
            allow_empty,
        } => {
            if !input.exists() {
                bail!(
                    "input not found: {} (dataset stays external; pass an absolute path)",
                    input.display()
                );
            }
            let range = BlockRange::new(from_block, to_block)
                .map_err(|e| eyre::eyre!(e.to_string()))?;
            let census_map = match census {
                Some(p) => {
                    if !p.exists() {
                        bail!("census not found: {}", p.display());
                    }
                    Some(load_census(&p).with_context(|| format!("load census {}", p.display()))?)
                }
                None => None,
            };
            let mut notes = notes;
            if notes.is_empty() {
                notes.push(
                    "Heuristic and exclusion categories are fixed in GROUND_TRUTH_SCHEMA_VERSION; \
                     see ACCEPTANCE_HEURISTIC in src/service/ground_truth.rs."
                        .into(),
                );
                notes.push(
                    "Real events JSONL/known-bots are external; only aggregate reports are committed."
                        .into(),
                );
            }
            let result = if allow_empty {
                let ext = input
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let candidates = if ext == "csv" {
                    load_candidates_dune_csv(&input)
                        .with_context(|| format!("load csv {}", input.display()))?
                } else {
                    load_candidates_jsonl(&input)
                        .with_context(|| format!("load jsonl {}", input.display()))?
                };
                let label = input
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| input.display().to_string());
                collect_from_candidates_allow_empty(
                    &candidates,
                    range,
                    census_map.as_ref(),
                    &label,
                    notes,
                )
            } else {
                collect_from_path(&input, range, census_map.as_ref(), notes)
                    .with_context(|| "collect")?
            };

            print_summary(&result.report);

            if let Some(ref p) = known_bots_out {
                write_known_bots_json(p, &result.events)
                    .with_context(|| format!("write known-bots {}", p.display()))?;
                println!("wrote known-bots → {}", p.display());
            }
            if let Some(ref p) = events_out {
                write_events_jsonl(p, &result.events)
                    .with_context(|| format!("write events {}", p.display()))?;
                println!("wrote events JSONL → {} (keep external)", p.display());
            }
            if let Some(ref p) = report_out {
                write_ground_truth_report(p, &result.report)
                    .with_context(|| format!("write report {}", p.display()))?;
                println!("wrote report → {}", p.display());
            }
            if let Some(ref p) = md_out {
                std::fs::write(p, render_report_markdown(&result.report))
                    .with_context(|| format!("write md {}", p.display()))?;
                println!("wrote markdown → {}", p.display());
            }
            if known_bots_out.is_none() && events_out.is_none() && report_out.is_none() {
                eprintln!(
                    "warning: no --known-bots-out / --events-out / --report-out; nothing written"
                );
            }
            Ok(())
        }
        Cmd::Sample {
            events,
            sample_size,
            out,
            blockscout_base,
        } => {
            // Events JSONL is KnownBotEvent lines (from collect --events-out).
            // Also accepts the wrapped known-bots JSON shape.
            let event_rows = if events
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("jsonl"))
                .unwrap_or(false)
            {
                load_events_jsonl(&events)
                    .map_err(|e| eyre::eyre!(e.to_string()))
                    .with_context(|| format!("load events jsonl {}", events.display()))?
            } else {
                load_known_bot_events(&events)
                    .with_context(|| format!("load known-bots {}", events.display()))?
            };
            let hashes = sample_tx_hashes(&event_rows, sample_size);
            let mut body = String::new();
            for h in &hashes {
                let base = blockscout_base.trim_end_matches('/');
                body.push_str(h);
                body.push('\t');
                body.push_str(&format!("{base}/tx/{h}"));
                body.push('\n');
            }
            if let Some(parent) = out.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            std::fs::write(&out, body).with_context(|| format!("write {}", out.display()))?;
            println!(
                "wrote {} sample hashes → {} (Blockscout base {})",
                hashes.len(),
                out.display(),
                blockscout_base
            );
            Ok(())
        }
        Cmd::VerifySample {
            report_in,
            labels,
            blockscout_dir,
            sample_hashes,
            report_out,
            md_out,
            method,
            notes,
        } => {
            let bytes = std::fs::read(&report_in)
                .with_context(|| format!("read report {}", report_in.display()))?;
            let mut report: GroundTruthReport = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse report {}", report_in.display()))?;

            let mut label_rows: Vec<VerificationLabel> = Vec::new();
            if let Some(p) = labels {
                label_rows.extend(
                    load_verification_labels(&p)
                        .with_context(|| format!("load labels {}", p.display()))?,
                );
            }
            if let Some(dir) = blockscout_dir {
                let hashes: Vec<String> = if let Some(hf) = sample_hashes {
                    std::fs::read_to_string(&hf)
                        .with_context(|| format!("read hashes {}", hf.display()))?
                        .lines()
                        .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                } else {
                    std::fs::read_dir(&dir)
                        .with_context(|| format!("read dir {}", dir.display()))?
                        .filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            name.strip_suffix(".json").map(|s| s.to_string())
                        })
                        .collect()
                };
                for h in hashes {
                    let path = dir.join(format!("{h}.json"));
                    if !path.exists() {
                        label_rows.push(VerificationLabel {
                            tx_hash: h,
                            verdict: "unverified".into(),
                            note: Some("missing blockscout json".into()),
                        });
                        continue;
                    }
                    let v: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(&path)
                            .with_context(|| format!("read {}", path.display()))?,
                    )
                    .with_context(|| format!("parse {}", path.display()))?;
                    label_rows.push(score_blockscout_tx(&v));
                }
            }
            if label_rows.is_empty() {
                bail!("provide --labels and/or --blockscout-dir");
            }
            report.verification = Some(verification_from_labels(&label_rows, method, notes));
            write_ground_truth_report(&report_out, &report)
                .with_context(|| format!("write report {}", report_out.display()))?;
            println!("wrote report → {}", report_out.display());
            if let Some(p) = md_out {
                std::fs::write(&p, render_report_markdown(&report))
                    .with_context(|| format!("write md {}", p.display()))?;
                println!("wrote markdown → {}", p.display());
            }
            if let Some(v) = &report.verification {
                println!(
                    "verification: sample={} tp={} fp={} uv={} precision={:?}",
                    v.sample_size,
                    v.verified_true_positive,
                    v.verified_false_positive,
                    v.unverified,
                    v.precision
                );
            }
            Ok(())
        }
    }
}

fn print_summary(report: &GroundTruthReport) {
    println!(
        "accepted={} bots={} candidates_seen={} range={}..{} fingerprint={}",
        report.accepted,
        report.distinct_bot_addresses,
        report.candidates_seen,
        report.from_block,
        report.to_block,
        report.events_fingerprint
    );
    let e = &report.exclusion_counts;
    println!(
        "exclusions: cex_dex={} liq={} jit={} sandwich={} swaps={} cycle={} gross={} oor={} missing={} total={}",
        e.cex_dex,
        e.liquidation,
        e.jit_lp,
        e.sandwich,
        e.insufficient_swaps,
        e.not_closed_cycle,
        e.no_gross_out,
        e.out_of_range,
        e.missing_fields,
        e.total()
    );
}
