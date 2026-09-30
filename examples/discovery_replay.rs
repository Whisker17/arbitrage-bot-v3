//! WHI-1527: offline, pinned discovery replay (one pass driver, one corpus format).
//!
//! These exact bytes are compiled unmodified at every compared commit. The
//! harness reads a frozen corpus written by `replay_capture`, builds one
//! `DiscoveryEngine`, and calls `discover` once per recorded pass. A raw
//! `metrics` recorder keeps every `stage::DISCOVERY` / `stage::OPTIMIZE`
//! sample (no buckets). Corpus parsing, pool-vector assembly and JSON output
//! happen outside `discover`, so they never enter either timer. There is no
//! provider here: no RPC at all.
//!
//! Output: one JSON line per pass with numeric counters from the structured
//! `DiscoveryStats` fields that exist at every compared commit, and the raw
//! timer samples in seconds.
//!
//! WHI-1572: `--estimator` enables the pinned discovery gas estimator (the
//! estimator-on arm of a paired replay; same binary, same corpus, same
//! profile). `--pool-universe` supplies pool → factory venue labels to both
//! arms. Each line also carries the WHI-1572 counters and the `discover` call's
//! thread CPU time.

use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, U256};
use amms::amms::amm::{AutomatedMarketMaker, AMM};
use amms::execution::{BlockFeeContext, RuntimeGasProfile, RuntimeProfileConfig};
use amms::metrics::{
    stage, DISCOVERY_REJECTED_TOTAL, LABEL_REASON, LABEL_STAGE, PIPELINE_STAGE_DURATION_SECONDS,
};
use amms::service::discovery::DiscoveryConfig;
use amms::service::fee_scoring::MeasuredFeeScoring;
use amms::service::gas_estimate::{DiscoveryGasEstimator, PoolVenueMap, MAINNET_ESTIMATOR_DIGEST};
use amms::service::path_index::DiscoveryEngine;
use amms::service::protocol::TipRefreshScope;
use amms::state_space::SnapshotId;
use clap::Parser;
use eyre::{eyre, Context, Result};
use metrics::{
    Counter, CounterFn, Gauge, Histogram, HistogramFn, Key, KeyName, Metadata, Recorder,
    SharedString, Unit,
};
use serde_json::{json, Value};

#[derive(Parser, Debug)]
struct Args {
    /// Corpus JSONL written by `replay_capture`.
    #[arg(long)]
    corpus: PathBuf,
    /// Gas profile artifact (the corpus's frozen copy).
    #[arg(long)]
    profile: PathBuf,
    /// Per-pass JSONL output.
    #[arg(long)]
    out: PathBuf,
    /// Free-form label copied into every output line (arm / repeat id).
    #[arg(long, default_value = "")]
    label: String,
    /// WHI-1572: discovery gas estimator artifact (estimator-on arm). Checked
    /// against the pinned mainnet digest and the loaded profile.
    #[arg(long)]
    estimator: Option<PathBuf>,
    /// WHI-1572: universe CSV for pool → factory venue labels (both arms).
    #[arg(long)]
    pool_universe: Option<PathBuf>,
}

/// Thread CPU seconds (the `discover` call is single-threaded).
fn thread_cpu_s() -> f64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid out-pointer; CLOCK_THREAD_CPUTIME_ID is supported on macOS and Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

const DISCOVERY: u8 = 0;
const OPTIMIZE: u8 = 1;

type Samples = Arc<Mutex<Vec<(u8, f64)>>>;

struct Sink {
    stage: u8,
    buf: Samples,
}

impl HistogramFn for Sink {
    fn record(&self, value: f64) {
        self.buf.lock().unwrap().push((self.stage, value));
    }
}

/// WHI-1572: per-reason `discovery_rejected_total` increments (bounded labels).
type Rejects = Arc<Mutex<BTreeMap<String, u64>>>;

struct RejectSink {
    reason: String,
    buf: Rejects,
}

impl CounterFn for RejectSink {
    fn increment(&self, value: u64) {
        *self.buf.lock().unwrap().entry(self.reason.clone()).or_default() += value;
    }
    fn absolute(&self, _: u64) {}
}

/// Keeps raw DISCOVERY / OPTIMIZE histogram samples and the discovery reject
/// reasons; everything else is a no-op.
struct RawStageRecorder {
    buf: Samples,
    rejects: Rejects,
}

impl Recorder for RawStageRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        if key.name() != DISCOVERY_REJECTED_TOTAL {
            return Counter::noop();
        }
        let reason = key
            .labels()
            .find(|l| l.key() == LABEL_REASON)
            .map(|l| l.value().to_string())
            .unwrap_or_default();
        Counter::from_arc(Arc::new(RejectSink {
            reason,
            buf: self.rejects.clone(),
        }))
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        if key.name() != PIPELINE_STAGE_DURATION_SECONDS {
            return Histogram::noop();
        }
        let stage_label = key
            .labels()
            .find(|l| l.key() == LABEL_STAGE)
            .map(|l| l.value().to_string());
        let code = match stage_label.as_deref() {
            Some(s) if s == stage::DISCOVERY => DISCOVERY,
            Some(s) if s == stage::OPTIMIZE => OPTIMIZE,
            _ => return Histogram::noop(),
        };
        Histogram::from_arc(Arc::new(Sink {
            stage: code,
            buf: self.buf.clone(),
        }))
    }
}

struct Pass {
    pass: u64,
    block: u64,
    hash: B256,
    timestamp: u64,
    base_fee: u128,
    gas_limit: u64,
    scope: TipRefreshScope,
    updated: Vec<AMM>,
}

fn field<'a>(v: &'a Value, k: &str) -> Result<&'a Value> {
    v.get(k).ok_or_else(|| eyre!("corpus field `{k}` missing"))
}

fn as_u64(v: &Value, k: &str) -> Result<u64> {
    field(v, k)?
        .as_u64()
        .ok_or_else(|| eyre!("corpus field `{k}` not u64"))
}

fn as_str<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    field(v, k)?
        .as_str()
        .ok_or_else(|| eyre!("corpus field `{k}` not a string"))
}

/// Pass line. Parsed straight into `AMM` (not via `serde_json::Value`, which
/// cannot hold the pools' 128-bit integers).
#[derive(serde::Deserialize)]
struct PassLine {
    pass: u64,
    block: u64,
    hash: String,
    timestamp: u64,
    base_fee: String,
    gas_limit: u64,
    scope: String,
    dirty: Vec<String>,
    #[serde(default)]
    updated: Vec<AMM>,
}

fn parse_pass(line: &str) -> Result<Pass> {
    let v: PassLine = serde_json::from_str(line).context("pass line")?;
    let scope = match v.scope.as_str() {
        "full" => TipRefreshScope::Full,
        "touched" => {
            let mut set = HashSet::new();
            for a in &v.dirty {
                set.insert(a.parse()?);
            }
            TipRefreshScope::Touched(set)
        }
        other => return Err(eyre!("unknown scope {other}")),
    };
    Ok(Pass {
        pass: v.pass,
        block: v.block,
        hash: v.hash.parse()?,
        timestamp: v.timestamp,
        base_fee: v.base_fee.parse()?,
        gas_limit: v.gas_limit,
        scope,
        updated: v.updated,
    })
}

/// `fee_resolution_failures` exists only at commits that carry it; there is no
/// structured field common to every arm, so take it from `Debug` when present.
fn debug_field(debug: &str, name: &str) -> Option<u64> {
    let tail = debug.split(&format!("{name}: ")).nth(1)?;
    tail.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Diagnostics only (e.g. RUST_LOG=bot.discovery=debug); never set for timed runs.
    if std::env::var_os("RUST_LOG").is_some() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_writer(std::io::stderr)
            .init();
    }

    // --- Load everything before the first timed call. ---
    let reader = BufReader::new(std::fs::File::open(&args.corpus).context("open corpus")?);
    let mut lines = reader.lines();
    let meta: Value = serde_json::from_str(&lines.next().ok_or_else(|| eyre!("empty corpus"))??)?;
    if as_str(&meta, "kind")? != "meta" {
        return Err(eyre!("first corpus line must be meta"));
    }
    let cfg = field(&meta, "discovery_config")?;
    let settlement: Address = as_str(cfg, "settlement_asset")?.parse()?;
    let max_hops = as_u64(cfg, "max_hops")? as usize;
    let max_input: U256 = as_str(cfg, "max_input_wei")?.parse()?;
    let min_profit: U256 = as_str(cfg, "min_profit_wei")?.parse()?;
    let priority: u128 = as_str(cfg, "priority_fee_wei")?.parse()?;
    let reserve = as_u64(cfg, "block_gas_reserve")?;
    let chain_id = as_u64(&meta, "chain_id")?;
    let profile_digest = as_str(&meta, "profile_digest")?.to_string();

    let mut passes = Vec::new();
    for line in lines {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        passes.push(parse_pass(&line)?);
    }

    let profile = Arc::new(
        RuntimeGasProfile::load(
            &args.profile,
            RuntimeProfileConfig {
                expected_content_digest: profile_digest.clone(),
                ..RuntimeProfileConfig::mantle_mainnet(Vec::new())
            },
        )
        .map_err(|e| eyre!("load gas profile: {e}"))?,
    );

    let estimator = match &args.estimator {
        Some(path) => Some(Arc::new(
            DiscoveryGasEstimator::load(path, MAINNET_ESTIMATOR_DIGEST, &profile)
                .map_err(|e| eyre!("load estimator: {e}"))?,
        )),
        None => None,
    };
    let pool_venues = match &args.pool_universe {
        Some(path) => {
            let mut rdr = csv::Reader::from_path(path).context("open pool universe")?;
            let mut pairs = Vec::new();
            for row in rdr.records() {
                let row = row?;
                pairs.push((row[2].parse::<Address>()?, row[1].parse::<Address>()?));
            }
            Some(Arc::new(PoolVenueMap::from_pairs(pairs)))
        }
        None => None,
    };

    let buf: Samples = Arc::new(Mutex::new(Vec::new()));
    let rejects: Rejects = Arc::new(Mutex::new(BTreeMap::new()));
    metrics::set_global_recorder(RawStageRecorder {
        buf: buf.clone(),
        rejects: rejects.clone(),
    })
        .map_err(|e| eyre!("install recorder: {e}"))?;

    let mut state: BTreeMap<Address, AMM> = BTreeMap::new();
    let mut engine: Option<DiscoveryEngine> = None;
    let mut out = BufWriter::new(std::fs::File::create(&args.out)?);

    for p in passes {
        for amm in p.updated {
            state.insert(amm.address(), amm);
        }
        // Address order (BTreeMap): fixed across arms and repeats.
        let pools: Vec<AMM> = state.values().cloned().collect();

        let mut discovery = DiscoveryConfig::for_settlement(settlement);
        discovery.max_hops = max_hops;
        discovery.min_profit = min_profit;
        discovery.max_input = max_input;
        discovery.block_timestamp = p.timestamp;
        discovery.snapshot_id = SnapshotId::new(chain_id, p.block, p.hash);
        discovery.measured_fee = Some(MeasuredFeeScoring::new(
            profile.clone(),
            priority,
            reserve,
            BlockFeeContext {
                block_number: p.block,
                block_hash: p.hash,
                base_fee_per_gas: p.base_fee,
                block_gas_limit: p.gas_limit,
            },
        ));

        discovery.gas_estimator = estimator.clone();
        discovery.pool_venues = pool_venues.clone();

        if engine.is_none() {
            engine = Some(DiscoveryEngine::build(&pools, settlement, max_hops)?);
        }
        let eng = engine.as_mut().expect("built");

        buf.lock().unwrap().clear();
        rejects.lock().unwrap().clear();
        let cpu0 = thread_cpu_s();
        let wall = std::time::Instant::now();
        let (opps, stats) = eng.discover(&pools, &discovery, &p.scope)?;
        let wall_s = wall.elapsed().as_secs_f64();
        let cpu_s = thread_cpu_s() - cpu0;
        let samples = std::mem::take(&mut *buf.lock().unwrap());
        let reject_reasons = std::mem::take(&mut *rejects.lock().unwrap());

        let discovery_s: Vec<f64> = samples.iter().filter(|s| s.0 == DISCOVERY).map(|s| s.1).collect();
        let optimize_s: Vec<f64> = samples.iter().filter(|s| s.0 == OPTIMIZE).map(|s| s.1).collect();
        let debug = format!("{stats:?}");
        let line = json!({
            "label": args.label,
            "pass": p.pass,
            "block": p.block,
            "scope_in": p.scope.as_metric_label(),
            "stats": {
                "cycles_total": stats.cycles_total,
                "cycles_optimized": stats.cycles_optimized,
                "dirty_pools": stats.dirty_pools,
                "paths_quoted": stats.paths_quoted,
                "amm_quotes": stats.amm_quotes,
                "gas_rescores": stats.gas_rescores,
                "scope": stats.scope,
                "rejects": stats.rejects,
                "liveness_alarm": stats.liveness_alarm,
                "fee_resolution_failures_debug": debug_field(&debug, "fee_resolution_failures"),
                "fee_resolution_failures": stats.fee_resolution_failures,
                "search_tiers": stats.search_tiers,
                "measured_resolutions": stats.measured_resolutions,
                "estimated_resolutions": stats.estimated_resolutions,
                "fee_failure_reasons": stats.fee_failure_reasons,
                "simulations": stats.simulations,
                "candidates_measured": stats.candidates_measured,
                "candidates_estimated": stats.candidates_estimated,
            },
            "reject_reasons": reject_reasons,
            "opportunities": opps.len(),
            "discover_call_wall_s": wall_s,
            "discover_call_cpu_s": cpu_s,
            "discovery_s": discovery_s,
            "optimize_s": optimize_s,
        });
        serde_json::to_writer(&mut out, &line)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
