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
    snapshot::MAX_BIN_ID, MoeBinRange, MoeError, MoeSlot0, MoeSlot0BatchResponse, MoeSnapshot,
    MoeSnapshotContext, MoeSnapshotSyncConfig,
};

/// True when an RPC/contract error looks like CREATE bytecode size rejection.
///
/// Mantle public nodes have returned `CreateContractSizeLimit` on concurrent
/// Moe bin-data batch CREATE eth_calls (WHI-862). Retry path splits the chunk.
fn is_create_size_limit(err: &AMMError) -> bool {
    let s = err.to_string();
    s.contains("CreateContractSizeLimit") || s.contains("max code size exceeded")
}

fn bin_data_request(
    pair: Address,
    range: MoeBinRange,
) -> GetMoeLBPairBinDataBatchRequest::BinDataRequest {
    GetMoeLBPairBinDataBatchRequest::BinDataRequest {
        pair,
        ids: (range.start..=range.end)
            .map(U256::from)
            .map(|id| id.to())
            .collect(),
    }
}

async fn call_slot0_batch<N, P>(
    provider: P,
    block: BlockId,
    addresses: Vec<Address>,
) -> Result<Vec<MoeSlot0BatchResponse>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let data = GetMoeLBPairSlot0BatchRequest::deploy_builder(provider, addresses)
        .call_raw()
        .block(block)
        .await?;
    Ok(MoeSlot0BatchResponse::decode_batch(&data)?)
}

async fn call_bin_data_batch<N, P>(
    provider: P,
    block: BlockId,
    requests: Vec<GetMoeLBPairBinDataBatchRequest::BinDataRequest>,
) -> Result<Vec<Vec<(u128, u128)>>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    let data = GetMoeLBPairBinDataBatchRequest::deploy_builder(provider, requests)
        .call_raw()
        .block(block)
        .await?;
    Ok(<Vec<Vec<(u128, u128)>>>::abi_decode(&data)?)
}

/// One bin-range fetch unit with the pool address already resolved (so the async
/// CREATE call does not need to borrow the AMM slice).
#[derive(Debug, Clone, Copy)]
struct ResolvedBinQuery {
    query: BinQuery,
    pair: Address,
}

/// Fetch bin data for a resolved query chunk. On CREATE-size rejection, fall
/// back to one CREATE per query so a single oversized batch cannot sink sync.
async fn fetch_bin_data_chunk<N, P>(
    provider: P,
    block: BlockId,
    queries: &[ResolvedBinQuery],
) -> Result<Vec<(BinQuery, Vec<(u128, u128)>)>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let requests: Vec<_> = queries
        .iter()
        .map(|q| bin_data_request(q.pair, q.query.range))
        .collect();
    match call_bin_data_batch(provider.clone(), block, requests).await {
        Ok(decoded) => {
            if decoded.len() != queries.len() {
                return Err(MoeError::MalformedBatchResponse {
                    expected: queries.len(),
                    actual: decoded.len(),
                }
                .into());
            }
            Ok(queries.iter().map(|q| q.query).zip(decoded).collect())
        }
        Err(e) if is_create_size_limit(&e) && queries.len() > 1 => {
            tracing::warn!(
                target: "amms.moe.sync",
                queries = queries.len(),
                error = %e,
                "Moe bin batch hit CREATE size limit; retrying one query at a time"
            );
            let mut out = Vec::with_capacity(queries.len());
            for q in queries {
                let single_req = vec![bin_data_request(q.pair, q.query.range)];
                let decoded =
                    call_bin_data_batch(provider.clone(), block, single_req).await?;
                if decoded.len() != 1 {
                    return Err(MoeError::MalformedBatchResponse {
                        expected: 1,
                        actual: decoded.len(),
                    }
                    .into());
                }
                out.push((q.query, decoded.into_iter().next().unwrap()));
            }
            Ok(out)
        }
        Err(e) => Err(e),
    }
}
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
    // Sequential slot0 waves: public Mantle nodes have rejected concurrent CREATE
    // eth_calls with CreateContractSizeLimit under rate pressure (WHI-862).
    const SLOT0_CHUNK: usize = 8;
    let mut slots = HashMap::with_capacity(targets.len());
    for target_chunk in targets.chunks(SLOT0_CHUNK) {
        let indices: Vec<usize> = target_chunk.iter().map(|(index, _)| *index).collect();
        let addresses: Vec<Address> = target_chunk.iter().map(|(_, address)| *address).collect();
        let decoded = match call_slot0_batch(provider.clone(), block, addresses.clone()).await {
            Ok(d) => d,
            Err(e) if is_create_size_limit(&e) && addresses.len() > 1 => {
                tracing::warn!(
                    target: "amms.moe.sync",
                    n = addresses.len(),
                    error = %e,
                    "Moe slot0 batch hit CREATE size limit; retrying one pool at a time"
                );
                let mut singles = Vec::with_capacity(addresses.len());
                for addr in &addresses {
                    let one = call_slot0_batch(provider.clone(), block, vec![*addr]).await?;
                    singles.extend(one);
                }
                singles
            }
            Err(e) => return Err(e),
        };
        if decoded.len() != indices.len() {
            return Err(MoeError::MalformedBatchResponse {
                expected: indices.len(),
                actual: decoded.len(),
            }
            .into());
        }
        for (index, slot) in indices.into_iter().zip(decoded) {
            slots.insert(
                index,
                SlotData {
                    token_x: slot.token_x,
                    token_y: slot.token_y,
                    slot0: slot.slot0,
                },
            );
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
    // Bounded concurrency for bin CREATE eth_calls (WHI-862).
    // One request per CREATE (chunk=1) avoids CreateContractSizeLimit; a small
    // wave of concurrent singles keeps tip-refresh latency usable on free-tier
    // Mantle RPC without reopening the all-futures-at-once storm.
    const BIN_CHUNK: usize = 1;
    const BIN_WAVE: usize = 4;

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

    let resolved: Vec<ResolvedBinQuery> = bin_queries
        .into_iter()
        .map(|query| ResolvedBinQuery {
            pair: amms[query.pair_index].address(),
            query,
        })
        .collect();

    let query_chunks: Vec<&[ResolvedBinQuery]> = resolved.chunks(BIN_CHUNK).collect();
    for wave in query_chunks.chunks(BIN_WAVE) {
        let mut bin_futures = FuturesUnordered::new();
        for query_chunk in wave {
            let queries = query_chunk.to_vec();
            let provider = provider.clone();
            bin_futures.push(async move { fetch_bin_data_chunk(provider, block, &queries).await });
        }
        while let Some(result) = bin_futures.next().await {
            for (query, data) in result? {
                snapshots
                    .get_mut(&query.pair_index)
                    .ok_or(MoeError::InvalidSnapshot)?
                    .replace_range(query.range, &data)?;
            }
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
