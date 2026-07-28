//! Committed Moe Liquidity Book pool allowlist.
//!
//! Moe LB pairs are not CREATE2-derivable the way V2/V3 pairs are (contract comment at
//! `ArbitrageExecutor.sol:201`: "Moe LB pair addresses are not CREATE2(tokenX,tokenY)-
//! simple; allowlist only"), so shadow mode instead validates a candidate Moe pool
//! against a committed `config/gas_profiles/moe_allowlist.<network>.json` file.

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::digest::digest_of;

pub(crate) const MOE_ALLOWLIST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoeAllowlistEntry {
    pub pool: Address,
    pub token_x: Address,
    pub token_y: Address,
    pub bin_step: u32,
    /// Pinned `keccak256` of the pool's on-chain runtime bytecode (`eth_getCode`),
    /// independently verified at check time — Moe LB pairs are not CREATE2-derivable
    /// (see this module's doc comment), so a pinned runtime codehash is the substitute
    /// proof that the allowlisted address still carries the expected contract, not a
    /// swapped-in impersonator.
    pub runtime_codehash: B256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoeAllowlist {
    pub schema_version: u32,
    pub entries: Vec<MoeAllowlistEntry>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MoeAllowlistError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("unsupported schema_version {0}, expected {MOE_ALLOWLIST_SCHEMA_VERSION}")]
    UnsupportedSchemaVersion(u32),
    #[error("duplicate pool entry {0}")]
    DuplicatePool(Address),
}

fn validate(allowlist: &MoeAllowlist) -> Result<(), MoeAllowlistError> {
    if allowlist.schema_version != MOE_ALLOWLIST_SCHEMA_VERSION {
        return Err(MoeAllowlistError::UnsupportedSchemaVersion(
            allowlist.schema_version,
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &allowlist.entries {
        if !seen.insert(entry.pool) {
            return Err(MoeAllowlistError::DuplicatePool(entry.pool));
        }
    }
    Ok(())
}

pub fn load_moe_allowlist(path: &Path) -> Result<MoeAllowlist, MoeAllowlistError> {
    let raw = fs::read_to_string(path).map_err(|error| MoeAllowlistError::Io(error.to_string()))?;
    let allowlist: MoeAllowlist =
        serde_json::from_str(&raw).map_err(|error| MoeAllowlistError::Json(error.to_string()))?;
    validate(&allowlist)?;
    Ok(allowlist)
}

/// Looks up the allowlist entry matching `pool` with exactly the claimed `(token_x,
/// token_y, bin_step)` identity — a partial match (right pool, wrong tokens/bin step)
/// is rejected just as a missing entry would be.
pub fn find_entry(
    allowlist: &MoeAllowlist,
    pool: Address,
    token_x: Address,
    token_y: Address,
    bin_step: u32,
) -> Option<&MoeAllowlistEntry> {
    allowlist.entries.iter().find(|entry| {
        entry.pool == pool
            && entry.token_x == token_x
            && entry.token_y == token_y
            && entry.bin_step == bin_step
    })
}

pub(crate) fn digest(allowlist: &MoeAllowlist) -> Result<B256, MoeAllowlistError> {
    let value: Value = serde_json::to_value(allowlist)
        .map_err(|error| MoeAllowlistError::Json(error.to_string()))?;
    Ok(digest_of(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample() -> MoeAllowlist {
        MoeAllowlist {
            schema_version: MOE_ALLOWLIST_SCHEMA_VERSION,
            entries: vec![MoeAllowlistEntry {
                pool: address!("f6C9020c9E915808481757779EDB53DACEaE2415"),
                token_x: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
                token_y: address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
                bin_step: 15,
                runtime_codehash: B256::repeat_byte(0x77),
                notes: None,
            }],
        }
    }

    #[test]
    fn loads_the_committed_mantle_mainnet_allowlist() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("config/gas_profiles/moe_allowlist.mantle_mainnet.json");
        let allowlist = load_moe_allowlist(&path).unwrap();
        assert_eq!(allowlist.schema_version, MOE_ALLOWLIST_SCHEMA_VERSION);
        assert!(find_entry(
            &allowlist,
            address!("f6C9020c9E915808481757779EDB53DACEaE2415"),
            address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
            15,
        )
        .is_some());
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let mut allowlist = sample();
        allowlist.schema_version = 999;
        let error = validate(&allowlist).unwrap_err();
        assert_eq!(error, MoeAllowlistError::UnsupportedSchemaVersion(999));
    }

    #[test]
    fn rejects_duplicate_pool_entries() {
        let mut allowlist = sample();
        let dup = allowlist.entries[0].clone();
        allowlist.entries.push(dup);
        let error = validate(&allowlist).unwrap_err();
        assert!(matches!(error, MoeAllowlistError::DuplicatePool(_)));
    }

    #[test]
    fn matching_pool_but_wrong_bin_step_has_no_entry() {
        let allowlist = sample();
        assert!(find_entry(
            &allowlist,
            address!("f6C9020c9E915808481757779EDB53DACEaE2415"),
            address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
            20,
        )
        .is_none());
    }

    #[test]
    fn unknown_pool_has_no_entry() {
        let allowlist = sample();
        assert!(find_entry(
            &allowlist,
            address!("0000000000000000000000000000000000000001"),
            address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            address!("201EBa5CC46D216Ce6DC03F6a759e8E766e956aE"),
            15,
        )
        .is_none());
    }

    #[test]
    fn digest_is_deterministic_and_changes_with_content() {
        let a = sample();
        let mut b = sample();
        assert_eq!(digest(&a).unwrap(), digest(&a).unwrap());
        b.entries[0].bin_step = 20;
        assert_ne!(digest(&a).unwrap(), digest(&b).unwrap());
    }
}
