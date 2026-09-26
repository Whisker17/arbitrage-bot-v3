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
//!
//! **WHI-1413:** a protocol may carry several entries, one per factory (FusionX V2
//! and Merchant Moe V1 classic are both `UniswapV2`). A pool is verified when it is
//! the CREATE2 output of *any* committed entry for its protocol — each entry is an
//! explicitly listed authority, so this admits nothing a single entry would not.
//! An entry states its derivation explicitly: a classic factory's fixed
//! `init_code_hash`, or the `clone_implementation` of an immutable-args clone
//! factory (Moe V1), whose init code embeds the token pair.

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::state_space::PoolProtocol;

use super::create2::immutable_clone_init_code_hash;
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
    /// Fixed init-code hash of a classic CREATE2 factory. Exactly one of this and
    /// [`Self::clone_implementation`] is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_code_hash: Option<B256>,
    /// Implementation behind an immutable-args clone factory (V2 only): the
    /// per-pair init code hash is derived from it and the token pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clone_implementation: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl ApprovedPoolEntry {
    /// The init-code hash this entry's factory would have used for `(token_a,
    /// token_b)`. `validate` guarantees exactly one derivation is set.
    pub fn init_code_hash_for(&self, token_a: Address, token_b: Address) -> B256 {
        match (self.init_code_hash, self.clone_implementation) {
            (Some(hash), None) => hash,
            (None, Some(implementation)) => {
                immutable_clone_init_code_hash(implementation, token_a, token_b)
            }
            _ => unreachable!("validate() admits exactly one derivation per entry"),
        }
    }
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
    #[error("duplicate entry for protocol {0:?} factory {1}")]
    DuplicateEntry(ApprovedPoolProtocol, Address),
    #[error("entry for protocol {0:?} factory {1} must set exactly one of init_code_hash / clone_implementation")]
    AmbiguousDerivation(ApprovedPoolProtocol, Address),
    #[error("entry for protocol {0:?} factory {1}: clone_implementation is only defined for uniswap_v2")]
    CloneDerivationNotV2(ApprovedPoolProtocol, Address),
}

fn validate(config: &ApprovedPoolsConfig) -> Result<(), ApprovedPoolsError> {
    if config.schema_version != APPROVED_POOLS_SCHEMA_VERSION {
        return Err(ApprovedPoolsError::UnsupportedSchemaVersion(
            config.schema_version,
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for entry in &config.entries {
        if !seen.insert((entry.protocol, entry.factory)) {
            return Err(ApprovedPoolsError::DuplicateEntry(entry.protocol, entry.factory));
        }
        if entry.init_code_hash.is_some() == entry.clone_implementation.is_some() {
            return Err(ApprovedPoolsError::AmbiguousDerivation(entry.protocol, entry.factory));
        }
        if entry.clone_implementation.is_some() && entry.protocol != ApprovedPoolProtocol::UniswapV2 {
            return Err(ApprovedPoolsError::CloneDerivationNotV2(entry.protocol, entry.factory));
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

/// Every committed entry for `protocol`, in file order. Empty when none is
/// committed (an unverifiable pool type, per spec, must not proceed as if
/// verified — the caller treats an empty set as a rejection, not a pass).
pub fn approved_entries_for(
    config: &ApprovedPoolsConfig,
    protocol: PoolProtocol,
) -> impl Iterator<Item = &ApprovedPoolEntry> {
    config
        .entries
        .iter()
        .filter(move |entry| entry.protocol.matches(protocol))
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
                    init_code_hash: Some(B256::repeat_byte(0xAA)),
                    clone_implementation: None,
                    notes: None,
                },
                ApprovedPoolEntry {
                    protocol: ApprovedPoolProtocol::UniswapV3,
                    factory: address!("2222222222222222222222222222222222222222"),
                    init_code_hash: Some(B256::repeat_byte(0xBB)),
                    clone_implementation: None,
                    notes: None,
                },
                ApprovedPoolEntry {
                    protocol: ApprovedPoolProtocol::Agni,
                    factory: address!("3333333333333333333333333333333333333333"),
                    init_code_hash: Some(B256::repeat_byte(0xCC)),
                    clone_implementation: None,
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
    fn rejects_duplicate_protocol_factory_entries() {
        let mut config = sample();
        let dup = config.entries[0].clone();
        config.entries.push(dup);
        let error = validate(&config).unwrap_err();
        assert!(matches!(error, ApprovedPoolsError::DuplicateEntry(..)));
    }

    /// WHI-1413: two V2 factories (FusionX V2 classic, Moe V1 clone) under one
    /// protocol, each explicitly listed.
    #[test]
    fn accepts_several_factories_per_protocol_each_with_one_derivation() {
        let mut config = sample();
        config.entries.push(ApprovedPoolEntry {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory: address!("4444444444444444444444444444444444444444"),
            init_code_hash: None,
            clone_implementation: Some(address!("5555555555555555555555555555555555555555")),
            notes: None,
        });
        validate(&config).unwrap();
        assert_eq!(approved_entries_for(&config, PoolProtocol::UniswapV2).count(), 2);
    }

    #[test]
    fn rejects_entries_with_zero_or_two_derivations() {
        for (hash, clone) in [(None, None), (Some(B256::repeat_byte(1)), Some(Address::repeat_byte(2)))] {
            let mut config = sample();
            config.entries[0].init_code_hash = hash;
            config.entries[0].clone_implementation = clone;
            assert!(matches!(
                validate(&config).unwrap_err(),
                ApprovedPoolsError::AmbiguousDerivation(..)
            ));
        }
    }

    #[test]
    fn rejects_a_clone_derivation_outside_v2() {
        let mut config = sample();
        config.entries[1].init_code_hash = None;
        config.entries[1].clone_implementation = Some(Address::repeat_byte(2));
        assert!(matches!(
            validate(&config).unwrap_err(),
            ApprovedPoolsError::CloneDerivationNotV2(..)
        ));
    }

    #[test]
    fn approved_entries_for_finds_the_matching_protocol() {
        let config = sample();
        let entries: Vec<_> = approved_entries_for(&config, PoolProtocol::UniswapV3).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].factory,
            address!("2222222222222222222222222222222222222222")
        );
    }

    #[test]
    fn approved_entries_for_is_empty_when_no_entry_is_committed() {
        let config = ApprovedPoolsConfig {
            schema_version: APPROVED_POOLS_SCHEMA_VERSION,
            entries: vec![],
        };
        assert_eq!(approved_entries_for(&config, PoolProtocol::UniswapV2).count(), 0);
    }

    #[test]
    fn approved_entries_for_never_matches_moe_lb() {
        let config = sample();
        assert_eq!(approved_entries_for(&config, PoolProtocol::MoeLb).count(), 0);
    }

    /// WHI-1413: every admitted V2 venue has a committed registration authority,
    /// and every V2 row of the committed universe is the CREATE2 output of one of
    /// them — so shadow provenance accepts exactly the V2 pools the bot loads.
    #[test]
    fn committed_config_covers_every_admitted_v2_venue_and_universe_v2_row() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config =
            load_approved_pools(&root.join("config/gas_profiles/approved_pools.mantle_mainnet.json"))
                .unwrap();
        for venue in crate::service::v2_venues::V2_VENUES {
            assert!(
                approved_entries_for(&config, PoolProtocol::UniswapV2).any(|e| e.factory == venue.factory),
                "admitted V2 venue {} has no approved_pools entry",
                venue.label
            );
        }
        let rows =
            crate::service::unified_universe::read_unified_csv(&root.join("data/pool_universe.csv"))
                .unwrap();
        let mut v2_rows = 0;
        for row in rows.iter().filter(|r| r.protocol == "agni-v2") {
            v2_rows += 1;
            let matched = approved_entries_for(&config, PoolProtocol::UniswapV2).find(|e| {
                crate::execution::shadow::create2::expected_create2_derivation(
                    PoolProtocol::UniswapV2,
                    e.factory,
                    row.token0,
                    row.token1,
                    0,
                    e.init_code_hash_for(row.token0, row.token1),
                )
                .is_some_and(|d| d.address == row.pool)
            });
            let entry = matched.unwrap_or_else(|| panic!("universe V2 row {} derives under no entry", row.pool));
            assert_eq!(entry.factory, row.factory, "row {} derives under another factory", row.pool);
        }
        assert!(v2_rows > 0, "committed universe has no V2 rows to check");
    }

    #[test]
    fn digest_is_deterministic_and_changes_with_content() {
        let a = sample();
        let mut b = sample();
        assert_eq!(digest(&a).unwrap(), digest(&a).unwrap());
        b.entries[0].init_code_hash = Some(B256::repeat_byte(0xFF));
        assert_ne!(digest(&a).unwrap(), digest(&b).unwrap());
    }
}
