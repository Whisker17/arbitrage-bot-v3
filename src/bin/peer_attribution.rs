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
//!
//! ## Six-way breakdown (WHI-1412)
//!
//! `--gas-profile` splits the in-universe residual by the engine's own route-class
//! predicate (`topology_profile_support`, the WHI-1409 pre-simulation filter):
//! `route_class_unknown` (no profile entry at any bucket) vs
//! `route_class_unapproved` (entries, none approved). `--six-way-json-out` writes
//! absent pool / route class unknown / route class unapproved / evaluated and
//! unprofitable / profitable but not attempted / attempted and lost the race, with
//! the out-of-scope separators and any residual counted explicitly.
//!
//! `--pre-state` keys every piece of ledger evidence (observations, dirty-set
//! views, candidates) by `observed block + 1` — the earliest block a transaction
//! built on that post-block state could land in — so a peer arb at block N is
//! judged against the engine's post-(N−1) state, not the post-arb state of N.
//!
//! Candidate rows that carry no joinable `context` row (the signerless shadow
//! ledger writes none) are ingested directly: block = the preceding observation
//! row, ordered pools = the `signature=` segment of `detail`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use amms::execution::{
    attach_benchmark_buckets, attribute_all, load_block_views, load_events, load_events_jsonl,
    render_peer_attribution_markdown, write_peer_attribution_report, AttributedEvent,
    AttributionInputs, BlockDiscoveryView, Cause, LedgerBytes, ProtocolKind, RuntimeGasProfile,
    RuntimeProfileConfig, ShadowLedgerIndex, ShadowOpportunity, UniverseContext,
};
use amms::service::fee_scoring::{topology_profile_support, ProfileSupport};
use amms::service::normalize_address;
use clap::Parser;
use eyre::{bail, eyre, Context, Result};
use serde::Serialize;

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
    /// Gas profile artifact (mainnet identity-checked) for the route-class split.
    #[arg(long)]
    gas_profile: Option<PathBuf>,
    /// Key ledger evidence by `observed block + 1` (judge a peer arb at N
    /// against the engine's post-(N−1) state). Disables the WHI-715 bucket
    /// cross-check, which keys at the event block.
    #[arg(long, default_value_t = false)]
    pre_state: bool,
    /// Six-way breakdown JSON (requires `--gas-profile`).
    #[arg(long)]
    six_way_json_out: Option<PathBuf>,
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
    let offset = u64::from(args.pre_state);
    let ledger_index = if ledger_bytes.is_empty() {
        None
    } else {
        let mut index = ShadowLedgerIndex::from_ledgers(&ledger_bytes)
            .map_err(|e| eyre::eyre!(e.to_string()))
            .context("index ledgers")?;
        for o in &mut index.opportunities {
            o.block_number += offset;
        }
        for lb in &ledger_bytes {
            let added = add_bare_candidates(&mut index, lb, offset)
                .with_context(|| format!("bare candidate rows {}", lb.label))?;
            println!("bare candidate rows ingested from {}: {added}", lb.label);
        }
        Some(index)
    };

    let mut observed: BTreeSet<u64> = BTreeSet::new();
    let mut views_from_ledger = std::collections::HashMap::new();
    for lb in &ledger_bytes {
        let (blocks, disc) =
            amms::execution::peer_attribution::load_observation_index_from_ledger(lb)
                .with_context(|| format!("observations {}", lb.label))?;
        observed.extend(blocks.into_iter().map(|b| b + offset));
        views_from_ledger.extend(disc.into_iter().map(|(b, v)| (b + offset, v)));
    }
    let observed_ref = if observed.is_empty() {
        None
    } else {
        Some(&observed)
    };

    let mut views: HashMap<u64, BlockDiscoveryView> = match args.block_views {
        Some(ref p) => load_block_views(p)
            .with_context(|| format!("load block views {}", p.display()))?
            .into_iter()
            .map(|(b, v)| (b + offset, v))
            .collect(),
        None => HashMap::new(),
    };
    // Ledger-embedded discovery (post WHI-957 observation fields) fills gaps;
    // explicit --block-views wins on key collision.
    for (k, v) in views_from_ledger {
        views.entry(k).or_insert(v);
    }
    let views = if views.is_empty() { None } else { Some(views) };

    let inputs = AttributionInputs {
        events: &events,
        universe: &universe,
        ledger_index: ledger_index.as_ref(),
        observed_blocks: observed_ref,
        block_views: views.as_ref(),
        outside_window_is_unattributable: !args.outside_window_is_skip,
    };

    let mut report = attribute_all(&inputs).context("attribute")?;
    if args.with_benchmark_buckets && !ledger_bytes.is_empty() && !args.pre_state {
        attach_benchmark_buckets(&mut report, &events, &ledger_bytes)
            .context("benchmark buckets")?;
    }
    if args.pre_state {
        report.notes.push(
            "pre_state: ledger evidence keyed by observed block + 1 (event at N judged against post-(N-1) state); WHI-715 bucket cross-check skipped".into(),
        );
    }

    let six_way = match args.gas_profile {
        Some(ref p) => {
            let profile =
                RuntimeGasProfile::load(p, RuntimeProfileConfig::mantle_mainnet(Vec::new()))
                    .map_err(|e| eyre!("load gas profile {}: {e}", p.display()))?;
            let protocols = load_pool_protocols(&args.universe)?;
            let mut sw = build_six_way(&report.events, &protocols, &profile)?;
            sw.pre_state = args.pre_state;
            println!(
                "six_way: in_scope={} absent_pool={} route_class_unknown={} route_class_unapproved={} evaluated_and_unprofitable={} profitable_but_not_attempted={} attempted_and_lost_race={} residual={:?}",
                sw.in_scope_denominator,
                sw.buckets["absent_pool"],
                sw.buckets["route_class_unknown"],
                sw.buckets["route_class_unapproved"],
                sw.buckets["evaluated_and_unprofitable"],
                sw.buckets["profitable_but_not_attempted"],
                sw.buckets["attempted_and_lost_race"],
                sw.residual,
            );
            Some(sw)
        }
        None => None,
    };
    if args.six_way_json_out.is_some() && six_way.is_none() {
        bail!("--six-way-json-out requires --gas-profile");
    }
    if let (Some(p), Some(sw)) = (args.six_way_json_out.as_ref(), six_way.as_ref()) {
        let mut sw = sw.clone();
        if args.summary_only {
            sw.events.clear();
        }
        std::fs::write(p, serde_json::to_string_pretty(&sw)? + "\n")
            .with_context(|| format!("write {}", p.display()))?;
        println!("wrote {}", p.display());
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
        let mut md = render_peer_attribution_markdown(&report);
        if let Some(sw) = six_way.as_ref() {
            md.push_str(&render_six_way_markdown(sw));
        }
        std::fs::write(&p, md).with_context(|| format!("write {}", p.display()))?;
        println!("wrote {}", p.display());
    }

    Ok(())
}

/// Schema id of the `--six-way-json-out` artifact.
const SIX_WAY_SCHEMA_VERSION: &str = "whisker-arb/peer-attribution-six-way/v1";

/// The six WHI-1412 causes, in reporting order.
const SIX_WAY_BUCKETS: [&str; 6] = [
    "absent_pool",
    "route_class_unknown",
    "route_class_unapproved",
    "evaluated_and_unprofitable",
    "profitable_but_not_attempted",
    "attempted_and_lost_race",
];

#[derive(Debug, Clone, Serialize)]
struct SixWayEvent {
    tx_hash: String,
    block_number: u64,
    bucket: String,
    topology: Option<String>,
    lib_cause: &'static str,
    detail: String,
}

/// WHI-1412 six-way breakdown. `event_count` = Σ out_of_strategy_scope +
/// Σ buckets + Σ residual (checked before it is returned).
#[derive(Debug, Clone, Serialize)]
struct SixWay {
    schema_version: &'static str,
    gas_profile_identity: String,
    pre_state: bool,
    event_count: usize,
    /// Separators the strategy excludes by design (aggregator, flash, hop cap, non-WMNT, adapter).
    out_of_strategy_scope: BTreeMap<&'static str, usize>,
    /// `event_count` − Σ `out_of_strategy_scope`; the base of `bucket_shares`.
    in_scope_denominator: usize,
    buckets: BTreeMap<&'static str, usize>,
    bucket_shares: BTreeMap<&'static str, f64>,
    /// In-scope events on an approvable route class that none of the six can claim
    /// (block not observed, dirty-cycle skip, no evidence). Explicit, never dropped.
    residual: BTreeMap<&'static str, usize>,
    /// Bucket → `h<n>:<protocols>` → count, for the route-class buckets and below.
    topologies: BTreeMap<&'static str, BTreeMap<String, usize>>,
    events: Vec<SixWayEvent>,
}

/// Universe CSV `pool` → the gas-profile protocol kind the engine builds for its
/// `protocol` label (agni-v2 → UniswapV2Pool → v2, agni-v3 → v3, moe → Moe LB).
fn load_pool_protocols(path: &Path) -> Result<HashMap<String, ProtocolKind>> {
    let mut rdr = csv::Reader::from_path(path)
        .with_context(|| format!("open universe {}", path.display()))?;
    let mut out = HashMap::new();
    for (i, rec) in rdr.deserialize::<HashMap<String, String>>().enumerate() {
        let row = rec.with_context(|| format!("universe row {}", i + 2))?;
        let (Some(pool), Some(label)) = (row.get("pool"), row.get("protocol")) else {
            bail!("universe row {}: missing pool/protocol", i + 2);
        };
        let kind = match label.trim() {
            "agni-v2" => ProtocolKind::V2,
            "agni-v3" => ProtocolKind::V3,
            "moe" => ProtocolKind::Moe,
            other => bail!("universe row {}: unknown protocol label `{other}`", i + 2),
        };
        out.insert(normalize_address(pool), kind);
    }
    Ok(out)
}

fn topology_label(protocols: &[ProtocolKind]) -> String {
    let names: Vec<&str> = protocols.iter().map(|p| p.as_str()).collect();
    format!("h{}:{}", protocols.len(), names.join("+"))
}

/// Map the lib's single cause per event onto the six WHI-1412 buckets, splitting
/// the in-universe events by the engine's route-class predicate first (a
/// structural property: the engine rejects an unapproved class before it
/// simulates, whatever the block or ledger say).
fn build_six_way(
    events: &[AttributedEvent],
    pool_protocols: &HashMap<String, ProtocolKind>,
    profile: &RuntimeGasProfile,
) -> Result<SixWay> {
    let mut out_of_strategy_scope = BTreeMap::new();
    let mut buckets: BTreeMap<&'static str, usize> =
        SIX_WAY_BUCKETS.iter().map(|b| (*b, 0)).collect();
    let mut residual: BTreeMap<&'static str, usize> = [
        Cause::BlockSkipped,
        Cause::DirtyCycleFilterSkipped,
        Cause::Unattributable,
    ]
    .iter()
    .map(|c| (c.as_str(), 0))
    .collect();
    let mut topologies: BTreeMap<&'static str, BTreeMap<String, usize>> = BTreeMap::new();
    let mut rows = Vec::with_capacity(events.len());

    for e in events {
        let mut topology = None;
        let bucket: &'static str =
            if e.cause.is_out_of_scope() || e.cause == Cause::AggregatorMisclass {
                *out_of_strategy_scope.entry(e.cause.as_str()).or_insert(0) += 1;
                e.cause.as_str()
            } else if e.cause == Cause::NotInUniverse {
                "absent_pool"
            } else if e.ordered_pools.is_empty() {
                Cause::Unattributable.as_str()
            } else {
                let protocols = e
                    .ordered_pools
                    .iter()
                    .map(|p| {
                        pool_protocols
                            .get(&normalize_address(p))
                            .copied()
                            .ok_or_else(|| {
                                eyre!(
                                    "{}: pool {p} passed the universe check but has no protocol",
                                    e.tx_hash
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let label = topology_label(&protocols);
                let support = topology_profile_support(profile, &protocols)
                    .map_err(|err| eyre!("{}: route class of {label}: {err}", e.tx_hash))?;
                topology = Some(label);
                match support {
                    ProfileSupport::Unknown => "route_class_unknown",
                    ProfileSupport::Unapproved => "route_class_unapproved",
                    ProfileSupport::Supported => match e.cause {
                        Cause::EvaluatedButUnprofitable => "evaluated_and_unprofitable",
                        Cause::ProfitableButNotAttempted => "profitable_but_not_attempted",
                        Cause::AttemptedAndLostRace => "attempted_and_lost_race",
                        other => other.as_str(),
                    },
                }
            };
        if let Some(n) = buckets.get_mut(bucket) {
            *n += 1;
        } else if let Some(n) = residual.get_mut(bucket) {
            *n += 1;
        }
        if let Some(t) = &topology {
            *topologies
                .entry(bucket)
                .or_default()
                .entry(t.clone())
                .or_insert(0) += 1;
        }
        rows.push(SixWayEvent {
            tx_hash: e.tx_hash.clone(),
            block_number: e.block_number,
            bucket: bucket.to_string(),
            topology,
            lib_cause: e.cause.as_str(),
            detail: e.detail.clone(),
        });
    }

    let oos: usize = out_of_strategy_scope.values().sum();
    let accounted = oos + buckets.values().sum::<usize>() + residual.values().sum::<usize>();
    if accounted != events.len() {
        bail!("six-way accounted {accounted} != events {}", events.len());
    }
    let in_scope_denominator = events.len() - oos;
    let bucket_shares = buckets
        .iter()
        .map(|(k, n)| {
            let share = if in_scope_denominator == 0 {
                0.0
            } else {
                *n as f64 / in_scope_denominator as f64
            };
            (*k, share)
        })
        .collect();
    Ok(SixWay {
        schema_version: SIX_WAY_SCHEMA_VERSION,
        gas_profile_identity: profile.artifact_digest().to_string(),
        pre_state: false,
        event_count: events.len(),
        out_of_strategy_scope,
        in_scope_denominator,
        buckets,
        bucket_shares,
        residual,
        topologies,
        events: rows,
    })
}

/// Ordered pools from a candidate row's `detail`
/// (`… signature=v2:0xIn->0xOut/0xPool|v3:…/0xPool`).
fn signature_pools(detail: &str) -> Option<Vec<String>> {
    let sig = detail
        .split_whitespace()
        .find_map(|t| t.strip_prefix("signature="))?;
    let pools: Vec<String> = sig
        .split('|')
        .map(|hop| {
            hop.rsplit('/')
                .next()
                .map(normalize_address)
                .unwrap_or_default()
        })
        .collect();
    (!pools.is_empty() && pools.iter().all(|p| p.len() == 42)).then_some(pools)
}

/// Ingest candidate rows that no `context` row joins (the signerless shadow writes
/// none): block = preceding observation row + `offset`, pools from `signature=`.
fn add_bare_candidates(
    index: &mut ShadowLedgerIndex,
    ledger: &LedgerBytes,
    offset: u64,
) -> Result<usize> {
    let known: HashSet<String> = index
        .opportunities
        .iter()
        .map(|o| o.digest.clone())
        .collect();
    let mut service = String::new();
    let mut last_block: Option<u64> = None;
    let mut added = 0;
    for (i, line) in ledger.bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_slice(line).with_context(|| format!("line {}", i + 1))?;
        match v.get("row_type").and_then(|x| x.as_str()) {
            Some("run_header") => {
                service = v["service"].as_str().unwrap_or_default().to_string();
            }
            Some("observation") => {
                last_block = v
                    .pointer("/snapshot_id/block_number")
                    .and_then(|x| x.as_u64());
            }
            Some("candidate") => {
                let digest = v["digest"].as_str().unwrap_or_default().to_string();
                if known.contains(&digest) {
                    continue;
                }
                let Some(block) = last_block else {
                    bail!("line {}: candidate row before any observation row", i + 1);
                };
                let detail = v["detail"].as_str().unwrap_or_default();
                let ordered_pools = signature_pools(detail).ok_or_else(|| {
                    eyre!(
                        "line {}: candidate detail has no parsable signature=",
                        i + 1
                    )
                })?;
                let amount_in = detail
                    .split_whitespace()
                    .find_map(|t| t.strip_prefix("amount_in="))
                    .unwrap_or("unrecorded")
                    .to_string();
                index.opportunities.push(ShadowOpportunity {
                    service: service.clone(),
                    block_number: block + offset,
                    digest,
                    opportunity_id: String::new(),
                    ordered_pools,
                    net_profit: "unrecorded".into(),
                    gross_profit: "unrecorded".into(),
                    amount_in,
                    outcome_kind: v
                        .pointer("/outcome/kind")
                        .and_then(|x| x.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    outcome_reason: detail.split_whitespace().next().map(str::to_string),
                });
                added += 1;
            }
            _ => {}
        }
    }
    Ok(added)
}

fn render_six_way_markdown(sw: &SixWay) -> String {
    let mut out = String::from("\n## Six-way breakdown (WHI-1412)\n\n");
    out.push_str(&format!(
        "- Gas profile: `{}`; pre_state: {}\n- Events: {}; out of strategy scope: {}; in-scope denominator: **{}**\n\n",
        sw.gas_profile_identity,
        sw.pre_state,
        sw.event_count,
        sw.out_of_strategy_scope.values().sum::<usize>(),
        sw.in_scope_denominator
    ));
    out.push_str("| Cause | Count | Share of in-scope |\n| --- | ---: | ---: |\n");
    for b in SIX_WAY_BUCKETS {
        out.push_str(&format!(
            "| `{b}` | {} | {:.2}% |\n",
            sw.buckets[b],
            sw.bucket_shares[b] * 100.0
        ));
    }
    for (k, n) in &sw.residual {
        out.push_str(&format!("| residual `{k}` | {n} | |\n"));
    }
    for (k, n) in &sw.out_of_strategy_scope {
        out.push_str(&format!("| out of scope `{k}` | {n} | |\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mainnet_profile() -> RuntimeGasProfile {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("config/gas_profiles/mantle_mainnet_v1.json");
        RuntimeGasProfile::load(&path, RuntimeProfileConfig::mantle_mainnet(Vec::new()))
            .expect("committed mainnet profile loads")
    }

    fn attributed(cause: Cause, pools: &[&str]) -> AttributedEvent {
        AttributedEvent {
            cause,
            bot_address: "0xbot".into(),
            tx_hash: format!("0x{}", pools.join("")),
            block_number: 7,
            ordered_pools: pools.iter().map(|p| p.to_string()).collect(),
            hop_count: Some(pools.len() as u32),
            funding: None,
            settlement_asset: None,
            route: None,
            benchmark_bucket: None,
            detail: String::new(),
        }
    }

    #[test]
    fn route_class_split_follows_the_engine_predicate() {
        use ProtocolKind::{Moe, V2, V3};
        let p = mainnet_profile();
        let s = |t: &[ProtocolKind]| topology_profile_support(&p, t).unwrap();
        assert_eq!(s(&[V2, V2, V2]), ProfileSupport::Supported);
        assert_eq!(s(&[V2, V3]), ProfileSupport::Supported); // ticks=0 approved only
        assert_eq!(s(&[V3, V3]), ProfileSupport::Unapproved);
        assert_eq!(s(&[Moe, V3]), ProfileSupport::Unapproved);
        assert_eq!(s(&[V3, V3, V3, V3]), ProfileSupport::Unknown);
    }

    #[test]
    fn six_way_places_every_event_exactly_once() {
        let pools: HashMap<String, ProtocolKind> = [
            (
                "0x00000000000000000000000000000000000000a2",
                ProtocolKind::V2,
            ),
            (
                "0x00000000000000000000000000000000000000b2",
                ProtocolKind::V2,
            ),
            (
                "0x00000000000000000000000000000000000000a3",
                ProtocolKind::V3,
            ),
            (
                "0x00000000000000000000000000000000000000b3",
                ProtocolKind::V3,
            ),
        ]
        .into_iter()
        .map(|(a, k)| (a.to_string(), k))
        .collect();
        let (a2, b2, a3, b3) = (
            "0x00000000000000000000000000000000000000a2",
            "0x00000000000000000000000000000000000000b2",
            "0x00000000000000000000000000000000000000a3",
            "0x00000000000000000000000000000000000000b3",
        );
        let four_v3 = [a3, b3, a3, b3];
        let events = vec![
            attributed(Cause::OutOfScopeHopCap, &four_v3),
            attributed(Cause::NotInUniverse, &["0xmissing", a2]),
            // Unapproved class wins over the lib's (post-arb) "evaluated" and over
            // an unobserved block: the engine never simulates that class.
            attributed(Cause::EvaluatedButUnprofitable, &[a3, b3]),
            attributed(Cause::Unattributable, &[b3, a3]),
            attributed(Cause::ProfitableButNotAttempted, &[a2, b2]),
            attributed(Cause::EvaluatedButUnprofitable, &[b2, a2]),
            attributed(Cause::AttemptedAndLostRace, &[a2, a3]),
            attributed(Cause::DirtyCycleFilterSkipped, &[b2, b2]),
        ];
        let sw = build_six_way(&events, &pools, &mainnet_profile()).unwrap();
        assert_eq!(sw.event_count, 8);
        assert_eq!(sw.out_of_strategy_scope["out_of_scope_hop_cap"], 1);
        assert_eq!(sw.in_scope_denominator, 7);
        assert_eq!(sw.buckets["absent_pool"], 1);
        assert_eq!(sw.buckets["route_class_unknown"], 0);
        assert_eq!(sw.buckets["route_class_unapproved"], 2);
        assert_eq!(sw.buckets["profitable_but_not_attempted"], 1);
        assert_eq!(sw.buckets["evaluated_and_unprofitable"], 1);
        assert_eq!(sw.buckets["attempted_and_lost_race"], 1);
        assert_eq!(sw.residual["dirty_cycle_filter_skipped"], 1);
        assert_eq!(sw.topologies["route_class_unapproved"]["h2:v3+v3"], 2);
        assert_eq!(sw.events.len(), 8);
    }

    #[test]
    fn bare_candidate_rows_key_to_the_next_block_under_pre_state() {
        let (a, b) = (
            "0x00000000000000000000000000000000000000a2",
            "0x00000000000000000000000000000000000000b2",
        );
        let bytes = format!(
            "{}\n{}\n{}\n{}\n",
            r#"{"row_type":"run_header","schema_version":"whisker-arb/shadow-ledger/v3","service":"bot","send_capability":"no_send"}"#,
            r#"{"row_type":"observation","snapshot_id":{"chain_id":5000,"block_number":100,"block_hash":"0x1"}}"#,
            format!(
                r#"{{"row_type":"candidate","digest":"0xd","outcome":{{"kind":"env_unsupported"}},"detail":"production_gate_blocked amount_in=10 signature=v2:0xw->0xx/{a}|v2:0xx->0xw/{b}"}}"#
            ),
            r#"{"row_type":"observation","snapshot_id":{"chain_id":5000,"block_number":101,"block_hash":"0x2"}}"#,
        );
        let ledger = LedgerBytes {
            label: "t".into(),
            bytes: bytes.into_bytes(),
        };
        let mut index = ShadowLedgerIndex::from_ledgers(std::slice::from_ref(&ledger)).unwrap();
        assert!(
            index.opportunities.is_empty(),
            "no context row → lib joins nothing"
        );
        assert_eq!(add_bare_candidates(&mut index, &ledger, 1).unwrap(), 1);
        let o = &index.opportunities[0];
        assert_eq!(
            (o.block_number, o.ordered_pools.clone()),
            (101, vec![a.to_string(), b.to_string()])
        );
        assert_eq!(o.outcome_reason.as_deref(), Some("production_gate_blocked"));

        let universe = UniverseContext {
            pools: [a, b].iter().map(|p| p.to_string()).collect(),
            adapter_factories: HashSet::new(),
            pool_factory: HashMap::new(),
            settlement_wmnt: format!("{:#x}", amms::service::config::DEFAULT_WMNT),
            max_hops: 3,
            aggregator_hop_threshold: 50,
        };
        let event = |block| amms::execution::KnownBotEvent {
            bot_address: "0xbot".into(),
            tx_hash: "0xt".into(),
            block_number: block,
            ordered_pools: vec![a.into(), b.into()],
            route: None,
            label: None,
            hop_count: Some(2),
            funding: Some("self_funded".into()),
            venues: None,
            settlement_asset: None,
        };
        let cause_at = |block| {
            let e = event(block);
            amms::execution::attribute_event(
                &e,
                &AttributionInputs {
                    events: std::slice::from_ref(&e),
                    universe: &universe,
                    ledger_index: Some(&index),
                    observed_blocks: None,
                    block_views: None,
                    outside_window_is_unattributable: true,
                },
            )
            .cause
        };
        // Peer landed at 101: we held a gate-blocked candidate on post-100 state.
        assert_eq!(cause_at(101), Cause::ProfitableButNotAttempted);
        // At 100 itself the candidate did not exist yet (it is built on post-100 state).
        assert_ne!(cause_at(100), Cause::ProfitableButNotAttempted);
    }
}
