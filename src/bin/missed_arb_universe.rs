//! Backwards universe selection from missed arbs (WHI-999).
//!
//! WHI-906 measured coverage *forward* from a candidate universe. This binary
//! runs the inverse: start from ground-truth arbs, keep only the ones the
//! strategy could actually take, and ask which pools we would have needed —
//! ranked by **marginal** arbs unlocked, with the full admission cost (pool
//! count, production cycle count, estimated cold start) of each candidate set.
//!
//! The arb dataset and census stay **external** (same contract as
//! `arb_coverage` / `ground_truth_collector`); only aggregate reports are
//! committed.
//!
//! ```bash
//! # Offline: ranking + cycle counts, candidate TVL left unmeasured
//! cargo run --release --bin missed_arb_universe -- \
//!   --arbs <external>/arbs_month.jsonl \
//!   --census <external>/pool_census.json \
//!   --json-out evidence/missed-arbs/report.json \
//!   --md-out evidence/missed-arbs/report.md
//!
//! # With candidate TVL (read-only RPC, no signer) so the TVL floor can be
//! # attributed per pool
//! cargo run --release --bin missed_arb_universe -- \
//!   --arbs <external>/arbs_month.jsonl \
//!   --census <external>/pool_census.json \
//!   --rpc-url https://rpc.mantle.xyz \
//!   --json-out evidence/missed-arbs/report.json \
//!   --md-out evidence/missed-arbs/report.md
//! ```
//!
//! Without `--rpc-url` the report is still complete except for the TVL columns:
//! every affected pool carries `tvl_not_measured` and `tvl_measured=false` is
//! stamped in the header, so a missing valuation can never read as "cleared the
//! floor".

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use amms::service::unified_universe::read_unified_csv;
use amms::service::{
    analyze_missed_arbs, connect_http_provider, load_census, load_missed_arb_events,
    load_unified_meta, recommended_throttle_rps, render_missed_arb_markdown, value_pools_wmnt,
    AnalysisConfig, CandidatePool, PoolTvl, RpcProviderConfig, DEFAULT_MIN_TVL_WMNT_WEI,
    DEFAULT_POOL_UNIVERSE_REL, DEFAULT_WMNT,
};
use amms::state_space::EFFECTIVE_MAX_HOPS;
use clap::Parser;
use eyre::{bail, Context, Result};
use tracing::{info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "missed_arb_universe",
    about = "Rank the pools we would have needed, from the arbs we missed (WHI-999)"
)]
struct Args {
    /// Frozen unified universe CSV.
    #[arg(long, default_value = DEFAULT_POOL_UNIVERSE_REL, env = "BOT_POOL_UNIVERSE")]
    universe: PathBuf,

    /// Ground-truth arbs JSONL (`{"block":…,"path":[…],"nSwaps":…,"pos":[…]}`).
    #[arg(long, env = "MISSED_ARB_ARBS")]
    arbs: PathBuf,

    /// Pool census JSON (address → `{kind,factory,t0,t1,s0,s1,swaps}`).
    #[arg(long, env = "MISSED_ARB_CENSUS")]
    census: PathBuf,

    /// Settlement asset (default WMNT).
    #[arg(long)]
    settlement: Option<String>,

    /// Strategy hop cap. Must equal `EFFECTIVE_MAX_HOPS`; present for
    /// documentation, not override (WHI-999 quantifies, it does not change it).
    #[arg(long, default_value_t = EFFECTIVE_MAX_HOPS)]
    max_hops: u8,

    /// TVL floor in WMNT wei (decimal). Default 1000 WMNT, matching the
    /// generator policy the frozen universe was built under.
    #[arg(long)]
    min_tvl_wmnt_wei: Option<String>,

    /// Candidate-set sizes, comma-separated.
    #[arg(long, default_value = "10,25,50")]
    set_sizes: String,

    /// Rows in each ranking table.
    #[arg(long, default_value_t = 50)]
    top_n: usize,

    /// Share of in-scope arbs a candidate set must reach for the verdict to
    /// call a reachable universe "non-trivial".
    #[arg(long, default_value_t = 25.0)]
    non_trivial_pct: f64,

    /// Read-only RPC URL. When set, candidate pools are valued so the TVL floor
    /// can be attributed per pool. Omit for a fully offline run.
    #[arg(long, env = "MANTLE_HTTP_URL")]
    rpc_url: Option<String>,

    /// Block to pin candidate TVL reads to. Defaults to the universe's own
    /// `snapshot_block`, so "below the floor" reproduces the decision the
    /// generator made rather than a later state; falls back to chain head when
    /// the meta sidecar is absent.
    #[arg(long)]
    tvl_block: Option<u64>,

    /// Optional chain-wide atomic-arb gross in USD/day. When given, the verdict
    /// states the value implied by the best reachable set instead of leaving arb
    /// *count* to be read as arb *value*.
    #[arg(long)]
    chain_gross_usd_per_day: Option<f64>,

    /// JSON report output path.
    #[arg(long)]
    json_out: Option<PathBuf>,

    /// Markdown report output path.
    #[arg(long)]
    md_out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    if args.max_hops != EFFECTIVE_MAX_HOPS {
        bail!(
            "--max-hops must equal EFFECTIVE_MAX_HOPS ({EFFECTIVE_MAX_HOPS}); got {}. \
             WHI-999 prices a cap change, it does not apply one.",
            args.max_hops
        );
    }
    if args.top_n == 0 {
        bail!("--top-n must be > 0");
    }
    if !(0.0..=100.0).contains(&args.non_trivial_pct) {
        bail!(
            "--non-trivial-pct must be a percentage in 0..=100; got {}",
            args.non_trivial_pct
        );
    }

    let settlement = match &args.settlement {
        Some(s) => Address::from_str(s).context("parse --settlement")?,
        None => DEFAULT_WMNT,
    };
    let min_tvl = match &args.min_tvl_wmnt_wei {
        Some(s) => U256::from_str(s).with_context(|| format!("parse min_tvl_wmnt_wei={s}"))?,
        None => U256::from(DEFAULT_MIN_TVL_WMNT_WEI),
    };
    let set_sizes = parse_set_sizes(&args.set_sizes)?;

    // ── universe ───────────────────────────────────────────────────────────
    let universe = read_unified_csv(&args.universe)
        .with_context(|| format!("read universe {}", args.universe.display()))?;
    if universe.is_empty() {
        bail!(
            "universe {} is empty — regenerate with `cargo run --release --bin universe_gen`",
            args.universe.display()
        );
    }
    let held: HashSet<String> = universe
        .iter()
        .map(|p| format!("{:?}", p.pool).to_ascii_lowercase())
        .collect();
    let held_tokens: HashMap<String, (Address, Address)> = universe
        .iter()
        .map(|p| {
            (
                format!("{:?}", p.pool).to_ascii_lowercase(),
                (p.token0, p.token1),
            )
        })
        .collect();
    let meta = load_unified_meta(&args.universe).ok();

    // ── datasets ───────────────────────────────────────────────────────────
    let events = load_missed_arb_events(&args.arbs)
        .with_context(|| format!("load arbs {}", args.arbs.display()))?;
    if events.is_empty() {
        bail!("no usable arb events in {}", args.arbs.display());
    }
    let census =
        load_census(&args.census).with_context(|| format!("load census {}", args.census.display()))?;
    info!(
        events = events.len(),
        census_pools = census.len(),
        universe_pools = held.len(),
        "loaded inputs"
    );

    // ── candidate TVL (optional RPC) ────────────────────────────────────────
    let mut tvl: HashMap<String, PoolTvl> = HashMap::new();
    let mut tvl_measured = false;
    let mut tvl_block = None;

    if let Some(rpc) = &args.rpc_url {
        let candidates = candidate_pools_for_valuation(&events, &universe, &census);
        if candidates.len() <= universe.len() {
            warn!("--rpc-url given but no missing pool has a census token pair to value");
        } else {
            // Valuation is two reads per pool side, so pace it like the
            // generator does: throttle scaled to the pool count (WHI-862/921).
            let mut rpc_config = RpcProviderConfig::from_env();
            rpc_config.throttle_rps = recommended_throttle_rps(candidates.len());
            info!(
                throttle_rps = rpc_config.throttle_rps,
                pools = candidates.len(),
                "valuation throttle"
            );
            let provider = connect_http_provider(rpc, &rpc_config)
                .with_context(|| format!("build provider for {rpc}"))?;
            // Prefer the universe's own snapshot block: the TVL floor verdict is
            // only an attribution of the generator's decision if it is read at
            // the block the generator read.
            let block = match (args.tvl_block, meta.as_ref().map(|m| m.snapshot_block)) {
                (Some(b), _) => b,
                (None, Some(snapshot)) => {
                    info!(snapshot, "pinning TVL reads to the universe snapshot block");
                    snapshot
                }
                (None, None) => {
                    warn!(
                        "no universe meta: pinning TVL reads to chain head, so floor \
                         attribution reflects current state, not the generator's"
                    );
                    provider
                        .get_block_number()
                        .await
                        .context("fetch chain head for TVL pin")?
                }
            };
            info!(
                pools = candidates.len(),
                block, "valuing candidate pools (read-only)"
            );
            let valued = value_pools_wmnt(&provider, &candidates, settlement, block)
                .await
                .context("value candidate pools")?;
            // Held pools were only a price basis; the TVL map covers candidates.
            for c in &candidates {
                let key = format!("{:?}", c.pool).to_ascii_lowercase();
                if held.contains(&key) {
                    continue;
                }
                tvl.insert(
                    key,
                    match valued.get(&c.pool) {
                        Some(Some(v)) => PoolTvl::Valued(*v),
                        // Present-but-None and absent both mean "no valuation
                        // established"; never fall through to a silent zero.
                        _ => PoolTvl::Unavailable,
                    },
                );
            }
            tvl_measured = true;
            tvl_block = Some(block);
        }
    }

    // ── analyze ────────────────────────────────────────────────────────────
    let cfg = AnalysisConfig {
        held,
        held_tokens,
        settlement,
        max_hops: args.max_hops as u32,
        min_tvl_wmnt_wei: min_tvl,
        tvl,
        tvl_measured,
        tvl_block,
        set_sizes,
        top_n: args.top_n,
        non_trivial_pct: args.non_trivial_pct,
        chain_gross_usd_per_day: args.chain_gross_usd_per_day,
        universe_fingerprint: meta.as_ref().and_then(|m| m.fingerprint.clone()),
        universe_snapshot_block: meta.as_ref().map(|m| m.snapshot_block),
        arb_dataset: basename(&args.arbs),
        census_dataset: basename(&args.census),
    };
    let report = analyze_missed_arbs(&events, &census, &cfg);

    // ── emit ───────────────────────────────────────────────────────────────
    let md = render_missed_arb_markdown(&report);
    if let Some(path) = &args.json_out {
        write_out(path, &serde_json::to_string_pretty(&report)?)?;
        info!(path = %path.display(), "wrote JSON report");
    }
    if let Some(path) = &args.md_out {
        write_out(path, &md)?;
        info!(path = %path.display(), "wrote Markdown report");
    }
    if args.json_out.is_none() && args.md_out.is_none() {
        println!("{md}");
    } else {
        println!("{}", report.verdict.statement);
        println!("{}", report.verdict.economics_caveat);
    }

    Ok(())
}

/// Missing pools that carry a census token pair, as valuation candidates.
///
/// The frozen universe is included too: pricing a non-WMNT candidate needs a
/// direct WMNT pair for one of its tokens *somewhere in the candidate list*, so
/// a narrower list would quarantine pools the generator could value. Held pools
/// are marked so the caller can drop them from the TVL map afterwards.
fn candidate_pools_for_valuation(
    events: &[amms::service::MissedArbEvent],
    universe: &[CandidatePool],
    census: &HashMap<String, amms::service::PoolCensusEntry>,
) -> Vec<CandidatePool> {
    let mut seen: HashSet<String> = universe
        .iter()
        .map(|p| format!("{:?}", p.pool).to_ascii_lowercase())
        .collect();
    let mut out: Vec<CandidatePool> = universe.to_vec();
    for event in events {
        for pool in &event.pools {
            if !seen.insert(pool.clone()) {
                continue;
            }
            let Some(entry) = census.get(pool) else { continue };
            let (Some(t0), Some(t1)) = (
                entry.token0.as_deref().and_then(|t| t.parse::<Address>().ok()),
                entry.token1.as_deref().and_then(|t| t.parse::<Address>().ok()),
            ) else {
                continue;
            };
            let Ok(addr) = pool.parse::<Address>() else {
                continue;
            };
            out.push(CandidatePool {
                protocol: entry.kind.clone().unwrap_or_default(),
                factory: entry
                    .factory
                    .as_deref()
                    .and_then(|f| f.parse::<Address>().ok())
                    .unwrap_or(Address::ZERO),
                pool: addr,
                token0: t0,
                token1: t1,
                fee_tier: None,
                bin_step: None,
                creation_block: None,
            });
        }
    }
    out
}

fn parse_set_sizes(raw: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let n: usize = part
            .parse()
            .with_context(|| format!("parse --set-sizes entry {part:?}"))?;
        if n == 0 {
            bail!("--set-sizes entries must be > 0");
        }
        out.push(n);
    }
    if out.is_empty() {
        bail!("--set-sizes produced no sizes");
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

fn basename(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn write_out(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
    }
    fs::write(path, body).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
