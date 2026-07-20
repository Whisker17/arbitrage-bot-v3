use std::collections::HashMap;

use alloy::{
    primitives::{Address, B256, U256},
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};

use crate::amms::error::AMMError;

use super::{BinReserve, MoeError, MoeSlot0};

pub(crate) const MAX_BIN_ID: u32 = 0xFF_FFFF;

type RawSlot0Response = (
    Address,
    Address,
    u32,
    u16,
    u128,
    u128,
    u16,
    u16,
    u16,
    u16,
    u32,
    u16,
    u32,
    u32,
    u32,
    u32,
    u64,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
    bool,
);

#[derive(Debug, Clone)]
pub(crate) struct MoeSlot0BatchResponse {
    pub(crate) token_x: Address,
    pub(crate) token_y: Address,
    pub(crate) slot0: MoeSlot0,
}

impl MoeSlot0BatchResponse {
    pub(crate) fn decode_batch(data: &[u8]) -> Result<Vec<Self>, AMMError> {
        let decoded = <Vec<RawSlot0Response>>::abi_decode(data)?;
        decoded
            .into_iter()
            .map(|slot| {
                if !(slot.17 && slot.18 && slot.19 && slot.20 && slot.21 && slot.22 && slot.23) {
                    return Err(MoeError::IncompleteState.into());
                }
                if slot.0 == Address::ZERO || slot.1 == Address::ZERO {
                    return Err(MoeError::IncompleteState.into());
                }
                let slot0 = MoeSlot0 {
                    active_id: slot.2,
                    bin_step: slot.3,
                    reserve_x: slot.4,
                    reserve_y: slot.5,
                    base_factor: slot.6,
                    filter_period: slot.7,
                    decay_period: slot.8,
                    reduction_factor: slot.9,
                    variable_fee_control: slot.10,
                    protocol_share_bps: slot.11,
                    max_volatility_acc: slot.12,
                    volatility_accumulator: slot.13,
                    volatility_reference: slot.14,
                    id_reference: slot.15,
                    timestamp: U256::from(slot.16),
                };
                MoeSnapshot::validate_slot0(&slot0)?;
                Ok(Self {
                    token_x: slot.0,
                    token_y: slot.1,
                    slot0,
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoeBinRange {
    pub start: u32,
    pub end: u32,
}

impl MoeBinRange {
    pub const fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    pub const fn contains(self, id: u32) -> bool {
        self.start <= id && id <= self.end
    }

    pub(crate) fn is_valid(self) -> bool {
        self.start <= self.end && self.end <= MAX_BIN_ID
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoeSnapshotContext {
    pub block_hash: B256,
    pub block_timestamp: u64,
}

impl MoeSnapshotContext {
    pub const fn new(block_hash: B256, block_timestamp: u64) -> Self {
        Self {
            block_hash,
            block_timestamp,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoeSnapshotSyncConfig {
    pub bins_radius: u32,
    pub bins_per_request: u32,
}

impl Default for MoeSnapshotSyncConfig {
    fn default() -> Self {
        Self {
            bins_radius: 50,
            bins_per_request: 15,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoeSnapshot {
    pub slot0: MoeSlot0,
    pub bins: HashMap<u32, BinReserve>,
    pub queried_ranges: Vec<MoeBinRange>,
    pub block_hash: B256,
    pub block_timestamp: u64,
}

impl MoeSnapshot {
    pub(crate) fn assembling(slot0: MoeSlot0, context: MoeSnapshotContext) -> Self {
        Self {
            slot0,
            bins: HashMap::new(),
            queried_ranges: Vec::new(),
            block_hash: context.block_hash,
            block_timestamp: context.block_timestamp,
        }
    }

    pub fn new(
        slot0: MoeSlot0,
        bins: HashMap<u32, BinReserve>,
        queried_ranges: Vec<MoeBinRange>,
        context: MoeSnapshotContext,
    ) -> Result<Self, MoeError> {
        let snapshot = Self {
            slot0,
            bins,
            queried_ranges,
            block_hash: context.block_hash,
            block_timestamp: context.block_timestamp,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn validate(&self) -> Result<(), MoeError> {
        Self::validate_slot0(&self.slot0)?;
        if !self.covers(self.slot0.active_id)
            || self.queried_ranges.is_empty()
            || self.queried_ranges.iter().any(|range| !range.is_valid())
            || self.bins.keys().any(|id| !self.covers(*id))
        {
            return Err(MoeError::InvalidSnapshot);
        }
        Ok(())
    }

    pub(crate) fn validate_slot0(slot0: &MoeSlot0) -> Result<(), MoeError> {
        if slot0.active_id > MAX_BIN_ID
            || slot0.bin_step == 0
            || slot0.variable_fee_control > 0xFF_FFFF
            || slot0.max_volatility_acc > 0xFF_FFFF
            || slot0.volatility_accumulator > 0xFF_FFFF
            || slot0.volatility_reference > 0xFF_FFFF
            || slot0.id_reference > 0xFF_FFFF
            || slot0.timestamp > U256::from((1u64 << 40) - 1)
        {
            return Err(MoeError::InvalidSnapshot);
        }
        Ok(())
    }

    pub fn covers(&self, id: u32) -> bool {
        self.queried_ranges.iter().any(|range| range.contains(id))
    }

    pub(crate) fn replace_range(
        &mut self,
        range: MoeBinRange,
        values: &[(u128, u128)],
    ) -> Result<(), MoeError> {
        if !range.is_valid() || values.len() != range_len(range) {
            return Err(MoeError::MalformedBatchResponse {
                expected: range_len(range),
                actual: values.len(),
            });
        }

        self.bins.retain(|id, _| !range.contains(*id));
        for (offset, (reserve_x, reserve_y)) in values.iter().enumerate() {
            if *reserve_x != 0 || *reserve_y != 0 {
                let id = range.start + offset as u32;
                self.bins.insert(
                    id,
                    BinReserve {
                        reserve_x: *reserve_x,
                        reserve_y: *reserve_y,
                    },
                );
            }
        }
        self.queried_ranges.push(range);
        self.normalize_ranges();
        Ok(())
    }

    fn normalize_ranges(&mut self) {
        self.queried_ranges
            .sort_unstable_by_key(|range| range.start);
        let mut normalized: Vec<MoeBinRange> = Vec::with_capacity(self.queried_ranges.len());
        for range in self.queried_ranges.drain(..) {
            if let Some(last) = normalized.last_mut() {
                if range.start <= last.end.saturating_add(1) {
                    last.end = last.end.max(range.end);
                    continue;
                }
            }
            normalized.push(range);
        }
        self.queried_ranges = normalized;
    }
}

fn range_len(range: MoeBinRange) -> usize {
    (u64::from(range.end) - u64::from(range.start) + 1) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot0() -> MoeSlot0 {
        MoeSlot0 {
            active_id: 8_388_608,
            bin_step: 20,
            reserve_x: 1,
            reserve_y: 1,
            volatility_accumulator: 0,
            volatility_reference: 0,
            id_reference: 8_388_608,
            timestamp: U256::from(1_700_000_000u64),
            base_factor: 0,
            filter_period: 0,
            decay_period: 0,
            reduction_factor: 0,
            variable_fee_control: 0,
            protocol_share_bps: 0,
            max_volatility_acc: 0,
        }
    }

    #[test]
    fn resync_removes_depleted_bins_without_erasing_other_ranges() {
        let context = MoeSnapshotContext::new(B256::repeat_byte(1), 1_700_000_001);
        let mut snapshot = MoeSnapshot::new(
            slot0(),
            HashMap::new(),
            vec![MoeBinRange::new(8_388_608, 8_388_608)],
            context,
        )
        .unwrap();
        snapshot
            .replace_range(MoeBinRange::new(8_388_608, 8_388_608), &[(10, 10)])
            .unwrap();
        snapshot
            .replace_range(MoeBinRange::new(8_388_609, 8_388_609), &[(20, 20)])
            .unwrap();
        snapshot
            .replace_range(MoeBinRange::new(8_388_608, 8_388_608), &[(0, 0)])
            .unwrap();

        assert!(!snapshot.bins.contains_key(&8_388_608));
        assert_eq!(snapshot.bins[&8_388_609].reserve_x, 20);
    }

    #[test]
    fn snapshot_requires_active_id_coverage() {
        let result = MoeSnapshot::new(
            slot0(),
            HashMap::new(),
            vec![MoeBinRange::new(8_388_609, 8_388_609)],
            MoeSnapshotContext::new(B256::repeat_byte(1), 1_700_000_000),
        );

        assert!(matches!(result, Err(MoeError::InvalidSnapshot)));
    }
}
