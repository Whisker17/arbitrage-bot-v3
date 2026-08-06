//! Load-once, frozen, fingerprinted pool-universe source (WHI-727 / WHI-784 / WHI-793).
//!
//! The live bot never discovers pools from factories. The **unified** pool list
//! is regenerated offline by `cargo run --release --bin universe_gen` and
//! committed as `data/pool_universe.csv` + `.meta.json`; startup only loads +
//! fingerprints it, then fails closed when missing or stale.
//!
//! Legacy per-protocol CSV sources remain for seed/tools; the bot binary uses
//! [`crate::service::unified_universe::UnifiedPoolUniverseSource`].
//!
//! No reload/promotion path (out of scope: M3-10 / WHI-536).

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::amms::moe::pool_list::meta_path_for;
use crate::amms::moe::MoePoolList;
use crate::state_space::{pool_universe_fingerprint, PoolProtocol, PoolUniverseRow};
use alloy::primitives::{Address, B256};
use async_trait::async_trait;
use csv::ReaderBuilder;
use eyre::{eyre, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use crate::service::error::PoolUniverseSourceError;

/// Default max age (blocks) for a pool-universe `snapshot_block` vs chain tip.
///
/// Mantle ~2 s/block ≈ 43_200 blocks/day; 250_000 ≈ ~5.8 days. Operators who
/// need a longer window can raise `--universe-max-age-blocks` / env, but the
/// live binary never falls back to factory discovery to "catch up".
pub const DEFAULT_UNIVERSE_MAX_AGE_BLOCKS: u64 = 250_000;

/// Offline regeneration command for Agni-V3 CSV (`data/poolLists.csv`).
pub const REGENERATE_AGNI_POOL_LIST: &str =
    "cargo run --example list_mantle_agni_pools  # or get_all_agni_pools; commit data/poolLists.csv";

/// Operator guidance when the V2 list is missing (no committed V2 generator yet).
///
/// Default path is `data/poolLists_v2.csv`. Until a V2 offline generator is
/// committed, exclude `agni-v2` from `--protocols` or supply a V2-only CSV.
pub const REGENERATE_V2_POOL_LIST: &str =
    "supply data/poolLists_v2.csv (V2-only; not the Agni list) or drop agni-v2 from --protocols";

/// Offline regeneration command for Moe CSV + meta.
pub const REGENERATE_MOE_POOL_LIST: &str =
    "cargo run --example generate_moe_pool_list  # commits data/poolLists_moe.csv + .meta.json";

/// Result of a load-once universe load.
#[derive(Debug, Clone)]
pub struct LoadedPoolUniverse {
    pub rows: Vec<PoolUniverseRow>,
    pub fingerprint: B256,
    pub addresses: Vec<Address>,
    /// Provenance from companion `.meta.json` when present (required for Moe).
    pub snapshot_block: Option<u64>,
}

/// Load-once pool-universe source.
///
/// Implementations freeze the universe at the first successful load; callers
/// must not attempt hot reload (not supported).
#[async_trait]
pub trait PoolUniverseSource: Send + Sync {
    /// Load CSV (or other) rows and compute a stable fingerprint.
    ///
    /// `chain_id` / `settlement_asset` participate in the fingerprint domain
    /// separation. Live CSV sources do offline structure checks only (Moe via
    /// `MoePoolList::load_path` + required meta). Full on-chain provenance
    /// validation remains available via `MoePoolList::load_and_validate_on_chain`
    /// for offline tooling, not the live bot hot path.
    async fn load(
        &self,
        chain_id: u64,
        settlement_asset: Address,
    ) -> Result<LoadedPoolUniverse, PoolUniverseSourceError>;
}

/// Fail closed when `snapshot_block` is older than `max_age_blocks` behind tip.
///
/// `tip_block < snapshot_block` is accepted (list generated against a tip the
/// runtime has not yet observed) — only lag behind tip is fatal.
pub fn assert_universe_freshness(
    protocol: &str,
    snapshot_block: u64,
    tip_block: u64,
    max_age_blocks: u64,
    regenerate: &str,
) -> Result<(), PoolUniverseSourceError> {
    let age_blocks = tip_block.saturating_sub(snapshot_block);
    if age_blocks > max_age_blocks {
        return Err(PoolUniverseSourceError::Stale {
            protocol: protocol.to_string(),
            snapshot_block,
            tip_block,
            age_blocks,
            max_age_blocks,
            regenerate: regenerate.to_string(),
        });
    }
    Ok(())
}

/// Apply freshness when a snapshot block is known; no-op when absent.
///
/// **Legacy multi-file path only.** The unified universe (WHI-793) must use
/// [`enforce_universe_freshness`] — missing provenance is fail-closed there.
pub fn enforce_freshness_if_present(
    protocol: &str,
    snapshot_block: Option<u64>,
    tip_block: u64,
    max_age_blocks: u64,
    regenerate: &str,
) -> Result<(), PoolUniverseSourceError> {
    if let Some(snapshot) = snapshot_block {
        assert_universe_freshness(protocol, snapshot, tip_block, max_age_blocks, regenerate)?;
    }
    Ok(())
}

/// Unconditional freshness for the unified pool-universe path (WHI-793).
///
/// A universe with no `snapshot_block` fails closed — the asymmetry where
/// Agni lists without meta passed silently is closed on this path.
pub fn enforce_universe_freshness(
    protocol: &str,
    snapshot_block: Option<u64>,
    tip_block: u64,
    max_age_blocks: u64,
    regenerate: &str,
) -> Result<(), PoolUniverseSourceError> {
    let Some(snapshot) = snapshot_block else {
        return Err(PoolUniverseSourceError::MetaMissing {
            protocol: protocol.to_string(),
            path: "(snapshot_block missing on loaded universe)".to_string(),
            regenerate: regenerate.to_string(),
        });
    };
    assert_universe_freshness(protocol, snapshot, tip_block, max_age_blocks, regenerate)
}

/// CSV pool-list source (V2 / V3 shape: `Pair Address` + optional `Protocol` column).
///
/// Factory identity (WHI-910):
/// 1. Per-row `Factory` column when present and non-empty (authoritative).
/// 2. Else [`Self::protocol_factory_resolver`] when the Protocol column maps.
/// 3. Else the constructor fallback [`Self::factory`] (legacy single-factory CSVs).
#[derive(Debug, Clone)]
pub struct CsvPoolUniverseSource {
    pub path: PathBuf,
    /// Protocol tag written into every emitted [`PoolUniverseRow`].
    pub protocol: PoolProtocol,
    /// Fallback factory when the row has no factory column and no resolver hit.
    pub factory: Address,
    /// When set, only rows whose Protocol column matches (case-insensitive
    /// substring) are kept — used by Agni-V3 to filter `data/poolLists.csv`.
    pub protocol_filter: Option<String>,
    /// Optional map of Protocol-column tag → factory (case-insensitive keys).
    /// Used to seed multiple UniV3-family venues from one legacy CSV (WHI-910).
    pub protocol_factory_map: Option<std::collections::BTreeMap<String, Address>>,
    /// Human label for error messages (`agni-v2`, `agni-v3`, …).
    pub protocol_label: String,
    /// Offline regeneration / operator hint embedded in fail-closed errors.
    pub regenerate_hint: String,
}

#[derive(Debug, Deserialize)]
struct CsvPoolRow {
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "Protocol", default)]
    protocol: String,
    /// Optional per-row factory (unified / multi-factory seeds). Empty → fallback.
    #[serde(rename = "Factory", default)]
    factory: String,
    #[serde(rename = "TokenA Address", default)]
    token_a: String,
    #[serde(rename = "TokenB Address", default)]
    token_b: String,
    #[serde(rename = "Token0 Address", default)]
    token0: String,
    #[serde(rename = "Token1 Address", default)]
    token1: String,
}

impl CsvPoolUniverseSource {
    pub fn new(
        path: impl Into<PathBuf>,
        protocol: PoolProtocol,
        factory: Address,
    ) -> Self {
        let path = path.into();
        let protocol_label = match protocol {
            PoolProtocol::UniswapV2 => "agni-v2",
            PoolProtocol::UniswapV3 | PoolProtocol::Agni => "agni-v3",
            PoolProtocol::MoeLb => "moe",
        }
        .to_string();
        let regenerate_hint = match protocol {
            PoolProtocol::UniswapV2 => REGENERATE_V2_POOL_LIST,
            _ => REGENERATE_AGNI_POOL_LIST,
        }
        .to_string();
        Self {
            path,
            protocol,
            factory,
            protocol_filter: None,
            protocol_factory_map: None,
            protocol_label,
            regenerate_hint,
        }
    }

    pub fn with_protocol_filter(mut self, filter: impl Into<String>) -> Self {
        self.protocol_filter = Some(filter.into());
        self
    }

    pub fn with_protocol_label(mut self, label: impl Into<String>) -> Self {
        self.protocol_label = label.into();
        self
    }

    /// Map legacy Protocol-column tags → factory addresses (keys lowercased).
    ///
    /// When set, rows whose Protocol tag is absent from the map are **skipped**
    /// (not stamped with the constructor fallback) so multi-factory seeds never
    /// silently re-attribute a venue.
    pub fn with_protocol_factory_map(
        mut self,
        map: std::collections::BTreeMap<String, Address>,
    ) -> Self {
        let normalized = map
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect();
        self.protocol_factory_map = Some(normalized);
        self
    }

    /// Parse the CSV without computing a fingerprint (test helper / dry load).
    pub fn read_rows(&self) -> Result<Vec<PoolUniverseRow>, PoolUniverseSourceError> {
        if !self.path.exists() {
            return Err(PoolUniverseSourceError::Missing {
                protocol: self.protocol_label.clone(),
                path: self.path.display().to_string(),
                regenerate: self.regenerate_hint.clone(),
            });
        }
        read_csv_rows(
            &self.path,
            self.protocol,
            self.factory,
            self.protocol_filter.as_deref(),
            self.protocol_factory_map.as_ref(),
        )
    }
}

#[async_trait]
impl PoolUniverseSource for CsvPoolUniverseSource {
    async fn load(
        &self,
        chain_id: u64,
        settlement_asset: Address,
    ) -> Result<LoadedPoolUniverse, PoolUniverseSourceError> {
        let rows = self.read_rows()?;
        if rows.is_empty() {
            return Err(PoolUniverseSourceError::Empty {
                protocol: self.protocol_label.clone(),
                path: self.path.display().to_string(),
                regenerate: self.regenerate_hint.clone(),
            });
        }
        let addresses: Vec<Address> = rows.iter().map(|r| r.pool).collect();
        let fingerprint = pool_universe_fingerprint(chain_id, settlement_asset, rows.clone())?;
        // Optional companion meta for Agni lists (not currently committed).
        let snapshot_block = load_optional_snapshot_block(&self.path)?;
        Ok(LoadedPoolUniverse {
            rows,
            fingerprint,
            addresses,
            snapshot_block,
        })
    }
}

fn read_csv_rows(
    path: &Path,
    protocol: PoolProtocol,
    fallback_factory: Address,
    protocol_filter: Option<&str>,
    protocol_factory_map: Option<&std::collections::BTreeMap<String, Address>>,
) -> Result<Vec<PoolUniverseRow>, PoolUniverseSourceError> {
    let mut reader = ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
    let mut rows = Vec::new();
    for result in reader.deserialize::<CsvPoolRow>() {
        let row = result?;
        if let Some(filter) = protocol_filter {
            if !row.protocol.to_lowercase().contains(&filter.to_lowercase()) {
                continue;
            }
        }
        let factory = resolve_row_factory(&row, fallback_factory, protocol_factory_map)?;
        // When a protocol→factory map is active, unmapped protocols are skipped
        // (resolve returns None) so venues are never re-stamped.
        let Some(factory) = factory else {
            continue;
        };
        let pool = row
            .pair_address
            .trim()
            .parse::<Address>()
            .map_err(|e| PoolUniverseSourceError::Other(format!("bad pair address: {e}")))?;
        let token0 = first_address(&[&row.token0, &row.token_a])?;
        let token1 = first_address(&[&row.token1, &row.token_b])?;
        rows.push(PoolUniverseRow {
            protocol,
            factory,
            pool,
            token0,
            token1,
        });
    }
    Ok(rows)
}

/// Resolve factory for one CSV row. `Ok(None)` means "skip this row".
fn resolve_row_factory(
    row: &CsvPoolRow,
    fallback_factory: Address,
    protocol_factory_map: Option<&std::collections::BTreeMap<String, Address>>,
) -> Result<Option<Address>, PoolUniverseSourceError> {
    // 1. Explicit per-row Factory column wins (unified / multi-factory seeds).
    let raw = row.factory.trim();
    if !raw.is_empty() {
        let addr = raw
            .parse::<Address>()
            .map_err(|e| PoolUniverseSourceError::Other(format!("bad factory address: {e}")))?;
        return Ok(Some(addr));
    }
    // 2. Protocol-column map (legacy multi-venue seed).
    if let Some(map) = protocol_factory_map {
        let key = row.protocol.trim().to_ascii_lowercase();
        return Ok(map.get(&key).copied());
    }
    // 3. Constructor fallback (single-factory legacy CSVs).
    Ok(Some(fallback_factory))
}

fn first_address(candidates: &[&str]) -> Result<Address, PoolUniverseSourceError> {
    for raw in candidates {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        return trimmed
            .parse::<Address>()
            .map_err(|e| PoolUniverseSourceError::Other(format!("bad token address: {e}")));
    }
    // Token columns optional in some CSVs; zero placeholder is filled later by
    // on-chain init. Fingerprint identity for those rows is incomplete until
    // tokens are known — callers that need a strict fingerprint must supply
    // tokens or validate on-chain first.
    Ok(Address::ZERO)
}

/// Optional companion meta for Agni CSVs (`{stem}.meta.json` with `snapshot_block`).
///
/// Moe requires meta via [`MoePoolList`]; Agni lists do not yet ship committed
/// meta (WHI-536). When present, callers enforce freshness; when absent, load
/// still succeeds and logs `snapshot_block=None`.
fn load_optional_snapshot_block(csv_path: &Path) -> Result<Option<u64>, PoolUniverseSourceError> {
    let meta_path = meta_path_for(csv_path);
    if !meta_path.exists() {
        return Ok(None);
    }
    #[derive(Deserialize)]
    struct LooseMeta {
        snapshot_block: u64,
    }
    let file = std::fs::File::open(&meta_path)?;
    let meta: LooseMeta = serde_json::from_reader(file)
        .map_err(|e| PoolUniverseSourceError::Other(format!("meta {}: {e}", meta_path.display())))?;
    Ok(Some(meta.snapshot_block))
}

/// Moe pool-list CSV source (`factory,pool,token_x,token_y,...` schema).
///
/// Load-once / frozen — no hot reload (M3-10 out of scope). Requires companion
/// `.meta.json` so staleness can fail closed (WHI-784).
#[derive(Debug, Clone)]
pub struct MoeCsvPoolUniverseSource {
    pub path: PathBuf,
}

impl MoeCsvPoolUniverseSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn read_rows(&self) -> Result<Vec<PoolUniverseRow>, PoolUniverseSourceError> {
        Ok(moe_entries_to_rows(self.load_list()?.entries))
    }

    fn load_list(&self) -> Result<MoePoolList, PoolUniverseSourceError> {
        if !self.path.exists() {
            return Err(PoolUniverseSourceError::Missing {
                protocol: "moe".to_string(),
                path: self.path.display().to_string(),
                regenerate: REGENERATE_MOE_POOL_LIST.to_string(),
            });
        }
        let list = MoePoolList::load_path(&self.path)
            .map_err(|e| PoolUniverseSourceError::Other(e.to_string()))?;
        if list.meta.is_none() {
            return Err(PoolUniverseSourceError::MetaMissing {
                protocol: "moe".to_string(),
                path: meta_path_for(&self.path).display().to_string(),
                regenerate: REGENERATE_MOE_POOL_LIST.to_string(),
            });
        }
        if list.is_empty() {
            return Err(PoolUniverseSourceError::Empty {
                protocol: "moe".to_string(),
                path: self.path.display().to_string(),
                regenerate: REGENERATE_MOE_POOL_LIST.to_string(),
            });
        }
        Ok(list)
    }
}

fn moe_entries_to_rows(
    entries: impl IntoIterator<Item = crate::amms::moe::MoePoolListEntry>,
) -> Vec<PoolUniverseRow> {
    entries
        .into_iter()
        .map(|e| PoolUniverseRow {
            protocol: PoolProtocol::MoeLb,
            factory: e.factory,
            pool: e.pool,
            token0: e.token_x,
            token1: e.token_y,
        })
        .collect()
}

#[async_trait]
impl PoolUniverseSource for MoeCsvPoolUniverseSource {
    async fn load(
        &self,
        chain_id: u64,
        settlement_asset: Address,
    ) -> Result<LoadedPoolUniverse, PoolUniverseSourceError> {
        let list = self.load_list()?;
        let snapshot_block = list.snapshot_block();
        let rows = moe_entries_to_rows(list.entries);
        let addresses: Vec<Address> = rows.iter().map(|r| r.pool).collect();
        let fingerprint = pool_universe_fingerprint(chain_id, settlement_asset, rows.clone())?;
        Ok(LoadedPoolUniverse {
            rows,
            fingerprint,
            addresses,
            snapshot_block,
        })
    }
}

/// Build fingerprint rows from already-loaded AMMs (shared with legacy helper).
pub fn fingerprint_from_amms(
    chain_id: u64,
    settlement_asset: Address,
    factory: Address,
    protocol: PoolProtocol,
    pools: impl IntoIterator<Item = AMM>,
) -> Result<B256> {
    let rows = pools
        .into_iter()
        .map(|pool| {
            let tokens = pool.tokens();
            if tokens.len() != 2 {
                return Err(eyre!(
                    "pool {} does not expose exactly two venue-ordered tokens",
                    pool.address()
                ));
            }
            Ok(PoolUniverseRow {
                protocol,
                factory,
                pool: pool.address(),
                token0: tokens[0],
                token1: tokens[1],
            })
        })
        .collect::<Result<Vec<_>>>()
        .context("building pool-universe rows from AMMs")?;
    pool_universe_fingerprint(chain_id, settlement_asset, rows).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::io::Write;
    use tempfile::NamedTempFile;
    use tempfile::TempDir;

    #[tokio::test]
    async fn csv_source_filters_protocol_and_fingerprints() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "Protocol,Pair Address,TokenA Address,TokenB Address\n\
             Agni,0x0000000000000000000000000000000000000001,0x0000000000000000000000000000000000000002,0x0000000000000000000000000000000000000003\n\
             Other,0x0000000000000000000000000000000000000004,0x0000000000000000000000000000000000000005,0x0000000000000000000000000000000000000006"
        )
        .unwrap();

        let factory = address!("1000000000000000000000000000000000000001");
        let source = CsvPoolUniverseSource::new(file.path(), PoolProtocol::Agni, factory)
            .with_protocol_filter("agni");
        let loaded = source
            .load(5000, address!("00000000000000000000000000000000000000aa"))
            .await
            .unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(
            loaded.rows[0].pool,
            address!("0000000000000000000000000000000000000001")
        );
        assert_ne!(loaded.fingerprint, B256::ZERO);
        assert!(loaded.snapshot_block.is_none());
    }

    #[test]
    fn freshness_accepts_recent_snapshot() {
        assert_universe_freshness("moe", 100, 150, 100, "regen").unwrap();
        assert_universe_freshness("moe", 100, 100, 0, "regen").unwrap();
        // Tip behind snapshot is fine.
        assert_universe_freshness("moe", 200, 100, 0, "regen").unwrap();
    }

    #[test]
    fn freshness_rejects_stale_snapshot_with_regenerate_hint() {
        let err = assert_universe_freshness("moe", 100, 500, 100, REGENERATE_MOE_POOL_LIST)
            .unwrap_err();
        match err {
            PoolUniverseSourceError::Stale {
                protocol,
                snapshot_block,
                tip_block,
                age_blocks,
                max_age_blocks,
                regenerate,
            } => {
                assert_eq!(protocol, "moe");
                assert_eq!(snapshot_block, 100);
                assert_eq!(tip_block, 500);
                assert_eq!(age_blocks, 400);
                assert_eq!(max_age_blocks, 100);
                assert!(regenerate.contains("generate_moe_pool_list"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn missing_csv_fails_closed_with_regenerate_hint() {
        let source = CsvPoolUniverseSource::new(
            "/tmp/whi-784-does-not-exist.csv",
            PoolProtocol::Agni,
            Address::ZERO,
        );
        let err = source
            .load(5000, address!("00000000000000000000000000000000000000aa"))
            .await
            .unwrap_err();
        match err {
            PoolUniverseSourceError::Missing {
                protocol,
                regenerate,
                ..
            } => {
                assert_eq!(protocol, "agni-v3");
                assert!(regenerate.contains("list_mantle_agni_pools"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn moe_source_requires_meta_and_surfaces_snapshot_block() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("poolLists_moe.csv");
        let meta = dir.path().join("poolLists_moe.meta.json");
        // Minimal valid Moe row using the canonical factory.
        std::fs::write(
            &csv,
            "factory,pool,token_x,token_y,bin_step,creation_block\n\
             0xa6630671775c4EA2743840F9A5016dCf2A104054,\
             0x00000000000000000000000000000000000000aa,\
             0x00000000000000000000000000000000000000bb,\
             0x00000000000000000000000000000000000000cc,\
             25,61742961\n",
        )
        .unwrap();

        // Without meta → MetaMissing
        let source = MoeCsvPoolUniverseSource::new(&csv);
        let err = source
            .load(5000, address!("00000000000000000000000000000000000000aa"))
            .await
            .unwrap_err();
        assert!(matches!(err, PoolUniverseSourceError::MetaMissing { .. }));

        std::fs::write(
            &meta,
            r#"{
  "schema_version": 1,
  "factory": "0xa6630671775c4ea2743840f9a5016dcf2a104054",
  "factory_creation_block": 61742960,
  "snapshot_block": 62000000,
  "pool_count": 1
}"#,
        )
        .unwrap();

        let loaded = source
            .load(5000, address!("00000000000000000000000000000000000000aa"))
            .await
            .unwrap();
        assert_eq!(loaded.rows.len(), 1);
        assert_eq!(loaded.snapshot_block, Some(62_000_000));
        assert_ne!(loaded.fingerprint, B256::ZERO);
    }

    #[test]
    fn enforce_freshness_skips_when_snapshot_absent() {
        enforce_freshness_if_present("agni-v3", None, 99_000_000, 1, "regen").unwrap();
    }

    #[test]
    fn enforce_universe_freshness_requires_snapshot() {
        let err = enforce_universe_freshness(
            "unified",
            None,
            99_000_000,
            1,
            crate::service::REGENERATE_POOL_UNIVERSE,
        )
        .unwrap_err();
        assert!(matches!(err, PoolUniverseSourceError::MetaMissing { .. }));
    }

    #[test]
    fn enforce_universe_freshness_rejects_stale() {
        let err = enforce_universe_freshness("unified", Some(100), 500_000, 100, "regen")
            .unwrap_err();
        assert!(matches!(err, PoolUniverseSourceError::Stale { .. }));
    }

    #[test]
    fn csv_preserves_per_row_factory_column() {
        let mut file = NamedTempFile::new().unwrap();
        let f1 = address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035");
        let f2 = address!("530d2766d1988cc1c000c8b7d00334c14b69ad71");
        writeln!(
            file,
            "Protocol,Pair Address,Factory,TokenA Address,TokenB Address\n\
             Agni,0x0000000000000000000000000000000000000001,{f1:?},0x0000000000000000000000000000000000000002,0x0000000000000000000000000000000000000003\n\
             FusionX,0x0000000000000000000000000000000000000004,{f2:?},0x0000000000000000000000000000000000000005,0x0000000000000000000000000000000000000006"
        )
        .unwrap();

        // Fallback must NOT overwrite the per-row Factory column.
        let source = CsvPoolUniverseSource::new(
            file.path(),
            PoolProtocol::Agni,
            address!("1111111111111111111111111111111111111111"),
        );
        let rows = source.read_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].factory, f1);
        assert_eq!(rows[1].factory, f2);
    }

    #[test]
    fn csv_protocol_factory_map_assigns_distinct_factories() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            "Protocol,Pair Address,TokenA Address,TokenB Address\n\
             Agni,0x0000000000000000000000000000000000000001,0x0000000000000000000000000000000000000002,0x0000000000000000000000000000000000000003\n\
             FusionX,0x0000000000000000000000000000000000000004,0x0000000000000000000000000000000000000005,0x0000000000000000000000000000000000000006\n\
             Other,0x0000000000000000000000000000000000000007,0x0000000000000000000000000000000000000008,0x0000000000000000000000000000000000000009"
        )
        .unwrap();

        let f_agni = address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035");
        let f_fx = address!("530d2766d1988cc1c000c8b7d00334c14b69ad71");
        let mut map = std::collections::BTreeMap::new();
        map.insert("agni".into(), f_agni);
        map.insert("fusionx".into(), f_fx);

        let source = CsvPoolUniverseSource::new(
            file.path(),
            PoolProtocol::Agni,
            address!("1111111111111111111111111111111111111111"),
        )
        .with_protocol_factory_map(map);
        let rows = source.read_rows().unwrap();
        assert_eq!(rows.len(), 2, "Other must be skipped when unmapped");
        assert_eq!(rows[0].factory, f_agni);
        assert_eq!(rows[1].factory, f_fx);
        // Fallback must not have been applied to either row.
        assert!(!rows
            .iter()
            .any(|r| r.factory == address!("1111111111111111111111111111111111111111")));
    }
}
