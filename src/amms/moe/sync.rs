use crate::amms::{
    amm::{AutomatedMarketMaker, AMM},
    error::AMMError,
    GetMoeLBPairBinDataBatchRequest, GetMoeLBPairSlot0BatchRequest, Token,
};
use alloy::{
    eips::BlockId,
    network::Network,
    primitives::{Address, U256},
    providers::Provider,
    sol_types::SolValue,
};
use futures::{stream::FuturesUnordered, StreamExt};
use std::collections::HashMap;

use super::{
    MoeBinRange, MoeError, MoeSlot0, MoeSnapshot, MoeSnapshotContext, MoeSnapshotSyncConfig,
};
const MAX_BIN_ID: u32 = 0xFF_FFFF;
#[derive(Debug, Clone)]
struct SlotData {
    token_x: Address,
    token_y: Address,
    slot0: MoeSlot0,
}
#[derive(Debug, Clone, Copy)]
struct BinQuery {
    pair_index: usize,
    range: MoeBinRange,
}
type Slot0Response = (
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
pub async fn sync_moe_snapshots_batch<N, P>(
    amms: &mut [AMM],
    block: BlockId,
    provider: P,
    context: MoeSnapshotContext,
    config: MoeSnapshotSyncConfig,
) -> Result<(), AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    if config.bins_per_request == 0 {
        return Err(MoeError::InvalidSnapshot.into());
    }
    let BlockId::Hash(hash) = block else {
        return Err(MoeError::InvalidSnapshot.into());
    };
    if hash.block_hash != context.block_hash {
        return Err(MoeError::InvalidSnapshot.into());
    }
    let targets: Vec<(usize, Address)> = amms
        .iter()
        .enumerate()
        .filter_map(|(index, amm)| match amm {
            AMM::MoeLbPair(pair) => Some((index, pair.address)),
            _ => None,
        })
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    let mut slot_futures = FuturesUnordered::new();
    for target_chunk in targets.chunks(255) {
        let indices: Vec<usize> = target_chunk.iter().map(|(index, _)| *index).collect();
        let addresses: Vec<Address> = target_chunk.iter().map(|(_, address)| *address).collect();
        let provider = provider.clone();
        slot_futures.push(async move {
            let data = GetMoeLBPairSlot0BatchRequest::deploy_builder(provider, addresses)
                .call_raw()
                .block(block)
                .await?;
            let decoded = <Vec<Slot0Response>>::abi_decode(&data)?;
            if decoded.len() != indices.len() {
                return Err::<Vec<(usize, SlotData)>, AMMError>(
                    MoeError::MalformedBatchResponse {
                        expected: indices.len(),
                        actual: decoded.len(),
                    }
                    .into(),
                );
            }
            let slots = indices
                .into_iter()
                .zip(decoded)
                .map(|(index, slot)| {
                    if !(slot.17 && slot.18 && slot.19 && slot.20 && slot.21 && slot.22 && slot.23)
                    {
                        return Err::<(usize, SlotData), AMMError>(
                            MoeError::IncompleteState.into(),
                        );
                    }
                    Ok((
                        index,
                        SlotData {
                            token_x: slot.0,
                            token_y: slot.1,
                            slot0: MoeSlot0 {
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
                            },
                        },
                    ))
                })
                .collect::<Result<Vec<_>, AMMError>>()?;
            Ok(slots)
        });
    }
    let mut slots = HashMap::with_capacity(targets.len());
    while let Some(result) = slot_futures.next().await {
        for (index, slot) in result? {
            if slot.token_x == Address::ZERO
                || slot.token_y == Address::ZERO
                || slot.slot0.bin_step == 0
            {
                return Err(MoeError::IncompleteState.into());
            }
            slots.insert(index, slot);
        }
    }
    if slots.len() != targets.len() {
        return Err(MoeError::MalformedBatchResponse {
            expected: targets.len(),
            actual: slots.len(),
        }
        .into());
    }
    let mut bin_queries = Vec::new();
    for (index, _) in &targets {
        let slot = slots.get(index).ok_or(MoeError::InvalidSnapshot)?;
        let start = slot.slot0.active_id.saturating_sub(config.bins_radius);
        let end = slot
            .slot0
            .active_id
            .saturating_add(config.bins_radius)
            .min(MAX_BIN_ID);
        let mut range_start = start;
        loop {
            let range_end = range_start
                .saturating_add(config.bins_per_request - 1)
                .min(end);
            bin_queries.push(BinQuery {
                pair_index: *index,
                range: MoeBinRange::new(range_start, range_end),
            });
            if range_end == end {
                break;
            }
            range_start = range_end + 1;
        }
    }
    let mut bin_futures = FuturesUnordered::new();
    for query_chunk in bin_queries.chunks(5) {
        let queries = query_chunk.to_vec();
        let requests = queries
            .iter()
            .map(|query| {
                let pair = amms[query.pair_index].address();
                GetMoeLBPairBinDataBatchRequest::BinDataRequest {
                    pair,
                    ids: (query.range.start..=query.range.end)
                        .map(U256::from)
                        .map(|id| id.to())
                        .collect(),
                }
            })
            .collect();
        let provider = provider.clone();
        bin_futures.push(async move {
            let data = GetMoeLBPairBinDataBatchRequest::deploy_builder(provider, requests)
                .call_raw()
                .block(block)
                .await?;
            let decoded = <Vec<Vec<(u128, u128)>>>::abi_decode(&data)?;
            if decoded.len() != queries.len() {
                return Err(MoeError::MalformedBatchResponse {
                    expected: queries.len(),
                    actual: decoded.len(),
                }
                .into());
            }
            Ok::<Vec<(BinQuery, Vec<(u128, u128)>)>, AMMError>(
                queries.into_iter().zip(decoded).collect(),
            )
        });
    }

    let mut snapshots: HashMap<usize, MoeSnapshot> = targets
        .iter()
        .map(|(index, _)| {
            let slot = slots.get(index).ok_or(MoeError::InvalidSnapshot)?;
            Ok::<(usize, MoeSnapshot), AMMError>((
                *index,
                MoeSnapshot::assembling(slot.slot0.clone(), context),
            ))
        })
        .collect::<Result<_, _>>()?;
    while let Some(result) = bin_futures.next().await {
        for (query, data) in result? {
            snapshots
                .get_mut(&query.pair_index)
                .ok_or(MoeError::InvalidSnapshot)?
                .replace_range(query.range, &data)?;
        }
    }

    for (index, _) in targets {
        let slot = slots.remove(&index).ok_or(MoeError::InvalidSnapshot)?;
        let snapshot = snapshots.remove(&index).ok_or(MoeError::InvalidSnapshot)?;
        snapshot.validate()?;
        let AMM::MoeLbPair(pair) = &mut amms[index] else {
            return Err(MoeError::InvalidSnapshot.into());
        };
        pair.token_x = Token::new_with_decimals(slot.token_x, pair.token_x.decimals);
        pair.token_y = Token::new_with_decimals(slot.token_y, pair.token_y.decimals);
        pair.apply_snapshot(snapshot);
    }
    Ok(())
}
