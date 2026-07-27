//! Pure, I/O-free storage-slot derivation from `BuildEvidence::storage_layout()`'s
//! compiled `storageLayout` JSON (`contracts/executor/artifacts/ArbitrageExecutor.full.json`).
//!
//! `mainnet_fork_harness.rs` hand-encodes `ADMIN_SLOT`, `REGISTERED_POOLS_BASE_SLOT`,
//! and the `RegisteredPool`/`Venue` packed-word layouts as constants verified once
//! against this same artifact. This module replaces those hand-written constants
//! with a real lookup against the JSON so a future Solidity storage-layout change
//! (a reordered field, a new variable) is caught by a compile-time-embedded-artifact
//! mismatch instead of silently producing wrong overrides.

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlotInfo {
    /// Storage slot, relative to the containing mapping's/struct's base slot for a
    /// struct member; absolute for a top-level variable.
    pub slot: u64,
    /// Byte offset within the 32-byte word, counted from the low end (Solidity's
    /// own convention).
    pub offset: u8,
    pub number_of_bytes: u16,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SlotError {
    #[error("storageLayout is missing a top-level `storage` array")]
    MissingStorageArray,
    #[error("storageLayout is missing a top-level `types` object")]
    MissingTypesObject,
    #[error("no top-level storage variable named `{0}`")]
    UnknownLabel(String),
    #[error("storageLayout `types` has no entry for `{0}`")]
    UnknownType(String),
    #[error("type `{0}` has no `members` array (not a struct type)")]
    NotAStruct(String),
    #[error("struct type `{type_key}` has no member named `{member}`")]
    UnknownMember { type_key: String, member: String },
    #[error("mapping type `{0}` is missing its `value` type key")]
    MissingMappingValue(String),
    #[error("malformed `{field}` field on storage entry: {detail}")]
    Malformed { field: &'static str, detail: String },
}

fn get_str<'a>(entry: &'a Value, field: &'static str) -> Result<&'a str, SlotError> {
    entry
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| SlotError::Malformed {
            field,
            detail: "missing or not a JSON string".to_string(),
        })
}

fn parse_string_field<T>(entry: &Value, field: &'static str) -> Result<T, SlotError>
where
    T: std::str::FromStr,
{
    get_str(entry, field)?
        .parse::<T>()
        .map_err(|_| SlotError::Malformed {
            field,
            detail: "not a parseable integer".to_string(),
        })
}

fn parse_number_field<T: TryFrom<u64>>(entry: &Value, field: &'static str) -> Result<T, SlotError> {
    let raw = entry
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| SlotError::Malformed {
            field,
            detail: "missing or not a JSON number".to_string(),
        })?;
    T::try_from(raw).map_err(|_| SlotError::Malformed {
        field,
        detail: "out of range".to_string(),
    })
}

fn type_number_of_bytes(layout: &Value, type_key: &str) -> Result<u16, SlotError> {
    let types = layout.get("types").ok_or(SlotError::MissingTypesObject)?;
    let type_entry = types
        .get(type_key)
        .ok_or_else(|| SlotError::UnknownType(type_key.to_string()))?;
    parse_string_field(type_entry, "numberOfBytes")
}

fn slot_info_from_entry(layout: &Value, entry: &Value) -> Result<SlotInfo, SlotError> {
    let slot = parse_string_field(entry, "slot")?;
    let offset = parse_number_field(entry, "offset")?;
    let type_key = get_str(entry, "type")?;
    let number_of_bytes = type_number_of_bytes(layout, type_key)?;
    Ok(SlotInfo {
        slot,
        offset,
        number_of_bytes,
    })
}

fn storage_array(layout: &Value) -> Result<&Vec<Value>, SlotError> {
    layout
        .get("storage")
        .and_then(Value::as_array)
        .ok_or(SlotError::MissingStorageArray)
}

fn find_by_label<'a>(entries: &'a [Value], label: &str) -> Option<&'a Value> {
    entries
        .iter()
        .find(|entry| entry.get("label").and_then(Value::as_str) == Some(label))
}

/// Looks up a top-level state-variable's absolute `SlotInfo` by its Solidity name,
/// e.g. `"admin"`, `"paused"`, `"registeredPools"`, `"venues"`.
pub(crate) fn top_level_slot(layout: &Value, label: &str) -> Result<SlotInfo, SlotError> {
    let entries = storage_array(layout)?;
    let entry =
        find_by_label(entries, label).ok_or_else(|| SlotError::UnknownLabel(label.to_string()))?;
    slot_info_from_entry(layout, entry)
}

/// Looks up a packed struct member's `SlotInfo`, *relative to the struct's own base
/// word* (word 0, 1, 2, ...) — for a mapping's value type, combine with
/// `mapping_slot(pad_key, pad_u64(top_level_slot(...).slot))` plus this member's
/// `slot` to get the absolute storage key.
///
/// `mapping_label` is the top-level mapping variable name (e.g. `"registeredPools"`,
/// `"venues"`); `member_label` is the struct field name (e.g. `"poolType"`, `"fee"`).
pub(crate) fn mapping_struct_member_slot(
    layout: &Value,
    mapping_label: &str,
    member_label: &str,
) -> Result<SlotInfo, SlotError> {
    let entries = storage_array(layout)?;
    let mapping_entry = find_by_label(entries, mapping_label)
        .ok_or_else(|| SlotError::UnknownLabel(mapping_label.to_string()))?;
    let mapping_type_key = get_str(mapping_entry, "type")?;

    let types = layout.get("types").ok_or(SlotError::MissingTypesObject)?;
    let mapping_type = types
        .get(mapping_type_key)
        .ok_or_else(|| SlotError::UnknownType(mapping_type_key.to_string()))?;
    let value_type_key = mapping_type
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| SlotError::MissingMappingValue(mapping_type_key.to_string()))?;
    let value_type = types
        .get(value_type_key)
        .ok_or_else(|| SlotError::UnknownType(value_type_key.to_string()))?;
    let members = value_type
        .get("members")
        .and_then(Value::as_array)
        .ok_or_else(|| SlotError::NotAStruct(value_type_key.to_string()))?;
    let member_entry =
        find_by_label(members, member_label).ok_or_else(|| SlotError::UnknownMember {
            type_key: value_type_key.to_string(),
            member: member_label.to_string(),
        })?;
    slot_info_from_entry(layout, member_entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const MAINNET_BUILD_EVIDENCE_JSON: &str =
        include_str!("../../../contracts/executor/artifacts/ArbitrageExecutor.full.json");

    fn layout() -> Value {
        let artifact: Value = serde_json::from_str(MAINNET_BUILD_EVIDENCE_JSON)
            .expect("embedded artifact is valid JSON");
        artifact
            .get("storageLayout")
            .cloned()
            .expect("artifact has a storageLayout field")
    }

    #[test]
    fn admin_slot_matches_mainnet_fork_harness_hand_verified_constant() {
        let layout = layout();
        let info = top_level_slot(&layout, "admin").unwrap();
        assert_eq!(
            info.slot,
            crate::execution::mainnet_fork_harness::ADMIN_SLOT
        );
        assert_eq!(info.offset, 0);
    }

    #[test]
    fn registered_pools_base_slot_matches_mainnet_fork_harness_hand_verified_constant() {
        let layout = layout();
        let info = top_level_slot(&layout, "registeredPools").unwrap();
        assert_eq!(
            info.slot,
            crate::execution::mainnet_fork_harness::REGISTERED_POOLS_BASE_SLOT
        );
    }

    #[test]
    fn guardian_and_paused_share_slot_one_packed() {
        let layout = layout();
        let guardian = top_level_slot(&layout, "guardian").unwrap();
        let paused = top_level_slot(&layout, "paused").unwrap();
        assert_eq!(guardian.slot, 1);
        assert_eq!(guardian.offset, 0);
        assert_eq!(paused.slot, 1);
        assert_eq!(paused.offset, 20);
    }

    #[test]
    fn venues_top_level_slot_is_four() {
        let layout = layout();
        let info = top_level_slot(&layout, "venues").unwrap();
        assert_eq!(info.slot, 4);
    }

    #[test]
    fn registered_pool_member_layout_matches_hand_encoded_packing() {
        let layout = layout();
        let pool_type = mapping_struct_member_slot(&layout, "registeredPools", "poolType").unwrap();
        let token0 = mapping_struct_member_slot(&layout, "registeredPools", "token0").unwrap();
        let token1 = mapping_struct_member_slot(&layout, "registeredPools", "token1").unwrap();
        let fee = mapping_struct_member_slot(&layout, "registeredPools", "fee").unwrap();
        let enabled = mapping_struct_member_slot(&layout, "registeredPools", "enabled").unwrap();

        // Matches the packing hand-encoded in
        // `mainnet_fork_harness::registered_pool_slots`: word 0 = poolType (offset 0)
        // | token0 (offset 1); word 1 = token1 (offset 0) | fee (offset 20) |
        // enabled (offset 23).
        assert_eq!((pool_type.slot, pool_type.offset), (0, 0));
        assert_eq!((token0.slot, token0.offset), (0, 1));
        assert_eq!((token1.slot, token1.offset), (1, 0));
        assert_eq!((fee.slot, fee.offset), (1, 20));
        assert_eq!((enabled.slot, enabled.offset), (1, 23));
    }

    #[test]
    fn venue_member_layout_is_one_field_per_word() {
        let layout = layout();
        let factory = mapping_struct_member_slot(&layout, "venues", "factory").unwrap();
        let init_code_hash = mapping_struct_member_slot(&layout, "venues", "initCodeHash").unwrap();
        let enabled = mapping_struct_member_slot(&layout, "venues", "enabled").unwrap();

        assert_eq!((factory.slot, factory.offset), (0, 0));
        assert_eq!((init_code_hash.slot, init_code_hash.offset), (1, 0));
        assert_eq!((enabled.slot, enabled.offset), (2, 0));
    }

    #[test]
    fn unknown_label_is_a_typed_error() {
        let layout = layout();
        let error = top_level_slot(&layout, "doesNotExist").unwrap_err();
        assert_eq!(error, SlotError::UnknownLabel("doesNotExist".to_string()));
    }

    #[test]
    fn unknown_member_is_a_typed_error() {
        let layout = layout();
        let error =
            mapping_struct_member_slot(&layout, "registeredPools", "doesNotExist").unwrap_err();
        assert!(matches!(
            error,
            SlotError::UnknownMember { ref member, .. } if member == "doesNotExist"
        ));
    }
}
