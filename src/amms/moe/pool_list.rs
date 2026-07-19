//! Dedicated Moe LB pool list: parse, offline validate, write, fail-closed load.
//!
//! Schema (M0 stopgap; superseded by M3 cross-protocol manifest):
//! `factory,pool,token_x,token_y,bin_step,creation_block`
//!
//! Companion metadata (recorded canonical snapshot):
//! `data/poolLists_moe.meta.json`

use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{address, Address, B256},
    providers::Provider,
    rpc::types::{Filter, FilterSet, Log},
    sol_types::SolEvent,
};
use futures::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;

use super::{IMoeFactory, IMoeLBPair, MoeFactory};
use crate::amms::factory::AutomatedMarketMakerFactory;

/// Canonical Merchant Moe LB factory on Mantle mainnet.
pub const CANONICAL_MOE_FACTORY: Address = address!("0xa6630671775c4EA2743840F9A5016dCf2A104054");

/// Deployment block of [`CANONICAL_MOE_FACTORY`] (first code present).
pub const CANONICAL_MOE_FACTORY_CREATION_BLOCK: u64 = 61_742_960;

/// Recorded end block used to generate the committed `data/poolLists_moe.csv`.
/// Regeneration without an explicit override must use this block for determinism.
pub const COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK: u64 = 98_172_112;

/// Default relative path under the crate root.
pub const DEFAULT_MOE_POOL_LIST_REL: &str = "data/poolLists_moe.csv";

/// Companion metadata path (same stem + `.meta.json`).
pub const DEFAULT_MOE_POOL_LIST_META_REL: &str = "data/poolLists_moe.meta.json";

/// Mantle public RPC eth_getLogs max range.
pub const MOE_LOG_CHUNK_SIZE: u64 = 10_000;

const ON_CHAIN_VALIDATE_CONCURRENCY: usize = 8;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoePoolListError {
    #[error("Moe pool list not found: {0}")]
    NotFound(String),
    #[error("Moe pool list metadata not found: {0}")]
    MetaNotFound(String),
    #[error("Moe pool list is empty")]
    Empty,
    #[error("failed to read Moe pool list: {0}")]
    Io(String),
    #[error("failed to parse Moe pool list CSV: {0}")]
    Csv(String),
    #[error("failed to parse Moe pool list metadata: {0}")]
    Meta(String),
    #[error("failed to decode LBPairCreated log: {0}")]
    Decode(String),
    #[error("invalid address in Moe pool list row {row}: {value}")]
    InvalidAddress { row: usize, value: String },
    #[error("invalid bin_step in Moe pool list row {row}: {value}")]
    InvalidBinStep { row: usize, value: String },
    #[error("invalid creation_block in Moe pool list row {row}: {value}")]
    InvalidCreationBlock { row: usize, value: String },
    #[error("duplicate pool address in Moe pool list: {0}")]
    DuplicatePool(Address),
    #[error("non-canonical factory in row for pool {pool}: got {got}, expected {expected}")]
    NonCanonicalFactory {
        pool: Address,
        got: Address,
        expected: Address,
    },
    #[error("zero address in Moe pool list row for pool {0}")]
    ZeroAddress(Address),
    #[error("token_x == token_y for pool {0}")]
    IdenticalTokens(Address),
    #[error("bin_step must be > 0 for pool {0}")]
    ZeroBinStep(Address),
    #[error("creation_block must be > 0 for pool {0}")]
    ZeroCreationBlock(Address),
    #[error("metadata snapshot_block mismatch: meta={meta}, list max creation={max_creation}")]
    SnapshotInconsistent { meta: u64, max_creation: u64 },
    #[error("metadata pool_count mismatch: meta={meta}, list={list}")]
    PoolCountMismatch { meta: u64, list: u64 },
    #[error("metadata factory mismatch: meta={got:?}, expected={expected:?}")]
    MetaFactoryMismatch { got: Address, expected: Address },
    #[error("on-chain provenance mismatch for pool {pool}: {detail}")]
    ProvenanceMismatch { pool: Address, detail: String },
    #[error("RPC/contract error while validating Moe pool list: {0}")]
    Provider(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoePoolListEntry {
    pub factory: Address,
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub bin_step: u16,
    pub creation_block: u64,
}

/// Recorded generation identity for deterministic regeneration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoePoolListMeta {
    pub schema_version: u32,
    pub factory: Address,
    pub factory_creation_block: u64,
    pub snapshot_block: u64,
    pub pool_count: u64,
}

impl MoePoolListMeta {
    pub fn new(factory: Address, factory_creation_block: u64, snapshot_block: u64, pool_count: u64) -> Self {
        Self {
            schema_version: 1,
            factory,
            factory_creation_block,
            snapshot_block,
            pool_count,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MoePoolList {
    pub entries: Vec<MoePoolListEntry>,
    /// Present when loaded/written with companion metadata.
    pub meta: Option<MoePoolListMeta>,
}

impl MoePoolList {
    pub fn new(entries: Vec<MoePoolListEntry>) -> Self {
        Self {
            entries,
            meta: None,
        }
    }

    pub fn with_meta(mut self, meta: MoePoolListMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn snapshot_block(&self) -> Option<u64> {
        self.meta.as_ref().map(|m| m.snapshot_block)
    }

    /// Sort by pool address bytes (lexicographic == lowercase hex order).
    pub fn sort_deterministic(&mut self) {
        self.entries.sort_by_key(|e| e.pool);
    }

    /// Offline structural validation against a required factory.
    pub fn validate_offline(&self, expected_factory: Address) -> Result<(), MoePoolListError> {
        if self.entries.is_empty() {
            return Err(MoePoolListError::Empty);
        }

        let mut seen = HashSet::with_capacity(self.entries.len());
        let mut max_creation = 0u64;
        for entry in &self.entries {
            if !seen.insert(entry.pool) {
                return Err(MoePoolListError::DuplicatePool(entry.pool));
            }
            if entry.factory != expected_factory {
                return Err(MoePoolListError::NonCanonicalFactory {
                    pool: entry.pool,
                    got: entry.factory,
                    expected: expected_factory,
                });
            }
            if entry.pool.is_zero()
                || entry.factory.is_zero()
                || entry.token_x.is_zero()
                || entry.token_y.is_zero()
            {
                return Err(MoePoolListError::ZeroAddress(entry.pool));
            }
            if entry.token_x == entry.token_y {
                return Err(MoePoolListError::IdenticalTokens(entry.pool));
            }
            if entry.bin_step == 0 {
                return Err(MoePoolListError::ZeroBinStep(entry.pool));
            }
            if entry.creation_block == 0 {
                return Err(MoePoolListError::ZeroCreationBlock(entry.pool));
            }
            max_creation = max_creation.max(entry.creation_block);
        }

        if let Some(meta) = &self.meta {
            if meta.factory != expected_factory {
                return Err(MoePoolListError::MetaFactoryMismatch {
                    got: meta.factory,
                    expected: expected_factory,
                });
            }
            if meta.pool_count != self.entries.len() as u64 {
                return Err(MoePoolListError::PoolCountMismatch {
                    meta: meta.pool_count,
                    list: self.entries.len() as u64,
                });
            }
            if meta.snapshot_block < max_creation {
                return Err(MoePoolListError::SnapshotInconsistent {
                    meta: meta.snapshot_block,
                    max_creation,
                });
            }
        }

        Ok(())
    }

    pub fn parse_csv(reader: impl Read) -> Result<Self, MoePoolListError> {
        let mut rdr = csv::ReaderBuilder::new()
            .has_headers(true)
            .trim(csv::Trim::All)
            .from_reader(reader);

        let mut entries = Vec::new();
        for (idx, result) in rdr.deserialize::<CsvRow>().enumerate() {
            let row = result.map_err(|e| MoePoolListError::Csv(e.to_string()))?;
            let row_num = idx + 2; // header is line 1
            entries.push(MoePoolListEntry {
                factory: parse_address(&row.factory, row_num)?,
                pool: parse_address(&row.pool, row_num)?,
                token_x: parse_address(&row.token_x, row_num)?,
                token_y: parse_address(&row.token_y, row_num)?,
                bin_step: parse_bin_step(&row.bin_step, row_num)?,
                creation_block: parse_creation_block(&row.creation_block, row_num)?,
            });
        }

        let list = Self {
            entries,
            meta: None,
        };
        list.validate_offline(CANONICAL_MOE_FACTORY)?;
        Ok(list)
    }

    pub fn load_path(path: impl AsRef<Path>) -> Result<Self, MoePoolListError> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(MoePoolListError::NotFound(path.display().to_string()));
        }
        let file = File::open(path).map_err(|e| MoePoolListError::Io(e.to_string()))?;
        let mut list = Self::parse_csv(file)?;

        let meta_path = meta_path_for(path);
        if meta_path.exists() {
            list.meta = Some(load_meta_path(&meta_path)?);
            list.validate_offline(CANONICAL_MOE_FACTORY)?;
        }

        Ok(list)
    }

    /// Load default list + metadata and require a recorded snapshot block.
    pub fn load_default() -> Result<Self, MoePoolListError> {
        let list = Self::load_path(default_moe_pool_list_path())?;
        if list.meta.is_none() {
            return Err(MoePoolListError::MetaNotFound(
                default_moe_pool_list_meta_path().display().to_string(),
            ));
        }
        Ok(list)
    }

    /// Fail-closed load used by Moe services: offline checks + on-chain provenance.
    pub async fn load_and_validate_on_chain<N, P>(
        path: impl AsRef<Path>,
        provider: P,
        block_id: BlockId,
        expected_factory: Address,
    ) -> Result<Self, MoePoolListError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        let list = Self::load_path(path)?;
        if list.meta.is_none() {
            return Err(MoePoolListError::MetaNotFound(
                "companion .meta.json required for service load".into(),
            ));
        }
        list.validate_on_chain(provider, block_id, expected_factory)
            .await?;
        Ok(list)
    }

    pub fn write_csv(&self, mut writer: impl Write) -> Result<(), MoePoolListError> {
        self.validate_offline(CANONICAL_MOE_FACTORY)?;
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(&mut writer);
        for entry in &self.entries {
            wtr.serialize(CsvRow {
                factory: format!("{:?}", entry.factory),
                pool: format!("{:?}", entry.pool),
                token_x: format!("{:?}", entry.token_x),
                token_y: format!("{:?}", entry.token_y),
                bin_step: entry.bin_step.to_string(),
                creation_block: entry.creation_block.to_string(),
            })
            .map_err(|e| MoePoolListError::Csv(e.to_string()))?;
        }
        wtr.flush()
            .map_err(|e| MoePoolListError::Io(e.to_string()))?;
        Ok(())
    }

    pub fn write_path(&self, path: impl AsRef<Path>) -> Result<(), MoePoolListError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| MoePoolListError::Io(e.to_string()))?;
        }
        let mut file = File::create(path).map_err(|e| MoePoolListError::Io(e.to_string()))?;
        self.write_csv(&mut file)?;

        if let Some(meta) = &self.meta {
            write_meta_path(meta, meta_path_for(path))?;
        }
        Ok(())
    }

    /// Validate each row against on-chain factory/token/bin getters at `block_id`.
    pub async fn validate_on_chain<N, P>(
        &self,
        provider: P,
        block_id: BlockId,
        expected_factory: Address,
    ) -> Result<(), MoePoolListError>
    where
        N: Network,
        P: Provider<N> + Clone,
    {
        self.validate_offline(expected_factory)?;

        let mut stream = stream::iter(self.entries.iter().cloned().map(|entry| {
            let provider = provider.clone();
            async move { validate_entry_on_chain(entry, provider, block_id, expected_factory).await }
        }))
        .buffer_unordered(ON_CHAIN_VALIDATE_CONCURRENCY);

        while let Some(result) = stream.next().await {
            result?;
        }
        Ok(())
    }
}

async fn validate_entry_on_chain<N, P>(
    entry: MoePoolListEntry,
    provider: P,
    block_id: BlockId,
    expected_factory: Address,
) -> Result<(), MoePoolListError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let pair = IMoeLBPair::new(entry.pool, provider);
    let on_factory = pair
        .getFactory()
        .call()
        .block(block_id)
        .await
        .map_err(|e| MoePoolListError::Provider(format!("{}: {e}", entry.pool)))?;
    if on_factory != expected_factory {
        return Err(MoePoolListError::ProvenanceMismatch {
            pool: entry.pool,
            detail: format!("getFactory()={on_factory:?}, expected {expected_factory:?}"),
        });
    }
    if on_factory != entry.factory {
        return Err(MoePoolListError::ProvenanceMismatch {
            pool: entry.pool,
            detail: format!(
                "getFactory()={on_factory:?}, csv factory={:?}",
                entry.factory
            ),
        });
    }

    let token_x = pair
        .getTokenX()
        .call()
        .block(block_id)
        .await
        .map_err(|e| MoePoolListError::Provider(format!("{} tokenX: {e}", entry.pool)))?;
    let token_y = pair
        .getTokenY()
        .call()
        .block(block_id)
        .await
        .map_err(|e| MoePoolListError::Provider(format!("{} tokenY: {e}", entry.pool)))?;
    let bin_step = pair
        .getBinStep()
        .call()
        .block(block_id)
        .await
        .map_err(|e| MoePoolListError::Provider(format!("{} binStep: {e}", entry.pool)))?;

    if token_x != entry.token_x {
        return Err(MoePoolListError::ProvenanceMismatch {
            pool: entry.pool,
            detail: format!("getTokenX()={token_x:?}, csv={:?}", entry.token_x),
        });
    }
    if token_y != entry.token_y {
        return Err(MoePoolListError::ProvenanceMismatch {
            pool: entry.pool,
            detail: format!("getTokenY()={token_y:?}, csv={:?}", entry.token_y),
        });
    }
    if bin_step != entry.bin_step {
        return Err(MoePoolListError::ProvenanceMismatch {
            pool: entry.pool,
            detail: format!("getBinStep()={bin_step}, csv={}", entry.bin_step),
        });
    }
    Ok(())
}

/// Shared Mantle-safe paginated `eth_getLogs` over a factory creation event.
pub async fn fetch_chunked_factory_logs<N, P>(
    provider: P,
    factory: Address,
    event: B256,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<Log>, MoePoolListError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    if to_block < from_block {
        return Err(MoePoolListError::Provider(format!(
            "to_block {to_block} is before from_block {from_block}"
        )));
    }

    let mut logs_out = Vec::new();
    let mut from = from_block;
    while from <= to_block {
        let chunk_to = from.saturating_add(MOE_LOG_CHUNK_SIZE - 1).min(to_block);
        let filter = Filter::new()
            .event_signature(FilterSet::from(vec![event]))
            .address(vec![factory])
            .from_block(from)
            .to_block(chunk_to);

        let logs = provider
            .get_logs(&filter)
            .await
            .map_err(|e| MoePoolListError::Provider(e.to_string()))?;
        logs_out.extend(logs);
        from = chunk_to.saturating_add(1);
    }
    Ok(logs_out)
}

/// Discover all LB pairs created by the factory up to `to_block` (inclusive).
///
/// Deterministic for the same `(factory, factory_creation_block, to_block)`.
pub async fn discover_moe_pool_list<N, P>(
    provider: P,
    factory: Address,
    factory_creation_block: u64,
    to_block: u64,
) -> Result<MoePoolList, MoePoolListError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let factory_helper = MoeFactory::new(factory, factory_creation_block);
    let logs = fetch_chunked_factory_logs(
        provider,
        factory,
        factory_helper.pool_creation_event(),
        factory_creation_block,
        to_block,
    )
    .await?;

    let mut entries = Vec::with_capacity(logs.len());
    for log in logs {
        entries.push(entry_from_creation_log(log, factory)?);
    }

    let mut list = MoePoolList::new(entries);
    list.sort_deterministic();
    let mut seen = HashSet::new();
    list.entries.retain(|e| seen.insert(e.pool));
    list.meta = Some(MoePoolListMeta::new(
        factory,
        factory_creation_block,
        to_block,
        list.entries.len() as u64,
    ));
    list.validate_offline(factory)?;
    Ok(list)
}

pub fn entry_from_creation_log(
    log: Log,
    expected_factory: Address,
) -> Result<MoePoolListEntry, MoePoolListError> {
    let ev = IMoeFactory::LBPairCreated::decode_log(&log.inner)
        .map_err(|e| MoePoolListError::Decode(e.to_string()))?;

    let creation_block = log.block_number.ok_or_else(|| {
        MoePoolListError::Provider("LBPairCreated log missing block_number".into())
    })?;

    let bin_step = bin_step_from_event(ev.binStep)?;

    Ok(MoePoolListEntry {
        factory: expected_factory,
        pool: ev.LBPair,
        token_x: ev.tokenX,
        token_y: ev.tokenY,
        bin_step,
        creation_block,
    })
}

pub fn bin_step_from_event(bin_step: alloy::primitives::U256) -> Result<u16, MoePoolListError> {
    u16::try_from(bin_step).map_err(|_| {
        MoePoolListError::Provider(format!("binStep too large for u16: {bin_step}"))
    })
}

pub fn default_moe_pool_list_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_MOE_POOL_LIST_REL)
}

pub fn default_moe_pool_list_meta_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(DEFAULT_MOE_POOL_LIST_META_REL)
}

pub fn meta_path_for(csv_path: impl AsRef<Path>) -> PathBuf {
    let path = csv_path.as_ref();
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("poolLists_moe");
    path.with_file_name(format!("{stem}.meta.json"))
}

fn load_meta_path(path: &Path) -> Result<MoePoolListMeta, MoePoolListError> {
    let file = File::open(path).map_err(|e| MoePoolListError::Io(e.to_string()))?;
    serde_json::from_reader(file).map_err(|e| MoePoolListError::Meta(e.to_string()))
}

fn write_meta_path(meta: &MoePoolListMeta, path: PathBuf) -> Result<(), MoePoolListError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| MoePoolListError::Io(e.to_string()))?;
    }
    let file = File::create(&path).map_err(|e| MoePoolListError::Io(e.to_string()))?;
    serde_json::to_writer_pretty(file, meta).map_err(|e| MoePoolListError::Meta(e.to_string()))?;
    Ok(())
}

#[derive(Debug, Deserialize, Serialize)]
struct CsvRow {
    factory: String,
    pool: String,
    token_x: String,
    token_y: String,
    bin_step: String,
    creation_block: String,
}

fn parse_address(value: &str, row: usize) -> Result<Address, MoePoolListError> {
    Address::from_str(value.trim()).map_err(|_| MoePoolListError::InvalidAddress {
        row,
        value: value.to_string(),
    })
}

fn parse_bin_step(value: &str, row: usize) -> Result<u16, MoePoolListError> {
    value
        .trim()
        .parse::<u16>()
        .map_err(|_| MoePoolListError::InvalidBinStep {
            row,
            value: value.to_string(),
        })
}

fn parse_creation_block(value: &str, row: usize) -> Result<u64, MoePoolListError> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| MoePoolListError::InvalidCreationBlock {
            row,
            value: value.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_entry(pool_byte: u8) -> MoePoolListEntry {
        let mut pool = [0u8; 20];
        pool[19] = pool_byte;
        let mut token_x = [0u8; 20];
        token_x[19] = 0x11;
        let mut token_y = [0u8; 20];
        token_y[19] = 0x22;
        MoePoolListEntry {
            factory: CANONICAL_MOE_FACTORY,
            pool: Address::from(pool),
            token_x: Address::from(token_x),
            token_y: Address::from(token_y),
            bin_step: 15,
            creation_block: 61_742_961,
        }
    }

    fn sample_csv(entries: &[MoePoolListEntry]) -> String {
        let mut out = String::from("factory,pool,token_x,token_y,bin_step,creation_block\n");
        for e in entries {
            out.push_str(&format!(
                "{:?},{:?},{:?},{:?},{},{}\n",
                e.factory, e.pool, e.token_x, e.token_y, e.bin_step, e.creation_block
            ));
        }
        out
    }

    fn sample_meta(pool_count: u64) -> MoePoolListMeta {
        MoePoolListMeta::new(
            CANONICAL_MOE_FACTORY,
            CANONICAL_MOE_FACTORY_CREATION_BLOCK,
            COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK,
            pool_count,
        )
    }

    #[test]
    fn parse_valid_list_is_non_empty() {
        let csv = sample_csv(&[sample_entry(1), sample_entry(2)]);
        let list = MoePoolList::parse_csv(Cursor::new(csv)).unwrap();
        assert_eq!(list.len(), 2);
        assert!(!list.is_empty());
    }

    #[test]
    fn rejects_empty_list() {
        let csv = "factory,pool,token_x,token_y,bin_step,creation_block\n";
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert_eq!(err, MoePoolListError::Empty);
    }

    #[test]
    fn rejects_missing_file() {
        let err = MoePoolList::load_path("/tmp/definitely-missing-moe-pool-list-whi-507.csv")
            .unwrap_err();
        assert!(matches!(err, MoePoolListError::NotFound(_)));
    }

    #[test]
    fn rejects_duplicate_pool() {
        let e = sample_entry(1);
        let csv = sample_csv(&[e.clone(), e]);
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::DuplicatePool(_)));
    }

    #[test]
    fn rejects_non_canonical_factory() {
        let mut e = sample_entry(1);
        e.factory = address!("0x1111111111111111111111111111111111111111");
        let csv = sample_csv(&[e]);
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::NonCanonicalFactory { .. }));
    }

    #[test]
    fn rejects_zero_bin_step() {
        let mut e = sample_entry(1);
        e.bin_step = 0;
        let csv = sample_csv(&[e]);
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::ZeroBinStep(_)));
    }

    #[test]
    fn rejects_zero_creation_block() {
        let mut e = sample_entry(1);
        e.creation_block = 0;
        let csv = sample_csv(&[e]);
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::ZeroCreationBlock(_)));
    }

    #[test]
    fn rejects_identical_tokens() {
        let mut e = sample_entry(1);
        e.token_y = e.token_x;
        let csv = sample_csv(&[e]);
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::IdenticalTokens(_)));
    }

    #[test]
    fn rejects_meta_pool_count_mismatch() {
        let list = MoePoolList::new(vec![sample_entry(1)]).with_meta(sample_meta(99));
        let err = list.validate_offline(CANONICAL_MOE_FACTORY).unwrap_err();
        assert!(matches!(err, MoePoolListError::PoolCountMismatch { .. }));
    }

    #[test]
    fn roundtrip_csv_is_deterministic() {
        let mut list = MoePoolList::new(vec![sample_entry(2), sample_entry(1)]);
        list.sort_deterministic();
        let mut buf1 = Vec::new();
        list.write_csv(&mut buf1).unwrap();
        let parsed = MoePoolList::parse_csv(Cursor::new(&buf1)).unwrap();
        let mut buf2 = Vec::new();
        parsed.write_csv(&mut buf2).unwrap();
        assert_eq!(buf1, buf2);
        assert!(parsed.entries[0].pool < parsed.entries[1].pool);
    }

    #[test]
    fn does_not_accept_agni_shaped_headers() {
        let csv = "Protocol,Pair Name,Pair Address,TokenA Address,TokenB Address,Fee Tier\n\
Agni,USDC-WMNT,0x8E2C009E45420D2B36bC15315F9de8CeCa2cc724,0x09Bc4E0D864854c6aFB6eB9A9cdF58aC190D0dF9,0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8,10000\n";
        let err = MoePoolList::parse_csv(Cursor::new(csv)).unwrap_err();
        assert!(matches!(err, MoePoolListError::Csv(_)));
    }

    #[test]
    fn loads_committed_dedicated_moe_list_with_meta() {
        let list = MoePoolList::load_default().expect("committed data/poolLists_moe.csv + meta");
        assert!(!list.is_empty(), "Moe pool list must be non-empty");
        let meta = list.meta.as_ref().expect("meta required");
        assert_eq!(meta.factory, CANONICAL_MOE_FACTORY);
        assert_eq!(meta.snapshot_block, COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK);
        assert_eq!(meta.pool_count, list.len() as u64);
        list.validate_offline(CANONICAL_MOE_FACTORY)
            .expect("committed list must pass offline validation");
        assert!(list
            .entries
            .iter()
            .all(|e| e.factory == CANONICAL_MOE_FACTORY));
    }

    #[test]
    fn meta_path_for_csv_uses_meta_json_suffix() {
        let p = PathBuf::from("/tmp/data/poolLists_moe.csv");
        assert_eq!(
            meta_path_for(&p),
            PathBuf::from("/tmp/data/poolLists_moe.meta.json")
        );
    }
}
