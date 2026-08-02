//! Load the merged multi-protocol pool-address set for Check A.
//!
//! Mirrors `src/bin/bot.rs` unified universe source so the probe exercises the
//! same `eth_getLogs` address width the merged bot will issue (WHI-793).

use crate::service::UnifiedPoolUniverseSource;
use alloy::primitives::{keccak256, Address, B256};
use eyre::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the address set came from (reported in Check A).
#[derive(Debug, Clone)]
pub struct AddressSetSource {
    pub pool_universe: PathBuf,
    pub pool_count: usize,
    pub unique_count: usize,
    pub address_multiplier: f64,
    pub effective_count: usize,
}

impl AddressSetSource {
    pub fn derivation_summary(&self) -> String {
        format!(
            "unified pool universe: {} (rows={}) → unique={} × multiplier={} → effective={}",
            self.pool_universe.display(),
            self.pool_count,
            self.unique_count,
            self.address_multiplier,
            self.effective_count
        )
    }
}

/// Load unique pool addresses from the unified universe CSV the bot uses.
///
/// When `address_multiplier > 1.0`, pads with deterministic synthetic addresses
/// so the `eth_getLogs` address array is larger than today's universe (headroom).
pub fn load_merged_pool_addresses(
    pool_universe: &Path,
    address_multiplier: f64,
) -> Result<(Vec<Address>, AddressSetSource)> {
    if address_multiplier < 1.0 {
        bail!("address_multiplier must be >= 1.0, got {address_multiplier}");
    }

    let source = UnifiedPoolUniverseSource::new(pool_universe);
    let rows = source
        .read_candidates()
        .with_context(|| format!("read unified pool universe {}", pool_universe.display()))?;

    let mut unique: BTreeSet<Address> = BTreeSet::new();
    for row in &rows {
        unique.insert(row.pool);
    }

    let mut addresses: Vec<Address> = unique.iter().copied().collect();
    let unique_count = addresses.len();
    if unique_count == 0 {
        bail!(
            "merged pool universe is empty ({})",
            pool_universe.display()
        );
    }

    let target = ((unique_count as f64) * address_multiplier).ceil() as usize;
    if target > unique_count {
        pad_with_synthetic_addresses(&mut addresses, target);
    }

    let source = AddressSetSource {
        pool_universe: pool_universe.to_path_buf(),
        pool_count: rows.len(),
        unique_count,
        address_multiplier,
        effective_count: addresses.len(),
    };
    Ok((addresses, source))
}

/// Deterministic padding addresses so multi-address width can exceed the live
/// universe without depending on extra CSV rows.
fn pad_with_synthetic_addresses(addresses: &mut Vec<Address>, target: usize) {
    let mut i: u64 = 0;
    while addresses.len() < target {
        let mut material = Vec::with_capacity(16);
        material.extend_from_slice(b"rpc_probe_pad");
        material.extend_from_slice(&i.to_le_bytes());
        let digest: B256 = keccak256(&material);
        let addr = Address::from_slice(&digest.as_slice()[12..32]);
        if !addresses.contains(&addr) {
            addresses.push(addr);
        }
        i = i.saturating_add(1);
        if i > target as u64 * 4 {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{build_meta, write_unified_csv, write_unified_meta, CandidatePool};
    use alloy::primitives::{address, U256};
    use tempfile::TempDir;

    fn write_unified(dir: &TempDir, n: usize) -> PathBuf {
        let csv = dir.path().join("pool_universe.csv");
        let mut pools = Vec::new();
        for i in 0..n {
            pools.push(CandidatePool {
                protocol: "agni-v3".into(),
                factory: address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035"),
                pool: Address::with_last_byte((i + 1) as u8),
                token0: address!("78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"),
                token1: address!("201eba5cc46d216ce6dc03f6a759e8e766e956ae"),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: None,
            });
        }
        write_unified_csv(&csv, &pools).unwrap();
        write_unified_meta(
            &csv,
            &build_meta(
                5000,
                1,
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
        csv
    }

    #[test]
    fn loads_union_and_multiplier_padding() {
        let dir = TempDir::new().unwrap();
        let csv = write_unified(&dir, 3);
        let (addrs, src) = load_merged_pool_addresses(&csv, 2.0).unwrap();
        assert_eq!(src.unique_count, 3);
        assert_eq!(src.effective_count, 6);
        assert_eq!(addrs.len(), 6);
        assert!(src.derivation_summary().contains("unique=3"));
        assert!(src.derivation_summary().contains("effective=6"));
    }

    #[test]
    fn rejects_empty_universe() {
        let dir = TempDir::new().unwrap();
        let csv = dir.path().join("pool_universe.csv");
        write_unified_csv(&csv, &[]).unwrap();
        let err = load_merged_pool_addresses(&csv, 1.0).unwrap_err();
        let msg = format!("{err:#}").to_lowercase();
        assert!(
            msg.contains("empty") || msg.contains("pool"),
            "unexpected error: {err:#}"
        );
    }
}
