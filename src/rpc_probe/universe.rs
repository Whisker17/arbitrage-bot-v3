//! Load the merged multi-protocol pool-address set for Check A.
//!
//! Mirrors `src/bin/bot.rs` CSV sources so the probe exercises the same
//! `eth_getLogs` address width the merged bot will issue.

use crate::service::{CsvPoolUniverseSource, MoeCsvPoolUniverseSource};
use crate::state_space::PoolProtocol;
use alloy::primitives::{keccak256, Address, B256};
use eyre::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Where the address set came from (reported in Check A).
#[derive(Debug, Clone)]
pub struct AddressSetSource {
    pub v2_pool_list: PathBuf,
    pub v3_pool_list: PathBuf,
    pub moe_pool_list: PathBuf,
    pub v2_count: usize,
    pub v3_count: usize,
    pub moe_count: usize,
    pub unique_count: usize,
    pub address_multiplier: f64,
    pub effective_count: usize,
}

impl AddressSetSource {
    pub fn derivation_summary(&self) -> String {
        format!(
            "union of bot CSV pool lists: v2={} ({}) + v3/agni={} ({}) + moe={} ({}) → unique={} × multiplier={} → effective={}",
            self.v2_count,
            self.v2_pool_list.display(),
            self.v3_count,
            self.v3_pool_list.display(),
            self.moe_count,
            self.moe_pool_list.display(),
            self.unique_count,
            self.address_multiplier,
            self.effective_count
        )
    }
}

/// Load unique pool addresses from the same CSV paths the bot uses.
///
/// * V2: unfiltered rows from `v2_pool_list` (bot falls back to unfiltered when
///   the `"v2"` protocol filter is empty).
/// * V3: rows matching protocol filter `"agni"`.
/// * Moe: `MoeCsvPoolUniverseSource` schema.
///
/// When `address_multiplier > 1.0`, pads with deterministic synthetic addresses
/// so the `eth_getLogs` address array is larger than today's universe (headroom).
pub fn load_merged_pool_addresses(
    v2_pool_list: &Path,
    v3_pool_list: &Path,
    moe_pool_list: &Path,
    address_multiplier: f64,
) -> Result<(Vec<Address>, AddressSetSource)> {
    if address_multiplier < 1.0 {
        bail!("address_multiplier must be >= 1.0, got {address_multiplier}");
    }

    let v2_source =
        CsvPoolUniverseSource::new(v2_pool_list, PoolProtocol::UniswapV2, Address::ZERO);
    let v2_rows = v2_source
        .read_rows()
        .with_context(|| format!("read v2 pool list {}", v2_pool_list.display()))?;

    let v3_source = CsvPoolUniverseSource::new(v3_pool_list, PoolProtocol::Agni, Address::ZERO)
        .with_protocol_filter("agni");
    let v3_rows = v3_source
        .read_rows()
        .with_context(|| format!("read v3 pool list {}", v3_pool_list.display()))?;

    let moe_source = MoeCsvPoolUniverseSource::new(moe_pool_list);
    let moe_rows = moe_source
        .read_rows()
        .with_context(|| format!("read moe pool list {}", moe_pool_list.display()))?;

    let mut unique: BTreeSet<Address> = BTreeSet::new();
    for row in &v2_rows {
        unique.insert(row.pool);
    }
    for row in &v3_rows {
        unique.insert(row.pool);
    }
    for row in &moe_rows {
        unique.insert(row.pool);
    }

    let mut addresses: Vec<Address> = unique.iter().copied().collect();
    let unique_count = addresses.len();
    if unique_count == 0 {
        bail!(
            "merged pool universe is empty (v2={} v3={} moe={})",
            v2_pool_list.display(),
            v3_pool_list.display(),
            moe_pool_list.display()
        );
    }

    let target = ((unique_count as f64) * address_multiplier).ceil() as usize;
    if target > unique_count {
        pad_with_synthetic_addresses(&mut addresses, target);
    }

    let source = AddressSetSource {
        v2_pool_list: v2_pool_list.to_path_buf(),
        v3_pool_list: v3_pool_list.to_path_buf(),
        moe_pool_list: moe_pool_list.to_path_buf(),
        v2_count: v2_rows.len(),
        v3_count: v3_rows.len(),
        moe_count: moe_rows.len(),
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
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_v2_csv(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "{contents}").unwrap();
        f
    }

    #[test]
    fn loads_union_and_multiplier_padding() {
        let v2 = write_v2_csv(
            "Protocol,Pair Address,TokenA Address,TokenB Address\n\
             Agni,0x1111111111111111111111111111111111111111,0x2222222222222222222222222222222222222222,0x3333333333333333333333333333333333333333\n\
             Agni,0x4444444444444444444444444444444444444444,0x2222222222222222222222222222222222222222,0x3333333333333333333333333333333333333333\n",
        );
        let moe = write_v2_csv(
            "factory,pool,token_x,token_y\n\
             0xa6630671775c4ea2743840f9a5016dcf2a104054,0x5555555555555555555555555555555555555555,0x6666666666666666666666666666666666666666,0x7777777777777777777777777777777777777777\n",
        );
        let (addrs, src) =
            load_merged_pool_addresses(v2.path(), v2.path(), moe.path(), 2.0).unwrap();
        assert_eq!(src.unique_count, 3);
        assert_eq!(src.effective_count, 6);
        assert_eq!(addrs.len(), 6);
        assert!(src.derivation_summary().contains("unique=3"));
        assert!(src.derivation_summary().contains("effective=6"));
    }

    #[test]
    fn rejects_empty_universe() {
        let empty = write_v2_csv("Protocol,Pair Address\n");
        let moe = write_v2_csv("factory,pool,token_x,token_y\n");
        let err = load_merged_pool_addresses(empty.path(), empty.path(), moe.path(), 1.0)
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }
}
