//! Storage-slot derivation from the compiler-emitted `storageLayout`
//! (`runtime_identity::BuildEvidence::storage_layout`), rather than hand-copied
//! constants — mirrors `mainnet_fork_harness.rs`'s slot-arithmetic primitives, but
//! looks slots up by field label instead of assuming a fixed layout, so a contract
//! recompile that reorders fields is caught (a lookup miss) rather than silently
//! writing the wrong slot.

use alloy::primitives::{Address, B256};
use serde_json::Value;
use thiserror::Error;

use crate::execution::mainnet_fork_harness::{mapping_slot, pad_address, pad_u64};

const PAUSED_LABEL: &str = "paused";
const IS_HOT_EXECUTOR_LABEL: &str = "isHotExecutor";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlotsError {
    #[error("storage layout missing a top-level 'storage' array")]
    MissingStorageArray,
    #[error("no storage-layout entry labelled {0:?}")]
    LabelNotFound(String),
    #[error("storage-layout entry {0:?} has a malformed slot/offset/type field")]
    Malformed(String),
    #[error("storage-layout entry {label:?} has type {actual:?}, expected {expected:?}")]
    UnexpectedType {
        label: String,
        expected: &'static str,
        actual: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StorageSlotInfo {
    slot: u64,
    type_identifier: String,
}

fn find_storage_slot(storage_layout: &Value, label: &str) -> Result<StorageSlotInfo, SlotsError> {
    let entries = storage_layout
        .get("storage")
        .and_then(Value::as_array)
        .ok_or(SlotsError::MissingStorageArray)?;

    let entry = entries
        .iter()
        .find(|entry| entry.get("label").and_then(Value::as_str) == Some(label))
        .ok_or_else(|| SlotsError::LabelNotFound(label.to_string()))?;

    let slot = entry
        .get("slot")
        .and_then(Value::as_str)
        .and_then(|slot| slot.parse::<u64>().ok())
        .ok_or_else(|| SlotsError::Malformed(label.to_string()))?;
    let type_identifier = entry
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| SlotsError::Malformed(label.to_string()))?
        .to_string();

    Ok(StorageSlotInfo {
        slot,
        type_identifier,
    })
}

/// Derives `paused`'s slot from the storage layout. `paused` is packed alongside
/// `guardian` in the same word (see `mainnet_fork_harness.rs`'s hand-verified layout,
/// cross-checked against the compiled `storageLayout`), but `executeArbitrage` — the
/// only function shadow mode's `eth_call` ever targets — is gated by
/// `onlyHotExecutor whenNotPaused` and never reads `guardian`, so callers may zero the
/// whole word without an RPC read of the real `guardian` value.
fn paused_slot(storage_layout: &Value) -> Result<u64, SlotsError> {
    let info = find_storage_slot(storage_layout, PAUSED_LABEL)?;
    if info.type_identifier != "t_bool" {
        return Err(SlotsError::UnexpectedType {
            label: PAUSED_LABEL.to_string(),
            expected: "t_bool",
            actual: info.type_identifier,
        });
    }
    Ok(info.slot)
}

/// Derives `isHotExecutor`'s mapping base slot from the storage layout.
fn is_hot_executor_base_slot(storage_layout: &Value) -> Result<u64, SlotsError> {
    let info = find_storage_slot(storage_layout, IS_HOT_EXECUTOR_LABEL)?;
    if !info.type_identifier.starts_with("t_mapping") {
        return Err(SlotsError::UnexpectedType {
            label: IS_HOT_EXECUTOR_LABEL.to_string(),
            expected: "t_mapping(...)",
            actual: info.type_identifier,
        });
    }
    Ok(info.slot)
}

/// Forces `paused` to `false` — see [`paused_slot`]'s doc comment for why zeroing the
/// whole word (rather than splicing) is safe here.
pub(crate) fn paused_override(storage_layout: &Value) -> Result<(B256, B256), SlotsError> {
    let slot = paused_slot(storage_layout)?;
    Ok((pad_u64(slot), B256::ZERO))
}

/// Grants `caller` the hot-executor role: `isHotExecutor[caller] = true`. Mapping
/// value slots are always full, dedicated words in Solidity (never packed across
/// entries), so this is a direct write, not a splice.
pub(crate) fn is_hot_executor_override(
    storage_layout: &Value,
    caller: Address,
) -> Result<(B256, B256), SlotsError> {
    let base_slot = is_hot_executor_base_slot(storage_layout)?;
    let slot = mapping_slot(pad_address(caller), pad_u64(base_slot));
    let mut value = [0u8; 32];
    value[31] = 1;
    Ok((slot, B256::from(value)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use serde_json::json;

    fn sample_storage_layout() -> Value {
        json!({
            "storage": [
                {"astId": 1, "contract": "c", "label": "admin", "offset": 0, "slot": "0", "type": "t_address"},
                {"astId": 2, "contract": "c", "label": "guardian", "offset": 0, "slot": "1", "type": "t_address"},
                {"astId": 3, "contract": "c", "label": "paused", "offset": 20, "slot": "1", "type": "t_bool"},
                {"astId": 4, "contract": "c", "label": "isHotExecutor", "offset": 0, "slot": "2", "type": "t_mapping(t_address,t_bool)"},
                {"astId": 5, "contract": "c", "label": "registeredPools", "offset": 0, "slot": "3", "type": "t_mapping(t_address,t_struct(RegisteredPool)storage)"}
            ],
            "types": {
                "t_address": {"encoding": "inplace", "label": "address", "numberOfBytes": "20"},
                "t_bool": {"encoding": "inplace", "label": "bool", "numberOfBytes": "1"}
            }
        })
    }

    #[test]
    fn paused_slot_finds_the_labelled_entry() {
        assert_eq!(paused_slot(&sample_storage_layout()).unwrap(), 1);
    }

    #[test]
    fn paused_slot_rejects_a_type_mismatch() {
        let mut layout = sample_storage_layout();
        layout["storage"][2]["type"] = json!("t_address");
        let error = paused_slot(&layout).unwrap_err();
        assert!(matches!(error, SlotsError::UnexpectedType { .. }));
    }

    #[test]
    fn is_hot_executor_base_slot_finds_the_labelled_entry() {
        assert_eq!(is_hot_executor_base_slot(&sample_storage_layout()).unwrap(), 2);
    }

    #[test]
    fn find_storage_slot_errors_on_a_missing_label() {
        let error = find_storage_slot(&sample_storage_layout(), "nonexistent").unwrap_err();
        assert!(matches!(error, SlotsError::LabelNotFound(label) if label == "nonexistent"));
    }

    #[test]
    fn find_storage_slot_errors_on_a_missing_storage_array() {
        let error = find_storage_slot(&json!({}), "paused").unwrap_err();
        assert_eq!(error, SlotsError::MissingStorageArray);
    }

    #[test]
    fn paused_override_zeroes_the_whole_word() {
        let (slot, value) = paused_override(&sample_storage_layout()).unwrap();
        assert_eq!(slot, pad_u64(1));
        assert_eq!(value, B256::ZERO);
    }

    #[test]
    fn is_hot_executor_override_writes_a_mapping_slot_set_to_true() {
        let caller = address!("2222222222222222222222222222222222222222");
        let (slot, value) = is_hot_executor_override(&sample_storage_layout(), caller).unwrap();
        assert_eq!(slot, mapping_slot(pad_address(caller), pad_u64(2)));
        let mut expected = [0u8; 32];
        expected[31] = 1;
        assert_eq!(value, B256::from(expected));
    }

    #[test]
    fn is_hot_executor_override_differs_per_caller() {
        let a = address!("1111111111111111111111111111111111111111");
        let b = address!("2222222222222222222222222222222222222222");
        let (slot_a, _) = is_hot_executor_override(&sample_storage_layout(), a).unwrap();
        let (slot_b, _) = is_hot_executor_override(&sample_storage_layout(), b).unwrap();
        assert_ne!(slot_a, slot_b);
    }

    #[test]
    fn matches_the_real_compiled_storage_layout() {
        use crate::execution::runtime_identity::BuildEvidence;
        use std::path::Path;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let artifact_dir = root.join("contracts/executor/artifacts");
        let evidence = BuildEvidence::load(&artifact_dir).unwrap();
        let storage_layout = evidence.storage_layout();

        assert_eq!(paused_slot(storage_layout).unwrap(), 1);
        assert_eq!(is_hot_executor_base_slot(storage_layout).unwrap(), 2);
    }
}
