//! Committed CREATE2 registration authority for V2/V3/Agni pools.
//!
//! Moe LB pairs use a committed allowlist instead (`moe_allowlist.rs`, not
//! CREATE2-derivable — see that module's doc comment). Every other pool protocol
//! shadow mode can encounter (`UniswapV2`, `UniswapV3`, `Agni`) is CREATE2-derivable
//! from a `(factory, init_code_hash)` pair, but that pair must itself come from a
//! committed, hash-pinned config rather than a live `factory()`/`INIT_CODE_HASH()`
//! call — shadow mode issues zero RPC calls of its own accord, and even if it did,
//! a pool's self-reported factory is exactly the kind of claim CREATE2 verification
//! exists to not trust blindly.

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::state_space::PoolProtocol;

use super::digest::digest_of;

pub(crate) const APPROVED_POOLS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovedPoolProtocol {
    UniswapV2,
    UniswapV3,
    Agni,
}

impl ApprovedPoolProtocol {
    fn matches(self, protocol: PoolProtocol) -> bool {
        matches!(
            (self, protocol),
            (ApprovedPoolProtocol::UniswapV2, PoolProtocol::UniswapV2)
                | (ApprovedPoolProtocol::UniswapV3, PoolProtocol::UniswapV3)
                | (ApprovedPoolProtocol::Agni, PoolProtocol::Agni)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedPoolEntry {
    pub protocol: ApprovedPoolProtocol,
    pub factory: Address,
    pub init_code_hash: B256,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovedPoolsConfig {
    pub schema_version: u32,
    pub entries: Vec<ApprovedPoolEntry>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ApprovedPoolsError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("unsupported schema_version {0}, expected {APPROVED_POOLS_SCHEMA_VERSION}")]
    UnsupportedSchemaVersion(u32),
    #[error("duplicate entry for protocol {0:?}")]
    DuplicateProtocol(ApprovedPoolProtocol),
}

fn validate(config: &ApprovedPoolsConfig) -> Result<(), ApprovedPoolsError> {
    if config.schema_version != APPROVED_POOLS_SCHEMA_VERSION {
        return Err(ApprovedPoolsError::UnsupportedSchemaVersion(
            config.schema_version,
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &config.entries {
        if !seen.insert(entry.protocol) {
            return Err(ApprovedPoolsError::DuplicateProtocol(entry.protocol));
        }
    }
    Ok(())
}

pub fn load_approved_pools(path: &Path) -> Result<ApprovedPoolsConfig, ApprovedPoolsError> {
    let raw =
        fs::read_to_string(path).map_err(|error| ApprovedPoolsError::Io(error.to_string()))?;
    let config: ApprovedPoolsConfig =
        serde_json::from_str(&raw).map_err(|error| ApprovedPoolsError::Json(error.to_string()))?;
    validate(&config)?;
    Ok(config)
}

/// Looks up the committed `(factory, init_code_hash)` entry for `protocol`, or
/// `None` if no entry is committed for it (an unverifiable pool type, per spec,
/// must not proceed as if verified — the caller is responsible for treating a
/// `None` as a rejection, not a pass).
pub fn approved_entry_for(
    config: &ApprovedPoolsConfig,
    protocol: PoolProtocol,
) -> Option<&ApprovedPoolEntry> {
    config
        .entries
        .iter()
        .find(|entry| entry.protocol.matches(protocol))
}

pub(crate) fn digest(config: &ApprovedPoolsConfig) -> Result<B256, ApprovedPoolsError> {
    let value: Value = serde_json::to_value(config)
        .map_err(|error| ApprovedPoolsError::Json(error.to_string()))?;
    Ok(digest_of(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample() -> ApprovedPoolsConfig {
        ApprovedPoolsConfig {
            schema_version: APPROVED_POOLS_SCHEMA_VERSION,
            entries: vec![
                ApprovedPoolEntry {
                    protocol: ApprovedPoolProtocol::UniswapV2,
                    factory: address!("1111111111111111111111111111111111111111"),
                    init_code_hash: B256::repeat_byte(0xAA),
                    notes: None,
                },
                ApprovedPoolEntry {
                    protocol: ApprovedPoolProtocol::UniswapV3,
                    factory: address!("2222222222222222222222222222222222222222"),
                    init_code_hash: B256::repeat_byte(0xBB),
                    notes: None,
                },
                ApprovedPoolEntry {
                    protocol: ApprovedPoolProtocol::Agni,
                    factory: address!("3333333333333333333333333333333333333333"),
                    init_code_hash: B256::repeat_byte(0xCC),
                    notes: None,
                },
            ],
        }
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let mut config = sample();
        config.schema_version = 999;
        let error = validate(&config).unwrap_err();
        assert_eq!(error, ApprovedPoolsError::UnsupportedSchemaVersion(999));
    }

    #[test]
    fn rejects_duplicate_protocol_entries() {
        let mut config = sample();
        let dup = config.entries[0].clone();
        config.entries.push(dup);
        let error = validate(&config).unwrap_err();
        assert!(matches!(error, ApprovedPoolsError::DuplicateProtocol(_)));
    }

    #[test]
    fn approved_entry_for_finds_the_matching_protocol() {
        let config = sample();
        let entry = approved_entry_for(&config, PoolProtocol::UniswapV3).unwrap();
        assert_eq!(
            entry.factory,
            address!("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn approved_entry_for_is_none_when_no_entry_is_committed() {
        let config = ApprovedPoolsConfig {
            schema_version: APPROVED_POOLS_SCHEMA_VERSION,
            entries: vec![],
        };
        assert!(approved_entry_for(&config, PoolProtocol::UniswapV2).is_none());
    }

    #[test]
    fn approved_entry_for_never_matches_moe_lb() {
        let config = sample();
        assert!(approved_entry_for(&config, PoolProtocol::MoeLb).is_none());
    }

    #[test]
    fn digest_is_deterministic_and_changes_with_content() {
        let a = sample();
        let mut b = sample();
        assert_eq!(digest(&a).unwrap(), digest(&a).unwrap());
        b.entries[0].init_code_hash = B256::repeat_byte(0xFF);
        assert_ne!(digest(&a).unwrap(), digest(&b).unwrap());
    }
}
