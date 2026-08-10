//! Unified multi-protocol pool-universe CSV + meta sidecar (WHI-793).
//!
//! One file covers Agni-V2 / Agni-V3 / Moe. The live bot loads this path only;
//! companion `.meta.json` is **required** (fail closed — no silent
//! `snapshot_block=None`).

use crate::service::error::PoolUniverseSourceError;
use crate::service::pool_universe::LoadedPoolUniverse;
use crate::service::select::SelectedProtocol;
use crate::service::universe_filter::{
    CandidatePool, FunnelCounts, QuarantineEntry, FILTER_POLICY_VERSION,
};
use crate::service::PoolUniverseSource;
use crate::state_space::{pool_universe_fingerprint, PoolProtocol, PoolUniverseRow};
use alloy::primitives::{Address, B256, U256};
use async_trait::async_trait;
use csv::{ReaderBuilder, WriterBuilder};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Default relative path for the unified pool list.
pub const DEFAULT_POOL_UNIVERSE_REL: &str = "data/pool_universe.csv";

/// Offline regeneration command embedded in fail-closed errors.
pub const REGENERATE_POOL_UNIVERSE: &str =
    "cargo run --release --bin universe_gen  # writes data/pool_universe.csv + .meta.json";

/// Unified schema version for the CSV + meta pair.
pub const UNIFIED_SCHEMA_VERSION: u32 = 1;

/// Companion meta path: `{stem}.meta.json` next to the CSV.
pub fn meta_path_for(csv_path: &Path) -> PathBuf {
    let mut s = csv_path.as_os_str().to_os_string();
    s.push(".meta.json");
    // Prefer `pool_universe.meta.json` over `pool_universe.csv.meta.json`.
    if let Some(stem) = csv_path.file_stem() {
        if let Some(parent) = csv_path.parent() {
            return parent.join(format!("{}.meta.json", stem.to_string_lossy()));
        }
    }
    PathBuf::from(s)
}

/// Quarantine list path next to the CSV.
pub fn quarantine_path_for(csv_path: &Path) -> PathBuf {
    if let Some(stem) = csv_path.file_stem() {
        if let Some(parent) = csv_path.parent() {
            return parent.join(format!("{}.quarantine.json", stem.to_string_lossy()));
        }
    }
    csv_path.with_extension("quarantine.json")
}

/// Meta sidecar for the unified universe (required on the live path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UnifiedUniverseMeta {
    pub schema_version: u32,
    pub chain_id: u64,
    pub snapshot_block: u64,
    /// Block hash at `snapshot_block` (`0x…` hex). Empty string if unknown.
    pub snapshot_hash: String,
    /// Unix timestamp of the pinned block when known.
    #[serde(default)]
    pub timestamp: Option<u64>,
    pub settlement_asset: String,
    pub pool_count: u64,
    pub per_protocol: BTreeMap<String, u64>,
    pub filter_policy: FilterPolicyMeta,
    /// Optional fingerprint hex for operator cross-check (not required by loader).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Observed on-chain arb coverage for this universe fingerprint (WHI-906).
    /// Offline analysis only; the live bot does not require it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_arb_coverage: Option<crate::service::arb_coverage::ObservedArbCoverage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilterPolicyMeta {
    pub version: u32,
    pub max_hops: u8,
    /// Floor in WMNT wei (decimal string).
    pub min_tvl_wmnt_wei: String,
    pub valuation: ValuationMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ValuationMeta {
    /// Always [`crate::service::valuation::VALUATION_METHOD`] on the generator
    /// path — the string lives next to the code that implements it.
    pub method: String,
    pub quote_asset: String,
    pub quote_decimals: u8,
    pub note: String,
}

impl Default for ValuationMeta {
    fn default() -> Self {
        Self {
            method: crate::service::valuation::VALUATION_METHOD.into(),
            quote_asset: "WMNT".into(),
            quote_decimals: 18,
            note: "Floor is WMNT-equivalent (no USD oracle). For a pool that holds WMNT, \
                   TVL ≈ 2 × WMNT-side balance (50/50 value assumption). Otherwise priced \
                   via a direct WMNT pair for either token when available; else quarantined."
                .into(),
        }
    }
}

/// One CSV row in the unified universe.
///
/// Optional venue fields are serialized as empty strings when absent so every
/// row has a fixed column count (csv crate rejects ragged records).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UnifiedCsvRow {
    pub protocol: String,
    pub factory: String,
    pub pool: String,
    pub token0: String,
    pub token1: String,
    #[serde(default, deserialize_with = "empty_as_none_u32", serialize_with = "none_as_empty_u32")]
    pub fee_tier: Option<u32>,
    #[serde(default, deserialize_with = "empty_as_none_u16", serialize_with = "none_as_empty_u16")]
    pub bin_step: Option<u16>,
    #[serde(
        default,
        deserialize_with = "empty_as_none_u64",
        serialize_with = "none_as_empty_u64"
    )]
    pub creation_block: Option<u64>,
}

fn empty_as_none_u32<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        Ok(None)
    } else {
        s.trim().parse().map(Some).map_err(serde::de::Error::custom)
    }
}

fn none_as_empty_u32<S>(v: &Option<u32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        Some(n) => serializer.serialize_str(&n.to_string()),
        None => serializer.serialize_str(""),
    }
}

fn empty_as_none_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        Ok(None)
    } else {
        s.trim().parse().map(Some).map_err(serde::de::Error::custom)
    }
}

fn none_as_empty_u16<S>(v: &Option<u16>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        Some(n) => serializer.serialize_str(&n.to_string()),
        None => serializer.serialize_str(""),
    }
}

fn empty_as_none_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s.trim().is_empty() {
        Ok(None)
    } else {
        s.trim().parse().map(Some).map_err(serde::de::Error::custom)
    }
}

fn none_as_empty_u64<S>(v: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match v {
        Some(n) => serializer.serialize_str(&n.to_string()),
        None => serializer.serialize_str(""),
    }
}

impl UnifiedCsvRow {
    pub fn from_candidate(c: &CandidatePool) -> Self {
        Self {
            protocol: c.protocol.clone(),
            factory: format!("{:?}", c.factory),
            pool: format!("{:?}", c.pool),
            token0: format!("{:?}", c.token0),
            token1: format!("{:?}", c.token1),
            fee_tier: c.fee_tier,
            bin_step: c.bin_step,
            creation_block: c.creation_block,
        }
    }

    pub fn to_candidate(&self) -> Result<CandidatePool, PoolUniverseSourceError> {
        Ok(CandidatePool {
            protocol: self.protocol.clone(),
            factory: parse_addr(&self.factory, "factory")?,
            pool: parse_addr(&self.pool, "pool")?,
            token0: parse_addr(&self.token0, "token0")?,
            token1: parse_addr(&self.token1, "token1")?,
            fee_tier: self.fee_tier,
            bin_step: self.bin_step,
            creation_block: self.creation_block,
        })
    }

    pub fn to_universe_row(&self) -> Result<PoolUniverseRow, PoolUniverseSourceError> {
        let protocol = protocol_label_to_pool_protocol(&self.protocol)?;
        Ok(PoolUniverseRow {
            protocol,
            factory: parse_addr(&self.factory, "factory")?,
            pool: parse_addr(&self.pool, "pool")?,
            token0: parse_addr(&self.token0, "token0")?,
            token1: parse_addr(&self.token1, "token1")?,
        })
    }
}

fn parse_addr(raw: &str, field: &str) -> Result<Address, PoolUniverseSourceError> {
    Address::from_str(raw.trim()).map_err(|e| {
        PoolUniverseSourceError::Other(format!("bad {field} address '{raw}': {e}"))
    })
}

/// Map unified CSV `protocol` column → [`PoolProtocol`].
pub fn protocol_label_to_pool_protocol(
    label: &str,
) -> Result<PoolProtocol, PoolUniverseSourceError> {
    match label.trim().to_ascii_lowercase().as_str() {
        "agni-v2" | "v2" | "uniswap-v2" => Ok(PoolProtocol::UniswapV2),
        "agni-v3" | "agni" | "v3" => Ok(PoolProtocol::Agni),
        "moe" | "moe-lb" | "moelb" => Ok(PoolProtocol::MoeLb),
        other => Err(PoolUniverseSourceError::Other(format!(
            "unknown protocol label '{other}' in unified pool universe \
             (expected agni-v2|agni-v3|moe)"
        ))),
    }
}

/// Map [`SelectedProtocol`] → CSV protocol label.
pub fn selected_to_protocol_label(p: SelectedProtocol) -> &'static str {
    match p {
        SelectedProtocol::AgniV2 => "agni-v2",
        SelectedProtocol::AgniV3 => "agni-v3",
        SelectedProtocol::Moe => "moe",
    }
}

/// Map [`PoolProtocol`] → CSV protocol label (lossy for UniswapV3 → agni-v3).
pub fn pool_protocol_to_label(p: PoolProtocol) -> &'static str {
    match p {
        PoolProtocol::UniswapV2 => "agni-v2",
        PoolProtocol::UniswapV3 | PoolProtocol::Agni => "agni-v3",
        PoolProtocol::MoeLb => "moe",
    }
}

/// Write unified CSV deterministically (sorted by protocol, pool).
pub fn write_unified_csv(
    path: &Path,
    pools: &[CandidatePool],
) -> Result<(), PoolUniverseSourceError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<&CandidatePool> = pools.iter().collect();
    sorted.sort_by(|a, b| {
        (&a.protocol, format!("{:?}", a.pool)).cmp(&(&b.protocol, format!("{:?}", b.pool)))
    });

    let file = File::create(path)?;
    let mut wtr = WriterBuilder::new().from_writer(BufWriter::new(file));
    for c in sorted {
        wtr.serialize(UnifiedCsvRow::from_candidate(c))
            .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    }
    wtr.flush()
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    Ok(())
}

/// Read all rows from a unified CSV (no meta check).
pub fn read_unified_csv(path: &Path) -> Result<Vec<CandidatePool>, PoolUniverseSourceError> {
    if !path.exists() {
        return Err(PoolUniverseSourceError::Missing {
            protocol: "unified".into(),
            path: path.display().to_string(),
            regenerate: REGENERATE_POOL_UNIVERSE.into(),
        });
    }
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    let mut out = Vec::new();
    for (i, result) in reader.deserialize::<UnifiedCsvRow>().enumerate() {
        let row = result.map_err(|e| {
            PoolUniverseSourceError::Other(format!("unified csv row {}: {e}", i + 1))
        })?;
        out.push(row.to_candidate()?);
    }
    Ok(out)
}

/// Load and validate required meta sidecar.
pub fn load_unified_meta(csv_path: &Path) -> Result<UnifiedUniverseMeta, PoolUniverseSourceError> {
    let meta_path = meta_path_for(csv_path);
    if !meta_path.exists() {
        return Err(PoolUniverseSourceError::MetaMissing {
            protocol: "unified".into(),
            path: meta_path.display().to_string(),
            regenerate: REGENERATE_POOL_UNIVERSE.into(),
        });
    }
    let file = File::open(&meta_path)?;
    let meta: UnifiedUniverseMeta = serde_json::from_reader(BufReader::new(file)).map_err(|e| {
        PoolUniverseSourceError::Other(format!("meta {}: {e}", meta_path.display()))
    })?;
    if meta.schema_version == 0 {
        return Err(PoolUniverseSourceError::Other(format!(
            "meta {}: schema_version must be >= 1",
            meta_path.display()
        )));
    }
    Ok(meta)
}

/// Write meta sidecar (pretty JSON, trailing newline).
pub fn write_unified_meta(
    csv_path: &Path,
    meta: &UnifiedUniverseMeta,
) -> Result<(), PoolUniverseSourceError> {
    let meta_path = meta_path_for(csv_path);
    if let Some(parent) = meta_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(&meta_path)?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, meta)
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    w.write_all(b"\n")
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    Ok(())
}

/// Write quarantine list (pretty JSON).
pub fn write_quarantine(
    csv_path: &Path,
    entries: &[QuarantineEntry],
) -> Result<(), PoolUniverseSourceError> {
    #[derive(Serialize)]
    struct Row<'a> {
        pool: String,
        protocol: &'a str,
        reason: &'a str,
    }
    let rows: Vec<Row<'_>> = entries
        .iter()
        .map(|e| Row {
            pool: format!("{:?}", e.pool),
            protocol: &e.protocol,
            reason: &e.reason,
        })
        .collect();
    let path = quarantine_path_for(csv_path);
    let file = File::create(&path)?;
    let mut w = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut w, &rows)
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    w.write_all(b"\n")
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    Ok(())
}

/// Build meta from generation outputs.
pub fn build_meta(
    chain_id: u64,
    snapshot_block: u64,
    snapshot_hash: B256,
    timestamp: Option<u64>,
    settlement: Address,
    kept: &[CandidatePool],
    max_hops: u8,
    min_tvl_wmnt_wei: U256,
    fingerprint: Option<B256>,
) -> UnifiedUniverseMeta {
    let mut per_protocol: BTreeMap<String, u64> = BTreeMap::new();
    for c in kept {
        *per_protocol.entry(c.protocol.clone()).or_insert(0) += 1;
    }
    UnifiedUniverseMeta {
        schema_version: UNIFIED_SCHEMA_VERSION,
        chain_id,
        snapshot_block,
        snapshot_hash: format!("{snapshot_hash:?}"),
        timestamp,
        settlement_asset: format!("{settlement:?}"),
        pool_count: kept.len() as u64,
        per_protocol,
        filter_policy: FilterPolicyMeta {
            version: FILTER_POLICY_VERSION,
            max_hops,
            min_tvl_wmnt_wei: min_tvl_wmnt_wei.to_string(),
            valuation: ValuationMeta::default(),
        },
        fingerprint: fingerprint.map(|f| format!("{f:?}")),
        observed_arb_coverage: None,
    }
}

/// Load-once source for the unified universe (meta required).
#[derive(Debug, Clone)]
pub struct UnifiedPoolUniverseSource {
    pub path: PathBuf,
    /// When set, only rows whose protocol label maps to one of these are kept.
    pub protocol_filter: Option<Vec<SelectedProtocol>>,
}

impl UnifiedPoolUniverseSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            protocol_filter: None,
        }
    }

    pub fn with_protocol_filter(mut self, selected: Vec<SelectedProtocol>) -> Self {
        self.protocol_filter = Some(selected);
        self
    }

    pub fn read_candidates(&self) -> Result<Vec<CandidatePool>, PoolUniverseSourceError> {
        let mut rows = read_unified_csv(&self.path)?;
        if let Some(ref filter) = self.protocol_filter {
            let labels: Vec<&str> = filter.iter().copied().map(selected_to_protocol_label).collect();
            rows.retain(|c| {
                labels.iter().any(|l| {
                    protocol_label_to_pool_protocol(&c.protocol)
                        .ok()
                        .map(|pp| pool_protocol_to_label(pp) == *l)
                        .unwrap_or(false)
                        || c.protocol.eq_ignore_ascii_case(l)
                })
            });
        }
        Ok(rows)
    }
}

#[async_trait]
impl PoolUniverseSource for UnifiedPoolUniverseSource {
    async fn load(
        &self,
        chain_id: u64,
        settlement_asset: Address,
    ) -> Result<LoadedPoolUniverse, PoolUniverseSourceError> {
        let meta = load_unified_meta(&self.path)?;
        if meta.chain_id == 0 {
            return Err(PoolUniverseSourceError::Other(format!(
                "unified universe meta chain_id is 0 (invalid). \
                 Regenerate offline with: {REGENERATE_POOL_UNIVERSE}"
            )));
        }
        if meta.chain_id != chain_id {
            return Err(PoolUniverseSourceError::Other(format!(
                "unified universe chain_id mismatch: meta={} runtime={chain_id}. \
                 Regenerate offline with: {REGENERATE_POOL_UNIVERSE}",
                meta.chain_id
            )));
        }
        if !meta.settlement_asset.trim().is_empty() {
            match Address::from_str(meta.settlement_asset.trim()) {
                Ok(meta_settlement) if meta_settlement != settlement_asset => {
                    return Err(PoolUniverseSourceError::Other(format!(
                        "unified universe settlement mismatch: meta={} runtime={settlement_asset:?}. \
                         Regenerate offline with: {REGENERATE_POOL_UNIVERSE}",
                        meta.settlement_asset
                    )));
                }
                Err(e) => {
                    return Err(PoolUniverseSourceError::Other(format!(
                        "unified universe meta settlement_asset invalid ({}): {e}. \
                         Regenerate offline with: {REGENERATE_POOL_UNIVERSE}",
                        meta.settlement_asset
                    )));
                }
                Ok(_) => {}
            }
        }
        let candidates = self.read_candidates()?;
        if candidates.is_empty() {
            return Err(PoolUniverseSourceError::Empty {
                protocol: "unified".into(),
                path: self.path.display().to_string(),
                regenerate: REGENERATE_POOL_UNIVERSE.into(),
            });
        }
        let rows: Vec<PoolUniverseRow> = candidates
            .iter()
            .map(|c| {
                UnifiedCsvRow::from_candidate(c).to_universe_row()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let addresses: Vec<Address> = rows.iter().map(|r| r.pool).collect();
        let fingerprint = pool_universe_fingerprint(chain_id, settlement_asset, rows.clone())?;
        Ok(LoadedPoolUniverse {
            rows,
            fingerprint,
            addresses,
            snapshot_block: Some(meta.snapshot_block),
        })
    }
}

/// Format funnel counts for operator stdout.
pub fn format_funnel_report(
    funnel: &FunnelCounts,
    per_stage_protocol: &[(String, BTreeMap<String, usize>)],
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "funnel: enumerated={} → tvl_ok={} (tvl_reject={} quarantine={}) → cycle_ok={} (cycle_reject={}) → emitted={}\n",
        funnel.enumerated,
        funnel.tvl_surviving,
        funnel.tvl_rejected,
        funnel.quarantined,
        funnel.cycle_surviving,
        funnel.cycle_rejected,
        funnel.emitted,
    ));
    for (stage, counts) in per_stage_protocol {
        out.push_str(&format!("  {stage}: {:?}\n", counts));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use tempfile::TempDir;

    fn sample_candidate(proto: &str, pool_byte: u8) -> CandidatePool {
        CandidatePool {
            protocol: proto.into(),
            factory: address!("a6630671775c4ea2743840f9a5016dcf2a104054"),
            pool: Address::with_last_byte(pool_byte),
            token0: address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
            token1: address!("201eba5cc46d216ce6dc03f6a759e8e766e956ae"),
            fee_tier: Some(500),
            bin_step: None,
            creation_block: Some(1),
        }
    }

    #[test]
    fn round_trip_csv_and_meta() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let pools = vec![
            sample_candidate("agni-v3", 0x01),
            sample_candidate("moe", 0x02),
        ];
        write_unified_csv(&csv, &pools).unwrap();
        let meta = build_meta(
            5000,
            12_345_678,
            B256::ZERO,
            Some(1_700_000_000),
            address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
            &pools,
            3,
            U256::from(1000u64),
            None,
        );
        write_unified_meta(&csv, &meta).unwrap();

        let loaded = read_unified_csv(&csv).unwrap();
        assert_eq!(loaded.len(), 2);
        // Deterministic sort: agni-v3 before moe
        assert_eq!(loaded[0].protocol, "agni-v3");
        assert_eq!(loaded[1].protocol, "moe");

        let meta2 = load_unified_meta(&csv).unwrap();
        assert_eq!(meta2.snapshot_block, 12_345_678);
        assert_eq!(meta2.pool_count, 2);
    }

    #[tokio::test]
    async fn unified_source_requires_meta() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        write_unified_csv(&csv, &[sample_candidate("agni-v3", 0x01)]).unwrap();

        let source = UnifiedPoolUniverseSource::new(&csv);
        let err = source
            .load(5000, address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"))
            .await
            .unwrap_err();
        assert!(matches!(err, PoolUniverseSourceError::MetaMissing { .. }));
    }

    #[tokio::test]
    async fn unified_source_loads_with_snapshot() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let pools = vec![sample_candidate("agni-v3", 0x01)];
        write_unified_csv(&csv, &pools).unwrap();
        write_unified_meta(
            &csv,
            &build_meta(
                5000,
                99,
                B256::ZERO,
                None,
                address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
                &pools,
                3,
                U256::from(1u64),
                None,
            ),
        )
        .unwrap();

        let loaded = UnifiedPoolUniverseSource::new(&csv)
            .load(5000, address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"))
            .await
            .unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loaded.snapshot_block, Some(99));
        assert_eq!(loaded.rows[0].protocol, PoolProtocol::Agni);
    }

    #[tokio::test]
    async fn unified_source_rejects_zero_chain_id() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let pools = vec![sample_candidate("agni-v3", 0x01)];
        write_unified_csv(&csv, &pools).unwrap();
        let mut meta = build_meta(
            5000,
            99,
            B256::ZERO,
            None,
            address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
            &pools,
            3,
            U256::from(1u64),
            None,
        );
        meta.chain_id = 0;
        write_unified_meta(&csv, &meta).unwrap();
        let err = UnifiedPoolUniverseSource::new(&csv)
            .load(5000, address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("chain_id"));
    }

    #[tokio::test]
    async fn unified_source_rejects_settlement_mismatch() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let pools = vec![sample_candidate("agni-v3", 0x01)];
        write_unified_csv(&csv, &pools).unwrap();
        write_unified_meta(
            &csv,
            &build_meta(
                5000,
                99,
                B256::ZERO,
                None,
                address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
                &pools,
                3,
                U256::from(1u64),
                None,
            ),
        )
        .unwrap();
        let err = UnifiedPoolUniverseSource::new(&csv)
            .load(5000, address!("00000000000000000000000000000000000000aa"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("settlement"));
    }

    #[test]
    fn identical_write_is_byte_stable() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let pools = vec![
            sample_candidate("moe", 0x02),
            sample_candidate("agni-v3", 0x01),
        ];
        write_unified_csv(&csv, &pools).unwrap();
        let a = std::fs::read(&csv).unwrap();
        write_unified_csv(&csv, &pools).unwrap();
        let b = std::fs::read(&csv).unwrap();
        assert_eq!(a, b, "re-write at same inputs must be identical");
    }

    /// WHI-910 AC: a unified universe row's `factory` column survives loading
    /// unchanged — two different factories on the same protocol are preserved.
    #[tokio::test]
    async fn unified_source_preserves_multiple_factories_on_same_protocol() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let f_agni = address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035");
        let f_fusionx = address!("530d2766d1988cc1c000c8b7d00334c14b69ad71");
        let settlement = address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8");
        let pools = vec![
            CandidatePool {
                protocol: "agni-v3".into(),
                factory: f_agni,
                pool: Address::with_last_byte(0x01),
                token0: settlement,
                token1: address!("201eba5cc46d216ce6dc03f6a759e8e766e956ae"),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: Some(1),
            },
            CandidatePool {
                protocol: "agni-v3".into(),
                factory: f_fusionx,
                pool: Address::with_last_byte(0x02),
                token0: settlement,
                token1: address!("201eba5cc46d216ce6dc03f6a759e8e766e956ae"),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: Some(1),
            },
        ];
        write_unified_csv(&csv, &pools).unwrap();
        write_unified_meta(
            &csv,
            &build_meta(
                5000,
                99,
                B256::ZERO,
                None,
                settlement,
                &pools,
                3,
                U256::from(1u64),
                None,
            ),
        )
        .unwrap();

        let loaded = UnifiedPoolUniverseSource::new(&csv)
            .load(5000, settlement)
            .await
            .unwrap();
        assert_eq!(loaded.rows.len(), 2);
        assert_eq!(loaded.rows[0].protocol, PoolProtocol::Agni);
        assert_eq!(loaded.rows[1].protocol, PoolProtocol::Agni);
        let factories: std::collections::HashSet<_> =
            loaded.rows.iter().map(|r| r.factory).collect();
        assert_eq!(factories.len(), 2);
        assert!(factories.contains(&f_agni));
        assert!(factories.contains(&f_fusionx));
    }

    /// WHI-910 / WHI-938: all loadable drop-in V3 factories can appear in one
    /// universe load (Cleopatra CL is quarantined and not in DROP_IN_V3_VENUES).
    #[tokio::test]
    async fn unified_source_loads_all_loadable_drop_in_v3_factories() {
        use crate::service::v3_venues::{drop_in_v3_funnel_counts, DROP_IN_V3_VENUES};

        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        let settlement = address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8");
        let pools: Vec<CandidatePool> = DROP_IN_V3_VENUES
            .iter()
            .enumerate()
            .map(|(i, v)| CandidatePool {
                protocol: "agni-v3".into(),
                factory: v.factory,
                pool: Address::with_last_byte((i as u8).saturating_add(1)),
                token0: settlement,
                token1: address!("201eba5cc46d216ce6dc03f6a759e8e766e956ae"),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: Some(1),
            })
            .collect();
        write_unified_csv(&csv, &pools).unwrap();
        write_unified_meta(
            &csv,
            &build_meta(
                5000,
                99,
                B256::ZERO,
                None,
                settlement,
                &pools,
                3,
                U256::from(1u64),
                None,
            ),
        )
        .unwrap();

        let loaded = UnifiedPoolUniverseSource::new(&csv)
            .load(5000, settlement)
            .await
            .unwrap();
        assert_eq!(loaded.rows.len(), DROP_IN_V3_VENUES.len());
        let candidates: Vec<CandidatePool> = loaded
            .rows
            .iter()
            .map(|r| CandidatePool {
                protocol: "agni-v3".into(),
                factory: r.factory,
                pool: r.pool,
                token0: r.token0,
                token1: r.token1,
                fee_tier: None,
                bin_step: None,
                creation_block: None,
            })
            .collect();
        let counts = drop_in_v3_funnel_counts(&candidates);
        assert_eq!(counts.len(), DROP_IN_V3_VENUES.len());
        for (label, _factory, n) in counts {
            assert_eq!(n, 1, "expected one pool for {label}");
        }
    }
}
