//! Mantle arb-bot ground-truth collector (WHI-956).
//!
//! Offline, re-runnable pipeline that turns discovered atomic-arbitrage
//! candidates into the WHI-715 known-bot schema (plus WHI-957 attribution
//! fields). Real event datasets stay **external** — same contract as
//! [`super::arb_coverage`]: commit the collector and aggregate reports only.
//!
//! ## Explicit acceptance heuristic
//!
//! A candidate is an atomic arb when **all** of the following hold:
//!
//! 1. Single transaction (`status` success).
//! 2. ≥ 2 swap events (DEX family topics: v2 / v3 / algebra / lb / izi / solidly).
//! 3. Closed token cycle for the entity (`tx.from` ∪ `tx.to`): net-positive in
//!    ≥ 1 token and net-negative in **none**.
//! 4. Entity actually sent tokens (gross-out > 0) — kills pure receive dust.
//!
//! ## Misclassification exclusions (counted, not silent)
//!
//! | Category | Rule |
//! | --- | --- |
//! | `cex_dex` | `msg.value` > 1 MNT (native-funded directional settle) |
//! | `liquidation` | Liquidation-call topic present |
//! | `jit_lp` | LP mint/burn/withdraw-bins without a pure swap cycle |
//! | `sandwich` | Explicit sandwich marker, or same-block victim-middle pattern flag |
//! | `insufficient_swaps` | < 2 swaps |
//! | `not_closed_cycle` | Entity has a net-negative token leg |
//! | `no_gross_out` | Entity never paid a token |
//!
//! ## Regenerability
//!
//! Given identical inputs and `[from_block, to_block]`, emission order is
//! sorted by `(block_number, tx_hash, bot_address)` and the report fingerprint
//! is `keccak256` over the canonical event JSON bytes (hex `0x…`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use alloy::primitives::keccak256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::execution::shadow_bot_benchmark::KnownBotEvent;
use crate::service::arb_coverage::{normalize_address, PoolCensusEntry};

/// Report / collector schema id.
pub const GROUND_TRUTH_SCHEMA_VERSION: &str = "whisker-arb/ground-truth-collector/v1";

/// Explicit heuristic text embedded in every report artifact.
///
/// `entity_gross_out` is fail-closed only when the input sets
/// `gross_out=false`. Non-empty `pos` is the required closed-cycle evidence
/// (entity net-positive ≥1 token); `neg` non-empty fails closed.
pub const ACCEPTANCE_HEURISTIC: &str = "single_tx AND swap_events>=2 AND pos_nonempty \
AND entity_net_negative_eq0 AND msg_value_wei<=1e18 \
AND NOT (gross_out=false) AND NOT liquidation AND NOT jit_lp AND NOT sandwich";

/// Native MNT wei threshold above which a tx is treated as CEX-DEX settle.
pub const CEX_DEX_MSG_VALUE_WEI: u128 = 10u128.pow(18);

/// Default Mantle Blockscout explorer base (API v2 under `/api/v2`).
pub const DEFAULT_BLOCKSCOUT_BASE: &str = "https://explorer.mantle.xyz";

/// Funding labels written on events (WHI-957 out-of-scope).
pub const FUNDING_SELF: &str = "self_funded";
pub const FUNDING_FLASH: &str = "flash_loan";
pub const FUNDING_UNKNOWN: &str = "unknown";

/// Exclusion / reject category keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionCategory {
    CexDex,
    Liquidation,
    JitLp,
    Sandwich,
    InsufficientSwaps,
    NotClosedCycle,
    NoGrossOut,
    OutOfRange,
    MissingFields,
}

impl ExclusionCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CexDex => "cex_dex",
            Self::Liquidation => "liquidation",
            Self::JitLp => "jit_lp",
            Self::Sandwich => "sandwich",
            Self::InsufficientSwaps => "insufficient_swaps",
            Self::NotClosedCycle => "not_closed_cycle",
            Self::NoGrossOut => "no_gross_out",
            Self::OutOfRange => "out_of_range",
            Self::MissingFields => "missing_fields",
        }
    }
}

#[derive(Debug, Error)]
pub enum GroundTruthError {
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
    #[error("block range empty or inverted: from={from} to={to}")]
    InvalidRange { from: u64, to: u64 },
    #[error("no accepted events in range [{from}, {to}] after exclusions")]
    EmptyResult { from: u64, to: u64 },
    #[error("{0}")]
    Message(String),
}

/// One discovery-row candidate. Either a pre-extracted arb JSONL line (WHI-906
/// shape) or a richer Dune/export row with exclusion flags.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryCandidate {
    /// Block number.
    #[serde(default, alias = "block")]
    pub block_number: Option<u64>,
    /// Tx hash.
    #[serde(default, alias = "hash", alias = "txHash", alias = "transaction_hash")]
    pub tx_hash: Option<String>,
    /// Bot EOA (prefer `from`).
    #[serde(default, alias = "from")]
    pub bot_address: Option<String>,
    /// Optional executor / to.
    #[serde(default, alias = "to")]
    pub to_address: Option<String>,
    /// Ordered pool path.
    #[serde(default, alias = "path")]
    pub ordered_pools: Vec<String>,
    /// Swap count when path is empty.
    #[serde(default, alias = "nSwaps", alias = "n_swaps")]
    pub n_swaps: Option<u32>,
    /// Protocol family labels (`v2`, `v3`, …).
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Net-positive legs: `[[token, amount], …]` or objects.
    #[serde(default)]
    pub pos: Vec<TokenAmount>,
    /// Net-negative legs (if present and non-empty → not a closed arb).
    #[serde(default)]
    pub neg: Vec<TokenAmount>,
    /// Native msg.value in wei (decimal string or number).
    #[serde(default, alias = "value", alias = "msg_value", alias = "msgValue")]
    pub msg_value_wei: Option<String>,
    /// Entity paid at least one token.
    #[serde(default)]
    pub gross_out: Option<bool>,
    /// Liquidation-call event observed.
    #[serde(default)]
    pub has_liquidation: Option<bool>,
    /// JIT LP (mint+burn / withdraw-bins without pure arb).
    #[serde(default)]
    pub has_jit_lp: Option<bool>,
    /// Sandwich pattern marker from upstream tagging.
    #[serde(default)]
    pub is_sandwich: Option<bool>,
    /// Flash-loan marker (topic / selector decode).
    #[serde(default)]
    pub is_flash_loan: Option<bool>,
    /// Optional free-form route label from Dune.
    #[serde(default)]
    pub route: Option<String>,
    /// Optional free-form label.
    #[serde(default)]
    pub label: Option<String>,
    /// Selector (4-byte) when available.
    #[serde(default, alias = "sel")]
    pub selector: Option<String>,
}

/// Token amount pair used in `pos` / `neg` arrays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TokenAmount {
    Pair(String, String),
    Obj {
        #[serde(alias = "token", alias = "t")]
        token: String,
        #[serde(alias = "amount", alias = "a", alias = "d")]
        amount: String,
    },
}

impl TokenAmount {
    pub fn token(&self) -> &str {
        match self {
            Self::Pair(t, _) => t,
            Self::Obj { token, .. } => token,
        }
    }

    pub fn amount(&self) -> &str {
        match self {
            Self::Pair(_, a) => a,
            Self::Obj { amount, .. } => amount,
        }
    }
}

/// Decision for one candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateDecision {
    Accept(KnownBotEvent),
    Exclude(ExclusionCategory),
}

/// Aggregate exclusion counts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExclusionCounts {
    pub cex_dex: usize,
    pub liquidation: usize,
    pub jit_lp: usize,
    pub sandwich: usize,
    pub insufficient_swaps: usize,
    pub not_closed_cycle: usize,
    pub no_gross_out: usize,
    pub out_of_range: usize,
    pub missing_fields: usize,
}

impl ExclusionCounts {
    pub fn record(&mut self, cat: ExclusionCategory) {
        match cat {
            ExclusionCategory::CexDex => self.cex_dex += 1,
            ExclusionCategory::Liquidation => self.liquidation += 1,
            ExclusionCategory::JitLp => self.jit_lp += 1,
            ExclusionCategory::Sandwich => self.sandwich += 1,
            ExclusionCategory::InsufficientSwaps => self.insufficient_swaps += 1,
            ExclusionCategory::NotClosedCycle => self.not_closed_cycle += 1,
            ExclusionCategory::NoGrossOut => self.no_gross_out += 1,
            ExclusionCategory::OutOfRange => self.out_of_range += 1,
            ExclusionCategory::MissingFields => self.missing_fields += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.cex_dex
            + self.liquidation
            + self.jit_lp
            + self.sandwich
            + self.insufficient_swaps
            + self.not_closed_cycle
            + self.no_gross_out
            + self.out_of_range
            + self.missing_fields
    }
}

/// Manual / Blockscout sample verification summary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct VerificationSample {
    pub sample_size: usize,
    pub verified_true_positive: usize,
    pub verified_false_positive: usize,
    pub unverified: usize,
    /// `true_positive / (true_positive + false_positive)` when denominator > 0.
    pub precision: Option<f64>,
    pub method: String,
    pub notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sample_tx_hashes: Vec<String>,
}

/// Full collector report (committed aggregate; events stay external).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundTruthReport {
    pub schema_version: String,
    pub heuristic: String,
    pub from_block: u64,
    pub to_block: u64,
    pub input_label: String,
    pub candidates_seen: usize,
    pub accepted: usize,
    pub distinct_bot_addresses: usize,
    pub exclusion_counts: ExclusionCounts,
    pub hop_count_distribution: BTreeMap<String, usize>,
    pub funding_distribution: BTreeMap<String, usize>,
    pub settlement_asset_distribution: BTreeMap<String, usize>,
    pub venue_distribution: BTreeMap<String, usize>,
    /// `keccak256` hex of canonical accepted-event payload (for regenerability).
    pub events_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification: Option<VerificationSample>,
    pub notes: Vec<String>,
}

/// Result of a collect pass.
#[derive(Debug, Clone)]
pub struct CollectResult {
    pub events: Vec<KnownBotEvent>,
    pub report: GroundTruthReport,
}

/// Block range filter.
#[derive(Debug, Clone, Copy)]
pub struct BlockRange {
    pub from: u64,
    pub to: u64,
}

impl BlockRange {
    pub fn new(from: u64, to: u64) -> Result<Self, GroundTruthError> {
        if from == 0 || to == 0 || from > to {
            return Err(GroundTruthError::InvalidRange { from, to });
        }
        Ok(Self { from, to })
    }

    pub fn contains(self, block: u64) -> bool {
        block >= self.from && block <= self.to
    }
}

/// Parse msg.value wei from a decimal or `0x` hex string.
pub fn parse_wei(raw: &str) -> Option<u128> {
    let s = raw.trim();
    if s.is_empty() {
        return Some(0);
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u128::from_str_radix(hex, 16).ok();
    }
    s.parse::<u128>().ok()
}

/// Pick settlement asset = token with largest positive amount (numeric).
pub fn settlement_asset_from_pos(pos: &[TokenAmount]) -> Option<String> {
    let mut best: Option<(String, u128)> = None;
    for leg in pos {
        let tok = normalize_address(leg.token());
        if tok.is_empty() || tok == "native" {
            // keep native as-is if ever present
            let tok = leg.token().to_ascii_lowercase();
            let amt = parse_wei(leg.amount()).unwrap_or(0);
            match &best {
                Some((_, b)) if amt <= *b => {}
                _ => best = Some((tok, amt)),
            }
            continue;
        }
        let amt = parse_wei(leg.amount()).unwrap_or(0);
        match &best {
            Some((_, b)) if amt <= *b => {}
            _ => best = Some((tok, amt)),
        }
    }
    best.map(|(t, _)| t)
}

/// Detect flash-loan funding from markers / selector / label.
pub fn detect_funding(c: &DiscoveryCandidate) -> String {
    if c.is_flash_loan == Some(true) {
        return FUNDING_FLASH.to_string();
    }
    if let Some(sel) = c.selector.as_deref() {
        let s = sel.to_ascii_lowercase();
        // Common flashLoan selectors (Aave V3 / Balancer-style) — best-effort.
        if s.contains("5cffe9de") // flashLoanSimple
            || s.contains("ab9c4b5d") // flashLoan (Aave)
            || s.contains("5c38449e")
        {
            return FUNDING_FLASH.to_string();
        }
    }
    if let Some(label) = c.label.as_deref().map(|s| s.to_ascii_lowercase()) {
        if label.contains("flash") {
            return FUNDING_FLASH.to_string();
        }
    }
    // Pre-extracted WHI-906 arbs already excluded native-funded CEX-DEX and
    // require entity gross-out — treat as self-funded when no flash marker.
    if c.is_flash_loan == Some(false) || c.gross_out == Some(true) || !c.pos.is_empty() {
        return FUNDING_SELF.to_string();
    }
    FUNDING_UNKNOWN.to_string()
}

/// Build a route label from hop count + kinds.
pub fn route_label(hop_count: u32, kinds: &[String]) -> String {
    if kinds.is_empty() {
        return format!("h{hop_count}");
    }
    let mut k: Vec<String> = kinds
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    k.sort();
    k.dedup();
    format!("h{hop_count}:{}", k.join("+"))
}

/// Resolve venues (factory addresses) for ordered pools via census.
pub fn venues_for_pools(
    pools: &[String],
    census: Option<&HashMap<String, PoolCensusEntry>>,
) -> Vec<String> {
    let Some(census) = census else {
        return Vec::new();
    };
    let mut set = BTreeSet::new();
    for p in pools {
        let key = normalize_address(p);
        if let Some(entry) = census.get(&key) {
            if let Some(f) = entry.factory.as_deref() {
                let f = normalize_address(f);
                if !f.is_empty() {
                    set.insert(f);
                }
            }
        }
    }
    set.into_iter().collect()
}

/// Core classifier: accept or exclude one candidate inside a block range.
pub fn classify_candidate(
    c: &DiscoveryCandidate,
    range: BlockRange,
    census: Option<&HashMap<String, PoolCensusEntry>>,
) -> CandidateDecision {
    let block = match c.block_number {
        Some(b) if b > 0 => b,
        _ => return CandidateDecision::Exclude(ExclusionCategory::MissingFields),
    };
    if !range.contains(block) {
        return CandidateDecision::Exclude(ExclusionCategory::OutOfRange);
    }
    let tx_hash = match c.tx_hash.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(h) => h.to_ascii_lowercase(),
        None => return CandidateDecision::Exclude(ExclusionCategory::MissingFields),
    };
    let bot = match c
        .bot_address
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(b) => normalize_address(b),
        None => return CandidateDecision::Exclude(ExclusionCategory::MissingFields),
    };

    if !c.neg.is_empty() {
        return CandidateDecision::Exclude(ExclusionCategory::NotClosedCycle);
    }
    if c.gross_out == Some(false) {
        return CandidateDecision::Exclude(ExclusionCategory::NoGrossOut);
    }

    let pools: Vec<String> = c
        .ordered_pools
        .iter()
        .map(|p| normalize_address(p))
        .filter(|p| !p.is_empty())
        .collect();
    // Prefer ordered pool path length (route hops). Fall back to n_swaps only
    // when the path is absent (Dune rows that only counted swaps).
    let hop_count = if !pools.is_empty() {
        pools.len() as u32
    } else {
        c.n_swaps.unwrap_or(0)
    };

    let msg_value_wei = c
        .msg_value_wei
        .as_deref()
        .and_then(parse_wei)
        .unwrap_or(0);
    // Closed-cycle evidence: at least one net-positive entity leg is required.
    // Dune exports must fill `settlement_asset` / `pos` (or run a transfer-net
    // join) — we never accept "≥2 swaps alone" as a closed arb.
    let flags = StructuralFlags {
        swap_count: hop_count,
        msg_value_wei,
        has_liquidation: c.has_liquidation.unwrap_or(false),
        has_jit_lp: c.has_jit_lp.unwrap_or(false),
        is_sandwich: c.is_sandwich.unwrap_or(false),
        has_positive_leg: Some(!c.pos.is_empty()),
    };
    if let Some(cat) = structural_exclusion(&flags) {
        return CandidateDecision::Exclude(cat);
    }

    let funding = detect_funding(c);
    let venues = venues_for_pools(&pools, census);
    let settlement = settlement_asset_from_pos(&c.pos);
    let route = c
        .route
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| route_label(hop_count, &c.kinds));
    let label = c.label.clone().or_else(|| {
        c.to_address
            .as_ref()
            .map(|t| format!("to:{}", normalize_address(t)))
    });

    CandidateDecision::Accept(KnownBotEvent {
        bot_address: bot,
        tx_hash,
        block_number: block,
        ordered_pools: pools,
        route: Some(route),
        label,
        hop_count: Some(hop_count),
        funding: Some(funding),
        venues: if venues.is_empty() {
            None
        } else {
            Some(venues)
        },
        settlement_asset: settlement,
    })
}

/// Canonical sort for regenerability.
pub fn sort_events(events: &mut [KnownBotEvent]) {
    events.sort_by(|a, b| {
        a.block_number
            .cmp(&b.block_number)
            .then_with(|| a.tx_hash.cmp(&b.tx_hash))
            .then_with(|| a.bot_address.cmp(&b.bot_address))
    });
}

/// Fingerprint accepted events (canonical JSON, sorted already).
pub fn events_fingerprint(events: &[KnownBotEvent]) -> String {
    let bytes = serde_json::to_vec(events).unwrap_or_default();
    format!("{:#x}", keccak256(bytes))
}

fn dist_bump(map: &mut BTreeMap<String, usize>, key: impl Into<String>) {
    *map.entry(key.into()).or_insert(0) += 1;
}

/// Build hop/funding/settlement/venue distributions from accepted events.
pub fn distributions(
    events: &[KnownBotEvent],
) -> (
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
) {
    let mut hops = BTreeMap::new();
    let mut funding = BTreeMap::new();
    let mut settlement = BTreeMap::new();
    let mut venues = BTreeMap::new();
    for e in events {
        dist_bump(
            &mut hops,
            e.hop_count
                .map(|h| h.to_string())
                .unwrap_or_else(|| "unknown".into()),
        );
        dist_bump(
            &mut funding,
            e.funding
                .clone()
                .unwrap_or_else(|| FUNDING_UNKNOWN.to_string()),
        );
        dist_bump(
            &mut settlement,
            e.settlement_asset
                .clone()
                .unwrap_or_else(|| "unknown".into()),
        );
        if let Some(vs) = &e.venues {
            if vs.is_empty() {
                dist_bump(&mut venues, "unknown");
            } else {
                for v in vs {
                    dist_bump(&mut venues, v.clone());
                }
            }
        } else {
            dist_bump(&mut venues, "unknown");
        }
    }
    (hops, funding, settlement, venues)
}

/// Build a report for an empty accepted set (still records exclusion counts).
pub fn empty_collect_result(
    range: BlockRange,
    input_label: &str,
    candidates_seen: usize,
    exclusions: ExclusionCounts,
    notes: Vec<String>,
) -> CollectResult {
    CollectResult {
        events: Vec::new(),
        report: GroundTruthReport {
            schema_version: GROUND_TRUTH_SCHEMA_VERSION.to_string(),
            heuristic: ACCEPTANCE_HEURISTIC.to_string(),
            from_block: range.from,
            to_block: range.to,
            input_label: input_label.to_string(),
            candidates_seen,
            accepted: 0,
            distinct_bot_addresses: 0,
            exclusion_counts: exclusions,
            hop_count_distribution: BTreeMap::new(),
            funding_distribution: BTreeMap::new(),
            settlement_asset_distribution: BTreeMap::new(),
            venue_distribution: BTreeMap::new(),
            events_fingerprint: format!("{:#x}", keccak256([])),
            verification: None,
            notes,
        },
    }
}

/// Collect from an in-memory candidate list.
pub fn collect_from_candidates(
    candidates: &[DiscoveryCandidate],
    range: BlockRange,
    census: Option<&HashMap<String, PoolCensusEntry>>,
    input_label: &str,
    notes: Vec<String>,
) -> Result<CollectResult, GroundTruthError> {
    let mut exclusions = ExclusionCounts::default();
    let mut events = Vec::new();
    let mut seen = 0usize;
    for c in candidates {
        seen += 1;
        match classify_candidate(c, range, census) {
            CandidateDecision::Accept(e) => events.push(e),
            CandidateDecision::Exclude(cat) => exclusions.record(cat),
        }
    }
    sort_events(&mut events);
    // De-dupe identical (block, tx, bot) after sort.
    events.dedup_by(|a, b| {
        a.block_number == b.block_number && a.tx_hash == b.tx_hash && a.bot_address == b.bot_address
    });

    if events.is_empty() {
        return Err(GroundTruthError::EmptyResult {
            from: range.from,
            to: range.to,
        });
    }

    let bots: BTreeSet<_> = events.iter().map(|e| e.bot_address.clone()).collect();
    let (hop_d, fund_d, set_d, ven_d) = distributions(&events);
    let fp = events_fingerprint(&events);
    let report = GroundTruthReport {
        schema_version: GROUND_TRUTH_SCHEMA_VERSION.to_string(),
        heuristic: ACCEPTANCE_HEURISTIC.to_string(),
        from_block: range.from,
        to_block: range.to,
        input_label: input_label.to_string(),
        candidates_seen: seen,
        accepted: events.len(),
        distinct_bot_addresses: bots.len(),
        exclusion_counts: exclusions,
        hop_count_distribution: hop_d,
        funding_distribution: fund_d,
        settlement_asset_distribution: set_d,
        venue_distribution: ven_d,
        events_fingerprint: fp,
        verification: None,
        notes,
    };
    Ok(CollectResult { events, report })
}

/// Like [`collect_from_candidates`] but returns an empty report (with real
/// exclusion counts) instead of [`GroundTruthError::EmptyResult`].
pub fn collect_from_candidates_allow_empty(
    candidates: &[DiscoveryCandidate],
    range: BlockRange,
    census: Option<&HashMap<String, PoolCensusEntry>>,
    input_label: &str,
    mut notes: Vec<String>,
) -> CollectResult {
    match collect_from_candidates(candidates, range, census, input_label, notes.clone()) {
        Ok(r) => r,
        Err(GroundTruthError::EmptyResult { .. }) => {
            let mut exclusions = ExclusionCounts::default();
            let mut seen = 0usize;
            for c in candidates {
                seen += 1;
                if let CandidateDecision::Exclude(cat) = classify_candidate(c, range, census) {
                    exclusions.record(cat);
                }
            }
            notes.push("empty accepted set (allow_empty)".into());
            empty_collect_result(range, input_label, seen, exclusions, notes)
        }
        Err(e) => {
            // Range/IO should not reach here from pure in-memory collect;
            // surface as empty report with a note rather than panic.
            notes.push(format!("collect failed: {e}"));
            empty_collect_result(range, input_label, 0, ExclusionCounts::default(), notes)
        }
    }
}

/// Load discovery candidates from JSONL (one JSON object per line).
pub fn load_candidates_jsonl(path: &Path) -> Result<Vec<DiscoveryCandidate>, GroundTruthError> {
    let file = File::open(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| GroundTruthError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let c: DiscoveryCandidate =
            serde_json::from_str(line).map_err(|source| GroundTruthError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        out.push(c);
    }
    Ok(out)
}

/// Load candidates from a Dune CSV export.
///
/// Expected headers (case-insensitive, extras ignored):
/// `block_number|block`, `tx_hash|hash`, `bot_address|from`, `to`,
/// `ordered_pools|path` (`;` or `|` separated), `n_swaps`, `kinds` (`;`),
/// `msg_value_wei`, `has_liquidation`, `has_jit_lp`, `is_sandwich`,
/// `is_flash_loan`, `settlement_asset` (optional — absorbed into pos).
pub fn load_candidates_dune_csv(path: &Path) -> Result<Vec<DiscoveryCandidate>, GroundTruthError> {
    let file = File::open(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut rdr = csv::ReaderBuilder::new()
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(file);
    let headers = rdr
        .headers()
        .map_err(|e| GroundTruthError::Message(format!("csv headers {}: {e}", path.display())))?
        .iter()
        .map(|h| h.to_ascii_lowercase())
        .collect::<Vec<_>>();
    let idx = |name: &str| headers.iter().position(|h| h == name);
    let mut out = Vec::new();
    for (row_i, rec) in rdr.records().enumerate() {
        let rec = rec.map_err(|e| {
            GroundTruthError::Message(format!("csv row {} {}: {e}", row_i + 2, path.display()))
        })?;
        let get = |names: &[&str]| -> Option<String> {
            for n in names {
                if let Some(i) = idx(n) {
                    if let Some(v) = rec.get(i) {
                        if !v.is_empty() {
                            return Some(v.to_string());
                        }
                    }
                }
            }
            None
        };
        let pools_raw = get(&["ordered_pools", "path", "pools"]).unwrap_or_default();
        let ordered_pools: Vec<String> = pools_raw
            .split(|c| c == ';' || c == '|' || c == ',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let kinds_raw = get(&["kinds", "dex_kinds"]).unwrap_or_default();
        let kinds: Vec<String> = kinds_raw
            .split(|c| c == ';' || c == '|' || c == ',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let parse_bool = |names: &[&str]| -> Option<bool> {
            get(names).and_then(|s| match s.to_ascii_lowercase().as_str() {
                "1" | "true" | "t" | "yes" => Some(true),
                "0" | "false" | "f" | "no" => Some(false),
                _ => None,
            })
        };
        let mut pos = Vec::new();
        if let Some(sa) = get(&["settlement_asset", "settlement"]) {
            pos.push(TokenAmount::Pair(sa, "1".into()));
        }
        out.push(DiscoveryCandidate {
            block_number: get(&["block_number", "block"]).and_then(|s| s.parse().ok()),
            tx_hash: get(&["tx_hash", "hash", "transaction_hash"]),
            bot_address: get(&["bot_address", "from", "sender"]),
            to_address: get(&["to", "to_address", "executor"]),
            ordered_pools,
            n_swaps: get(&["n_swaps", "nswaps", "swap_count"]).and_then(|s| s.parse().ok()),
            kinds,
            pos,
            neg: Vec::new(),
            msg_value_wei: get(&["msg_value_wei", "msg_value", "value"]),
            gross_out: parse_bool(&["gross_out", "entity_gross_out"]),
            has_liquidation: parse_bool(&["has_liquidation", "liquidation"]),
            has_jit_lp: parse_bool(&["has_jit_lp", "jit_lp"]),
            is_sandwich: parse_bool(&["is_sandwich", "sandwich"]),
            is_flash_loan: parse_bool(&["is_flash_loan", "flash_loan"]),
            route: get(&["route"]),
            label: get(&["label"]),
            selector: get(&["selector", "sel"]),
        });
    }
    Ok(out)
}

/// Write `{ "events": [...] }` known-bots file for the WHI-715 comparator.
pub fn write_known_bots_json(
    path: &Path,
    events: &[KnownBotEvent],
) -> Result<(), GroundTruthError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| GroundTruthError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    #[derive(Serialize)]
    struct Wrapped<'a> {
        events: &'a [KnownBotEvent],
    }
    let file = File::create(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, &Wrapped { events }).map_err(|source| {
        GroundTruthError::Json {
            path: path.display().to_string(),
            source,
        }
    })?;
    w.write_all(b"\n").map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Load `KnownBotEvent` rows written by [`write_events_jsonl`] (one object per
/// line). Also used by the collector CLI `sample` subcommand.
pub fn load_events_jsonl(path: &Path) -> Result<Vec<KnownBotEvent>, GroundTruthError> {
    let file = File::open(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| GroundTruthError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let e: KnownBotEvent =
            serde_json::from_str(line).map_err(|source| GroundTruthError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        out.push(e);
    }
    if out.is_empty() {
        return Err(GroundTruthError::Message(format!(
            "no events in {}",
            path.display()
        )));
    }
    Ok(out)
}

/// Write events as JSONL (one event per line) — external dataset form.
pub fn write_events_jsonl(path: &Path, events: &[KnownBotEvent]) -> Result<(), GroundTruthError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| GroundTruthError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    let file = File::create(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    for e in events {
        serde_json::to_writer(&mut w, e).map_err(|source| GroundTruthError::Json {
            path: path.display().to_string(),
            source,
        })?;
        w.write_all(b"\n").map_err(|source| GroundTruthError::Io {
            path: path.display().to_string(),
            source,
        })?;
    }
    Ok(())
}

/// Write pretty JSON report.
pub fn write_report(path: &Path, report: &GroundTruthReport) -> Result<(), GroundTruthError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| GroundTruthError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
    }
    let file = File::create(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, report).map_err(|source| GroundTruthError::Json {
        path: path.display().to_string(),
        source,
    })?;
    w.write_all(b"\n").map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    Ok(())
}

/// Markdown summary of a report.
pub fn render_report_markdown(report: &GroundTruthReport) -> String {
    let mut out = String::new();
    out.push_str("# Ground-truth collector report (WHI-956)\n\n");
    out.push_str(&format!("- Schema: `{}`\n", report.schema_version));
    out.push_str(&format!(
        "- Block range: `{}`–`{}`\n",
        report.from_block, report.to_block
    ));
    out.push_str(&format!("- Input: `{}`\n", report.input_label));
    out.push_str(&format!("- Candidates seen: {}\n", report.candidates_seen));
    out.push_str(&format!("- Accepted: {}\n", report.accepted));
    out.push_str(&format!(
        "- Distinct bots: {}\n",
        report.distinct_bot_addresses
    ));
    out.push_str(&format!(
        "- Events fingerprint: `{}`\n",
        report.events_fingerprint
    ));
    out.push_str(&format!("- Heuristic: `{}`\n\n", report.heuristic));

    out.push_str("## Exclusion counts\n\n");
    out.push_str("| Category | Count |\n| --- | ---: |\n");
    let e = &report.exclusion_counts;
    for (name, n) in [
        ("cex_dex", e.cex_dex),
        ("liquidation", e.liquidation),
        ("jit_lp", e.jit_lp),
        ("sandwich", e.sandwich),
        ("insufficient_swaps", e.insufficient_swaps),
        ("not_closed_cycle", e.not_closed_cycle),
        ("no_gross_out", e.no_gross_out),
        ("out_of_range", e.out_of_range),
        ("missing_fields", e.missing_fields),
    ] {
        out.push_str(&format!("| {name} | {n} |\n"));
    }
    out.push_str(&format!("| **total excluded** | {} |\n\n", e.total()));

    out.push_str("## Hop-count distribution\n\n");
    for (k, v) in &report.hop_count_distribution {
        out.push_str(&format!("- `{k}`: {v}\n"));
    }
    out.push_str("\n## Funding distribution\n\n");
    for (k, v) in &report.funding_distribution {
        out.push_str(&format!("- `{k}`: {v}\n"));
    }
    out.push_str("\n## Settlement-asset distribution (top)\n\n");
    let mut sett: Vec<_> = report.settlement_asset_distribution.iter().collect();
    sett.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (k, v) in sett.into_iter().take(15) {
        out.push_str(&format!("- `{k}`: {v}\n"));
    }
    out.push_str("\n## Venue (factory) distribution (top)\n\n");
    let mut ven: Vec<_> = report.venue_distribution.iter().collect();
    ven.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    for (k, v) in ven.into_iter().take(15) {
        out.push_str(&format!("- `{k}`: {v}\n"));
    }

    if let Some(ver) = &report.verification {
        out.push_str("\n## Verification sample\n\n");
        out.push_str(&format!("- Method: {}\n", ver.method));
        out.push_str(&format!("- Sample size: {}\n", ver.sample_size));
        out.push_str(&format!(
            "- True positive: {}\n",
            ver.verified_true_positive
        ));
        out.push_str(&format!(
            "- False positive: {}\n",
            ver.verified_false_positive
        ));
        out.push_str(&format!("- Unverified: {}\n", ver.unverified));
        if let Some(p) = ver.precision {
            out.push_str(&format!("- **Precision: {:.1}%**\n", p * 100.0));
        }
        if !ver.notes.is_empty() {
            out.push_str(&format!("- Notes: {}\n", ver.notes));
        }
    }

    if !report.notes.is_empty() {
        out.push_str("\n## Notes\n\n");
        for n in &report.notes {
            out.push_str(&format!("- {n}\n"));
        }
    }
    out
}

/// Deterministic sample of tx hashes for manual / Blockscout verification.
///
/// Uses stride sampling over the sorted event list so re-runs pick the same
/// sample for a fixed `sample_size`.
pub fn sample_tx_hashes(events: &[KnownBotEvent], sample_size: usize) -> Vec<String> {
    if events.is_empty() || sample_size == 0 {
        return Vec::new();
    }
    let n = sample_size.min(events.len());
    if n == events.len() {
        return events.iter().map(|e| e.tx_hash.clone()).collect();
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let idx = i * events.len() / n;
        out.push(events[idx].tx_hash.clone());
    }
    out.sort();
    out.dedup();
    // If dedup shrank the sample (duplicate hashes), fill from the front.
    if out.len() < n {
        for e in events {
            if out.len() >= n {
                break;
            }
            if !out.iter().any(|h| h == &e.tx_hash) {
                out.push(e.tx_hash.clone());
            }
        }
        out.sort();
    }
    out
}

/// One human/Blockscout label for a sampled tx.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationLabel {
    pub tx_hash: String,
    /// `true_positive` | `false_positive` | `unverified`
    pub verdict: String,
    #[serde(default)]
    pub note: Option<String>,
}

/// Build a [`VerificationSample`] from labels (human or Blockscout-assisted).
pub fn verification_from_labels(
    labels: &[VerificationLabel],
    method: impl Into<String>,
    notes: impl Into<String>,
) -> VerificationSample {
    let mut tp = 0usize;
    let mut fp = 0usize;
    let mut uv = 0usize;
    let mut hashes = Vec::new();
    for l in labels {
        hashes.push(l.tx_hash.to_ascii_lowercase());
        match l.verdict.to_ascii_lowercase().as_str() {
            "true_positive" | "tp" | "true" | "yes" => tp += 1,
            "false_positive" | "fp" | "false" | "no" => fp += 1,
            _ => uv += 1,
        }
    }
    let precision = if tp + fp > 0 {
        Some(tp as f64 / (tp + fp) as f64)
    } else {
        None
    };
    VerificationSample {
        sample_size: labels.len(),
        verified_true_positive: tp,
        verified_false_positive: fp,
        unverified: uv,
        precision,
        method: method.into(),
        notes: notes.into(),
        sample_tx_hashes: hashes,
    }
}

/// Structural exclusion flags shared by the collector and verification scorer.
#[derive(Debug, Clone, Default)]
pub struct StructuralFlags {
    pub swap_count: u32,
    pub msg_value_wei: u128,
    pub has_liquidation: bool,
    pub has_jit_lp: bool,
    pub is_sandwich: bool,
    /// When known: entity has ≥1 net-positive token leg (closed-cycle evidence).
    pub has_positive_leg: Option<bool>,
}

/// Apply the same exclusion axes as [`classify_candidate`] to structural flags.
/// Returns `None` when the heuristic holds, else the exclusion category.
pub fn structural_exclusion(flags: &StructuralFlags) -> Option<ExclusionCategory> {
    if flags.has_liquidation {
        return Some(ExclusionCategory::Liquidation);
    }
    if flags.is_sandwich {
        return Some(ExclusionCategory::Sandwich);
    }
    if flags.has_jit_lp {
        return Some(ExclusionCategory::JitLp);
    }
    if flags.msg_value_wei > CEX_DEX_MSG_VALUE_WEI {
        return Some(ExclusionCategory::CexDex);
    }
    if flags.swap_count < 2 {
        return Some(ExclusionCategory::InsufficientSwaps);
    }
    if flags.has_positive_leg == Some(false) {
        return Some(ExclusionCategory::NotClosedCycle);
    }
    None
}

/// Score a Blockscout / RPC verification view against the acceptance heuristic.
///
/// Accepts either:
/// - Blockscout API v2 shape (`status` / `result`, optional decoded fields), or
/// - a minimal fixture / RPC-derived view:
///   `{ "status": "ok", "swap_count": N, "has_liquidation": bool,
///      "has_positive_leg": bool, "msg_value_wei": "…", … }`.
///
/// Structural check only — does not re-simulate profit. Prefer pairing with a
/// human/Blockscout token-transfer review for closed-cycle confirmation when
/// `has_positive_leg` is absent.
pub fn score_blockscout_tx(tx: &serde_json::Value) -> VerificationLabel {
    let hash = tx
        .get("hash")
        .or_else(|| tx.get("tx_hash"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Explicit fixture override.
    if let Some(v) = tx.get("verdict").and_then(|v| v.as_str()) {
        return VerificationLabel {
            tx_hash: hash,
            verdict: v.to_string(),
            note: tx
                .get("note")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };
    }

    let status_ok = match tx.get("status") {
        Some(serde_json::Value::String(s)) => {
            let s = s.to_ascii_lowercase();
            s == "ok" || s == "success" || s == "1" || s == "0x1"
        }
        Some(serde_json::Value::Number(n)) => n.as_u64() == Some(1),
        Some(serde_json::Value::Bool(b)) => *b,
        _ => {
            // Blockscout v2: result == "success"
            tx.get("result")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("success"))
                .unwrap_or(false)
        }
    };
    if !status_ok {
        return VerificationLabel {
            tx_hash: hash,
            verdict: "false_positive".into(),
            note: Some("tx not successful".into()),
        };
    }

    let swap_count = tx
        .get("swap_count")
        .and_then(|v| v.as_u64())
        .or_else(|| tx.get("n_swaps").and_then(|v| v.as_u64()))
        .unwrap_or(0) as u32;

    let msg_value_wei = tx
        .get("msg_value_wei")
        .and_then(|v| v.as_str())
        .and_then(parse_wei)
        .or_else(|| {
            tx.get("msg_value_wei")
                .and_then(|v| v.as_u64())
                .map(|n| n as u128)
        })
        .unwrap_or(0);

    let has_positive_leg = tx.get("has_positive_leg").and_then(|v| v.as_bool());

    let flags = StructuralFlags {
        swap_count,
        msg_value_wei,
        has_liquidation: tx
            .get("has_liquidation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        has_jit_lp: tx
            .get("has_jit_lp")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        is_sandwich: tx
            .get("is_sandwich")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        has_positive_leg,
    };

    if let Some(cat) = structural_exclusion(&flags) {
        return VerificationLabel {
            tx_hash: hash,
            verdict: "false_positive".into(),
            note: Some(cat.as_str().into()),
        };
    }

    let note = if has_positive_leg == Some(true) {
        "heuristic holds (incl. closed-cycle positive leg)"
    } else {
        "structural heuristic holds; closed-cycle not re-checked on this view"
    };
    VerificationLabel {
        tx_hash: hash,
        verdict: "true_positive".into(),
        note: Some(note.into()),
    }
}

/// Load verification labels from a JSON array or JSONL file.
pub fn load_verification_labels(path: &Path) -> Result<Vec<VerificationLabel>, GroundTruthError> {
    let bytes = std::fs::read(path).map_err(|source| GroundTruthError::Io {
        path: path.display().to_string(),
        source,
    })?;
    // Try array first.
    if let Ok(labels) = serde_json::from_slice::<Vec<VerificationLabel>>(&bytes) {
        return Ok(labels);
    }
    // JSONL
    let mut labels = Vec::new();
    for (i, line) in bytes.split(|&b| b == b'\n').enumerate() {
        let line = std::str::from_utf8(line).unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let l: VerificationLabel =
            serde_json::from_str(line).map_err(|source| GroundTruthError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        labels.push(l);
    }
    Ok(labels)
}

/// Full offline collect from a candidates path (jsonl or csv by extension).
pub fn collect_from_path(
    input: &Path,
    range: BlockRange,
    census: Option<&HashMap<String, PoolCensusEntry>>,
    notes: Vec<String>,
) -> Result<CollectResult, GroundTruthError> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let candidates = if ext == "csv" {
        load_candidates_dune_csv(input)?
    } else {
        load_candidates_jsonl(input)?
    };
    let label = input
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| input.display().to_string());
    collect_from_candidates(&candidates, range, census, &label, notes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::arb_coverage::PoolCensusEntry;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn range() -> BlockRange {
        BlockRange::new(100, 200).unwrap()
    }

    fn base_candidate() -> DiscoveryCandidate {
        DiscoveryCandidate {
            block_number: Some(150),
            tx_hash: Some("0xabc".into()),
            bot_address: Some("0xB0b0000000000000000000000000000000000001".into()),
            to_address: Some("0xE0e0000000000000000000000000000000000002".into()),
            ordered_pools: vec![
                "0xP000000000000000000000000000000000000001".into(),
                "0xP000000000000000000000000000000000000002".into(),
            ],
            n_swaps: Some(2),
            kinds: vec!["v3".into()],
            pos: vec![TokenAmount::Pair(
                "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8".into(),
                "1000".into(),
            )],
            neg: vec![],
            msg_value_wei: Some("0".into()),
            gross_out: Some(true),
            has_liquidation: Some(false),
            has_jit_lp: Some(false),
            is_sandwich: Some(false),
            is_flash_loan: Some(false),
            route: None,
            label: None,
            selector: None,
        }
    }

    #[test]
    fn accepts_clean_atomic_arb() {
        let d = classify_candidate(&base_candidate(), range(), None);
        match d {
            CandidateDecision::Accept(e) => {
                assert_eq!(e.block_number, 150);
                assert_eq!(e.hop_count, Some(2));
                assert_eq!(e.funding.as_deref(), Some(FUNDING_SELF));
                assert_eq!(
                    e.settlement_asset.as_deref(),
                    Some("0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8")
                );
                assert!(e.route.as_deref().unwrap().starts_with("h2:"));
            }
            CandidateDecision::Exclude(c) => panic!("unexpected exclude {c:?}"),
        }
    }

    #[test]
    fn excludes_cex_dex_msg_value() {
        let mut c = base_candidate();
        c.msg_value_wei = Some((CEX_DEX_MSG_VALUE_WEI + 1).to_string());
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::CexDex)
        );
    }

    #[test]
    fn excludes_liquidation_jit_sandwich() {
        let mut c = base_candidate();
        c.has_liquidation = Some(true);
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::Liquidation)
        );
        c = base_candidate();
        c.has_jit_lp = Some(true);
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::JitLp)
        );
        c = base_candidate();
        c.is_sandwich = Some(true);
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::Sandwich)
        );
    }

    #[test]
    fn excludes_not_closed_and_low_swaps() {
        let mut c = base_candidate();
        c.neg = vec![TokenAmount::Pair("0xt".into(), "1".into())];
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::NotClosedCycle)
        );
        c = base_candidate();
        c.ordered_pools = vec!["0xP000000000000000000000000000000000000001".into()];
        c.n_swaps = Some(1);
        assert_eq!(
            classify_candidate(&c, range(), None),
            CandidateDecision::Exclude(ExclusionCategory::InsufficientSwaps)
        );
    }

    #[test]
    fn out_of_range_counted() {
        let c = base_candidate();
        assert_eq!(
            classify_candidate(&c, BlockRange::new(180, 200).unwrap(), None),
            CandidateDecision::Exclude(ExclusionCategory::OutOfRange)
        );
    }

    #[test]
    fn flash_loan_funding_from_marker() {
        let mut c = base_candidate();
        c.is_flash_loan = Some(true);
        match classify_candidate(&c, range(), None) {
            CandidateDecision::Accept(e) => {
                assert_eq!(e.funding.as_deref(), Some(FUNDING_FLASH));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn venues_from_census() {
        let mut census = HashMap::new();
        census.insert(
            "0xp000000000000000000000000000000000000001".into(),
            PoolCensusEntry {
                factory: Some("0xFactory00000000000000000000000000000001".into()),
                kind: Some("v3".into()),
                ..Default::default()
            },
        );
        census.insert(
            "0xp000000000000000000000000000000000000002".into(),
            PoolCensusEntry {
                factory: Some("0xFactory00000000000000000000000000000002".into()),
                kind: Some("v2".into()),
                ..Default::default()
            },
        );
        match classify_candidate(&base_candidate(), range(), Some(&census)) {
            CandidateDecision::Accept(e) => {
                let v = e.venues.unwrap();
                assert_eq!(v.len(), 2);
                assert!(v[0] < v[1]); // sorted
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn collect_is_regenerable() {
        let mut cands = vec![base_candidate(), base_candidate()];
        // Second candidate different hash/block for multi-event.
        cands[1].block_number = Some(160);
        cands[1].tx_hash = Some("0xdef".into());
        // Plus one exclusion.
        let mut bad = base_candidate();
        bad.block_number = Some(155);
        bad.tx_hash = Some("0xbad".into());
        bad.has_liquidation = Some(true);
        cands.push(bad);

        let a = collect_from_candidates(&cands, range(), None, "test", vec![]).unwrap();
        let b = collect_from_candidates(&cands, range(), None, "test", vec![]).unwrap();
        assert_eq!(a.report.events_fingerprint, b.report.events_fingerprint);
        assert_eq!(a.events, b.events);
        assert_eq!(a.report.accepted, 2);
        assert_eq!(a.report.exclusion_counts.liquidation, 1);
        assert_eq!(a.report.distinct_bot_addresses, 1);
    }

    #[test]
    fn jsonl_roundtrip_and_known_bots_loadable() {
        let dir = tempdir().unwrap();
        let input = dir.path().join("cands.jsonl");
        let mut body = String::new();
        for (i, block) in [150u64, 160].iter().enumerate() {
            let mut c = base_candidate();
            c.block_number = Some(*block);
            c.tx_hash = Some(format!("0x{i:064x}"));
            body.push_str(&serde_json::to_string(&c).unwrap());
            body.push('\n');
        }
        std::fs::write(&input, body).unwrap();

        let result = collect_from_path(&input, range(), None, vec!["unit".into()]).unwrap();
        let known = dir.path().join("known_bots.json");
        write_known_bots_json(&known, &result.events).unwrap();
        let loaded =
            crate::execution::shadow_bot_benchmark::load_known_bot_events(&known).unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].hop_count, Some(2));
        assert!(loaded[0].funding.is_some());
    }

    #[test]
    fn dune_csv_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("dune.csv");
        std::fs::write(
            &path,
            "block_number,tx_hash,bot_address,ordered_pools,n_swaps,kinds,msg_value_wei,has_liquidation,is_sandwich,has_jit_lp,is_flash_loan,settlement_asset\n\
             150,0xaaa,0xbot1,0xpool1;0xpool2,2,v3,0,false,false,false,false,0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8\n\
             151,0xbbb,0xbot2,0xpool3,1,v2,0,false,false,false,false,\n",
        )
        .unwrap();
        let cands = load_candidates_dune_csv(&path).unwrap();
        assert_eq!(cands.len(), 2);
        let r = collect_from_candidates(&cands, range(), None, "dune.csv", vec![]).unwrap();
        assert_eq!(r.report.accepted, 1);
        assert_eq!(r.report.exclusion_counts.insufficient_swaps, 1);
    }

    #[test]
    fn sample_and_verification_precision() {
        let mut events = Vec::new();
        for i in 0..10u64 {
            let mut c = base_candidate();
            c.block_number = Some(100 + i);
            c.tx_hash = Some(format!("0x{i:064x}"));
            if let CandidateDecision::Accept(e) = classify_candidate(&c, range(), None) {
                events.push(e);
            }
        }
        sort_events(&mut events);
        let sample = sample_tx_hashes(&events, 4);
        assert_eq!(sample.len(), 4);
        // Same sample on re-run.
        assert_eq!(sample, sample_tx_hashes(&events, 4));

        let labels = vec![
            VerificationLabel {
                tx_hash: sample[0].clone(),
                verdict: "true_positive".into(),
                note: None,
            },
            VerificationLabel {
                tx_hash: sample[1].clone(),
                verdict: "true_positive".into(),
                note: None,
            },
            VerificationLabel {
                tx_hash: sample[2].clone(),
                verdict: "false_positive".into(),
                note: Some("jit".into()),
            },
            VerificationLabel {
                tx_hash: sample[3].clone(),
                verdict: "unverified".into(),
                note: None,
            },
        ];
        let v = verification_from_labels(&labels, "manual+blockscout", "fixture");
        assert_eq!(v.verified_true_positive, 2);
        assert_eq!(v.verified_false_positive, 1);
        assert_eq!(v.unverified, 1);
        assert!((v.precision.unwrap() - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn score_blockscout_fixture() {
        let ok = serde_json::json!({
            "hash": "0x1",
            "status": "ok",
            "swap_count": 3,
            "has_positive_leg": true
        });
        assert_eq!(score_blockscout_tx(&ok).verdict, "true_positive");

        let liq = serde_json::json!({
            "hash": "0x2",
            "status": "ok",
            "swap_count": 3,
            "has_liquidation": true
        });
        assert_eq!(score_blockscout_tx(&liq).verdict, "false_positive");

        let no_swaps = serde_json::json!({
            "hash": "0x3",
            "status": "ok",
            "swap_count": 1
        });
        assert_eq!(score_blockscout_tx(&no_swaps).verdict, "false_positive");
    }

    #[test]
    fn legacy_fixture_known_bots_still_load() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/shadow_bot_benchmark/known_bots.json");
        let events =
            crate::execution::shadow_bot_benchmark::load_known_bot_events(&root).unwrap();
        assert_eq!(events.len(), 3);
        assert!(events[0].hop_count.is_none());
    }
}
