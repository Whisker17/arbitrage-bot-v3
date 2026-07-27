//! Committed WMNT storage-shape descriptor.
//!
//! WMNT is the Mantle base/gas token (the `WmntValueInPools` code path replacing
//! upstream's Weth variants). Shadow mode needs to know which storage slot holds a
//! given holder's balance in order to build an `eth_call` `StateOverride` that credits
//! the executor with WMNT — and, for a proxied deployment, which slot holds the
//! implementation address. This is loaded from a committed
//! `config/gas_profiles/wmnt_descriptor.<network>.json` rather than hardcoded, so a
//! network migration (e.g. a WMNT proxy upgrade) is a config change, not a recompile.

use std::fs;
use std::path::Path;

use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use super::digest::digest_of;

pub(crate) const WMNT_DESCRIPTOR_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WmntStorageShape {
    /// Balances live directly on the WMNT contract's own storage (canonical WETH9-style
    /// layout) at `balance_mapping_slot`.
    Direct { balance_mapping_slot: u64 },
    /// Balances live behind an EIP-1967-style proxy: `implementation_slot` holds the
    /// logic contract address, and `balance_mapping_slot` is the mapping base slot on
    /// that logic contract's storage layout (which the proxy delegates into, so the
    /// override must still be written on the proxy's own storage).
    Proxy {
        implementation_slot: u64,
        balance_mapping_slot: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WmntDescriptor {
    pub schema_version: u32,
    pub wmnt_address: Address,
    pub storage_shape: WmntStorageShape,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WmntDescriptorError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("unsupported schema_version {0}, expected {WMNT_DESCRIPTOR_SCHEMA_VERSION}")]
    UnsupportedSchemaVersion(u32),
}

fn validate(descriptor: &WmntDescriptor) -> Result<(), WmntDescriptorError> {
    if descriptor.schema_version != WMNT_DESCRIPTOR_SCHEMA_VERSION {
        return Err(WmntDescriptorError::UnsupportedSchemaVersion(
            descriptor.schema_version,
        ));
    }
    Ok(())
}

pub fn load_wmnt_descriptor(path: &Path) -> Result<WmntDescriptor, WmntDescriptorError> {
    let raw =
        fs::read_to_string(path).map_err(|error| WmntDescriptorError::Io(error.to_string()))?;
    let descriptor: WmntDescriptor =
        serde_json::from_str(&raw).map_err(|error| WmntDescriptorError::Json(error.to_string()))?;
    validate(&descriptor)?;
    Ok(descriptor)
}

pub(crate) fn digest(descriptor: &WmntDescriptor) -> Result<B256, WmntDescriptorError> {
    let value: Value = serde_json::to_value(descriptor)
        .map_err(|error| WmntDescriptorError::Json(error.to_string()))?;
    Ok(digest_of(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn sample_direct() -> WmntDescriptor {
        WmntDescriptor {
            schema_version: WMNT_DESCRIPTOR_SCHEMA_VERSION,
            wmnt_address: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            storage_shape: WmntStorageShape::Direct {
                balance_mapping_slot: 0,
            },
            notes: None,
        }
    }

    #[test]
    fn loads_the_committed_mantle_mainnet_descriptor() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let path = root.join("config/gas_profiles/wmnt_descriptor.mantle_mainnet.json");
        let descriptor = load_wmnt_descriptor(&path).unwrap();
        assert_eq!(descriptor.schema_version, WMNT_DESCRIPTOR_SCHEMA_VERSION);
        assert_eq!(
            descriptor.wmnt_address,
            address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8")
        );
        assert_eq!(
            descriptor.storage_shape,
            WmntStorageShape::Direct {
                balance_mapping_slot: crate::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT,
            }
        );
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let mut descriptor = sample_direct();
        descriptor.schema_version = 999;
        let error = validate(&descriptor).unwrap_err();
        assert_eq!(error, WmntDescriptorError::UnsupportedSchemaVersion(999));
    }

    #[test]
    fn proxy_shape_round_trips_through_json() {
        let descriptor = WmntDescriptor {
            schema_version: WMNT_DESCRIPTOR_SCHEMA_VERSION,
            wmnt_address: address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"),
            storage_shape: WmntStorageShape::Proxy {
                implementation_slot: 1,
                balance_mapping_slot: 0,
            },
            notes: None,
        };
        let value = serde_json::to_value(&descriptor).unwrap();
        let round_tripped: WmntDescriptor = serde_json::from_value(value).unwrap();
        assert_eq!(descriptor, round_tripped);
    }

    #[test]
    fn digest_is_deterministic_and_changes_with_content() {
        let a = sample_direct();
        let mut b = sample_direct();
        assert_eq!(digest(&a).unwrap(), digest(&a).unwrap());
        b.storage_shape = WmntStorageShape::Direct {
            balance_mapping_slot: 1,
        };
        assert_ne!(digest(&a).unwrap(), digest(&b).unwrap());
    }
}
