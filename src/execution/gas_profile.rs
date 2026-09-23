//! Versioned measured gas profiles for Mantle arbitrage (WHI-546 / M0-9).
//!
//! This module owns:
//! - the shared schema consumed later by M0-2 / WHI-502
//! - deterministic generation of profile artifacts from pinned samples
//! - holdout + margin policy derivation that keeps `gas_limit` separate from
//!   `expected_gas_used`
//!
//! Production qualification samples must name the final replacement executor
//! runtime codehash (M0-8 / WHI-501). Historical / old-executor receipts are
//! research-only and never approve a production profile.

use alloy::primitives::keccak256;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// Schema version for on-disk / in-memory profile artifacts.
pub const GAS_PROFILE_SCHEMA_VERSION: u32 = 1;

/// Tool version embedded in generated artifacts (bump when generation logic changes).
pub const GAS_PROFILE_TOOL_VERSION: &str = "0.1.0";

/// Mantle mainnet chain id.
pub const MANTLE_MAINNET_CHAIN_ID: u64 = 5000;

/// WHI-501 optimized runtime **template** hash (`keccak256(deployedBytecode)` with the
/// `WMNT` immutable slot still zero-filled). Frozen build-provenance pin for this
/// gas-profile data (`config/gas_profiles/mantle_mainnet_v1.json`) — never a live
/// on-chain identity, since a real deployment has that slot patched. The live mainnet
/// identity is `WHI501_EXECUTOR_PATCHED_RUNTIME_HASH` (`gas_runtime.rs`, WHI-551).
/// Must equal `config/executor_identity.json`'s `template_hash` (WHI-557 / DI-17 pin
/// test in `gas_runtime_tests.rs`).
pub const WHI501_EXECUTOR_CODEHASH: &str =
    "0x50f51b776f893c4c86573ec5d669a1be3e84b3eef9817960376ba59ace2826ef";

// ---------------------------------------------------------------------------
// Schema types
// ---------------------------------------------------------------------------

/// Protocol hop kind used in ordered route keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolKind {
    /// UniswapV2-compatible (Agni V2 / Moe LP pair).
    V2,
    /// UniswapV3-compatible (Agni V3).
    V3,
    /// Moe Liquidity Book.
    Moe,
}

impl ProtocolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V2 => "v2",
            Self::V3 => "v3",
            Self::Moe => "moe",
        }
    }
}

impl fmt::Display for ProtocolKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// V3 tick-crossing bucket used in route keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum TickCrossingBucket {
    #[serde(rename = "0")]
    Zero,
    #[serde(rename = "1-5")]
    Low,
    #[serde(rename = "6-20")]
    Mid,
    #[serde(rename = "21+")]
    High,
}

impl TickCrossingBucket {
    /// Every bucket variant, in ascending order. Kept next to the enum so a
    /// new variant can't silently drop out of enumeration call sites (e.g.
    /// `path_index::route_key_candidates`) without a compile-time nudge here.
    pub const ALL: [Self; 4] = [Self::Zero, Self::Low, Self::Mid, Self::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::Low => "1-5",
            Self::Mid => "6-20",
            Self::High => "21+",
        }
    }

    pub fn from_crossings(crossings: u32) -> Self {
        match crossings {
            0 => Self::Zero,
            1..=5 => Self::Low,
            6..=20 => Self::Mid,
            _ => Self::High,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "0" => Some(Self::Zero),
            "1-5" => Some(Self::Low),
            "6-20" => Some(Self::Mid),
            "21+" => Some(Self::High),
            _ => None,
        }
    }
}

impl fmt::Display for TickCrossingBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Moe bin-crossing bucket used in route keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum BinCrossingBucket {
    #[serde(rename = "0")]
    Zero,
    #[serde(rename = "1-3")]
    Low,
    #[serde(rename = "4-10")]
    Mid,
    #[serde(rename = "11+")]
    High,
}

impl BinCrossingBucket {
    /// Every bucket variant, in ascending order. Kept next to the enum so a
    /// new variant can't silently drop out of enumeration call sites (e.g.
    /// `path_index::route_key_candidates`) without a compile-time nudge here.
    pub const ALL: [Self; 4] = [Self::Zero, Self::Low, Self::Mid, Self::High];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::Low => "1-3",
            Self::Mid => "4-10",
            Self::High => "11+",
        }
    }

    pub fn from_crossings(crossings: u32) -> Self {
        match crossings {
            0 => Self::Zero,
            1..=3 => Self::Low,
            4..=10 => Self::Mid,
            _ => Self::High,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "0" => Some(Self::Zero),
            "1-3" => Some(Self::Low),
            "4-10" => Some(Self::Mid),
            "11+" => Some(Self::High),
            _ => None,
        }
    }
}

impl fmt::Display for BinCrossingBucket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Ordered route class key.
///
/// Minimum distinction: ordered protocol sequence + hop count.
/// V3 / Moe keys must include tick / bin crossing buckets when those change the
/// distribution (see fixture evidence in pinned samples).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RouteKey {
    /// Ordered per-hop protocol sequence. Length must equal `hop_count`.
    pub protocols: Vec<ProtocolKind>,
    pub hop_count: u8,
    /// Present when the route contains at least one V3 hop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub v3_tick_crossings: Option<TickCrossingBucket>,
    /// Present when the route contains at least one Moe hop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moe_bin_crossings: Option<BinCrossingBucket>,
}

impl RouteKey {
    pub fn new(protocols: Vec<ProtocolKind>) -> Result<Self, GasProfileError> {
        let hop_count = u8::try_from(protocols.len())
            .map_err(|_| GasProfileError::InvalidRouteKey("hop_count overflow".into()))?;
        if hop_count == 0 {
            return Err(GasProfileError::InvalidRouteKey(
                "hop_count must be >= 1".into(),
            ));
        }
        let has_v3 = protocols.contains(&ProtocolKind::V3);
        let has_moe = protocols.contains(&ProtocolKind::Moe);
        Ok(Self {
            protocols,
            hop_count,
            v3_tick_crossings: if has_v3 {
                Some(TickCrossingBucket::Zero)
            } else {
                None
            },
            moe_bin_crossings: if has_moe {
                Some(BinCrossingBucket::Zero)
            } else {
                None
            },
        })
    }

    pub fn with_v3_ticks(mut self, bucket: TickCrossingBucket) -> Self {
        self.v3_tick_crossings = Some(bucket);
        self
    }

    pub fn with_moe_bins(mut self, bucket: BinCrossingBucket) -> Self {
        self.moe_bin_crossings = Some(bucket);
        self
    }

    pub fn key_string(&self) -> String {
        let protos: Vec<&str> = self.protocols.iter().map(|p| p.as_str()).collect();
        let mut s = format!("h{}:{}", self.hop_count, protos.join("+"));
        if let Some(t) = self.v3_tick_crossings {
            s.push_str(&format!(":ticks={t}"));
        }
        if let Some(b) = self.moe_bin_crossings {
            s.push_str(&format!(":bins={b}"));
        }
        s
    }

    pub fn validate_structure(&self) -> Result<(), GasProfileError> {
        if self.protocols.len() != self.hop_count as usize {
            return Err(GasProfileError::InvalidRouteKey(format!(
                "protocols len {} != hop_count {}",
                self.protocols.len(),
                self.hop_count
            )));
        }
        if self.hop_count == 0 {
            return Err(GasProfileError::InvalidRouteKey(
                "hop_count must be >= 1".into(),
            ));
        }
        let has_v3 = self.protocols.contains(&ProtocolKind::V3);
        let has_moe = self.protocols.contains(&ProtocolKind::Moe);
        if has_v3 && self.v3_tick_crossings.is_none() {
            return Err(GasProfileError::InvalidRouteKey(
                "V3 route missing v3_tick_crossings bucket".into(),
            ));
        }
        if !has_v3 && self.v3_tick_crossings.is_some() {
            return Err(GasProfileError::InvalidRouteKey(
                "non-V3 route must not set v3_tick_crossings".into(),
            ));
        }
        if has_moe && self.moe_bin_crossings.is_none() {
            return Err(GasProfileError::InvalidRouteKey(
                "Moe route missing moe_bin_crossings bucket".into(),
            ));
        }
        if !has_moe && self.moe_bin_crossings.is_some() {
            return Err(GasProfileError::InvalidRouteKey(
                "non-Moe route must not set moe_bin_crossings".into(),
            ));
        }
        Ok(())
    }
}

/// Provenance of a gas sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleSource {
    /// Hash-pinned fork/replay against the replacement executor bytecode.
    /// Only this source may qualify production profiles.
    ForkReplay,
    /// Canonical historical receipts (any executor codehash). Research only.
    ResearchHistorical,
    /// Revert / partial-failure observations. Never mixed into success limits.
    ResearchRevert,
    /// Foundry EVM replay against synthetic mock pools (not real chain state).
    /// Research only — never qualifies a production profile.
    FoundryMock,
}

/// Pool venue referenced by a hop in a measured sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VenueRef {
    pub protocol: ProtocolKind,
    pub pool: String,
}

/// On-chain call outcome for a measured sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleOutcome {
    Success,
    Reverted,
}

/// One measured gas observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasSample {
    pub route_key: RouteKey,
    pub gas_used: u64,
    pub source: SampleSource,
    /// Runtime codehash of the executor that produced this sample.
    pub executor_code_hash: String,
    pub chain_id: u64,
    pub block_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_gas_price_wei: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_fee_wei: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_gas_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inclusion_latency_blocks: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Per-hop venues (pool addresses) actually exercised by this sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub venues: Option<Vec<VenueRef>>,
    /// keccak256 digest of the exact calldata submitted for this sample.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub calldata_digest: Option<String>,
    /// Call outcome; `Reverted` is excluded from qualification regardless of `gas_used`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<SampleOutcome>,
}

/// How `gas_limit` and `expected_gas_used` are derived from a sample set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginPolicy {
    /// Stable name recorded in the artifact (not a free-form comment).
    pub name: String,
    /// Percentile used for `expected_gas_used` (typically 50).
    pub expected_percentile: u8,
    /// Limit is `ceil(tail * (10000 + margin_bps) / 10000) + absolute_overhead`
    /// where `tail = max(max_sample, p99)`.
    pub margin_bps: u64,
    pub absolute_overhead: u64,
    /// Minimum qualification samples (train + holdout) required to approve.
    pub min_samples: usize,
    /// Holdout fraction in basis points (e.g. 2000 = 20%).
    pub holdout_fraction_bps: u64,
}

impl Default for MarginPolicy {
    fn default() -> Self {
        Self {
            name: "tail_max_p99_plus_20pct_plus_50k".into(),
            expected_percentile: 50,
            margin_bps: 2000,
            absolute_overhead: 50_000,
            min_samples: 10,
            holdout_fraction_bps: 2000,
        }
    }
}

impl MarginPolicy {
    /// Derive a conservative execution gas limit from sorted success samples.
    pub fn gas_limit_from_sorted(&self, sorted: &[u64]) -> Result<u64, GasProfileError> {
        if sorted.is_empty() {
            return Err(GasProfileError::EmptySamples);
        }
        let max = *sorted.last().unwrap();
        let p99 = percentile_sorted(sorted, 99);
        let tail = max.max(p99);
        let with_margin = mul_div_ceil(tail, 10_000 + self.margin_bps, 10_000)?;
        Ok(with_margin.saturating_add(self.absolute_overhead))
    }

    pub fn expected_from_sorted(&self, sorted: &[u64]) -> Result<u64, GasProfileError> {
        if sorted.is_empty() {
            return Err(GasProfileError::EmptySamples);
        }
        Ok(percentile_sorted(sorted, self.expected_percentile))
    }

    pub fn describe(&self) -> String {
        format!(
            "expected=p{}; gas_limit=ceil(max(max,p99)*(10000+{}bps)/10000)+{}; min_samples={}; holdout_bps={}",
            self.expected_percentile,
            self.margin_bps,
            self.absolute_overhead,
            self.min_samples,
            self.holdout_fraction_bps
        )
    }
}

/// Empirical distribution over success gas_used values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistributionStats {
    pub sample_count: usize,
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

impl DistributionStats {
    pub fn from_sorted(sorted: &[u64]) -> Result<Self, GasProfileError> {
        if sorted.is_empty() {
            return Err(GasProfileError::EmptySamples);
        }
        Ok(Self {
            sample_count: sorted.len(),
            min: sorted[0],
            p50: percentile_sorted(sorted, 50),
            p95: percentile_sorted(sorted, 95),
            p99: percentile_sorted(sorted, 99),
            max: *sorted.last().unwrap(),
        })
    }
}

/// Holdout evaluation for a proposed gas_limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HoldoutResult {
    pub holdout_count: usize,
    pub holdout_max: u64,
    /// Holdout samples only: all strictly below the proposed limit.
    pub holdout_below_limit: bool,
    /// Train samples only: all strictly below the proposed limit.
    pub train_below_limit: bool,
    /// Holdout samples only: all strictly below observed min block gas limit.
    pub holdout_below_block_gas_limit: bool,
    /// Train samples only: all strictly below observed min block gas limit.
    pub train_below_block_gas_limit: bool,
    /// Convenience: train and holdout both clear the gas_limit gate.
    pub all_below_limit: bool,
    /// Convenience: train and holdout both clear the block gas limit gate.
    pub all_below_block_gas_limit: bool,
}

/// Approval status for one route class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileStatus {
    /// Evidence-backed and within block gas limit; safe for production lookup.
    Approved,
    /// Explicitly not supported; production must fail closed (no generic fallback).
    Unsupported,
    /// Historical / research only — never used for production gas sizing.
    ResearchOnly,
}

/// One route class entry in the generated artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteProfile {
    pub route_key: RouteKey,
    pub status: ProfileStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Qualification sample stats (Approved only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<DistributionStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_gas_used: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holdout: Option<HoldoutResult>,
    /// Research-only stats (never mixed into Approved limits).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub research_stats: Option<DistributionStats>,
}

/// Result of an O(1) production lookup (consumed by WHI-502).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GasQuote {
    pub route_key: RouteKey,
    pub gas_limit: u64,
    pub expected_gas_used: u64,
    pub profile_identity: String,
}

/// Dated fee / inclusion observations. Values are evidence, not constants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeObservation {
    pub block_number: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<String>,
    pub base_fee_wei: u128,
    pub block_gas_limit: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_priority_fee_wei: Option<u128>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inclusion_latency_blocks: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeAnalysis {
    /// Inclusive analysis window.
    pub start_block: u64,
    pub end_block: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_block_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_block_hash: Option<String>,
    pub observation_count: usize,
    pub min_base_fee_wei: u128,
    pub max_base_fee_wei: u128,
    pub min_block_gas_limit: u64,
    pub max_block_gas_limit: u64,
    /// Free-text dated notes; must state current stability ≠ permanent constant.
    pub notes: String,
    pub observations: Vec<FeeObservation>,
}

/// Sampling policy recorded for reproducibility.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SamplingPolicy {
    pub description: String,
    /// Only this codehash may produce Approved profiles.
    pub qualification_executor_code_hash: String,
    pub qualification_source: SampleSource,
    pub research_sources_excluded_from_limits: Vec<SampleSource>,
}

/// Generator input config (pinned on disk).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneratorConfig {
    pub chain_id: u64,
    pub schema_version: u32,
    pub tool_version: String,
    pub executor_code_hash: String,
    pub executor_abi_digest: String,
    pub margin_policy: MarginPolicy,
    pub sampling_policy: SamplingPolicy,
    /// Explicit production route classes. Missing evidence → Unsupported.
    pub active_route_classes: Vec<RouteKey>,
    pub fee_analysis: FeeAnalysis,
    /// Optional overhead measurements for the replacement (callback auth, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_overhead_notes: Option<String>,
}

/// Full versioned artifact written under `config/gas_profiles/`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GasProfileArtifact {
    pub schema_version: u32,
    pub tool_version: String,
    pub chain_id: u64,
    pub executor_code_hash: String,
    pub executor_abi_digest: String,
    pub fee_analysis: FeeAnalysis,
    pub margin_policy: MarginPolicy,
    pub margin_policy_description: String,
    pub sampling_policy: SamplingPolicy,
    pub route_key_definition: String,
    pub profiles: Vec<RouteProfile>,
    /// Distribution evidence that V3 tick / Moe bin buckets are required.
    pub crossing_bucket_evidence: CrossingBucketEvidence,
    /// Count of `fork_replay` samples that entered generation (not reverts).
    pub qualification_sample_count: usize,
    /// Count of research-historical samples (never used for limits).
    pub research_historical_sample_count: usize,
    /// Count of research-revert observations (never mixed into success limits).
    pub research_revert_sample_count: usize,
    /// Count of Foundry-mock samples (synthetic pools, never mixed into production limits).
    #[serde(default)]
    pub foundry_mock_sample_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement_overhead_notes: Option<String>,
    /// Content digest over the artifact with this field zeroed / excluded.
    pub content_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossingBucketEvidence {
    /// Why tick buckets are part of the key (or why they were disproved).
    pub v3_tick_conclusion: String,
    /// Why bin buckets are part of the key (or why they were disproved).
    pub moe_bin_conclusion: String,
    /// Optional per-bucket p50/max snapshots used to justify the conclusion.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub v3_bucket_max_by_label: BTreeMap<String, u64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub moe_bucket_max_by_label: BTreeMap<String, u64>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum GasProfileError {
    #[error("empty sample set")]
    EmptySamples,
    #[error("invalid route key: {0}")]
    InvalidRouteKey(String),
    #[error("unknown route class (fail closed): {0}")]
    UnknownRouteClass(String),
    #[error("route class unsupported: {0}")]
    UnsupportedRoute(String),
    #[error("profile not approved for production: {0}")]
    NotApproved(String),
    #[error("arithmetic overflow")]
    Overflow,
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("config: {0}")]
    Config(String),
    #[error("validation: {0}")]
    Validation(String),
}

// ---------------------------------------------------------------------------
// Percentiles & arithmetic
// ---------------------------------------------------------------------------

/// Nearest-rank percentile on a non-empty sorted slice. `p` in 0..=100.
pub fn percentile_sorted(sorted: &[u64], p: u8) -> u64 {
    assert!(!sorted.is_empty());
    let p = p.min(100) as usize;
    if sorted.len() == 1 {
        return sorted[0];
    }
    // nearest-rank: index = ceil(p/100 * n) - 1
    let n = sorted.len();
    let rank = (p * n).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn mul_div_ceil(a: u64, b: u64, d: u64) -> Result<u64, GasProfileError> {
    if d == 0 {
        return Err(GasProfileError::Overflow);
    }
    let a = u128::from(a);
    let b = u128::from(b);
    let d = u128::from(d);
    let num = a
        .checked_mul(b)
        .ok_or(GasProfileError::Overflow)?
        .checked_add(d - 1)
        .ok_or(GasProfileError::Overflow)?;
    u64::try_from(num / d).map_err(|_| GasProfileError::Overflow)
}

// ---------------------------------------------------------------------------
// Train / holdout split (deterministic)
// ---------------------------------------------------------------------------

/// Deterministic split: first `train_count` after stable sort by (block, gas, tx).
pub fn split_train_holdout(samples: &[GasSample], holdout_fraction_bps: u64) -> (Vec<GasSample>, Vec<GasSample>) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| {
        (
            a.block_number,
            a.gas_used,
            a.tx_hash.as_deref().unwrap_or(""),
            a.block_hash.as_deref().unwrap_or(""),
        )
            .cmp(&(
                b.block_number,
                b.gas_used,
                b.tx_hash.as_deref().unwrap_or(""),
                b.block_hash.as_deref().unwrap_or(""),
            ))
    });
    let n = sorted.len();
    if n == 0 {
        return (vec![], vec![]);
    }
    let holdout_n = ((n as u128 * holdout_fraction_bps as u128) / 10_000) as usize;
    // Keep at least 1 train sample when possible; holdout may be 0 for tiny sets.
    let holdout_n = holdout_n.min(n.saturating_sub(1));
    let train_n = n - holdout_n;
    let train = sorted[..train_n].to_vec();
    let holdout = sorted[train_n..].to_vec();
    (train, holdout)
}

// ---------------------------------------------------------------------------
// Profile generation
// ---------------------------------------------------------------------------

pub fn gas_values(samples: &[GasSample]) -> Vec<u64> {
    let mut v: Vec<u64> = samples.iter().map(|s| s.gas_used).collect();
    v.sort_unstable();
    v
}

/// Base Unsupported profile; callers set optional fields with struct-update syntax
/// so `stats` / `research_stats` cannot be swapped by positional accident.
fn unsupported_profile(route_key: &RouteKey, reason: impl Into<String>) -> RouteProfile {
    RouteProfile {
        route_key: route_key.clone(),
        status: ProfileStatus::Unsupported,
        reason: Some(reason.into()),
        stats: None,
        expected_gas_used: None,
        gas_limit: None,
        holdout: None,
        research_stats: None,
    }
}

/// Build a single route profile from qualification samples (and optional research).
pub fn build_route_profile(
    route_key: &RouteKey,
    qualification: &[GasSample],
    research: &[GasSample],
    policy: &MarginPolicy,
    min_block_gas_limit: u64,
    required_code_hash: &str,
) -> Result<RouteProfile, GasProfileError> {
    route_key.validate_structure()?;

    let research_stats = if research.is_empty() {
        None
    } else {
        Some(DistributionStats::from_sorted(&gas_values(research))?)
    };

    // Reject any qualification sample that is not fork_replay against the pin.
    for s in qualification {
        if s.source != SampleSource::ForkReplay {
            return Ok(RouteProfile {
                research_stats,
                ..unsupported_profile(
                    route_key,
                    format!(
                        "qualification sample has source {:?}; only fork_replay qualifies",
                        s.source
                    ),
                )
            });
        }
        if !hex_eq(&s.executor_code_hash, required_code_hash) {
            return Ok(RouteProfile {
                research_stats,
                ..unsupported_profile(
                    route_key,
                    format!(
                        "sample executor_code_hash {} != required {}",
                        s.executor_code_hash, required_code_hash
                    ),
                )
            });
        }
        if s.gas_used == 0 {
            return Ok(RouteProfile {
                research_stats,
                ..unsupported_profile(route_key, "zero gas_used in qualification sample")
            });
        }
        if s.outcome == Some(SampleOutcome::Reverted) {
            return Ok(RouteProfile {
                research_stats,
                ..unsupported_profile(
                    route_key,
                    "qualification sample outcome is reverted; only successful calls qualify",
                )
            });
        }
    }

    if qualification.len() < policy.min_samples {
        return Ok(RouteProfile {
            research_stats,
            ..unsupported_profile(
                route_key,
                format!(
                    "insufficient qualification samples: {} < min_samples {}",
                    qualification.len(),
                    policy.min_samples
                ),
            )
        });
    }

    let (train, holdout) = split_train_holdout(qualification, policy.holdout_fraction_bps);
    if train.is_empty() {
        return Ok(RouteProfile {
            research_stats,
            ..unsupported_profile(route_key, "empty train split")
        });
    }

    // Stats and limits are derived from the full qualification set (train+holdout
    // together) for published min/p50/p95/p99/max, while the holdout check uses a
    // limit re-derived from train only — so holdout is a true out-of-sample gate.
    let all_sorted = gas_values(qualification);
    let stats = DistributionStats::from_sorted(&all_sorted)?;
    let train_sorted = gas_values(&train);
    let gas_limit = policy.gas_limit_from_sorted(&train_sorted)?;
    let expected_gas_used = policy.expected_from_sorted(&all_sorted)?;

    if gas_limit >= min_block_gas_limit {
        return Ok(RouteProfile {
            research_stats,
            stats: Some(stats),
            expected_gas_used: Some(expected_gas_used),
            gas_limit: Some(gas_limit),
            ..unsupported_profile(
                route_key,
                format!(
                    "derived gas_limit {gas_limit} does not stay strictly below min_block_gas_limit {min_block_gas_limit}"
                ),
            )
        });
    }

    let holdout_vals = gas_values(&holdout);
    let holdout_max = holdout_vals.last().copied().unwrap_or(0);
    let holdout_below_limit = holdout_vals.iter().all(|g| *g < gas_limit);
    let holdout_below_block = holdout_vals.iter().all(|g| *g < min_block_gas_limit);
    let train_below_limit = train_sorted.iter().all(|g| *g < gas_limit);
    let train_below_block = train_sorted.iter().all(|g| *g < min_block_gas_limit);

    let holdout_result = HoldoutResult {
        holdout_count: holdout.len(),
        holdout_max,
        holdout_below_limit,
        train_below_limit,
        holdout_below_block_gas_limit: holdout_below_block,
        train_below_block_gas_limit: train_below_block,
        all_below_limit: holdout_below_limit && train_below_limit,
        all_below_block_gas_limit: holdout_below_block && train_below_block,
    };

    if !holdout_result.all_below_limit || !holdout_result.all_below_block_gas_limit {
        return Ok(RouteProfile {
            research_stats,
            stats: Some(stats),
            expected_gas_used: Some(expected_gas_used),
            gas_limit: Some(gas_limit),
            holdout: Some(holdout_result),
            ..unsupported_profile(
                route_key,
                format!(
                    "holdout/train failed limit gate: holdout_max={holdout_max} gas_limit={gas_limit} min_block={min_block_gas_limit}"
                ),
            )
        });
    }

    Ok(RouteProfile {
        route_key: route_key.clone(),
        status: ProfileStatus::Approved,
        reason: None,
        stats: Some(stats),
        expected_gas_used: Some(expected_gas_used),
        gas_limit: Some(gas_limit),
        holdout: Some(holdout_result),
        research_stats,
    })
}

/// Compare 0x-optional hex strings case-insensitively (codehash, digests, …).
fn hex_eq(a: &str, b: &str) -> bool {
    normalize_hex(a) == normalize_hex(b)
}

fn normalize_hex(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("0x").unwrap_or(s);
    s.to_ascii_lowercase()
}

/// Generate a full artifact from config + samples. Deterministic for identical inputs.
pub fn generate_artifact(
    config: &GeneratorConfig,
    samples: &[GasSample],
) -> Result<GasProfileArtifact, GasProfileError> {
    if config.schema_version != GAS_PROFILE_SCHEMA_VERSION {
        return Err(GasProfileError::Config(format!(
            "schema_version {} != supported {}",
            config.schema_version, GAS_PROFILE_SCHEMA_VERSION
        )));
    }
    if config.chain_id == 0 {
        return Err(GasProfileError::Config("chain_id must be non-zero".into()));
    }
    if config.executor_code_hash.is_empty() || config.executor_abi_digest.is_empty() {
        return Err(GasProfileError::Config(
            "executor_code_hash and executor_abi_digest are required".into(),
        ));
    }
    if config.fee_analysis.observations.is_empty() {
        return Err(GasProfileError::Config(
            "fee_analysis.observations must not be empty".into(),
        ));
    }
    if !config
        .fee_analysis
        .notes
        .to_ascii_lowercase()
        .contains("not a permanent")
        && !config
            .fee_analysis
            .notes
            .to_ascii_lowercase()
            .contains("not permanent")
    {
        return Err(GasProfileError::Config(
            "fee_analysis.notes must state that current stability is not a permanent constant"
                .into(),
        ));
    }

    let min_block_gas_limit = config.fee_analysis.min_block_gas_limit;
    if min_block_gas_limit == 0 {
        return Err(GasProfileError::Config(
            "fee_analysis.min_block_gas_limit must be > 0".into(),
        ));
    }

    // Index samples by route key.
    let mut qual_by_key: BTreeMap<RouteKey, Vec<GasSample>> = BTreeMap::new();
    let mut research_by_key: BTreeMap<RouteKey, Vec<GasSample>> = BTreeMap::new();
    let mut qualification_sample_count = 0usize;
    let mut research_historical_sample_count = 0usize;
    let mut research_revert_sample_count = 0usize;
    let mut foundry_mock_sample_count = 0usize;

    for s in samples {
        s.route_key.validate_structure()?;
        match s.source {
            SampleSource::ForkReplay => {
                qualification_sample_count += 1;
                qual_by_key
                    .entry(s.route_key.clone())
                    .or_default()
                    .push(s.clone());
            }
            SampleSource::ResearchHistorical => {
                research_historical_sample_count += 1;
                research_by_key
                    .entry(s.route_key.clone())
                    .or_default()
                    .push(s.clone());
            }
            SampleSource::ResearchRevert => {
                // Intentionally not mixed into success limits; counted for the artifact.
                research_revert_sample_count += 1;
            }
            SampleSource::FoundryMock => {
                // Synthetic mock-pool measurement; never qualifies or informs research
                // stats — counted separately so mock coverage stays visible in the artifact.
                foundry_mock_sample_count += 1;
            }
        }
    }

    // Active classes must be unique.
    let mut seen = BTreeSet::new();
    for rk in &config.active_route_classes {
        rk.validate_structure()?;
        if !seen.insert(rk.clone()) {
            return Err(GasProfileError::Config(format!(
                "duplicate active route class {}",
                rk.key_string()
            )));
        }
    }

    let mut profiles = Vec::with_capacity(config.active_route_classes.len());
    for rk in &config.active_route_classes {
        let q = qual_by_key.get(rk).map(Vec::as_slice).unwrap_or(&[]);
        let r = research_by_key.get(rk).map(Vec::as_slice).unwrap_or(&[]);
        profiles.push(build_route_profile(
            rk,
            q,
            r,
            &config.margin_policy,
            min_block_gas_limit,
            &config.executor_code_hash,
        )?);
    }

    // Sort profiles by route key for determinism.
    profiles.sort_by(|a, b| a.route_key.cmp(&b.route_key));

    let crossing_bucket_evidence = build_crossing_bucket_evidence(&qual_by_key);

    let mut artifact = GasProfileArtifact {
        schema_version: config.schema_version,
        tool_version: config.tool_version.clone(),
        chain_id: config.chain_id,
        executor_code_hash: config.executor_code_hash.clone(),
        executor_abi_digest: config.executor_abi_digest.clone(),
        fee_analysis: config.fee_analysis.clone(),
        margin_policy: config.margin_policy.clone(),
        margin_policy_description: config.margin_policy.describe(),
        sampling_policy: config.sampling_policy.clone(),
        route_key_definition: "ordered_protocols+hop_count+optional(v3_tick_crossings,moe_bin_crossings); \
             V3/Moe buckets required when protocol present; \
             labels ticks={0|1-5|6-20|21+} bins={0|1-3|4-10|11+}"
            .into(),
        profiles,
        crossing_bucket_evidence,
        qualification_sample_count,
        research_historical_sample_count,
        research_revert_sample_count,
        foundry_mock_sample_count,
        replacement_overhead_notes: config.replacement_overhead_notes.clone(),
        content_digest: String::new(),
    };
    artifact.content_digest = compute_content_digest(&artifact)?;
    validate_artifact(&artifact)?;
    Ok(artifact)
}

fn build_crossing_bucket_evidence(
    qual_by_key: &BTreeMap<RouteKey, Vec<GasSample>>,
) -> CrossingBucketEvidence {
    let mut v3_bucket_max_by_label: BTreeMap<String, u64> = BTreeMap::new();
    let mut moe_bucket_max_by_label: BTreeMap<String, u64> = BTreeMap::new();

    for (k, samples) in qual_by_key {
        if let Some(t) = k.v3_tick_crossings {
            let max = samples.iter().map(|s| s.gas_used).max().unwrap_or(0);
            v3_bucket_max_by_label
                .entry(t.as_str().to_string())
                .and_modify(|m| *m = (*m).max(max))
                .or_insert(max);
        }
        if let Some(b) = k.moe_bin_crossings {
            let max = samples.iter().map(|s| s.gas_used).max().unwrap_or(0);
            moe_bucket_max_by_label
                .entry(b.as_str().to_string())
                .and_modify(|m| *m = (*m).max(max))
                .or_insert(max);
        }
    }

    CrossingBucketEvidence {
        v3_tick_conclusion: crossing_bucket_conclusion("V3 tick", "tick", &v3_bucket_max_by_label),
        moe_bin_conclusion: crossing_bucket_conclusion("Moe bin", "bin", &moe_bucket_max_by_label),
        v3_bucket_max_by_label,
        moe_bucket_max_by_label,
    }
}

fn crossing_bucket_conclusion(
    kind: &str,
    unit: &str,
    bucket_max_by_label: &BTreeMap<String, u64>,
) -> String {
    match bucket_max_by_label.len() {
        0 => format!(
            "No {kind}-crossing qualification samples in this artifact; {unit} buckets still required on matching route keys. \
             Multi-modality is neither measured nor disproved here — deep buckets remain explicit Unsupported until Mantle state-fork evidence exists."
        ),
        1 => format!(
            "{kind}-crossing key present; only one bucket has qualification samples. \
             Multi-modality across deep {unit} crossings is not disproved — those buckets stay in the key and remain Unsupported \
             until state-fork distribution evidence is recorded (see docs/DEFERRED_ISSUES.md)."
        ),
        _ => {
            let vals: Vec<u64> = bucket_max_by_label.values().copied().collect();
            let min_b = vals.iter().copied().min().unwrap_or(0);
            let max_b = vals.iter().copied().max().unwrap_or(0);
            if max_b > min_b.saturating_mul(12).div_ceil(10).max(min_b + 50_000) {
                format!(
                    "{kind}-crossing buckets are REQUIRED: qualification maxima differ materially across labels; \
                     a hop-only key would under-limit deep {unit} paths."
                )
            } else {
                format!(
                    "{kind}-crossing buckets retained; observed maxima do not yet prove multi-modality but \
                     buckets stay in the key to avoid silent under-limit on deep crossings."
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Digest / canonical JSON
// ---------------------------------------------------------------------------

/// Content digest: `keccak256` of canonical JSON with `content_digest` cleared.
pub fn compute_content_digest(artifact: &GasProfileArtifact) -> Result<String, GasProfileError> {
    let mut for_hash = artifact.clone();
    for_hash.content_digest.clear();
    let bytes = canonical_json_bytes(&for_hash)?;
    let hash = keccak256(bytes);
    Ok(format!("0x{}", bytes_to_hex(hash.as_slice())))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, GasProfileError> {
    let v = serde_json::to_value(value).map_err(|e| GasProfileError::Json(e.to_string()))?;
    let canon = sort_json(v);
    serde_json::to_vec(&canon).map_err(|e| GasProfileError::Json(e.to_string()))
}

fn sort_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                if let Some(v) = map.get(&k) {
                    out.insert(k, sort_json(v.clone()));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.into_iter().map(sort_json).collect())
        }
        other => other,
    }
}

/// ABI digest: keccak256 of canonical JSON ABI array.
pub fn abi_digest_from_json(abi: &serde_json::Value) -> Result<String, GasProfileError> {
    let bytes = {
        let canon = sort_json(abi.clone());
        serde_json::to_vec(&canon).map_err(|e| GasProfileError::Json(e.to_string()))?
    };
    let hash = keccak256(bytes);
    Ok(format!("0x{}", bytes_to_hex(hash.as_slice())))
}

pub fn abi_digest_from_path(path: &Path) -> Result<String, GasProfileError> {
    let raw = fs::read_to_string(path).map_err(|e| GasProfileError::Io(e.to_string()))?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| GasProfileError::Json(e.to_string()))?;
    abi_digest_from_json(&v)
}

// ---------------------------------------------------------------------------
// Validation / lookup
// ---------------------------------------------------------------------------

pub fn validate_artifact(artifact: &GasProfileArtifact) -> Result<(), GasProfileError> {
    if artifact.schema_version != GAS_PROFILE_SCHEMA_VERSION {
        return Err(GasProfileError::Validation(format!(
            "unsupported schema_version {}",
            artifact.schema_version
        )));
    }
    let expected = compute_content_digest(artifact)?;
    if !hex_eq(&artifact.content_digest, &expected) {
        return Err(GasProfileError::Validation(format!(
            "content_digest mismatch: artifact={} computed={}",
            artifact.content_digest, expected
        )));
    }
    if artifact.profiles.is_empty() {
        return Err(GasProfileError::Validation(
            "artifact has no route profiles".into(),
        ));
    }
    let mut keys = BTreeSet::new();
    for p in &artifact.profiles {
        p.route_key.validate_structure()?;
        if !keys.insert(p.route_key.clone()) {
            return Err(GasProfileError::Validation(format!(
                "duplicate profile key {}",
                p.route_key.key_string()
            )));
        }
        match p.status {
            ProfileStatus::Approved => {
                let gl = p.gas_limit.ok_or_else(|| {
                    GasProfileError::Validation("approved profile missing gas_limit".into())
                })?;
                let exp = p.expected_gas_used.ok_or_else(|| {
                    GasProfileError::Validation("approved profile missing expected_gas_used".into())
                })?;
                if gl <= exp {
                    return Err(GasProfileError::Validation(format!(
                        "gas_limit {gl} must be > expected_gas_used {exp} for {}",
                        p.route_key.key_string()
                    )));
                }
                if gl >= artifact.fee_analysis.min_block_gas_limit {
                    return Err(GasProfileError::Validation(format!(
                        "gas_limit {gl} not below min_block_gas_limit {}",
                        artifact.fee_analysis.min_block_gas_limit
                    )));
                }
                let h = p.holdout.as_ref().ok_or_else(|| {
                    GasProfileError::Validation("approved profile missing holdout".into())
                })?;
                if !h.all_below_limit || !h.all_below_block_gas_limit {
                    return Err(GasProfileError::Validation(
                        "approved profile failed holdout gates".into(),
                    ));
                }
            }
            ProfileStatus::Unsupported | ProfileStatus::ResearchOnly => {
                // ok — explicit non-production outcomes
            }
        }
    }
    Ok(())
}

/// Production lookup: unknown class fails closed (no generic limit).
pub fn lookup_gas(
    artifact: &GasProfileArtifact,
    route_key: &RouteKey,
) -> Result<GasQuote, GasProfileError> {
    route_key.validate_structure()?;
    let profile = artifact
        .profiles
        .iter()
        .find(|p| &p.route_key == route_key)
        .ok_or_else(|| GasProfileError::UnknownRouteClass(route_key.key_string()))?;

    match profile.status {
        ProfileStatus::Approved => Ok(GasQuote {
            route_key: route_key.clone(),
            gas_limit: profile.gas_limit.ok_or_else(|| {
                GasProfileError::Validation("approved missing gas_limit".into())
            })?,
            expected_gas_used: profile.expected_gas_used.ok_or_else(|| {
                GasProfileError::Validation("approved missing expected_gas_used".into())
            })?,
            profile_identity: format!(
                "{}:{}:{}",
                artifact.content_digest,
                route_key.key_string(),
                artifact.executor_code_hash
            ),
        }),
        ProfileStatus::Unsupported => Err(GasProfileError::UnsupportedRoute(
            profile
                .reason
                .clone()
                .unwrap_or_else(|| route_key.key_string()),
        )),
        ProfileStatus::ResearchOnly => Err(GasProfileError::NotApproved(format!(
            "research_only: {}",
            route_key.key_string()
        ))),
    }
}

// ---------------------------------------------------------------------------
// IO helpers
// ---------------------------------------------------------------------------

pub fn load_generator_config(path: &Path) -> Result<GeneratorConfig, GasProfileError> {
    let raw = fs::read_to_string(path).map_err(|e| GasProfileError::Io(e.to_string()))?;
    serde_json::from_str(&raw).map_err(|e| GasProfileError::Json(e.to_string()))
}

pub fn load_samples_jsonl(path: &Path) -> Result<Vec<GasSample>, GasProfileError> {
    let file = fs::File::open(path).map_err(|e| GasProfileError::Io(e.to_string()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| GasProfileError::Io(e.to_string()))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let sample: GasSample = serde_json::from_str(line).map_err(|e| {
            GasProfileError::Json(format!("samples line {}: {}", i + 1, e))
        })?;
        out.push(sample);
    }
    Ok(out)
}

pub fn write_artifact(path: &Path, artifact: &GasProfileArtifact) -> Result<(), GasProfileError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| GasProfileError::Io(e.to_string()))?;
    }
    let json = serde_json::to_string_pretty(artifact)
        .map_err(|e| GasProfileError::Json(e.to_string()))?;
    let mut f = fs::File::create(path).map_err(|e| GasProfileError::Io(e.to_string()))?;
    f.write_all(json.as_bytes())
        .map_err(|e| GasProfileError::Io(e.to_string()))?;
    f.write_all(b"\n")
        .map_err(|e| GasProfileError::Io(e.to_string()))?;
    Ok(())
}

pub fn load_artifact(path: &Path) -> Result<GasProfileArtifact, GasProfileError> {
    let raw = fs::read_to_string(path).map_err(|e| GasProfileError::Io(e.to_string()))?;
    let artifact: GasProfileArtifact =
        serde_json::from_str(&raw).map_err(|e| GasProfileError::Json(e.to_string()))?;
    validate_artifact(&artifact)?;
    Ok(artifact)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod gas_profile_tests {
    use super::*;

    fn codehash() -> String {
        WHI501_EXECUTOR_CODEHASH.into()
    }

    fn v2_key(hops: u8) -> RouteKey {
        RouteKey {
            protocols: vec![ProtocolKind::V2; hops as usize],
            hop_count: hops,
            v3_tick_crossings: None,
            moe_bin_crossings: None,
        }
    }

    fn v3_key(hops: u8, ticks: TickCrossingBucket) -> RouteKey {
        RouteKey {
            protocols: vec![ProtocolKind::V3; hops as usize],
            hop_count: hops,
            v3_tick_crossings: Some(ticks),
            moe_bin_crossings: None,
        }
    }

    fn moe_key(hops: u8, bins: BinCrossingBucket) -> RouteKey {
        RouteKey {
            protocols: vec![ProtocolKind::Moe; hops as usize],
            hop_count: hops,
            v3_tick_crossings: None,
            moe_bin_crossings: Some(bins),
        }
    }

    fn qual_sample(route: RouteKey, gas: u64, block: u64) -> GasSample {
        GasSample {
            route_key: route,
            gas_used: gas,
            source: SampleSource::ForkReplay,
            executor_code_hash: codehash(),
            chain_id: MANTLE_MAINNET_CHAIN_ID,
            block_number: block,
            block_hash: Some(format!("0x{:064x}", block)),
            tx_hash: Some(format!("0x{:064x}", block * 3 + gas)),
            effective_gas_price_wei: Some(50_000_000_000 + 1_000_000),
            base_fee_wei: Some(50_000_000_000),
            block_gas_limit: Some(60_000_000),
            inclusion_latency_blocks: Some(1),
            notes: Some("unit-test fixture".into()),
            venues: None,
            calldata_digest: None,
            outcome: Some(SampleOutcome::Success),
        }
    }

    fn fee_analysis() -> FeeAnalysis {
        FeeAnalysis {
            start_block: 97_158_262,
            end_block: 98_158_262,
            start_block_hash: None,
            end_block_hash: None,
            observation_count: 2,
            min_base_fee_wei: 50_000_000_000,
            max_base_fee_wei: 50_000_000_000,
            min_block_gas_limit: 60_000_000,
            max_block_gas_limit: 60_000_000,
            notes: "2026-07-19 preliminary read-only sample: base fee and block gas limit were stable \
                    across consecutive and spaced blocks; this is not a permanent protocol constant \
                    and runtime must still read current header values."
                .into(),
            observations: vec![
                FeeObservation {
                    block_number: 98_121_659,
                    block_hash: Some(
                        "0xc44bbf683065f56d14cfa1abf20fbdafdcc39e45e8071ea30e603a95228469ed"
                            .into(),
                    ),
                    base_fee_wei: 50_000_000_000,
                    block_gas_limit: 60_000_000,
                    effective_priority_fee_wei: None,
                    inclusion_latency_blocks: None,
                },
                FeeObservation {
                    block_number: 97_158_262,
                    block_hash: None,
                    base_fee_wei: 50_000_000_000,
                    block_gas_limit: 60_000_000,
                    effective_priority_fee_wei: None,
                    inclusion_latency_blocks: None,
                },
            ],
        }
    }

    fn policy() -> MarginPolicy {
        MarginPolicy {
            name: "test_tail_20pct".into(),
            expected_percentile: 50,
            margin_bps: 2000,
            absolute_overhead: 50_000,
            min_samples: 10,
            holdout_fraction_bps: 2000,
        }
    }

    fn base_config(routes: Vec<RouteKey>) -> GeneratorConfig {
        GeneratorConfig {
            chain_id: MANTLE_MAINNET_CHAIN_ID,
            schema_version: GAS_PROFILE_SCHEMA_VERSION,
            tool_version: GAS_PROFILE_TOOL_VERSION.into(),
            executor_code_hash: codehash(),
            executor_abi_digest: "0x".to_owned() + &"ab".repeat(32),
            margin_policy: policy(),
            sampling_policy: SamplingPolicy {
                description: "unit test sampling".into(),
                qualification_executor_code_hash: codehash(),
                qualification_source: SampleSource::ForkReplay,
                research_sources_excluded_from_limits: vec![
                    SampleSource::ResearchHistorical,
                    SampleSource::ResearchRevert,
                ],
            },
            active_route_classes: routes,
            fee_analysis: fee_analysis(),
            replacement_overhead_notes: Some(
                "callback provenance + safe-transfer + role/deadline/minProfit overhead included \
                 in fork_replay fixtures; Base 1.24M tick sample is not imported as Mantle evidence."
                    .into(),
            ),
        }
    }

    fn samples_for(route: &RouteKey, gases: &[u64]) -> Vec<GasSample> {
        gases
            .iter()
            .enumerate()
            .map(|(i, g)| qual_sample(route.clone(), *g, 98_000_000 + i as u64))
            .collect()
    }

    #[test]
    fn percentile_nearest_rank_known_values() {
        let s = [10, 20, 30, 40, 50];
        assert_eq!(percentile_sorted(&s, 0), 10);
        assert_eq!(percentile_sorted(&s, 50), 30);
        assert_eq!(percentile_sorted(&s, 100), 50);
        // p95 on 5 elements → rank ceil(0.95*5)=5 → 50
        assert_eq!(percentile_sorted(&s, 95), 50);
    }

    #[test]
    fn margin_policy_uses_tail_not_average() {
        // Average would be ~100k; tail max=200k → limit = ceil(200k*1.2)+50k = 290k
        let sorted = [80_000u64, 90_000, 100_000, 110_000, 200_000];
        let p = policy();
        let limit = p.gas_limit_from_sorted(&sorted).unwrap();
        let expected = mul_div_ceil(200_000, 12_000, 10_000).unwrap() + 50_000;
        assert_eq!(limit, expected);
        assert!(limit > 200_000);
        // Must not be average * 1.2
        let avg_times = 116_000u64; // rough average * 1.2
        assert!(limit > avg_times);
    }

    #[test]
    fn historical_samples_cannot_qualify_production() {
        let route = v2_key(2);
        let mut samples = samples_for(&route, &[100_000; 12]);
        for s in &mut samples {
            s.source = SampleSource::ResearchHistorical;
            s.executor_code_hash = "0xdead".into();
        }
        let profile = build_route_profile(
            &route,
            &samples,
            &[],
            &policy(),
            60_000_000,
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Unsupported);
        assert!(profile
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("fork_replay"));
    }

    #[test]
    fn wrong_codehash_cannot_qualify() {
        let route = v2_key(2);
        let mut samples = samples_for(&route, &[100_000; 12]);
        for s in &mut samples {
            s.executor_code_hash = "0x00".to_owned() + &"11".repeat(31);
        }
        let profile = build_route_profile(
            &route,
            &samples,
            &[],
            &policy(),
            60_000_000,
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Unsupported);
    }

    #[test]
    fn approved_profile_separates_expected_and_limit() {
        let route = v2_key(2);
        // 12 samples: mostly ~150k, one tail 220k
        let mut gases = vec![140_000u64, 145_000, 148_000, 150_000, 151_000, 152_000];
        gases.extend_from_slice(&[153_000, 155_000, 158_000, 160_000, 165_000, 220_000]);
        let samples = samples_for(&route, &gases);
        let profile = build_route_profile(
            &route,
            &samples,
            &[],
            &policy(),
            60_000_000,
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Approved);
        let exp = profile.expected_gas_used.unwrap();
        let lim = profile.gas_limit.unwrap();
        assert!(lim > exp);
        assert!(lim < 60_000_000);
        let stats = profile.stats.unwrap();
        assert_eq!(stats.sample_count, 12);
        assert_eq!(stats.max, 220_000);
        assert!(profile.holdout.unwrap().all_below_limit);
    }

    #[test]
    fn insufficient_samples_are_explicitly_unsupported() {
        let route = v2_key(2);
        let samples = samples_for(&route, &[100_000; 3]);
        let profile = build_route_profile(
            &route,
            &samples,
            &[],
            &policy(),
            60_000_000,
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Unsupported);
        assert!(profile
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("insufficient"));
    }

    #[test]
    fn limit_above_block_gas_rejected() {
        let route = v2_key(2);
        // Extreme gas that with margin exceeds a tiny block limit
        let samples = samples_for(&route, &[9_000_000u64; 12]);
        let profile = build_route_profile(
            &route,
            &samples,
            &[],
            &policy(),
            10_000_000, // min block gas limit
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Unsupported);
        assert!(profile
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("min_block_gas_limit"));
    }

    #[test]
    fn unknown_route_lookup_fails_closed() {
        let route = v2_key(2);
        let samples = samples_for(&route, &[150_000; 12]);
        let cfg = base_config(vec![route.clone()]);
        let artifact = generate_artifact(&cfg, &samples).unwrap();
        let other = v2_key(3);
        let err = lookup_gas(&artifact, &other).unwrap_err();
        assert!(matches!(err, GasProfileError::UnknownRouteClass(_)));
    }

    #[test]
    fn digest_is_stable_for_identical_inputs() {
        let route = v2_key(2);
        let samples = samples_for(&route, &[150_000; 12]);
        let cfg = base_config(vec![route]);
        let a1 = generate_artifact(&cfg, &samples).unwrap();
        let a2 = generate_artifact(&cfg, &samples).unwrap();
        assert_eq!(a1.content_digest, a2.content_digest);
        assert!(!a1.content_digest.is_empty());
        assert!(a1.content_digest.starts_with("0x"));
    }

    #[test]
    fn digest_changes_when_samples_change() {
        let route = v2_key(2);
        let s1 = samples_for(&route, &[150_000; 12]);
        let s2 = samples_for(&route, &[151_000; 12]);
        let cfg = base_config(vec![route]);
        let a1 = generate_artifact(&cfg, &s1).unwrap();
        let a2 = generate_artifact(&cfg, &s2).unwrap();
        assert_ne!(a1.content_digest, a2.content_digest);
    }

    #[test]
    fn v3_and_moe_buckets_required_on_keys() {
        let bad_v3 = RouteKey {
            protocols: vec![ProtocolKind::V3, ProtocolKind::V3],
            hop_count: 2,
            v3_tick_crossings: None,
            moe_bin_crossings: None,
        };
        assert!(bad_v3.validate_structure().is_err());
        let bad_moe = RouteKey {
            protocols: vec![ProtocolKind::Moe],
            hop_count: 1,
            v3_tick_crossings: None,
            moe_bin_crossings: None,
        };
        assert!(bad_moe.validate_structure().is_err());
        assert!(v3_key(2, TickCrossingBucket::Low).validate_structure().is_ok());
        assert!(moe_key(2, BinCrossingBucket::Mid).validate_structure().is_ok());
    }

    #[test]
    fn crossing_bucket_evidence_records_material_spread() {
        let low = v3_key(2, TickCrossingBucket::Zero);
        let high = v3_key(2, TickCrossingBucket::High);
        let mut samples = samples_for(&low, &[200_000; 12]);
        samples.extend(samples_for(&high, &[1_200_000; 12]));
        let cfg = base_config(vec![low, high]);
        let artifact = generate_artifact(&cfg, &samples).unwrap();
        assert!(artifact
            .crossing_bucket_evidence
            .v3_tick_conclusion
            .contains("REQUIRED"));
        assert!(artifact
            .crossing_bucket_evidence
            .v3_bucket_max_by_label
            .contains_key("0"));
        assert!(artifact
            .crossing_bucket_evidence
            .v3_bucket_max_by_label
            .contains_key("21+"));
    }

    #[test]
    fn reverts_are_not_mixed_into_limits() {
        let route = v2_key(2);
        let mut samples = samples_for(&route, &[150_000; 12]);
        // Add a huge "revert" observation that must not inflate limits.
        samples.push(GasSample {
            route_key: route.clone(),
            gas_used: 50_000_000,
            source: SampleSource::ResearchRevert,
            executor_code_hash: codehash(),
            chain_id: MANTLE_MAINNET_CHAIN_ID,
            block_number: 99,
            block_hash: None,
            tx_hash: None,
            effective_gas_price_wei: None,
            base_fee_wei: None,
            block_gas_limit: Some(60_000_000),
            inclusion_latency_blocks: None,
            notes: Some("revert".into()),
            venues: None,
            calldata_digest: None,
            outcome: Some(SampleOutcome::Reverted),
        });
        let cfg = base_config(vec![route.clone()]);
        let artifact = generate_artifact(&cfg, &samples).unwrap();
        let p = artifact.profiles.iter().find(|p| p.route_key == route).unwrap();
        assert_eq!(p.status, ProfileStatus::Approved);
        assert!(p.stats.as_ref().unwrap().max < 50_000_000);
    }

    #[test]
    fn fee_notes_must_disclaim_permanent_constant() {
        let route = v2_key(2);
        let samples = samples_for(&route, &[150_000; 12]);
        let mut cfg = base_config(vec![route]);
        cfg.fee_analysis.notes = "base fee is 50 gwei forever".into();
        let err = generate_artifact(&cfg, &samples).unwrap_err();
        assert!(matches!(err, GasProfileError::Config(_)));
    }

    #[test]
    fn tick_and_bin_bucket_from_crossings() {
        assert_eq!(TickCrossingBucket::from_crossings(0), TickCrossingBucket::Zero);
        assert_eq!(TickCrossingBucket::from_crossings(3), TickCrossingBucket::Low);
        assert_eq!(TickCrossingBucket::from_crossings(10), TickCrossingBucket::Mid);
        assert_eq!(TickCrossingBucket::from_crossings(100), TickCrossingBucket::High);
        assert_eq!(BinCrossingBucket::from_crossings(0), BinCrossingBucket::Zero);
        assert_eq!(BinCrossingBucket::from_crossings(2), BinCrossingBucket::Low);
        assert_eq!(BinCrossingBucket::from_crossings(7), BinCrossingBucket::Mid);
        assert_eq!(BinCrossingBucket::from_crossings(20), BinCrossingBucket::High);
        assert_eq!(TickCrossingBucket::High.as_str(), "21+");
        assert_eq!(BinCrossingBucket::High.as_str(), "11+");
    }

    #[test]
    fn research_stats_attached_but_not_used_for_approval_limits() {
        let route = v2_key(2);
        let qual = samples_for(&route, &[150_000; 12]);
        let research: Vec<GasSample> = (0..5)
            .map(|i| {
                let mut s = qual_sample(route.clone(), 80_000 + i * 1000, 90_000_000 + i);
                s.source = SampleSource::ResearchHistorical;
                s.executor_code_hash = "0xold".into();
                s
            })
            .collect();
        let profile = build_route_profile(
            &route,
            &qual,
            &research,
            &policy(),
            60_000_000,
            &codehash(),
        )
        .unwrap();
        assert_eq!(profile.status, ProfileStatus::Approved);
        assert!(profile.research_stats.is_some());
        assert_eq!(profile.research_stats.unwrap().sample_count, 5);
        // limit driven by qual (~150k), not research
        assert!(profile.gas_limit.unwrap() > 150_000);
        assert!(profile.gas_limit.unwrap() < 500_000);
    }

    #[test]
    fn pinned_fixture_generation_is_deterministic_and_holdout_safe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let config_path = root.join("config/gas_profiles/pinned/generator_config.json");
        let samples_path = root.join("config/gas_profiles/pinned/samples.jsonl");
        if !config_path.exists() || !samples_path.exists() {
            // Allow crate check without fixtures in exotic packaging contexts.
            return;
        }
        let config = load_generator_config(&config_path).unwrap();
        let samples = load_samples_jsonl(&samples_path).unwrap();
        let a1 = generate_artifact(&config, &samples).unwrap();
        let a2 = generate_artifact(&config, &samples).unwrap();
        assert_eq!(a1.content_digest, a2.content_digest);
        assert_eq!(a1.executor_code_hash, WHI501_EXECUTOR_CODEHASH);
        assert!(!a1.profiles.is_empty());

        let approved: Vec<_> = a1
            .profiles
            .iter()
            .filter(|p| p.status == ProfileStatus::Approved)
            .collect();
        assert!(
            !approved.is_empty(),
            "pinned fixtures must produce at least one approved profile"
        );
        for p in &approved {
            let lim = p.gas_limit.unwrap();
            let exp = p.expected_gas_used.unwrap();
            assert!(lim > exp);
            assert!(lim < a1.fee_analysis.min_block_gas_limit);
            let h = p.holdout.as_ref().unwrap();
            assert!(h.all_below_limit);
            assert!(h.all_below_block_gas_limit);
        }
        // Explicit unsupported class present (no silent generic fallback path).
        assert!(a1
            .profiles
            .iter()
            .any(|p| p.status == ProfileStatus::Unsupported));
        // Crossing evidence recorded (single-bucket fixtures still document the gap).
        assert!(!a1.crossing_bucket_evidence.v3_tick_conclusion.is_empty());
        assert!(
            a1.crossing_bucket_evidence
                .v3_tick_conclusion
                .to_ascii_lowercase()
                .contains("bucket")
                || a1.crossing_bucket_evidence.v3_bucket_max_by_label.len() >= 1
        );
        assert!(a1
            .fee_analysis
            .notes
            .to_ascii_lowercase()
            .contains("not a permanent"));
        assert!(a1.fee_analysis.start_block_hash.is_some());
        assert!(a1.fee_analysis.end_block_hash.is_some());
        assert!(a1.research_revert_sample_count >= 1);
        assert!(a1.qualification_sample_count >= 10);

        // Committed artifact (if present) must match pinned regeneration digest.
        let committed = root.join("config/gas_profiles/mantle_mainnet_v1.json");
        if committed.exists() {
            let on_disk = load_artifact(&committed).unwrap();
            assert_eq!(
                on_disk.content_digest, a1.content_digest,
                "committed artifact digest drifted from pinned inputs; regenerate"
            );
        }
    }
}
