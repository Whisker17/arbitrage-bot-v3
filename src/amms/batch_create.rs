//! Batch CREATE eth_call payload budgeting and size-split recovery
//! (WHI-925 / WHI-929).
//!
//! Batch-request contracts are not deployed. Callers send their **creation
//! bytecode** as an `eth_call`; the constructor body runs and `return`s ABI-
//! encoded results. That return becomes the CREATE "code" and is subject to
//! EIP-170's 24 576-byte max code size (`CreateContractSizeLimit` /
//! `max code size exceeded` on Mantle).
//!
//! Chunk by **expected return bytes** when per-item size is fixed. When a
//! batch still hits the size limit — or when per-item size is unknowable
//! (tick-bitmap / tick-data) — **halve and retry** down to a single item
//! (no time backoff — size is not rate pressure; see WHI-921 for the Moe path
//! that *does* use backoff under 429s). A single item that still overflows can
//! optionally be **bisected** (narrower word/tick range) before failing with
//! a named [`BatchContractError::CreateSizeSinglePool`].

use std::future::Future;

use alloy::primitives::Address;

use super::error::{AMMError, BatchContractError};

/// EIP-170 maximum contract code size in bytes.
pub const EIP170_MAX_CODE_SIZE: usize = 24_576;

/// Conservative fraction of EIP-170 used as the return-payload budget
/// (numerator / denominator). Half leaves headroom for encoding variance and
/// node-specific enforcement.
pub const CREATE_RETURN_BUDGET_NUM: usize = 1;
pub const CREATE_RETURN_BUDGET_DEN: usize = 2;

/// ABI word size (static types pad to 32 bytes).
pub const ABI_WORD: usize = 32;

/// Overhead of Solidity `abi.encode(T[])`: offset word + length word.
pub const ABI_DYNAMIC_ARRAY_OVERHEAD: usize = 2 * ABI_WORD;

// ---------------------------------------------------------------------------
// Per-request return sizes (from the batch-contract return tuples)
// ---------------------------------------------------------------------------

/// V3 / Agni `Slot0Data`: `(int24 tick, uint128 liquidity, uint256 sqrtPrice)`.
/// Three static ABI words → 96 bytes per pool.
pub const V3_SLOT0_RETURN_WORDS: usize = 3;
pub const V3_SLOT0_RETURN_BYTES_PER_POOL: usize = V3_SLOT0_RETURN_WORDS * ABI_WORD;

/// `GetTokenDecimalsBatchRequest` returns `uint8[]` → one word per token.
pub const TOKEN_DECIMALS_RETURN_BYTES_PER: usize = ABI_WORD;

/// `GetUniswapV2PairsBatchRequest` returns `address[]` → one word per pair.
pub const V2_PAIRS_RETURN_BYTES_PER: usize = ABI_WORD;

/// `GetUniswapV2PoolDataBatchRequest` returns
/// `(address, address, uint128, uint128, uint32, uint32)` → six words.
pub const V2_POOL_DATA_RETURN_BYTES_PER: usize = 6 * ABI_WORD;

/// Moe `Slot0Data`: 17 value fields + 7 bools = 24 static ABI words
/// (`GetMoeLBPairSlot0BatchRequest.sol`).
///
/// Used by `moe/mod.rs` size-derived chunking. Live snapshot sync uses
/// `moe::sync::with_create_size_resilience` (WHI-921) with a smaller fixed
/// wave size; both must stay within this budget.
pub const MOE_SLOT0_RETURN_BYTES_PER: usize = 24 * ABI_WORD;

/// V3 / Agni tick-data `Info`: `(bool, uint128, int128)` → 3 ABI words.
/// Variable-size batches still rely on split-on-failure (density unknown);
/// this constant only documents the lower bound for a single tick.
pub const V3_TICK_DATA_RETURN_BYTES_PER_TICK: usize = 3 * ABI_WORD;

/// V3 / Agni tick-bitmap non-zero word: `(int16 wordPos, uint256 bitmap)` as
/// two packed `uint256` slots → 2 ABI words. Only non-zero words are returned,
/// so the real size is density-dependent — use split-on-failure, not a fixed
/// count, as the budget driver.
pub const V3_TICK_BITMAP_RETURN_BYTES_PER_NONEMPTY_WORD: usize = 2 * ABI_WORD;

/// Return-payload budget in bytes (conservative fraction of EIP-170).
pub fn create_return_budget_bytes() -> usize {
    EIP170_MAX_CODE_SIZE * CREATE_RETURN_BUDGET_NUM / CREATE_RETURN_BUDGET_DEN
}

/// Max items whose combined ABI return fits the budget.
///
/// `per_item_bytes` is the padded ABI size of one element; `overhead_bytes` is
/// the dynamic-array head (`ABI_DYNAMIC_ARRAY_OVERHEAD` for a top-level `T[]`).
pub fn max_items_for_return_size(per_item_bytes: usize, overhead_bytes: usize) -> usize {
    let budget = create_return_budget_bytes();
    if per_item_bytes == 0 {
        return 1;
    }
    if budget <= overhead_bytes {
        return 1;
    }
    ((budget - overhead_bytes) / per_item_bytes).max(1)
}

/// Chunk size for Agni / UniswapV3 slot0 batch CREATEs.
pub fn v3_slot0_chunk_size() -> usize {
    max_items_for_return_size(V3_SLOT0_RETURN_BYTES_PER_POOL, ABI_DYNAMIC_ARRAY_OVERHEAD)
}

/// Chunk size for Moe slot0 batch CREATEs (fixed 24-word return per pair).
pub fn moe_slot0_chunk_size() -> usize {
    max_items_for_return_size(MOE_SLOT0_RETURN_BYTES_PER, ABI_DYNAMIC_ARRAY_OVERHEAD)
}

/// True when an RPC/contract error looks like CREATE bytecode size rejection.
pub fn is_create_size_limit(err: &AMMError) -> bool {
    let s = err.to_string();
    s.contains("CreateContractSizeLimit") || s.contains("max code size exceeded")
}

/// Run a batch CREATE eth_call with **size-only** split recovery
/// (WHI-925 / WHI-929).
///
/// Policy (no time backoff — unlike [`super::moe::sync::with_create_size_resilience`]):
/// 1. Success → extend results in input order.
/// 2. `CreateContractSizeLimit` and `len > 1` → halve the chunk and reprocess.
/// 3. `CreateContractSizeLimit` on a single item → try `split_item`; if it
///    returns two halves, re-queue them. Otherwise fail with
///    [`BatchContractError::CreateSizeSinglePool`], naming the pool and
///    optional `detail_of` range context.
/// 4. Any other error → propagate immediately.
///
/// `split_item` returns `None` when the item is atomic (slot0 address, a
/// single tick, a one-word bitmap range). For tick-data / tick-bitmap it
/// bisects the requested tick list or word range so a dense pool can still
/// sync.
///
/// **Result cardinality:** when `split_item` fires, `out` has more entries
/// than the original `items` (one per leaf). Callers that require 1:1 with
/// the input (slot0) must pass `|_| None` for `split_item`.
pub async fn with_create_size_split<I, T, F, Fut, R, D, Sp>(
    items: Vec<I>,
    path: &'static str,
    pool_of: R,
    detail_of: D,
    split_item: Sp,
    mut call: F,
) -> Result<Vec<T>, AMMError>
where
    I: Clone,
    F: FnMut(Vec<I>) -> Fut,
    Fut: Future<Output = Result<Vec<T>, AMMError>>,
    R: Fn(&I) -> Option<Address>,
    D: Fn(&I) -> Option<String>,
    Sp: Fn(&I) -> Option<(I, I)>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // LIFO work stack. Halves are pushed right-then-left so left processes
    // first and `out` stays in original order.
    let mut pending: Vec<Vec<I>> = vec![items];
    let mut out: Vec<T> = Vec::new();

    while let Some(chunk) = pending.pop() {
        // debug: dense tick-data pools issue many attempts; path-level info
        // logs live at the call site (agni/uniswap_v3/moe).
        tracing::debug!(
            target: "amms.batch_create",
            path,
            item_count = chunk.len(),
            "batch CREATE attempt"
        );

        match call(chunk.clone()).await {
            Ok(decoded) => {
                if decoded.len() != chunk.len() {
                    return Err(BatchContractError::MalformedBatchResponse {
                        path,
                        expected: chunk.len(),
                        actual: decoded.len(),
                    }
                    .into());
                }
                out.extend(decoded);
            }
            Err(e) if is_create_size_limit(&e) => {
                if chunk.len() == 1 {
                    let item = &chunk[0];
                    if let Some((left, right)) = split_item(item) {
                        tracing::warn!(
                            target: "amms.batch_create",
                            path,
                            pool = ?pool_of(item),
                            detail = ?detail_of(item),
                            error = %e,
                            "CREATE size limit on single item; bisecting range"
                        );
                        pending.push(vec![right]);
                        pending.push(vec![left]);
                        continue;
                    }

                    let pool = pool_of(item);
                    let detail = detail_of(item);
                    tracing::error!(
                        target: "amms.batch_create",
                        pool = ?pool,
                        detail = ?detail,
                        path,
                        error = %e,
                        "CREATE size limit on single-item batch; not retrying"
                    );
                    return Err(BatchContractError::CreateSizeSinglePool {
                        path,
                        pool,
                        detail,
                        message: e.to_string(),
                    }
                    .into());
                }

                let mid = chunk.len() / 2;
                let (left, right) = chunk.split_at(mid);
                tracing::warn!(
                    target: "amms.batch_create",
                    chunk_len = chunk.len(),
                    left = left.len(),
                    right = right.len(),
                    path,
                    error = %e,
                    "CREATE size limit; halving chunk (no backoff)"
                );
                pending.push(right.to_vec());
                pending.push(left.to_vec());
            }
            Err(e) => return Err(e),
        }
    }

    Ok(out)
}

/// Bisect a closed word range `[min, max]` into two non-empty halves.
///
/// Returns `None` when the range is a single word (cannot narrow further).
pub fn bisect_i16_range(min: i16, max: i16) -> Option<((i16, i16), (i16, i16))> {
    if min >= max {
        return None;
    }
    let mid = min + ((max as i32 - min as i32) / 2) as i16;
    Some(((min, mid), (mid.saturating_add(1), max)))
}

/// Bisect a tick list into two non-empty halves.
///
/// Returns `None` when there is zero or one tick (cannot narrow further).
pub fn bisect_tick_list<T: Clone>(ticks: &[T]) -> Option<(Vec<T>, Vec<T>)> {
    if ticks.len() <= 1 {
        return None;
    }
    let mid = ticks.len() / 2;
    Some((ticks[..mid].to_vec(), ticks[mid..].to_vec()))
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

    #[test]
    fn chunk_size_is_driven_by_payload_size() {
        let small = max_items_for_return_size(96, ABI_DYNAMIC_ARRAY_OVERHEAD);
        let large = max_items_for_return_size(192, ABI_DYNAMIC_ARRAY_OVERHEAD);
        assert!(small > 1, "96-byte items must fit more than one");
        assert!(
            large < small,
            "doubling per-item size must shrink the chunk: small={small} large={large}"
        );
        // Boundary: N items whose combined return exceeds the budget must not
        // fit in one chunk; N-1 may.
        let budget = create_return_budget_bytes();
        let per = 96usize;
        let n = max_items_for_return_size(per, ABI_DYNAMIC_ARRAY_OVERHEAD);
        let combined_n = ABI_DYNAMIC_ARRAY_OVERHEAD + n * per;
        let combined_n1 = ABI_DYNAMIC_ARRAY_OVERHEAD + (n + 1) * per;
        assert!(combined_n <= budget);
        assert!(combined_n1 > budget);
    }

    #[test]
    fn v3_slot0_chunk_matches_abi_tuple() {
        let expected =
            max_items_for_return_size(V3_SLOT0_RETURN_BYTES_PER_POOL, ABI_DYNAMIC_ARRAY_OVERHEAD);
        assert_eq!(v3_slot0_chunk_size(), expected);
        // Sanity: 94 pools (current agni-v3 count) must not require more than
        // a few chunks at the derived size — and must be finite.
        assert!(v3_slot0_chunk_size() >= 1);
        assert!(v3_slot0_chunk_size() < EIP170_MAX_CODE_SIZE);
    }

    #[test]
    fn is_create_size_limit_matches_observed_shapes() {
        assert!(is_create_size_limit(&create_size_err()));
        assert!(is_create_size_limit(&AMMError::TransportError(
            alloy::transports::TransportErrorKind::custom_str("max code size exceeded")
        )));
        assert!(!is_create_size_limit(&other_err()));
    }

    #[tokio::test]
    async fn create_size_limit_halves_down_to_floor() {
        let call_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let sizes_c = Arc::clone(&call_sizes);
        let pools: Vec<Address> = (0..8u8)
            .map(|i| Address::with_last_byte(i + 1))
            .collect();

        // Fail when chunk > 2; succeed at 2 or below.
        let result = with_create_size_split(
            pools.clone(),
            "test_slot0",
            |a: &Address| Some(*a),
            |_| None,
            |_| None,
            move |chunk| {
                let sizes_c = Arc::clone(&sizes_c);
                async move {
                    sizes_c.lock().unwrap().push(chunk.len());
                    if chunk.len() > 2 {
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
        assert_eq!(sizes.first().copied(), Some(8));
        assert!(
            sizes.iter().any(|&s| s == 4),
            "expected a half of 4 in {sizes:?}"
        );
        assert!(
            sizes.iter().filter(|&&s| s == 2).count() >= 2,
            "expected successful width-2 calls in {sizes:?}"
        );
        assert!(
            !sizes.iter().any(|&s| s == 1),
            "must not fan out to 1 when 2 succeeds: {sizes:?}"
        );
    }

    #[tokio::test]
    async fn single_pool_create_size_reports_pool_address() {
        let pool = address!("0x2222222222222222222222222222222222222222");
        let err = with_create_size_split(
            vec![pool],
            "test_slot0",
            |a: &Address| Some(*a),
            |_| None,
            |_| None,
            |_chunk| async move { Err::<Vec<Address>, _>(create_size_err()) },
        )
        .await
        .expect_err("must fail on single-pool size limit");

        let msg = err.to_string();
        assert!(
            msg.contains("CREATE size limit on single pool") || msg.contains("single pool"),
            "expected named single-pool error, got: {msg}"
        );
        assert!(
            msg.contains("0x2222") || msg.contains("22222222"),
            "expected pool in message, got: {msg}"
        );
        match err {
            AMMError::BatchContractError(BatchContractError::CreateSizeSinglePool {
                path,
                pool: p,
                detail,
                ..
            }) => {
                assert_eq!(path, "test_slot0");
                assert_eq!(p, Some(pool));
                assert!(detail.is_none());
            }
            other => panic!("unexpected error variant: {other}"),
        }
    }

    /// Tick-data style: each item carries a tick list. A batch whose total
    /// ticks exceed the budget is halved; a single item with too many ticks
    /// is bisected until under budget (WHI-929).
    #[derive(Clone, Debug)]
    struct FakeTickReq {
        pool: Address,
        ticks: Vec<i32>,
    }

    #[tokio::test]
    async fn tick_data_batch_over_limit_halves_and_completes() {
        let pool = address!("0x3333333333333333333333333333333333333333");
        // Three requests, 40 ticks each → total 120. Mock rejects any call
        // whose total ticks > 50 (simulates CREATE size limit).
        let items: Vec<FakeTickReq> = (0..3)
            .map(|i| FakeTickReq {
                pool,
                ticks: (i * 40..(i + 1) * 40).collect(),
            })
            .collect();

        let call_sizes: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
        let sizes_c = Arc::clone(&call_sizes);
        const MAX_TICKS_PER_CALL: usize = 50;

        let result = with_create_size_split(
            items,
            "test_tick_data",
            |r: &FakeTickReq| Some(r.pool),
            |r: &FakeTickReq| {
                Some(format!(
                    "ticks={} first={:?} last={:?}",
                    r.ticks.len(),
                    r.ticks.first(),
                    r.ticks.last()
                ))
            },
            |r: &FakeTickReq| {
                bisect_tick_list(&r.ticks).map(|(a, b)| {
                    (
                        FakeTickReq {
                            pool: r.pool,
                            ticks: a,
                        },
                        FakeTickReq {
                            pool: r.pool,
                            ticks: b,
                        },
                    )
                })
            },
            move |chunk| {
                let sizes_c = Arc::clone(&sizes_c);
                async move {
                    sizes_c.lock().unwrap().push(chunk.len());
                    let total_ticks: usize = chunk.iter().map(|r| r.ticks.len()).sum();
                    if total_ticks > MAX_TICKS_PER_CALL {
                        Err(create_size_err())
                    } else {
                        Ok(chunk
                            .into_iter()
                            .map(|r| r.ticks.len())
                            .collect::<Vec<_>>())
                    }
                }
            },
        )
        .await
        .expect("tick-data batch must complete via halving/bisect");

        // Original 120 ticks; after possible item bisects, sum of per-leaf
        // tick counts is still 120.
        assert_eq!(result.iter().sum::<usize>(), 120);
        let sizes = call_sizes.lock().unwrap().clone();
        assert!(
            sizes.first().copied() == Some(3) || sizes.iter().any(|&s| s > 1),
            "expected multi-item attempts before success: {sizes:?}"
        );
        assert!(
            sizes.len() > 1,
            "must split at least once, got call sizes {sizes:?}"
        );
    }

    #[tokio::test]
    async fn single_tick_range_over_limit_names_pool_and_range() {
        let pool = address!("0x4444444444444444444444444444444444444444");
        // One tick only — cannot bisect further.
        let items = vec![FakeTickReq {
            pool,
            ticks: vec![42],
        }];

        let err = with_create_size_split(
            items,
            "test_tick_data",
            |r: &FakeTickReq| Some(r.pool),
            |r: &FakeTickReq| {
                Some(format!(
                    "ticks={} first={:?} last={:?}",
                    r.ticks.len(),
                    r.ticks.first(),
                    r.ticks.last()
                ))
            },
            |r: &FakeTickReq| {
                bisect_tick_list(&r.ticks).map(|(a, b)| {
                    (
                        FakeTickReq {
                            pool: r.pool,
                            ticks: a,
                        },
                        FakeTickReq {
                            pool: r.pool,
                            ticks: b,
                        },
                    )
                })
            },
            |_chunk| async move { Err::<Vec<usize>, _>(create_size_err()) },
        )
        .await
        .expect_err("atomic single-tick item must fail loudly");

        let msg = err.to_string();
        assert!(
            msg.contains("4444") || msg.contains("0x4444"),
            "expected pool in message: {msg}"
        );
        assert!(
            msg.contains("ticks=1") || msg.contains("first=Some(42)"),
            "expected range detail in message: {msg}"
        );
        match err {
            AMMError::BatchContractError(BatchContractError::CreateSizeSinglePool {
                path,
                pool: p,
                detail,
                ..
            }) => {
                assert_eq!(path, "test_tick_data");
                assert_eq!(p, Some(pool));
                let d = detail.expect("detail required");
                assert!(d.contains("ticks=1"));
                assert!(d.contains("42"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn single_item_with_wide_range_bisects_until_success() {
        let pool = address!("0x5555555555555555555555555555555555555555");
        // 16 ticks; mock only accepts ≤4 ticks per call → requires item bisect.
        let items = vec![FakeTickReq {
            pool,
            ticks: (0..16).collect(),
        }];

        let bisects = Arc::new(AtomicUsize::new(0));
        let bisects_c = Arc::clone(&bisects);

        let result = with_create_size_split(
            items,
            "test_tick_data",
            |r: &FakeTickReq| Some(r.pool),
            |r: &FakeTickReq| Some(format!("ticks={}", r.ticks.len())),
            move |r: &FakeTickReq| {
                let split = bisect_tick_list(&r.ticks).map(|(a, b)| {
                    (
                        FakeTickReq {
                            pool: r.pool,
                            ticks: a,
                        },
                        FakeTickReq {
                            pool: r.pool,
                            ticks: b,
                        },
                    )
                });
                if split.is_some() {
                    bisects_c.fetch_add(1, Ordering::SeqCst);
                }
                split
            },
            |chunk| async move {
                let total: usize = chunk.iter().map(|r| r.ticks.len()).sum();
                if total > 4 {
                    Err(create_size_err())
                } else {
                    Ok(chunk
                        .into_iter()
                        .map(|r| r.ticks.len())
                        .collect::<Vec<_>>())
                }
            },
        )
        .await
        .expect("wide single-item range must bisect to success");

        assert_eq!(result.iter().sum::<usize>(), 16);
        assert!(
            bisects.load(Ordering::SeqCst) >= 1,
            "expected at least one item-level bisect"
        );
    }

    #[test]
    fn bisect_helpers_floor_correctly() {
        assert_eq!(bisect_i16_range(0, 0), None);
        assert_eq!(bisect_i16_range(3, 3), None);
        assert_eq!(bisect_i16_range(0, 1), Some(((0, 0), (1, 1))));
        assert_eq!(bisect_i16_range(-4, 3), Some(((-4, -1), (0, 3))));

        assert!(bisect_tick_list::<i32>(&[]).is_none());
        assert!(bisect_tick_list(&[1]).is_none());
        let (a, b) = bisect_tick_list(&[1, 2, 3, 4]).unwrap();
        assert_eq!(a, vec![1, 2]);
        assert_eq!(b, vec![3, 4]);
    }

    #[tokio::test]
    async fn non_create_size_error_propagates() {
        let err = with_create_size_split(
            vec![Address::with_last_byte(1)],
            "test_slot0",
            |a: &Address| Some(*a),
            |_| None,
            |_| None,
            |_chunk| async move { Err::<Vec<Address>, _>(other_err()) },
        )
        .await
        .expect_err("must propagate");
        assert!(err.to_string().contains("connection reset"));
    }

    #[tokio::test]
    async fn malformed_batch_length_is_rejected() {
        let pools: Vec<Address> = (0..4u8)
            .map(|i| Address::with_last_byte(i + 1))
            .collect();
        let err = with_create_size_split(
            pools,
            "test_slot0",
            |a: &Address| Some(*a),
            |_| None,
            |_| None,
            // Return fewer items than requested.
            |_chunk| async move { Ok::<Vec<Address>, _>(vec![Address::with_last_byte(1)]) },
        )
        .await
        .expect_err("must reject length mismatch");
        match err {
            AMMError::BatchContractError(BatchContractError::MalformedBatchResponse {
                path,
                expected,
                actual,
            }) => {
                assert_eq!(path, "test_slot0");
                assert_eq!(expected, 4);
                assert_eq!(actual, 1);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    /// Synthetic 3× current universe (≈400 pools): feed the full set as one
    /// work item so pre-chunking does not hide split recovery. Mock rejects
    /// any return above the **50% budget** (same limit production pre-chunks
    /// use), forcing `with_create_size_split` to halve until under budget.
    #[tokio::test]
    async fn scales_to_synthetic_400_pools() {
        let pool_count = 400usize;
        let per_item = V3_SLOT0_RETURN_BYTES_PER_POOL;
        let step = max_items_for_return_size(per_item, ABI_DYNAMIC_ARRAY_OVERHEAD);
        let budget = create_return_budget_bytes();
        assert!(
            pool_count > step,
            "400 pools must exceed one size-derived chunk ({step}) so split is exercised"
        );

        let pools: Vec<Address> = (0..pool_count)
            .map(|i| Address::with_last_byte(((i % 255) + 1) as u8))
            .collect();

        let calls = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let max_success = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let max_c = Arc::clone(&max_seen);
        let max_ok = Arc::clone(&max_success);

        let all = with_create_size_split(
            pools,
            "v3_slot0",
            |a: &Address| Some(*a),
            |_| None,
            |_| None,
            move |chunk| {
                let calls_c = Arc::clone(&calls_c);
                let max_c = Arc::clone(&max_c);
                let max_ok = Arc::clone(&max_ok);
                async move {
                    calls_c.fetch_add(1, Ordering::SeqCst);
                    max_c.fetch_max(chunk.len(), Ordering::SeqCst);
                    let ret_bytes = ABI_DYNAMIC_ARRAY_OVERHEAD + chunk.len() * per_item;
                    if ret_bytes > budget {
                        Err(create_size_err())
                    } else {
                        max_ok.fetch_max(chunk.len(), Ordering::SeqCst);
                        Ok(chunk)
                    }
                }
            },
        )
        .await
        .expect("400-pool sync must succeed via split");

        assert_eq!(all.len(), pool_count);
        // First attempt is the full set (must fail and split).
        assert!(
            max_seen.load(Ordering::SeqCst) == pool_count,
            "expected an initial full-width call of {pool_count}"
        );
        assert!(
            max_success.load(Ordering::SeqCst) <= step,
            "successful chunks must fit the size budget (≤{step})"
        );
        assert!(
            calls.load(Ordering::SeqCst) > 1,
            "must split at least once for 400 pools"
        );
    }

    #[test]
    fn audited_fixed_steps_are_within_budget_or_documented() {
        // Token decimals step was 765; size-derived is the source of truth.
        let decimals = max_items_for_return_size(
            TOKEN_DECIMALS_RETURN_BYTES_PER,
            ABI_DYNAMIC_ARRAY_OVERHEAD,
        );
        assert!(decimals >= 1);
        // V2 pool data step was 120 — often above 50% budget (~63).
        let v2_pool = max_items_for_return_size(
            V2_POOL_DATA_RETURN_BYTES_PER,
            ABI_DYNAMIC_ARRAY_OVERHEAD,
        );
        assert!(v2_pool >= 1);
        // V2 pairs step was 766.
        let v2_pairs =
            max_items_for_return_size(V2_PAIRS_RETURN_BYTES_PER, ABI_DYNAMIC_ARRAY_OVERHEAD);
        assert!(v2_pairs >= 1);
        // Moe slot0: size-derived (~15 at 50%); old hard-coded 255 was over budget.
        let moe = moe_slot0_chunk_size();
        assert!(moe >= 1);
        assert!(
            moe < 255,
            "Moe slot0 size-derived chunk ({moe}) must be under the old step=255"
        );
    }
}
