//! Peer-arb attribution for WHI-957 (bucket-1 cause taxonomy).
//!
//! Takes WHI-956 ground-truth events (+ optional concurrent shadow ledger and
//! per-block discovery view) and assigns **exactly one** cause to every event.
//!
//! ## Cause taxonomy (comment on WHI-957 + operator brief)
//!
//! Reachable miss / economic path (the five):
//! 1. [`Cause::NotInUniverse`] — some hop pool is outside the frozen universe
//! 2. [`Cause::DirtyCycleFilterSkipped`] — cycle is over universe pools, block
//!    was processed, but the dirty set touched none of its pools (WHI-940)
//! 3. [`Cause::EvaluatedButUnprofitable`] — we produced a candidate that was
//!    not a profitable preflight Pass
//! 4. [`Cause::ProfitableButNotAttempted`] — profitable Pass / sized path but
//!    eligibility / attempt budget blocked send
//! 5. [`Cause::AttemptedAndLostRace`] — we had a profitable Pass (shadow: would
//!    have been profitable); peer still landed first
//!
//! Separators (never mixed into the five):
//! - [`Cause::BlockSkipped`] — head not processed (WHI-977); separate count
//! - Out-of-scope: flash-loan, hop > max, non-WMNT settlement, adapter venue
//! - [`Cause::AggregatorMisclass`] — hop > 50 (noise, exclude from rates)
//! - [`Cause::Unattributable`] — residual with an explicit reason (never silent)
//!
//! Priority is fixed so the second and third causes cannot collapse into one.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::str::FromStr;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::execution::shadow_bot_benchmark::{
    compare, load_known_bot_events, Bucket, KnownBotEvent, LedgerBytes, ShadowLedgerIndex,
    ShadowOpportunity, BENCHMARK_REPORT_SCHEMA_VERSION,
};
use crate::service::arb_coverage::normalize_address;
use crate::service::config::DEFAULT_WMNT;

/// Report schema for peer attribution artifacts.
pub const PEER_ATTRIBUTION_SCHEMA_VERSION: &str = "whisker-arb/peer-attribution/v1";

/// Hop count above which a "path" is treated as aggregator batch noise, not
/// atomic arb (operator brief on WHI-957: 8/10501 rows, max 1139).
pub const AGGREGATOR_HOP_THRESHOLD: u32 = 50;

/// Strategy hop cap (WHI-529 / `EFFECTIVE_MAX_HOPS`).
pub const DEFAULT_MAX_HOPS: u32 = 3;

#[derive(Debug, Error)]
pub enum PeerAttributionError {
    #[error("io error on {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("json error on {path}: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },
    #[error("{0}")]
    Message(String),
}

/// Exactly one attributed cause per ground-truth event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// Aggregator / mill noise (hop > 50); excluded from reachable-miss rates.
    AggregatorMisclass,
    /// Flash-loan funded (out of scope this release).
    OutOfScopeFlashLoan,
    /// Path hop count > strategy max (out of scope by design).
    OutOfScopeHopCap,
    /// Settlement asset is not WMNT (out of scope by design).
    OutOfScopeNonWmntSettlement,
    /// Venue requires an adapter not loadable in-universe.
    OutOfScopeAdapterRequired,
    /// At least one ordered pool is outside the frozen universe.
    NotInUniverse,
    /// Block was not processed by the watch loop (WHI-977); not one of the five.
    BlockSkipped,
    /// Cycle over universe pools; dirty set missed every hop (WHI-940).
    DirtyCycleFilterSkipped,
    /// Candidate existed but was not a profitable Pass.
    EvaluatedButUnprofitable,
    /// Profitable / sized but eligibility or attempt budget blocked.
    ProfitableButNotAttempted,
    /// We scored a profitable Pass; peer still won the race.
    AttemptedAndLostRace,
    /// Explicit residual — never silent drop.
    Unattributable,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AggregatorMisclass => "aggregator_misclass",
            Self::OutOfScopeFlashLoan => "out_of_scope_flash_loan",
            Self::OutOfScopeHopCap => "out_of_scope_hop_cap",
            Self::OutOfScopeNonWmntSettlement => "out_of_scope_non_wmnt_settlement",
            Self::OutOfScopeAdapterRequired => "out_of_scope_adapter_required",
            Self::NotInUniverse => "not_in_universe",
            Self::BlockSkipped => "block_skipped",
            Self::DirtyCycleFilterSkipped => "dirty_cycle_filter_skipped",
            Self::EvaluatedButUnprofitable => "evaluated_but_unprofitable",
            Self::ProfitableButNotAttempted => "profitable_but_not_attempted",
            Self::AttemptedAndLostRace => "attempted_and_lost_race",
            Self::Unattributable => "unattributable",
        }
    }

    /// True when the cause is one of the five reachable economic/miss classes.
    pub fn is_five_core(self) -> bool {
        matches!(
            self,
            Self::NotInUniverse
                | Self::DirtyCycleFilterSkipped
                | Self::EvaluatedButUnprofitable
                | Self::ProfitableButNotAttempted
                | Self::AttemptedAndLostRace
        )
    }

    /// Out-of-scope by design (not a miss rate numerator).
    pub fn is_out_of_scope(self) -> bool {
        matches!(
            self,
            Self::OutOfScopeFlashLoan
                | Self::OutOfScopeHopCap
                | Self::OutOfScopeNonWmntSettlement
                | Self::OutOfScopeAdapterRequired
        )
    }
}

/// Per-block discovery view for dirty-cycle attribution (sidecar or future
/// ledger observation fields).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockDiscoveryView {
    pub block_number: u64,
    /// When true, the watch loop did not process this head (pin skip / halt).
    #[serde(default)]
    pub skipped: bool,
    #[serde(default)]
    pub skip_reason: Option<String>,
    /// Dirty pool addresses for the block (lower-case hex). Empty + not skipped
    /// means processed with an empty dirty set (optimize_ratio ≈ 0).
    #[serde(default)]
    pub dirty_pools: Vec<String>,
    /// Optional discovery counters for diagnostics.
    #[serde(default)]
    pub cycles_optimized: Option<u64>,
    #[serde(default)]
    pub cycles_total: Option<u64>,
    #[serde(default)]
    pub dirty_pool_count: Option<u64>,
    /// `"full"` | `"touched"` when known.
    #[serde(default)]
    pub scope: Option<String>,
}

/// Frozen universe + settlement context for offline membership checks.
#[derive(Debug, Clone)]
pub struct UniverseContext {
    pub pools: HashSet<String>,
    /// factory (lower) → adapter_required
    pub adapter_factories: HashSet<String>,
    /// pool → factory
    pub pool_factory: HashMap<String, String>,
    pub settlement_wmnt: String,
    pub max_hops: u32,
    pub aggregator_hop_threshold: u32,
}

impl UniverseContext {
    pub fn from_pool_csv(path: &Path) -> Result<Self, PeerAttributionError> {
        let file = File::open(path).map_err(|source| PeerAttributionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let mut rdr = csv::Reader::from_reader(file);
        let mut pools = HashSet::new();
        let mut pool_factory = HashMap::new();
        for (i, rec) in rdr.deserialize::<HashMap<String, String>>().enumerate() {
            let row = rec.map_err(|e| {
                PeerAttributionError::Message(format!(
                    "universe csv {} row {}: {e}",
                    path.display(),
                    i + 2
                ))
            })?;
            let pool = row
                .get("pool")
                .map(|s| normalize_address(s))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    PeerAttributionError::Message(format!(
                        "universe csv {} row {}: missing pool",
                        path.display(),
                        i + 2
                    ))
                })?;
            if let Some(f) = row.get("factory") {
                let f = normalize_address(f);
                if !f.is_empty() {
                    pool_factory.insert(pool.clone(), f);
                }
            }
            pools.insert(pool);
        }
        Ok(Self {
            pools,
            adapter_factories: HashSet::new(),
            pool_factory,
            settlement_wmnt: format!("{DEFAULT_WMNT:#x}"),
            max_hops: DEFAULT_MAX_HOPS,
            aggregator_hop_threshold: AGGREGATOR_HOP_THRESHOLD,
        })
    }

    pub fn with_adapter_factories(mut self, factories: impl IntoIterator<Item = String>) -> Self {
        self.adapter_factories = factories
            .into_iter()
            .map(|s| normalize_address(&s))
            .filter(|s| !s.is_empty())
            .collect();
        self
    }

    pub fn contains_pool(&self, addr: &str) -> bool {
        self.pools.contains(&normalize_address(addr))
    }
}

/// Load per-block discovery views from JSONL.
pub fn load_block_views(path: &Path) -> Result<HashMap<u64, BlockDiscoveryView>, PeerAttributionError> {
    let file = File::open(path).map_err(|source| PeerAttributionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut out = HashMap::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| PeerAttributionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let v: BlockDiscoveryView =
            serde_json::from_str(line).map_err(|source| PeerAttributionError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        out.insert(v.block_number, v);
    }
    Ok(out)
}

/// Parse observation blocks from a shadow ledger (processed heads).
pub fn load_observed_blocks_from_ledger(
    ledger: &LedgerBytes,
) -> Result<BTreeSet<u64>, PeerAttributionError> {
    let (blocks, _) = load_observation_index_from_ledger(ledger)?;
    Ok(blocks)
}

/// Parse observation rows into processed blocks + optional discovery views
/// (WHI-957 dirty-set fields on observation rows).
pub fn load_observation_index_from_ledger(
    ledger: &LedgerBytes,
) -> Result<(BTreeSet<u64>, HashMap<u64, BlockDiscoveryView>), PeerAttributionError> {
    let mut blocks = BTreeSet::new();
    let mut views = HashMap::new();
    for (i, line) in ledger.bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_slice(line).map_err(|source| PeerAttributionError::Json {
                path: format!("{}:line {}", ledger.label, i + 1),
                source,
            })?;
        if v.get("row_type").and_then(|x| x.as_str()) != Some("observation") {
            continue;
        }
        let Some(bn) = v
            .pointer("/snapshot_id/block_number")
            .and_then(|x| x.as_u64())
        else {
            continue;
        };
        blocks.insert(bn);
        if let Some(disc) = v.get("discovery") {
            let dirty_pools: Vec<String> = disc
                .get("dirty_pools")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|x| x.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let dirty_pool_count = disc
                .get("dirty_pool_count")
                .and_then(|x| x.as_u64())
                .or(Some(dirty_pools.len() as u64));
            views.insert(
                bn,
                BlockDiscoveryView {
                    block_number: bn,
                    skipped: disc
                        .get("skipped")
                        .and_then(|x| x.as_bool())
                        .unwrap_or(false),
                    skip_reason: disc
                        .get("skip_reason")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                    dirty_pools,
                    cycles_optimized: disc.get("cycles_optimized").and_then(|x| x.as_u64()),
                    cycles_total: disc.get("cycles_total").and_then(|x| x.as_u64()),
                    dirty_pool_count,
                    scope: disc
                        .get("scope")
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string()),
                },
            );
        }
    }
    Ok((blocks, views))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedEvent {
    pub cause: Cause,
    pub bot_address: String,
    pub tx_hash: String,
    pub block_number: u64,
    pub ordered_pools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hop_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_asset: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    /// WHI-715 bucket when a ledger was supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_bucket: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CauseCounts {
    pub aggregator_misclass: usize,
    pub out_of_scope_flash_loan: usize,
    pub out_of_scope_hop_cap: usize,
    pub out_of_scope_non_wmnt_settlement: usize,
    pub out_of_scope_adapter_required: usize,
    pub not_in_universe: usize,
    pub block_skipped: usize,
    pub dirty_cycle_filter_skipped: usize,
    pub evaluated_but_unprofitable: usize,
    pub profitable_but_not_attempted: usize,
    pub attempted_and_lost_race: usize,
    pub unattributable: usize,
}

impl CauseCounts {
    pub fn record(&mut self, c: Cause) {
        match c {
            Cause::AggregatorMisclass => self.aggregator_misclass += 1,
            Cause::OutOfScopeFlashLoan => self.out_of_scope_flash_loan += 1,
            Cause::OutOfScopeHopCap => self.out_of_scope_hop_cap += 1,
            Cause::OutOfScopeNonWmntSettlement => self.out_of_scope_non_wmnt_settlement += 1,
            Cause::OutOfScopeAdapterRequired => self.out_of_scope_adapter_required += 1,
            Cause::NotInUniverse => self.not_in_universe += 1,
            Cause::BlockSkipped => self.block_skipped += 1,
            Cause::DirtyCycleFilterSkipped => self.dirty_cycle_filter_skipped += 1,
            Cause::EvaluatedButUnprofitable => self.evaluated_but_unprofitable += 1,
            Cause::ProfitableButNotAttempted => self.profitable_but_not_attempted += 1,
            Cause::AttemptedAndLostRace => self.attempted_and_lost_race += 1,
            Cause::Unattributable => self.unattributable += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.aggregator_misclass
            + self.out_of_scope_flash_loan
            + self.out_of_scope_hop_cap
            + self.out_of_scope_non_wmnt_settlement
            + self.out_of_scope_adapter_required
            + self.not_in_universe
            + self.block_skipped
            + self.dirty_cycle_filter_skipped
            + self.evaluated_but_unprofitable
            + self.profitable_but_not_attempted
            + self.attempted_and_lost_race
            + self.unattributable
    }

    pub fn five_core_total(&self) -> usize {
        self.not_in_universe
            + self.dirty_cycle_filter_skipped
            + self.evaluated_but_unprofitable
            + self.profitable_but_not_attempted
            + self.attempted_and_lost_race
    }

    pub fn out_of_scope_total(&self) -> usize {
        self.out_of_scope_flash_loan
            + self.out_of_scope_hop_cap
            + self.out_of_scope_non_wmnt_settlement
            + self.out_of_scope_adapter_required
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerAttributionReport {
    pub schema_version: String,
    pub benchmark_schema_version: String,
    pub from_block: Option<u64>,
    pub to_block: Option<u64>,
    pub event_count: usize,
    pub attributed_count: usize,
    pub cause_counts: CauseCounts,
    /// Five-core counts as rates over events after removing aggregator_misclass.
    pub analysis_denominator: usize,
    pub reachable_miss_rate: f64,
    pub out_of_scope_rate: f64,
    pub dirty_cycle_filter_skipped_is_zero: bool,
    /// `not_measured` | `measured_zero` | `measured_nonzero`.
    ///
    /// `not_measured` means no event reached the dirty-cycle decision (missing
    /// concurrent `block_views`). A true WHI-940 answer requires `measured_*`.
    pub dirty_cycle_evidence: String,
    pub classification_rules: Vec<String>,
    pub notes: Vec<String>,
    pub events: Vec<AttributedEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub benchmark_bucket_counts: Option<BTreeMap<String, usize>>,
}

/// Inputs for one attribution pass.
pub struct AttributionInputs<'a> {
    pub events: &'a [KnownBotEvent],
    pub universe: &'a UniverseContext,
    pub ledger_index: Option<&'a ShadowLedgerIndex>,
    /// Blocks the ledger observed (processed). When set without a view entry,
    /// absence of a block means the event is outside the watch window → unattributable
    /// or block_skipped depending on policy.
    pub observed_blocks: Option<&'a BTreeSet<u64>>,
    pub block_views: Option<&'a HashMap<u64, BlockDiscoveryView>>,
    /// When true, events on blocks not in `observed_blocks` are `Unattributable`
    /// (outside concurrent window) rather than `BlockSkipped`.
    pub outside_window_is_unattributable: bool,
}

fn hop_count(event: &KnownBotEvent) -> u32 {
    if let Some(h) = event.hop_count {
        return h;
    }
    event.ordered_pools.len() as u32
}

fn is_flash(event: &KnownBotEvent) -> bool {
    event
        .funding
        .as_deref()
        .map(|s| s.eq_ignore_ascii_case("flash_loan"))
        .unwrap_or(false)
}

fn settlement_is_wmnt(event: &KnownBotEvent, universe: &UniverseContext) -> bool {
    match event.settlement_asset.as_deref() {
        None | Some("") => true, // unknown → do not out-of-scope on settlement
        Some(s) => normalize_address(s) == universe.settlement_wmnt,
    }
}

fn missing_pools(event: &KnownBotEvent, universe: &UniverseContext) -> Vec<String> {
    event
        .ordered_pools
        .iter()
        .map(|p| normalize_address(p))
        .filter(|p| !p.is_empty() && !universe.contains_pool(p))
        .collect()
}

fn path_uses_adapter(event: &KnownBotEvent, universe: &UniverseContext) -> bool {
    if universe.adapter_factories.is_empty() {
        return false;
    }
    for p in &event.ordered_pools {
        let key = normalize_address(p);
        if let Some(f) = universe.pool_factory.get(&key) {
            if universe.adapter_factories.contains(f) {
                return true;
            }
        }
    }
    false
}

fn dirty_set_for(
    block: u64,
    views: Option<&HashMap<u64, BlockDiscoveryView>>,
) -> Option<(bool, HashSet<String>)> {
    let v = views?.get(&block)?;
    let dirty: HashSet<String> = v
        .dirty_pools
        .iter()
        .map(|p| normalize_address(p))
        .filter(|p| !p.is_empty())
        .collect();
    Some((v.skipped, dirty))
}

fn path_pools(event: &KnownBotEvent) -> HashSet<String> {
    event
        .ordered_pools
        .iter()
        .map(|p| normalize_address(p))
        .filter(|p| !p.is_empty())
        .collect()
}

fn profitable_pass(opps: &[&ShadowOpportunity]) -> bool {
    opps.iter().any(|o| o.is_profitable_pass())
}

fn eligibility_blocked(opps: &[&ShadowOpportunity]) -> bool {
    opps.iter().any(|o| {
        let k = o.outcome_kind.to_ascii_lowercase();
        k.contains("skip")
            || k.contains("eligib")
            || k.contains("budget")
            || k == "env_unsupported"
            || o.outcome_reason
                .as_deref()
                .map(|r| {
                    let r = r.to_ascii_lowercase();
                    r.contains("eligib") || r.contains("budget") || r.contains("attempt")
                })
                .unwrap_or(false)
    })
}

/// Attribute one event. Pure function — the priority order is the contract.
pub fn attribute_event(event: &KnownBotEvent, inputs: &AttributionInputs<'_>) -> AttributedEvent {
    let hops = hop_count(event);
    let base = |cause: Cause, detail: String, bucket: Option<&str>| AttributedEvent {
        cause,
        bot_address: event.bot_address.clone(),
        tx_hash: event.tx_hash.clone(),
        block_number: event.block_number,
        ordered_pools: event.ordered_pools.clone(),
        hop_count: event.hop_count.or(Some(hops)),
        funding: event.funding.clone(),
        settlement_asset: event.settlement_asset.clone(),
        route: event.route.clone(),
        benchmark_bucket: bucket.map(|s| s.to_string()),
        detail,
    };

    // 1. Aggregator noise
    if hops > inputs.universe.aggregator_hop_threshold {
        return base(
            Cause::AggregatorMisclass,
            format!(
                "hop_count={hops} > aggregator threshold {}",
                inputs.universe.aggregator_hop_threshold
            ),
            None,
        );
    }

    // 2. Out-of-scope flash
    if is_flash(event) {
        return base(
            Cause::OutOfScopeFlashLoan,
            "funding=flash_loan (F13 deferred)".into(),
            None,
        );
    }

    // 3. Hop cap (4+ and still ≤50)
    if hops > inputs.universe.max_hops {
        return base(
            Cause::OutOfScopeHopCap,
            format!(
                "hop_count={hops} > max_hops={} (WHI-529 by design)",
                inputs.universe.max_hops
            ),
            None,
        );
    }

    // 4. Non-WMNT settlement
    if !settlement_is_wmnt(event, inputs.universe) {
        return base(
            Cause::OutOfScopeNonWmntSettlement,
            format!(
                "settlement_asset={:?} ≠ WMNT {}",
                event.settlement_asset, inputs.universe.settlement_wmnt
            ),
            None,
        );
    }

    // 5. Adapter-required venue
    if path_uses_adapter(event, inputs.universe) {
        return base(
            Cause::OutOfScopeAdapterRequired,
            "path touches adapter_required factory".into(),
            None,
        );
    }

    // 6. Not in universe
    let missing = missing_pools(event, inputs.universe);
    if !missing.is_empty() {
        return base(
            Cause::NotInUniverse,
            format!("pools not in universe: {missing:?}"),
            None,
        );
    }

    // Empty path with no missing pools still cannot match dirty-cycle logic.
    if event.ordered_pools.is_empty() {
        return base(
            Cause::Unattributable,
            "ordered_pools empty after universe pass".into(),
            None,
        );
    }

    // 7. Block skipped / outside window
    if let Some((skipped, _)) = dirty_set_for(event.block_number, inputs.block_views) {
        if skipped {
            return base(
                Cause::BlockSkipped,
                "block_views.skipped=true (WHI-977)".into(),
                None,
            );
        }
    } else if let Some(observed) = inputs.observed_blocks {
        if !observed.contains(&event.block_number) {
            if inputs.outside_window_is_unattributable {
                return base(
                    Cause::Unattributable,
                    "block not in concurrent ledger observation window".into(),
                    None,
                );
            }
            return base(
                Cause::BlockSkipped,
                "block absent from ledger observations".into(),
                None,
            );
        }
    }

    // 8–10. Ledger candidate outcomes (when present)
    if let Some(index) = inputs.ledger_index {
        let matches = index.matches(event.block_number, &event.ordered_pools);
        if profitable_pass(&matches) {
            return base(
                Cause::AttemptedAndLostRace,
                format!(
                    "ledger profitable Pass (n_matches={}); peer still landed",
                    matches.len()
                ),
                Some(Bucket::WouldHaveBeenProfitable.as_str()),
            );
        }
        if !matches.is_empty() {
            if eligibility_blocked(&matches) {
                return base(
                    Cause::ProfitableButNotAttempted,
                    format!(
                        "ledger candidate(s) eligibility/budget blocked: {:?}",
                        matches
                            .iter()
                            .map(|m| (&m.outcome_kind, &m.outcome_reason))
                            .collect::<Vec<_>>()
                    ),
                    Some(Bucket::UnprofitableOrRevert.as_str()),
                );
            }
            return base(
                Cause::EvaluatedButUnprofitable,
                format!(
                    "ledger candidate(s) without profitable Pass: kinds={:?}",
                    matches.iter().map(|m| &m.outcome_kind).collect::<Vec<_>>()
                ),
                Some(Bucket::UnprofitableOrRevert.as_str()),
            );
        }

        // No candidate at block/route → dirty-cycle vs unattributable
        if let Some((skipped, dirty)) = dirty_set_for(event.block_number, inputs.block_views) {
            if skipped {
                return base(
                    Cause::BlockSkipped,
                    "skipped (late view)".into(),
                    Some(Bucket::MissedDetection.as_str()),
                );
            }
            let path = path_pools(event);
            // Full scope optimizes everything → cannot be dirty-cycle skip.
            let scope_full = inputs
                .block_views
                .and_then(|m| m.get(&event.block_number))
                .and_then(|v| v.scope.as_deref())
                .map(|s| s.eq_ignore_ascii_case("full"))
                .unwrap_or(false);
            if !scope_full && path.iter().all(|p| !dirty.contains(p)) {
                return base(
                    Cause::DirtyCycleFilterSkipped,
                    format!(
                        "path fully in universe; dirty set (n={}) shares no pools with path (WHI-940)",
                        dirty.len()
                    ),
                    Some(Bucket::MissedDetection.as_str()),
                );
            }
            // Dirty touched the path (or full scope) but no candidate → evaluated empty.
            return base(
                Cause::EvaluatedButUnprofitable,
                "dirty set touched path (or full scope) but no ledger candidate".into(),
                Some(Bucket::MissedDetection.as_str()),
            );
        }

        return base(
            Cause::Unattributable,
            "missed detection without block_views dirty set — cannot separate dirty-cycle vs evaluate".into(),
            Some(Bucket::MissedDetection.as_str()),
        );
    }

    // No ledger: universe-side causes already applied; residual needs concurrent data.
    base(
        Cause::Unattributable,
        "no concurrent ledger / block_views — universe membership passed; dirty-cycle and evaluate causes need concurrent capture".into(),
        None,
    )
}

/// Attribute every event; fail closed if counts do not cover all events.
pub fn attribute_all(inputs: &AttributionInputs<'_>) -> Result<PeerAttributionReport, PeerAttributionError> {
    if inputs.events.is_empty() {
        return Err(PeerAttributionError::Message(
            "known-bot event list is empty".into(),
        ));
    }

    let mut counts = CauseCounts::default();
    let mut events = Vec::with_capacity(inputs.events.len());
    let mut from_block = None;
    let mut to_block = None;
    for e in inputs.events {
        from_block = Some(from_block.map_or(e.block_number, |b: u64| b.min(e.block_number)));
        to_block = Some(to_block.map_or(e.block_number, |b: u64| b.max(e.block_number)));
        let a = attribute_event(e, inputs);
        counts.record(a.cause);
        events.push(a);
    }

    if counts.total() != inputs.events.len() {
        return Err(PeerAttributionError::Message(format!(
            "attribution count {} != events {}",
            counts.total(),
            inputs.events.len()
        )));
    }

    let analysis_denominator = inputs
        .events
        .len()
        .saturating_sub(counts.aggregator_misclass);
    let reachable_miss = counts.not_in_universe
        + counts.dirty_cycle_filter_skipped
        + counts.evaluated_but_unprofitable
        + counts.profitable_but_not_attempted
        + counts.attempted_and_lost_race;
    // Reachable-miss rate: five-core over non-aggregator events.
    let reachable_miss_rate = if analysis_denominator == 0 {
        0.0
    } else {
        reachable_miss as f64 / analysis_denominator as f64
    };
    let out_of_scope_rate = if analysis_denominator == 0 {
        0.0
    } else {
        counts.out_of_scope_total() as f64 / analysis_denominator as f64
    };

    let mut notes = vec![
        "Cause priority: aggregator_misclass → oos(flash/hop/wmnt/adapter) → not_in_universe → block_skipped → ledger outcomes → dirty_cycle_filter_skipped → evaluated_but_unprofitable → unattributable.".into(),
        "dirty_cycle_filter_skipped requires concurrent block_views with dirty_pools; never inferred from 'no candidate' alone.".into(),
        "block_skipped is counted separately from the five core causes (WHI-977).".into(),
        format!(
            "aggregator_misclass: hop_count > {} excluded from rate denominators.",
            inputs.universe.aggregator_hop_threshold
        ),
    ];

    if counts.unattributable > 0 {
        notes.push(format!(
            "unattributable={} — residual explicit; not silently dropped",
            counts.unattributable
        ));
    }

    // Dirty-cycle evidence quality: measured only when some event was classified
    // as dirty_cycle_filter_skipped or as evaluated_but_unprofitable via the
    // dirty-touch path (ledger miss + block_views present).
    let dirty_measured = events.iter().any(|e| {
        e.cause == Cause::DirtyCycleFilterSkipped
            || (e.cause == Cause::EvaluatedButUnprofitable
                && e.detail.contains("dirty set touched"))
            || (e.cause == Cause::Unattributable
                && e.detail.contains("without block_views dirty set"))
    });
    let had_block_views = inputs.block_views.map(|m| !m.is_empty()).unwrap_or(false);
    let dirty_cycle_evidence = if !had_block_views {
        notes.push(
            "dirty_cycle_evidence=not_measured: no --block-views sidecar; count 0 is not a WHI-940 pass"
                .into(),
        );
        "not_measured".to_string()
    } else if counts.dirty_cycle_filter_skipped == 0 {
        notes.push(
            "dirty_cycle_evidence=measured_zero: concurrent block_views present and no dirty-cycle skips"
                .into(),
        );
        "measured_zero".to_string()
    } else {
        notes.push(format!(
            "dirty_cycle_evidence=measured_nonzero: dirty_cycle_filter_skipped={}",
            counts.dirty_cycle_filter_skipped
        ));
        "measured_nonzero".to_string()
    };
    let _ = dirty_measured; // retained for future richer heuristics

    Ok(PeerAttributionReport {
        schema_version: PEER_ATTRIBUTION_SCHEMA_VERSION.to_string(),
        benchmark_schema_version: BENCHMARK_REPORT_SCHEMA_VERSION.to_string(),
        from_block,
        to_block,
        event_count: inputs.events.len(),
        attributed_count: counts.total(),
        dirty_cycle_filter_skipped_is_zero: counts.dirty_cycle_filter_skipped == 0,
        dirty_cycle_evidence,
        cause_counts: counts,
        analysis_denominator,
        reachable_miss_rate,
        out_of_scope_rate,
        classification_rules: vec![
            "aggregator_misclass: hop_count > 50".into(),
            "out_of_scope_flash_loan: funding=flash_loan".into(),
            format!("out_of_scope_hop_cap: hop_count > {}", inputs.universe.max_hops),
            "out_of_scope_non_wmnt_settlement: settlement_asset set and ≠ WMNT".into(),
            "out_of_scope_adapter_required: pool factory in adapter set".into(),
            "not_in_universe: any ordered_pool ∉ universe CSV".into(),
            "block_skipped: block_views.skipped or absent observation (policy)".into(),
            "attempted_and_lost_race: ledger profitable Pass on matching route".into(),
            "profitable_but_not_attempted: ledger outcome eligibility/budget".into(),
            "evaluated_but_unprofitable: ledger non-Pass candidate, or dirty touched path with no candidate".into(),
            "dirty_cycle_filter_skipped: all pools in universe, hop≤max, not skipped, dirty∩path=∅, no candidate".into(),
            "unattributable: residual with detail (e.g. no concurrent dirty view)".into(),
        ],
        notes,
        events,
        benchmark_bucket_counts: None,
    })
}

/// Optional: attach WHI-715 bucket counts when ledgers were compared.
pub fn attach_benchmark_buckets(
    report: &mut PeerAttributionReport,
    events: &[KnownBotEvent],
    ledgers: &[LedgerBytes],
) -> Result<(), PeerAttributionError> {
    let bench = compare(ledgers, events).map_err(|e| PeerAttributionError::Message(e.to_string()))?;
    let mut m = BTreeMap::new();
    m.insert(
        Bucket::MissedDetection.as_str().to_string(),
        bench.bucket_counts.missed_detection,
    );
    m.insert(
        Bucket::UnprofitableOrRevert.as_str().to_string(),
        bench.bucket_counts.unprofitable_or_revert,
    );
    m.insert(
        Bucket::WouldHaveBeenProfitable.as_str().to_string(),
        bench.bucket_counts.would_have_been_profitable,
    );
    report.benchmark_bucket_counts = Some(m);
    report.notes.push(format!(
        "WHI-715 bucket cross-check: missed={} unprofitable={} would_have_been_profitable={}",
        bench.bucket_counts.missed_detection,
        bench.bucket_counts.unprofitable_or_revert,
        bench.bucket_counts.would_have_been_profitable
    ));
    Ok(())
}

/// Drop per-event rows for committed aggregate artifacts (events stay external).
pub fn strip_events(report: &mut PeerAttributionReport) {
    report.events.clear();
    report.notes.push(
        "per-event rows stripped from committed report (re-run with external events JSONL to regenerate)"
            .into(),
    );
}

pub fn write_report(path: &Path, report: &PeerAttributionReport) -> Result<(), PeerAttributionError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| PeerAttributionError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    let file = File::create(path).map_err(|source| PeerAttributionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, report).map_err(|source| PeerAttributionError::Json {
        path: path.display().to_string(),
        source,
    })?;
    w.write_all(b"\n").map_err(|source| PeerAttributionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

pub fn render_markdown(report: &PeerAttributionReport) -> String {
    let mut out = String::new();
    out.push_str("# Peer attribution report (WHI-957)\n\n");
    out.push_str(&format!("- Schema: `{}`\n", report.schema_version));
    out.push_str(&format!(
        "- Block range: {:?}–{:?}\n",
        report.from_block, report.to_block
    ));
    out.push_str(&format!("- Events: {}\n", report.event_count));
    out.push_str(&format!(
        "- Attributed: {} (must equal events)\n",
        report.attributed_count
    ));
    out.push_str(&format!(
        "- Analysis denominator (excl. aggregator_misclass): {}\n",
        report.analysis_denominator
    ));
    out.push_str(&format!(
        "- Reachable-miss rate (five-core / denom): **{:.2}%**\n",
        report.reachable_miss_rate * 100.0
    ));
    out.push_str(&format!(
        "- Out-of-scope rate: **{:.2}%**\n",
        report.out_of_scope_rate * 100.0
    ));
    out.push_str(&format!(
        "- **dirty_cycle_filter_skipped == 0?** **{}** (count={})\n",
        report.dirty_cycle_filter_skipped_is_zero,
        report.cause_counts.dirty_cycle_filter_skipped
    ));
    out.push_str(&format!(
        "- **dirty_cycle_evidence:** `{}` (only `measured_zero` answers WHI-940)\n\n",
        report.dirty_cycle_evidence
    ));

    out.push_str("## Five core causes\n\n");
    out.push_str("| Cause | Count |\n| --- | ---: |\n");
    let c = &report.cause_counts;
    for (name, n) in [
        ("not_in_universe", c.not_in_universe),
        ("dirty_cycle_filter_skipped", c.dirty_cycle_filter_skipped),
        ("evaluated_but_unprofitable", c.evaluated_but_unprofitable),
        ("profitable_but_not_attempted", c.profitable_but_not_attempted),
        ("attempted_and_lost_race", c.attempted_and_lost_race),
    ] {
        out.push_str(&format!("| `{name}` | {n} |\n"));
    }

    out.push_str("\n## Separators (not mixed into the five)\n\n");
    out.push_str("| Cause | Count |\n| --- | ---: |\n");
    for (name, n) in [
        ("block_skipped", c.block_skipped),
        ("aggregator_misclass", c.aggregator_misclass),
        ("out_of_scope_flash_loan", c.out_of_scope_flash_loan),
        ("out_of_scope_hop_cap", c.out_of_scope_hop_cap),
        ("out_of_scope_non_wmnt_settlement", c.out_of_scope_non_wmnt_settlement),
        ("out_of_scope_adapter_required", c.out_of_scope_adapter_required),
        ("unattributable", c.unattributable),
    ] {
        out.push_str(&format!("| `{name}` | {n} |\n"));
    }

    if let Some(b) = &report.benchmark_bucket_counts {
        out.push_str("\n## WHI-715 bucket cross-check\n\n");
        for (k, v) in b {
            out.push_str(&format!("- `{k}`: {v}\n"));
        }
    }

    out.push_str("\n## Classification rules\n\n");
    for r in &report.classification_rules {
        out.push_str(&format!("- {r}\n"));
    }
    out.push_str("\n## Notes\n\n");
    for n in &report.notes {
        out.push_str(&format!("- {n}\n"));
    }
    out
}

/// Load known-bot events (re-export path for the bin).
pub fn load_events(path: &Path) -> Result<Vec<KnownBotEvent>, PeerAttributionError> {
    load_known_bot_events(path).map_err(|e| PeerAttributionError::Message(e.to_string()))
}

/// Load events JSONL (KnownBotEvent per line) — external WHI-956 output.
pub fn load_events_jsonl(path: &Path) -> Result<Vec<KnownBotEvent>, PeerAttributionError> {
    let file = File::open(path).map_err(|source| PeerAttributionError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|source| PeerAttributionError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let e: KnownBotEvent =
            serde_json::from_str(line).map_err(|source| PeerAttributionError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        out.push(e);
    }
    if out.is_empty() {
        return Err(PeerAttributionError::Message(format!(
            "no events in {}",
            path.display()
        )));
    }
    Ok(out)
}

// Keep U256 parse available for tests of profit strings if needed later.
#[allow(dead_code)]
fn parse_u256(raw: &str) -> Option<U256> {
    U256::from_str(raw.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::shadow_bot_benchmark::KnownBotEvent;

    fn wmnt() -> String {
        format!("{DEFAULT_WMNT:#x}")
    }

    fn universe(pools: &[&str]) -> UniverseContext {
        UniverseContext {
            pools: pools.iter().map(|p| normalize_address(p)).collect(),
            adapter_factories: HashSet::new(),
            pool_factory: HashMap::new(),
            settlement_wmnt: wmnt(),
            max_hops: 3,
            aggregator_hop_threshold: 50,
        }
    }

    fn event(block: u64, pools: &[&str], hops: u32) -> KnownBotEvent {
        KnownBotEvent {
            bot_address: "0xbot".into(),
            tx_hash: format!("0x{block:064x}"),
            block_number: block,
            ordered_pools: pools.iter().map(|p| (*p).to_string()).collect(),
            route: None,
            label: None,
            hop_count: Some(hops),
            funding: Some("self_funded".into()),
            venues: None,
            settlement_asset: Some(wmnt()),
        }
    }

    fn inputs<'a>(
        events: &'a [KnownBotEvent],
        universe: &'a UniverseContext,
        ledger: Option<&'a ShadowLedgerIndex>,
        observed: Option<&'a BTreeSet<u64>>,
        views: Option<&'a HashMap<u64, BlockDiscoveryView>>,
    ) -> AttributionInputs<'a> {
        AttributionInputs {
            events,
            universe,
            ledger_index: ledger,
            observed_blocks: observed,
            block_views: views,
            outside_window_is_unattributable: true,
        }
    }

    #[test]
    fn every_event_gets_exactly_one_cause() {
        let u = universe(&["0xpool1", "0xpool2", "0xpool3"]);
        let events = vec![
            event(100, &["0xpool1", "0xpool2"], 2),
            event(101, &["0xmissing", "0xpool2"], 2),
            {
                let mut e = event(102, &["0xpool1", "0xpool2"], 3);
                e.funding = Some("flash_loan".into());
                e
            },
            event(103, &["0xpool1", "0xpool2", "0xpool3", "0xpool1"], 4),
            event(104, &["0xpool1", "0xpool2"], 1139),
        ];
        let report = attribute_all(&inputs(&events, &u, None, None, None)).unwrap();
        assert_eq!(report.attributed_count, 5);
        assert_eq!(report.cause_counts.total(), 5);
        assert_eq!(report.cause_counts.not_in_universe, 1);
        assert_eq!(report.cause_counts.out_of_scope_flash_loan, 1);
        assert_eq!(report.cause_counts.out_of_scope_hop_cap, 1);
        assert_eq!(report.cause_counts.aggregator_misclass, 1);
        assert_eq!(report.cause_counts.unattributable, 1); // in-universe, no ledger
    }

    #[test]
    fn dirty_cycle_separated_from_evaluated() {
        let u = universe(&["0xa", "0xb"]);
        let e = event(200, &["0xa", "0xb"], 2);
        let mut views = HashMap::new();
        // Dirty is some other pool — path never optimized.
        views.insert(
            200,
            BlockDiscoveryView {
                block_number: 200,
                skipped: false,
                dirty_pools: vec!["0xdead".into()],
                scope: Some("touched".into()),
                ..Default::default()
            },
        );
        // Empty ledger index (header-only parse leaves no opps).
        let index = ShadowLedgerIndex {
            no_send_enforced: true,
            ..Default::default()
        };
        let mut observed = BTreeSet::new();
        observed.insert(200);
        let a = attribute_event(
            &e,
            &inputs(std::slice::from_ref(&e), &u, Some(&index), Some(&observed), Some(&views)),
        );
        assert_eq!(a.cause, Cause::DirtyCycleFilterSkipped);

        // Same path, dirty touches path → evaluated unprofitable (no candidate).
        views.insert(
            200,
            BlockDiscoveryView {
                block_number: 200,
                skipped: false,
                dirty_pools: vec!["0xa".into()],
                scope: Some("touched".into()),
                ..Default::default()
            },
        );
        let a2 = attribute_event(
            &e,
            &inputs(std::slice::from_ref(&e), &u, Some(&index), Some(&observed), Some(&views)),
        );
        assert_eq!(a2.cause, Cause::EvaluatedButUnprofitable);
    }

    #[test]
    fn block_skipped_not_mixed_into_five() {
        let u = universe(&["0xa", "0xb"]);
        let e = event(300, &["0xa", "0xb"], 2);
        let mut views = HashMap::new();
        views.insert(
            300,
            BlockDiscoveryView {
                block_number: 300,
                skipped: true,
                skip_reason: Some("pin_timeout".into()),
                ..Default::default()
            },
        );
        let a = attribute_event(&e, &inputs(std::slice::from_ref(&e), &u, None, None, Some(&views)));
        assert_eq!(a.cause, Cause::BlockSkipped);
        assert!(!a.cause.is_five_core());
    }

    #[test]
    fn profitable_pass_is_attempted_and_lost_race() {
        let u = universe(&["0xa", "0xb"]);
        let e = event(400, &["0xa", "0xb"], 2);
        let mut index = ShadowLedgerIndex {
            no_send_enforced: true,
            ..Default::default()
        };
        index.opportunities.push(ShadowOpportunity {
            service: "bot".into(),
            block_number: 400,
            digest: "0xd".into(),
            opportunity_id: "0xo".into(),
            ordered_pools: vec!["0xa".into(), "0xb".into()],
            net_profit: "100".into(),
            gross_profit: "120".into(),
            amount_in: "50".into(),
            outcome_kind: "pass".into(),
            outcome_reason: None,
        });
        let a = attribute_event(
            &e,
            &inputs(std::slice::from_ref(&e), &u, Some(&index), None, None),
        );
        assert_eq!(a.cause, Cause::AttemptedAndLostRace);
    }

    #[test]
    fn report_answers_dirty_cycle_zero_flag() {
        let u = universe(&["0xa", "0xb"]);
        let events = vec![event(1, &["0xmissing", "0xa"], 2)];
        let report = attribute_all(&inputs(&events, &u, None, None, None)).unwrap();
        assert!(report.dirty_cycle_filter_skipped_is_zero);
        assert_eq!(report.dirty_cycle_evidence, "not_measured");
        assert_eq!(report.cause_counts.not_in_universe, 1);
    }

    #[test]
    fn measured_zero_when_views_present_and_dirty_touches() {
        let u = universe(&["0xa", "0xb"]);
        let e = event(200, &["0xa", "0xb"], 2);
        let mut views = HashMap::new();
        views.insert(
            200,
            BlockDiscoveryView {
                block_number: 200,
                skipped: false,
                dirty_pools: vec!["0xa".into()],
                scope: Some("touched".into()),
                ..Default::default()
            },
        );
        let index = ShadowLedgerIndex {
            no_send_enforced: true,
            ..Default::default()
        };
        let mut observed = BTreeSet::new();
        observed.insert(200);
        let report = attribute_all(&inputs(
            std::slice::from_ref(&e),
            &u,
            Some(&index),
            Some(&observed),
            Some(&views),
        ))
        .unwrap();
        assert_eq!(report.cause_counts.dirty_cycle_filter_skipped, 0);
        assert_eq!(report.dirty_cycle_evidence, "measured_zero");
        assert_eq!(report.cause_counts.evaluated_but_unprofitable, 1);
    }
}
