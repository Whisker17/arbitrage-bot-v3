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
use std::{collections::HashMap, future::Future, time::Duration};

use super::{
    snapshot::MAX_BIN_ID, MoeBinRange, MoeError, MoeSlot0, MoeSlot0BatchResponse, MoeSnapshot,
    MoeSnapshotContext, MoeSnapshotSyncConfig,
};

/// Default max attempts for a single CreateContractSizeLimit recovery budget
/// (WHI-921). Applies to both the single-item floor and the under-pressure
/// same-chunk retry path.
pub const CREATE_SIZE_MAX_ATTEMPTS: u32 = 5;
/// Base backoff between CreateContractSizeLimit retries (milliseconds).
pub const CREATE_SIZE_BASE_BACKOFF_MS: u64 = 200;
/// Delay between halved sub-chunks when splitting a CREATE batch (milliseconds).
pub const CREATE_SIZE_SPLIT_PACE_MS: u64 = 50;
/// Window in which a recent 429 / rate-limit event counts as "under pressure".
///
/// Sized to cover a full CreateContractSizeLimit pressure budget
/// (`base * (1+2+4+8)` ≈ 3 s at default 200 ms, plus margin) so re-sampling
/// does not flip to "halve" mid-budget while the endpoint is still hot.
pub const RATE_PRESSURE_WINDOW: Duration = Duration::from_secs(15);

/// Tunables for CreateContractSizeLimit recovery (WHI-921).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateSizeRetryConfig {
    pub max_attempts: u32,
    pub base_backoff_ms: u64,
    pub split_pace_ms: u64,
}

impl Default for CreateSizeRetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: CREATE_SIZE_MAX_ATTEMPTS,
            base_backoff_ms: CREATE_SIZE_BASE_BACKOFF_MS,
            split_pace_ms: CREATE_SIZE_SPLIT_PACE_MS,
        }
    }
}

/// True when an RPC/contract error looks like CREATE bytecode size rejection.
///
/// Mantle public nodes have returned `CreateContractSizeLimit` on concurrent
/// Moe bin-data batch CREATE eth_calls under rate pressure (WHI-862 / WHI-921).
fn is_create_size_limit(err: &AMMError) -> bool {
    let s = err.to_string();
    s.contains("CreateContractSizeLimit") || s.contains("max code size exceeded")
}

fn create_size_backoff(attempt: u32, config: CreateSizeRetryConfig) -> Duration {
    // attempt is 1-based after a failure; exponential with a small cap.
    let shift = attempt.saturating_sub(1).min(4);
    let ms = config.base_backoff_ms.saturating_mul(1u64 << shift);
    Duration::from_millis(ms.min(5_000))
}

/// Run a batch CREATE eth_call with CreateContractSizeLimit recovery (WHI-921).
///
/// Policy:
/// 1. Success → return decoded results in input order.
/// 2. `CreateContractSizeLimit` on a **single** item → sleep + retry the same
///    item up to `max_attempts`, then fail with [`MoeError::CreateSizeRetryExhausted`]
///    naming **that** pool. **Does not** abort without a floor.
/// 3. `CreateContractSizeLimit` **under rate pressure** (`under_pressure()` true
///    when re-sampled) → sleep + retry the **same** chunk (no fan-out to N
///    singles). After the pressure budget is spent, fall through to paced half
///    so cold-start can still complete once the endpoint cools.
/// 4. `CreateContractSizeLimit` without rate pressure and `len > 1` → sleep,
///    **halve** the chunk, process halves sequentially with pace delay.
///
/// `under_pressure` is re-invoked on each failure so a mid-batch 429 storm is
/// visible (WHI-921 discriminator). Callers pass a closure over
/// [`crate::rpc_rate_pressure::under_rpc_rate_pressure`] (or a test double).
///
/// `pool_of` extracts the pool address for exhaustion diagnostics from a chunk
/// item (slot0: identity; bin_data: pair address).
pub async fn with_create_size_resilience<I, T, F, Fut, P, R>(
    items: Vec<I>,
    config: CreateSizeRetryConfig,
    mut under_pressure: P,
    path: &'static str,
    pool_of: R,
    mut call: F,
) -> Result<Vec<T>, AMMError>
where
    I: Clone,
    F: FnMut(Vec<I>) -> Fut,
    Fut: Future<Output = Result<Vec<T>, AMMError>>,
    P: FnMut() -> bool,
    R: Fn(&I) -> Option<Address>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // LIFO work stack of pending chunks. Halves are pushed right-then-left so
    // left processes first and `out` stays in original order.
    let mut pending: Vec<Vec<I>> = vec![items];
    let mut out: Vec<T> = Vec::new();

    while let Some(chunk) = pending.pop() {
        let mut attempts: u32 = 0;
        loop {
            match call(chunk.clone()).await {
                Ok(decoded) => {
                    out.extend(decoded);
                    break;
                }
                Err(e) if is_create_size_limit(&e) => {
                    attempts = attempts.saturating_add(1);
                    let chunk_pool = chunk.first().and_then(|item| pool_of(item));

                    if chunk.len() == 1 {
                        if attempts >= config.max_attempts {
                            return Err(MoeError::CreateSizeRetryExhausted {
                                attempts,
                                chunk_len: 1,
                                pool: chunk_pool,
                                path,
                            }
                            .into());
                        }
                        tracing::warn!(
                            target: "amms.moe.sync",
                            attempt = attempts,
                            max_attempts = config.max_attempts,
                            pool = ?chunk_pool,
                            path,
                            error = %e,
                            "Moe CREATE size limit on single-item call; backing off"
                        );
                        tokio::time::sleep(create_size_backoff(attempts, config)).await;
                        continue;
                    }

                    // Re-sample pressure each failure (not a one-shot snapshot).
                    let pressure = under_pressure();
                    if pressure && attempts < config.max_attempts {
                        // Rate-pressure regime: splitting increases request count
                        // against an endpoint already returning 429. Slow down only.
                        tracing::warn!(
                            target: "amms.moe.sync",
                            attempt = attempts,
                            max_attempts = config.max_attempts,
                            chunk_len = chunk.len(),
                            path,
                            error = %e,
                            "Moe CREATE size limit under rate pressure; retrying same chunk after backoff (no fan-out)"
                        );
                        tokio::time::sleep(create_size_backoff(attempts, config)).await;
                        continue;
                    }
                    if pressure {
                        tracing::warn!(
                            target: "amms.moe.sync",
                            attempt = attempts,
                            chunk_len = chunk.len(),
                            path,
                            "Moe CREATE size limit pressure budget spent; falling through to paced half"
                        );
                    }

                    // No recent 429s (or pressure budget spent) → halve + pace.
                    let mid = chunk.len() / 2;
                    // mid >= 1 because chunk.len() > 1.
                    let (left, right) = chunk.split_at(mid);
                    tracing::warn!(
                        target: "amms.moe.sync",
                        chunk_len = chunk.len(),
                        left = left.len(),
                        right = right.len(),
                        path,
                        error = %e,
                        "Moe CREATE size limit; halving chunk with pace"
                    );
                    tokio::time::sleep(Duration::from_millis(config.split_pace_ms)).await;
                    pending.push(right.to_vec());
                    pending.push(left.to_vec());
                    break;
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(out)
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

/// Fetch bin data for a resolved query chunk with CreateContractSizeLimit
/// recovery (WHI-921).
async fn fetch_bin_data_chunk<N, P>(
    provider: P,
    block: BlockId,
    queries: &[ResolvedBinQuery],
    config: CreateSizeRetryConfig,
) -> Result<Vec<(BinQuery, Vec<(u128, u128)>)>, AMMError>
where
    N: Network,
    P: Provider<N> + Clone,
{
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let items: Vec<ResolvedBinQuery> = queries.to_vec();
    let provider = provider.clone();

    let decoded = with_create_size_resilience(
        items,
        config,
        || crate::rpc_rate_pressure::under_rpc_rate_pressure(RATE_PRESSURE_WINDOW),
        "bin_data",
        |q: &ResolvedBinQuery| Some(q.pair),
        |chunk| {
            let provider = provider.clone();
            async move {
                let requests: Vec<_> = chunk
                    .iter()
                    .map(|q| bin_data_request(q.pair, q.query.range))
                    .collect();
                let expected = chunk.len();
                let decoded = call_bin_data_batch(provider, block, requests).await?;
                if decoded.len() != expected {
                    return Err(MoeError::MalformedBatchResponse {
                        expected,
                        actual: decoded.len(),
                    }
                    .into());
                }
                Ok(chunk
                    .into_iter()
                    .zip(decoded)
                    .map(|(q, d)| (q.query, d))
                    .collect::<Vec<_>>())
            }
        },
    )
    .await?;
    Ok(decoded)
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
    // WHI-921: recovery backs off / halves instead of fanning out to N singles.
    const SLOT0_CHUNK: usize = 8;
    let create_size_cfg = CreateSizeRetryConfig::default();
    let mut slots = HashMap::with_capacity(targets.len());
    for target_chunk in targets.chunks(SLOT0_CHUNK) {
        let indices: Vec<usize> = target_chunk.iter().map(|(index, _)| *index).collect();
        let addresses: Vec<Address> = target_chunk.iter().map(|(_, address)| *address).collect();
        let provider = provider.clone();
        let expected = indices.len();
        let decoded = with_create_size_resilience(
            addresses,
            create_size_cfg,
            || crate::rpc_rate_pressure::under_rpc_rate_pressure(RATE_PRESSURE_WINDOW),
            "slot0",
            |addr: &Address| Some(*addr),
            |chunk| {
                let provider = provider.clone();
                async move { call_slot0_batch(provider, block, chunk).await }
            },
        )
        .await?;
        if decoded.len() != expected {
            return Err(MoeError::MalformedBatchResponse {
                expected,
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
            bin_futures.push(async move {
                fetch_bin_data_chunk(provider, block, &queries, create_size_cfg).await
            });
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    fn create_size_err() -> AMMError {
        AMMError::TransportError(alloy::transports::TransportErrorKind::custom_str(
            "server returned an error response: error code -32003: EVM error: CreateContractSizeLimit",
        ))
    }

    fn other_err() -> AMMError {
        AMMError::TransportError(alloy::transports::TransportErrorKind::custom_str(
            "connection reset by peer",
        ))
    }

    /// Single-address CreateContractSizeLimit is retried with backoff and does
    /// not abort when later attempts succeed (WHI-921 acceptance).
    #[tokio::test]
    async fn single_item_create_size_retries_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let cfg = CreateSizeRetryConfig {
            max_attempts: 5,
            base_backoff_ms: 10,
            split_pace_ms: 1,
        };
        let pool = address!("0x1111111111111111111111111111111111111111");

        let result = with_create_size_resilience(
            vec![pool],
            cfg,
            || false,
            "slot0",
            |a: &Address| Some(*a),
            move |chunk| {
                let calls_c = Arc::clone(&calls_c);
                async move {
                    let n = calls_c.fetch_add(1, Ordering::SeqCst);
                    if n < 3 {
                        Err(create_size_err())
                    } else {
                        Ok(chunk.iter().map(|a| *a).collect::<Vec<_>>())
                    }
                }
            },
        )
        .await
        .expect("must succeed after floor retries");

        assert_eq!(result, vec![pool]);
        assert_eq!(calls.load(Ordering::SeqCst), 4); // 3 failures + 1 success
    }

    /// Exhausted single-item budget fails with a named CreateSizeRetryExhausted.
    #[tokio::test]
    async fn single_item_create_size_exhausts_with_named_error() {
        let cfg = CreateSizeRetryConfig {
            max_attempts: 3,
            base_backoff_ms: 1,
            split_pace_ms: 1,
        };
        let pool = address!("0x2222222222222222222222222222222222222222");

        let err = with_create_size_resilience(
            vec![pool],
            cfg,
            || false,
            "slot0",
            |a: &Address| Some(*a),
            move |_chunk| async move { Err::<Vec<Address>, _>(create_size_err()) },
        )
        .await
        .expect_err("must exhaust");

        let msg = err.to_string();
        assert!(
            msg.contains("CREATE-size retry exhausted"),
            "expected exhausted message, got: {msg}"
        );
        assert!(
            msg.contains("0x2222") || msg.contains("22222222"),
            "expected pool in message, got: {msg}"
        );
        match err {
            AMMError::MoeError(MoeError::CreateSizeRetryExhausted {
                attempts,
                chunk_len,
                pool: p,
                path,
            }) => {
                assert_eq!(attempts, 3);
                assert_eq!(chunk_len, 1);
                assert_eq!(p, Some(pool));
                assert_eq!(path, "slot0");
            }
            other => panic!("unexpected error variant: {other}"),
        }
    }

    /// Under rate pressure, CreateContractSizeLimit must not fan out: request
    /// count stays one-per-attempt on the same chunk size (WHI-921 acceptance).
    #[tokio::test]
    async fn under_pressure_does_not_increase_request_count_via_fanout() {
        let call_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let sizes_c = Arc::clone(&call_sizes);
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let cfg = CreateSizeRetryConfig {
            max_attempts: 4,
            base_backoff_ms: 1,
            split_pace_ms: 1,
        };
        let pools: Vec<Address> = (0..8u8)
            .map(|i| Address::with_last_byte(i + 1))
            .collect();

        let result = with_create_size_resilience(
            pools.clone(),
            cfg,
            || true, // under pressure
            "slot0",
            |a: &Address| Some(*a),
            move |chunk| {
                let sizes_c = Arc::clone(&sizes_c);
                let calls_c = Arc::clone(&calls_c);
                async move {
                    sizes_c.lock().unwrap().push(chunk.len());
                    let n = calls_c.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(create_size_err())
                    } else {
                        Ok(chunk)
                    }
                }
            },
        )
        .await
        .expect("must succeed without fan-out");

        assert_eq!(result.len(), 8);
        let sizes = call_sizes.lock().unwrap().clone();
        // Every attempt must keep the original chunk size (8) — never 1..N fan-out.
        assert!(
            sizes.iter().all(|&s| s == 8),
            "under pressure must not split; call sizes were {sizes:?}"
        );
        // 2 failures + 1 success = 3 calls (not 1 + 8 singles).
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(sizes.len(), 3);
    }

    /// Without pressure, an oversized batch is halved (not N singles at once).
    #[tokio::test]
    async fn without_pressure_halves_rather_than_full_fanout() {
        let call_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let sizes_c = Arc::clone(&call_sizes);
        let cfg = CreateSizeRetryConfig {
            max_attempts: 5,
            base_backoff_ms: 1,
            split_pace_ms: 1,
        };
        let pools: Vec<Address> = (0..8u8)
            .map(|i| Address::with_last_byte(i + 1))
            .collect();

        // Fail only when chunk len > 4; succeed at 4 or below.
        let result = with_create_size_resilience(
            pools.clone(),
            cfg,
            || false,
            "slot0",
            |a: &Address| Some(*a),
            move |chunk| {
                let sizes_c = Arc::clone(&sizes_c);
                async move {
                    sizes_c.lock().unwrap().push(chunk.len());
                    if chunk.len() > 4 {
                        Err(create_size_err())
                    } else {
                        Ok(chunk)
                    }
                }
            },
        )
        .await
        .expect("halving must succeed");

        assert_eq!(result.len(), 8);
        let sizes = call_sizes.lock().unwrap().clone();
        // First call is 8 (fails), then two of 4 (succeed). Never eight 1s.
        assert_eq!(sizes.first().copied(), Some(8));
        assert!(
            sizes.iter().skip(1).all(|&s| s == 4),
            "expected halves of 4, got {sizes:?}"
        );
        assert!(
            !sizes.iter().any(|&s| s == 1),
            "must not fan out to width-1 immediately: {sizes:?}"
        );
        assert_eq!(sizes.len(), 3);
    }

    /// Non-create-size errors still propagate immediately.
    #[tokio::test]
    async fn non_create_size_error_propagates() {
        let err = with_create_size_resilience(
            vec![Address::with_last_byte(1)],
            CreateSizeRetryConfig::default(),
            || false,
            "slot0",
            |a: &Address| Some(*a),
            |_chunk| async move { Err::<Vec<Address>, _>(other_err()) },
        )
        .await
        .expect_err("must propagate");
        assert!(err.to_string().contains("connection reset"));
    }

    #[test]
    fn is_create_size_limit_matches_observed_shapes() {
        assert!(is_create_size_limit(&create_size_err()));
        assert!(is_create_size_limit(&AMMError::TransportError(
            alloy::transports::TransportErrorKind::custom_str("max code size exceeded")
        )));
        assert!(!is_create_size_limit(&other_err()));
    }
}
