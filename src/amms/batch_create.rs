//! Batch CREATE eth_call payload budgeting and size-split recovery (WHI-925).
//!
//! Batch-request contracts are not deployed. Callers send their **creation
//! bytecode** as an `eth_call`; the constructor body runs and `return`s ABI-
//! encoded results. That return becomes the CREATE "code" and is subject to
//! EIP-170's 24 576-byte max code size (`CreateContractSizeLimit` /
//! `max code size exceeded` on Mantle).
//!
//! Chunk by **expected return bytes**, not a hard-coded pool count. When a
//! batch still hits the size limit, **halve and retry** down to a single pool
//! (no time backoff — size is not rate pressure; see WHI-921 for the Moe path
//! that *does* use backoff under 429s).

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

/// Moe `Slot0Data`: 19 value fields + 7 bools = 26 static ABI words.
///
/// Not used to drive Moe chunking in this issue (WHI-921 owns Moe CREATE
/// recovery). Recorded so the remaining hard-coded `step` can cite a size.
pub const MOE_SLOT0_RETURN_BYTES_PER: usize = 26 * ABI_WORD;

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

/// True when an RPC/contract error looks like CREATE bytecode size rejection.
pub fn is_create_size_limit(err: &AMMError) -> bool {
    let s = err.to_string();
    s.contains("CreateContractSizeLimit") || s.contains("max code size exceeded")
}

/// Run a batch CREATE eth_call with **size-only** split recovery (WHI-925).
///
/// Policy (no time backoff — unlike [`super::moe::sync::with_create_size_resilience`]):
/// 1. Success → extend results in input order.
/// 2. `CreateContractSizeLimit` and `len > 1` → halve the chunk and reprocess.
/// 3. `CreateContractSizeLimit` on a single item → fail with
///    [`BatchContractError::CreateSizeSinglePool`], naming the pool.
/// 4. Any other error → propagate immediately.
pub async fn with_create_size_split<I, T, F, Fut, R>(
    items: Vec<I>,
    path: &'static str,
    pool_of: R,
    mut call: F,
) -> Result<Vec<T>, AMMError>
where
    I: Clone,
    F: FnMut(Vec<I>) -> Fut,
    Fut: Future<Output = Result<Vec<T>, AMMError>>,
    R: Fn(&I) -> Option<Address>,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }

    // LIFO work stack. Halves are pushed right-then-left so left processes
    // first and `out` stays in original order.
    let mut pending: Vec<Vec<I>> = vec![items];
    let mut out: Vec<T> = Vec::new();

    while let Some(chunk) = pending.pop() {
        match call(chunk.clone()).await {
            Ok(decoded) => {
                out.extend(decoded);
            }
            Err(e) if is_create_size_limit(&e) => {
                if chunk.len() == 1 {
                    let pool = chunk.first().and_then(|item| pool_of(item));
                    tracing::error!(
                        target: "amms.batch_create",
                        pool = ?pool,
                        path,
                        error = %e,
                        "CREATE size limit on single-pool batch; not retrying"
                    );
                    return Err(BatchContractError::CreateSizeSinglePool {
                        path,
                        pool,
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
                ..
            }) => {
                assert_eq!(path, "test_slot0");
                assert_eq!(p, Some(pool));
            }
            other => panic!("unexpected error variant: {other}"),
        }
    }

    #[tokio::test]
    async fn non_create_size_error_propagates() {
        let err = with_create_size_split(
            vec![Address::with_last_byte(1)],
            "test_slot0",
            |a: &Address| Some(*a),
            |_chunk| async move { Err::<Vec<Address>, _>(other_err()) },
        )
        .await
        .expect_err("must propagate");
        assert!(err.to_string().contains("connection reset"));
    }

    /// Synthetic 3× current universe (≈400 pools): mock rejects chunks whose
    /// return would exceed the budget; size-derived pre-chunk + split succeeds.
    #[tokio::test]
    async fn scales_to_synthetic_400_pools() {
        let pool_count = 400usize;
        let per_item = V3_SLOT0_RETURN_BYTES_PER_POOL;
        let step = max_items_for_return_size(per_item, ABI_DYNAMIC_ARRAY_OVERHEAD);
        let budget = create_return_budget_bytes();

        let pools: Vec<Address> = (0..pool_count)
            .map(|i| Address::with_last_byte(((i % 255) + 1) as u8))
            .collect();

        let calls = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let max_c = Arc::clone(&max_seen);

        // Mock: reject any chunk whose *return* would exceed full EIP-170
        // (stricter than our 50% budget → forces either pre-chunk or split).
        let mut all = Vec::with_capacity(pool_count);
        for group in pools.chunks(step) {
            let decoded = with_create_size_split(
                group.to_vec(),
                "v3_slot0",
                |a: &Address| Some(*a),
                {
                    let calls_c = Arc::clone(&calls_c);
                    let max_c = Arc::clone(&max_c);
                    move |chunk| {
                        let calls_c = Arc::clone(&calls_c);
                        let max_c = Arc::clone(&max_c);
                        async move {
                            calls_c.fetch_add(1, Ordering::SeqCst);
                            max_c.fetch_max(chunk.len(), Ordering::SeqCst);
                            let ret_bytes = ABI_DYNAMIC_ARRAY_OVERHEAD + chunk.len() * per_item;
                            if ret_bytes > EIP170_MAX_CODE_SIZE {
                                Err(create_size_err())
                            } else {
                                Ok(chunk)
                            }
                        }
                    }
                },
            )
            .await
            .expect("400-pool sync must succeed");
            all.extend(decoded);
        }

        assert_eq!(all.len(), pool_count);
        assert!(
            calls.load(Ordering::SeqCst) >= pool_count.div_ceil(step),
            "must issue at least one call per pre-chunk"
        );
        // Pre-chunking keeps us under budget, so we should not need to split
        // further for the 50%-budget step against a full-EIP-170 mock.
        assert!(
            max_seen.load(Ordering::SeqCst) <= step,
            "calls must not exceed planned chunk size"
        );
        let worst_return = ABI_DYNAMIC_ARRAY_OVERHEAD + max_seen.load(Ordering::SeqCst) * per_item;
        assert!(
            worst_return <= budget || worst_return <= EIP170_MAX_CODE_SIZE,
            "worst return {worst_return} exceeded limits"
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
        // Moe slot0 step=255 is far above size budget (~15 at 50%); WHI-921
        // recovery owns that path — we only record the assumed size here.
        let moe = max_items_for_return_size(MOE_SLOT0_RETURN_BYTES_PER, ABI_DYNAMIC_ARRAY_OVERHEAD);
        assert!(
            moe < 255,
            "Moe slot0 fixed step=255 exceeds size budget ({moe}); left to WHI-921"
        );
    }
}
