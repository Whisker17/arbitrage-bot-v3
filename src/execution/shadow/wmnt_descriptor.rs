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
    #[error(
        "wmnt_descriptor's balance_mapping_slot ({descriptor_value}) does not match the \
         independently-derived mainnet_fork_harness::WMNT_BALANCE_SLOT ({harness_constant})"
    )]
    BalanceSlotDrift {
        descriptor_value: u64,
        harness_constant: u64,
    },
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

/// Cross-checks the descriptor's WMNT balance-mapping slot against the independently
/// empirically-derived `mainnet_fork_harness::WMNT_BALANCE_SLOT` constant (see that
/// constant's doc comment for how it was originally brute-forced via live `cast`
/// probing against Mantle mainnet). Both values describe the same real-world fact —
/// WMNT's `balanceOf` mapping slot — from two independently maintained sources: the
/// committed descriptor config and the WHI-557 gas-measurement harness. WMNT is an
/// external, already-deployed ERC20 contract with no compiled build artifact in this
/// repo, so unlike `ArbitrageExecutor`'s own fields there is no `BuildEvidence` to
/// derive its layout from — this cross-check is the only drift detection available,
/// and it must run at startup, not just in a test, so a network migration (e.g. a WMNT
/// proxy upgrade) that updates one source without the other is caught fail-closed
/// rather than silently producing a wrong override.
///
/// A `Proxy` shape's balance mapping lives on the implementation contract's storage,
/// which the harness constant has no analog for, so this check only applies to the
/// `Direct` shape.
pub(crate) fn check_wmnt_balance_slot_drift(
    descriptor: &WmntDescriptor,
) -> Result<(), WmntDescriptorError> {
    let WmntStorageShape::Direct {
        balance_mapping_slot,
    } = descriptor.storage_shape
    else {
        return Ok(());
    };
    let harness_constant = crate::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT;
    if balance_mapping_slot != harness_constant {
        return Err(WmntDescriptorError::BalanceSlotDrift {
            descriptor_value: balance_mapping_slot,
            harness_constant,
        });
    }
    Ok(())
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
        assert!(check_wmnt_balance_slot_drift(&descriptor).is_ok());
    }

    #[test]
    fn check_wmnt_balance_slot_drift_passes_when_slots_match() {
        let mut descriptor = sample_direct();
        descriptor.storage_shape = WmntStorageShape::Direct {
            balance_mapping_slot: crate::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT,
        };
        assert!(check_wmnt_balance_slot_drift(&descriptor).is_ok());
    }

    #[test]
    fn check_wmnt_balance_slot_drift_rejects_a_mismatch() {
        let mut descriptor = sample_direct();
        let harness_constant = crate::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT;
        descriptor.storage_shape = WmntStorageShape::Direct {
            balance_mapping_slot: harness_constant + 1,
        };
        let error = check_wmnt_balance_slot_drift(&descriptor).unwrap_err();
        assert_eq!(
            error,
            WmntDescriptorError::BalanceSlotDrift {
                descriptor_value: harness_constant + 1,
                harness_constant,
            }
        );
    }

    #[test]
    fn check_wmnt_balance_slot_drift_skips_the_proxy_shape() {
        let mut descriptor = sample_direct();
        descriptor.storage_shape = WmntStorageShape::Proxy {
            implementation_slot: 1,
            balance_mapping_slot: crate::execution::mainnet_fork_harness::WMNT_BALANCE_SLOT + 1,
        };
        assert!(check_wmnt_balance_slot_drift(&descriptor).is_ok());
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
