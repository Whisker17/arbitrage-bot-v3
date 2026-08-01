//! Load-once, frozen, fingerprinted pool-universe source (WHI-727).
//!
//! No reload/promotion path (out of scope: M3-10 / WHI-527). Generalizes the
//! Moe `MoePoolList::load_and_validate_on_chain` pattern to a trait that CSV
//! sources for V2/V3 can also implement.

use crate::amms::amm::{AutomatedMarketMaker, AMM};
use crate::state_space::{pool_universe_fingerprint, PoolProtocol, PoolUniverseRow};
use alloy::primitives::{Address, B256};
use async_trait::async_trait;
use csv::ReaderBuilder;
use eyre::{eyre, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub use crate::service::error::PoolUniverseSourceError;

/// Result of a load-once universe load.
#[derive(Debug, Clone)]
pub struct LoadedPoolUniverse {
    pub rows: Vec<PoolUniverseRow>,
    pub fingerprint: B256,
    pub addresses: Vec<Address>,
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
    /// separation. On-chain validation of every row is protocol-specific and
    /// happens outside this trait for CSV sources (the Moe path validates via
    /// `MoePoolList::load_and_validate_on_chain` before constructing rows).
    async fn load(
        &self,
        chain_id: u64,
        settlement_asset: Address,
    ) -> Result<LoadedPoolUniverse, PoolUniverseSourceError>;
}

/// CSV pool-list source (V2 / V3 shape: `Pair Address` + optional `Protocol` column).
#[derive(Debug, Clone)]
pub struct CsvPoolUniverseSource {
    pub path: PathBuf,
    /// Protocol tag written into every emitted [`PoolUniverseRow`].
    pub protocol: PoolProtocol,
    /// Factory address written into every row (provenance identity).
    pub factory: Address,
    /// When set, only rows whose Protocol column matches (case-insensitive
    /// substring) are kept — used by Agni-V3 to filter `data/poolLists.csv`.
    pub protocol_filter: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CsvPoolRow {
    #[serde(rename = "Pair Address")]
    pair_address: String,
    #[serde(rename = "Protocol", default)]
    protocol: String,
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
        Self {
            path: path.into(),
            protocol,
            factory,
            protocol_filter: None,
        }
    }

    pub fn with_protocol_filter(mut self, filter: impl Into<String>) -> Self {
        self.protocol_filter = Some(filter.into());
        self
    }

    /// Parse the CSV without computing a fingerprint (test helper / dry load).
    pub fn read_rows(&self) -> Result<Vec<PoolUniverseRow>, PoolUniverseSourceError> {
        read_csv_rows(&self.path, self.protocol, self.factory, self.protocol_filter.as_deref())
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
        let addresses: Vec<Address> = rows.iter().map(|r| r.pool).collect();
        let fingerprint = pool_universe_fingerprint(chain_id, settlement_asset, rows.clone())?;
        Ok(LoadedPoolUniverse {
            rows,
            fingerprint,
            addresses,
        })
    }
}

fn read_csv_rows(
    path: &Path,
    protocol: PoolProtocol,
    factory: Address,
    protocol_filter: Option<&str>,
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
    }
}
