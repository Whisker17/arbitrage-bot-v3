use super::*;
use crate::execution::fee_context::{bump_fee_value, bump_fee_value_ceil, PriorFees};
use crate::execution::gas_profile::{ProtocolKind, RouteKey};
use crate::execution::types::IntentPolicy;
use crate::state_space::{
    BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
};
use alloy::primitives::{Address, B256, U256};
use std::collections::HashMap;
use std::sync::Arc;

fn snap(n: u64) -> SnapshotId {
    SnapshotId::new(5000, n, B256::from([n as u8; 32]))
}

fn header(ts: u64) -> BlockHeaderContext {
    BlockHeaderContext::new(B256::ZERO, ts)
}

fn candidate(n: u64) -> CandidateRef {
    CandidateRef {
        snapshot_id: snap(n),
        header: header(1_700_000_000 + n),
        pool_universe_fingerprint: B256::repeat_byte(n as u8),
        route_key: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap(),
        amount_in: U256::from(1_000_000u64),
    }
}

fn fee_ctx(n: u64) -> crate::execution::BlockFeeContext {
    crate::execution::BlockFeeContext {
        block_number: n,
        block_hash: B256::from([n as u8; 32]),
        base_fee_per_gas: 50_000_000_000,
        block_gas_limit: 60_000_000,
    }
}

fn ready_status(n: u64) -> SnapshotStatus {
    let mut coverage = ProtocolCoverage::default();
    coverage.pool_universe_fingerprint = Some(B256::repeat_byte(n as u8));
    SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        snap(n),
        header(1_700_000_000 + n),
        HashMap::new(),
        coverage,
    )))
}

fn reserve_ok(sm: &IntentStateMachine, n: u64) -> (u64, crate::execution::ExecutionPermit) {
    sm.reserve(candidate(n), &ready_status(n), fee_ctx(n))
        .unwrap()
}

fn policy() -> IntentPolicy {
    IntentPolicy::with_caps(200_000_000_000, 400_000_000_000)
}

fn sm() -> IntentStateMachine {
    IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        policy(),
        false,
    )
    .unwrap()
}

fn signed(nonce: u64, hash_byte: u8, candidate: CandidateRef) -> SignedSubmission {
    SignedSubmission {
        raw: Bytes::from(vec![hash_byte]),
        tx_hash: B256::from([hash_byte; 32]),
        fee_plan: FeePlan {
            block_fee_context: crate::execution::BlockFeeContext {
                block_number: candidate.snapshot_id.block_number,
                block_hash: candidate.snapshot_id.block_hash,
                base_fee_per_gas: 50_000_000_000,
                block_gas_limit: 60_000_000,
            },
            gas_limit: 300_000,
            expected_gas_used: 200_000,
            expected_gas_cost: U256::from(10_000_000_000_000u64),
            max_fee_per_gas: 50_000_100_000,
            max_priority_fee_per_gas: 100_000,
            profile_identity: "v2-v2".into(),
        },
        payload: PreparedPayload::Execute {
            params: crate::execution::ExecutionParams {
                amount_in: candidate.amount_in,
                route_key: candidate.route_key.clone(),
                crossing_buckets_verified: false,
                token_path: vec![Address::ZERO; 3],
                pool_addresses: vec![Address::ZERO; 2],
                pool_types: vec![0, 0],
                pool_tokens: vec![(Address::ZERO, Address::ZERO); 2],
                expected_reserves_u112: vec![alloy::primitives::aliases::U112::ZERO; 4],
                step_amounts_out: vec![U256::from(1_100_000u64); 2],
                min_amount_out: U256::from(1_100_000u64),
                expected_net_profit_mnt_wei: U256::from(50_000u64),
            },
            candidate: candidate.clone(),
        },
        calldata_digest: B256::from([hash_byte.wrapping_add(1); 32]),
        nonce,
        submitted_at: candidate.snapshot_id,
    }
}

#[test]
fn policy_rejects_zero_and_inverted_bounds() {
    let mut p = policy();
    p.confirmation_depth = 0;
    assert!(p.validate().is_err());
    p = policy();
    p.reorg_track_blocks = 0;
    assert!(p.validate().is_err());
    p = policy();
    p.reorg_track_blocks = crate::state_space::CACHE_SIZE as u64 + 1;
    assert!(p.validate().is_err());
    p = policy();
    p.confirmation_depth = 5;
    p.reorg_track_blocks = 3;
    assert!(p.validate().is_err());
    p = policy();
    p.cancel_fee_cap_wei = p.max_fee_cap_wei; // may fail min bump
                                              // with 12.5% bump, cancel must be >= ceil(max * 1.125)
    assert!(p.validate().is_err());
}

#[test]
fn reserve_creates_opaque_permit_with_nonce() {
    let sm = sm();
    let (nonce, permit) = reserve_ok(&sm, 1);
    assert_eq!(nonce, 0);
    assert_eq!(permit.nonce(), 0);
    assert_eq!(permit.snapshot_id(), snap(1));
}

#[test]
fn preparing_is_not_released_by_reconcile() {
    let sm = sm();
    let (nonce, _) = reserve_ok(&sm, 1);
    sm.begin_prepare(nonce).unwrap();
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        })
        .unwrap();
    assert!(released.is_empty());
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::Preparing));
}

#[test]
fn abort_prepare_returns_to_reserved_then_release() {
    let sm = sm();
    let (nonce, _) = reserve_ok(&sm, 1);
    sm.begin_prepare(nonce).unwrap();
    sm.abort_prepare(nonce).unwrap();
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        })
        .unwrap();
    assert_eq!(released, vec![nonce]);
    assert!(sm.intent(nonce).unwrap().is_none());
}

#[test]
fn record_submission_before_broadcast_and_blocks_release() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 7, cand);
    sm.record_submission(&s).unwrap();
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::Submitted));
    assert_eq!(intent.attempts.len(), 1);
    assert_eq!(intent.attempts[0].tx_hash, s.tx_hash);
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        })
        .unwrap();
    assert!(
        released.is_empty(),
        "ambiguous/broadcast attempt must not release"
    );
}

#[test]
fn broadcast_order_rejects_gap() {
    let sm = sm();
    let (n0, _) = reserve_ok(&sm, 1);
    let (n1, _) = reserve_ok(&sm, 2);
    // Try to submit n1 while n0 still Reserved zero-broadcast.
    sm.begin_prepare(n1).unwrap();
    let err = sm
        .record_submission(&signed(n1, 9, candidate(2)))
        .unwrap_err();
    assert!(matches!(
        err,
        IntentError::BroadcastOrder {
            nonce: 1,
            blocker: 0
        }
    ));
    // After n0 submits, n1 may submit.
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    sm.record_submission(&signed(n1, 2, candidate(2))).unwrap();
}

#[test]
fn reconcile_releases_contiguous_reserved_suffix_only() {
    let sm = sm();
    let (n0, _) = reserve_ok(&sm, 1);
    let (n1, _) = reserve_ok(&sm, 2);
    let (n2, _) = reserve_ok(&sm, 3);
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    // n1 and n2 still reserved zero-broadcast suffix.
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        })
        .unwrap();
    assert_eq!(released, vec![n2, n1]);
    assert!(sm.intent(n0).unwrap().is_some());
    assert!(sm.intent(n1).unwrap().is_none());
    assert!(sm.intent(n2).unwrap().is_none());
}

#[test]
fn chain_nonce_regression_lowers_counter() {
    let sm = sm();
    let (n0, _) = reserve_ok(&sm, 1);
    let (n1, _) = reserve_ok(&sm, 2);
    assert_eq!(sm.peek_next_nonce().unwrap(), 2);
    // Release both reserved.
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        })
        .unwrap();
    assert_eq!(released, vec![n1, n0]);
    assert_eq!(sm.peek_next_nonce().unwrap(), 0);
}

#[test]
fn receipt_mapping_execute_success_and_revert() {
    let sm = sm();
    let cand = candidate(10);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 3, cand);
    sm.record_submission(&s).unwrap();

    let mut canonical = HashMap::new();
    canonical.insert(10, B256::from([10; 32]));
    let outcome = ReceiptOutcome {
        success: true,
        block_number: 10,
        block_hash: B256::from([10; 32]),
        gas_used: 180_000,
        effective_gas_price: 50_000_000_000,
        l1_fee: Some(U256::from(1000u64)),
        execution_layer_only: false,
    };
    let mut receipts = HashMap::new();
    receipts.insert(s.tx_hash, Some(outcome.clone()));
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: 11,
                hash: B256::from([11; 32]),
            },
            &canonical,
            &receipts,
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 1,
                pending_nonce: 1,
            },
        )
        .unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, IntentEvent::Finalized { success: true, .. })));
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::Finalized));
    assert_eq!(
        outcome.actual_cost(),
        U256::from(180_000u64) * U256::from(50_000_000_000u64) + U256::from(1000u64)
    );
}

#[test]
fn cancel_receipt_never_qualifies_gas_and_is_terminal_even_on_failure() {
    let sm = sm();
    let cand = candidate(5);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    // First an execute attempt so cancel baseline exists.
    sm.record_submission(&signed(nonce, 1, cand.clone()))
        .unwrap();
    // Record cancel attempt.
    let cancel = SignedSubmission {
        raw: Bytes::from(vec![9]),
        tx_hash: B256::from([9; 32]),
        fee_plan: FeePlan {
            block_fee_context: crate::execution::BlockFeeContext {
                block_number: 5,
                block_hash: B256::from([5; 32]),
                base_fee_per_gas: 50_000_000_000,
                block_gas_limit: 60_000_000,
            },
            gas_limit: 21_000,
            expected_gas_used: 21_000,
            expected_gas_cost: U256::from(21_000u64 * 60_000_000_000u64),
            max_fee_per_gas: 60_000_000_000,
            max_priority_fee_per_gas: 112_500,
            profile_identity: FeePlan::CANCEL_PROFILE_IDENTITY.into(),
        },
        payload: PreparedPayload::Cancel {
            to: Address::ZERO,
            gas_limit: 21_000,
        },
        calldata_digest: B256::ZERO,
        nonce,
        submitted_at: cand.snapshot_id,
    };
    sm.record_submission(&cancel).unwrap();
    let mut canonical = HashMap::new();
    canonical.insert(6, B256::from([6; 32]));
    let mut receipts = HashMap::new();
    receipts.insert(
        cancel.tx_hash,
        Some(ReceiptOutcome {
            success: false, // status-false cancel still terminal
            block_number: 6,
            block_hash: B256::from([6; 32]),
            gas_used: 21_000, // 100% utilization
            effective_gas_price: 50_000_000_000,
            l1_fee: None,
            execution_layer_only: true,
        }),
    );
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: 7,
                hash: B256::from([7; 32]),
            },
            &canonical,
            &receipts,
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 1,
                pending_nonce: 1,
            },
        )
        .unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, IntentEvent::CancelFinalized { anomaly: true, .. })));
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::CancelFinalized));
}

#[test]
fn reorg_reopens_when_inclusion_hash_diverges_even_if_receipt_none() {
    let sm = sm();
    let cand = candidate(20);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 4, cand);
    sm.record_submission(&s).unwrap();

    // Include unconfirmed on block 20.
    let mut canonical = HashMap::new();
    canonical.insert(20, B256::from([20; 32]));
    let mut receipts = HashMap::new();
    receipts.insert(
        s.tx_hash,
        Some(ReceiptOutcome {
            success: true,
            block_number: 20,
            block_hash: B256::from([20; 32]),
            gas_used: 100,
            effective_gas_price: 1,
            l1_fee: None,
            execution_layer_only: true,
        }),
    );
    sm.on_new_block(
        CanonicalBlock {
            number: 20,
            hash: B256::from([20; 32]),
        },
        &canonical,
        &receipts,
        &HashMap::new(),
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::IncludedUnconfirmed { .. }
    ));

    // Reorg: same number, different hash; receipt now None.
    canonical.insert(20, B256::from([99; 32]));
    receipts.insert(s.tx_hash, None);
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: 21,
                hash: B256::from([21; 32]),
            },
            &canonical,
            &receipts,
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 0,
                pending_nonce: 1,
            },
        )
        .unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, IntentEvent::Reopened { .. })));
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::Submitted
    ));
}

#[test]
fn deep_reorg_beyond_window_halts() {
    let mut p = policy();
    p.reorg_track_blocks = 2;
    p.confirmation_depth = 1;
    let sm = IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        p,
        false,
    )
    .unwrap();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 5, cand);
    sm.record_submission(&s).unwrap();
    // Finalize at block 1.
    let mut canonical = HashMap::new();
    canonical.insert(1, B256::from([1; 32]));
    let mut receipts = HashMap::new();
    receipts.insert(
        s.tx_hash,
        Some(ReceiptOutcome {
            success: true,
            block_number: 1,
            block_hash: B256::from([1; 32]),
            gas_used: 100,
            effective_gas_price: 1,
            l1_fee: None,
            execution_layer_only: true,
        }),
    );
    sm.on_new_block(
        CanonicalBlock {
            number: 2,
            hash: B256::from([2; 32]),
        },
        &canonical,
        &receipts,
        &HashMap::new(),
        ChainNonceView {
            latest_nonce: 1,
            pending_nonce: 1,
        },
    )
    .unwrap();
    // Head far ahead, then diverge deeper than window.
    canonical.insert(1, B256::from([77; 32]));
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: 10,
                hash: B256::from([10; 32]),
            },
            &canonical,
            &HashMap::new(),
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 1,
                pending_nonce: 1,
            },
        )
        .unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(e, IntentEvent::Halted { .. })));
    assert!(sm.is_halted().unwrap());
}

#[test]
fn drop_detection_is_debounced() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(
            cand.clone(),
            &ready_status(cand.snapshot_id.block_number),
            fee_ctx(cand.snapshot_id.block_number),
        )
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 8, cand);
    sm.record_submission(&s).unwrap();
    let mut presence = HashMap::new();
    presence.insert(s.tx_hash, false);
    sm.on_new_block(
        CanonicalBlock {
            number: 2,
            hash: B256::from([2; 32]),
        },
        &HashMap::new(),
        &HashMap::new(),
        &presence,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        },
    )
    .unwrap();
    assert!(!sm.is_dropped(nonce).unwrap()); // only 1 absent block
    sm.on_new_block(
        CanonicalBlock {
            number: 3,
            hash: B256::from([3; 32]),
        },
        &HashMap::new(),
        &HashMap::new(),
        &presence,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        },
    )
    .unwrap();
    assert!(sm.is_dropped(nonce).unwrap()); // 2 >= drop_confirm_blocks
}

#[test]
fn fee_plan_for_cancel_allows_near_cap_squeeze() {
    let ctx = crate::execution::BlockFeeContext {
        block_number: 1,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50_000_000_000,
        block_gas_limit: 60_000_000,
    };
    // prior near execute cap 200 gwei
    let prior = PriorFees::new(100_000u128, 190_000_000_000u128);
    let plan = FeePlan::for_cancel(21_000, prior, 1_250, 400_000_000_000, &ctx, 1).unwrap();
    assert_eq!(plan.gas_limit, 21_000);
    assert_eq!(plan.expected_gas_used, 21_000);
    assert_eq!(plan.profile_identity, FeePlan::CANCEL_PROFILE_IDENTITY);
    assert!(plan.max_fee_per_gas > 190_000_000_000);
    assert!(plan.max_fee_per_gas <= 400_000_000_000);
}

#[test]
fn fee_policy_build_still_rejects_equal_gas_used_limit() {
    use crate::execution::{FeePolicy, GasQuote};
    let ctx = crate::execution::BlockFeeContext {
        block_number: 1,
        block_hash: B256::ZERO,
        base_fee_per_gas: 1,
        block_gas_limit: 30_000_000,
    };
    let quote = GasQuote {
        route_key: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap(),
        gas_limit: 21_000,
        expected_gas_used: 21_000,
        profile_identity: "x".into(),
    };
    let err = FeePolicy::new(1, 1).build(&quote, &ctx).unwrap_err();
    assert!(matches!(
        err,
        crate::execution::FeePlanError::InvalidGasQuote
    ));
}

#[test]
fn deadline_from_header_not_u64_max() {
    let sm = sm();
    let d = sm.deadline_for_header(&header(1_000)).unwrap();
    assert_eq!(d, U256::from(1_060u64));
}

#[test]
fn startup_holds_pending_gap() {
    let sm = IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 3,
            pending_nonce: 5,
        },
        policy(),
        false,
    )
    .unwrap();
    assert_eq!(sm.peek_next_nonce().unwrap(), 5);
    let err = sm
        .reserve(candidate(1), &ready_status(1), fee_ctx(1))
        .unwrap_err();
    assert!(matches!(err, IntentError::ExternalNonceActivity(_)));
}

#[test]
fn latest_wins_slot_keeps_only_latest() {
    let slot = LatestWinsSlot::new();
    slot.publish(1u32);
    slot.publish(2u32);
    assert_eq!(slot.take(), Some(2));
    assert_eq!(slot.take(), None);
}

#[test]
fn pause_cancel_sweep_purges_queue_and_lists_broadcast_intents() {
    let sm = sm();
    let slot = LatestWinsSlot::new();
    slot.publish(99u32);
    let (n0, _) = reserve_ok(&sm, 1);
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    // Reserved-but-not-broadcast must not appear in the cancel sweep.
    let (n1, _) = reserve_ok(&sm, 2);
    let targets = sm.begin_pause_cancel_sweep(&slot).unwrap();
    assert!(slot.take().is_none(), "queued candidate must be purged");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].nonce, n0);
    assert!(sm.intent(n1).unwrap().is_some());
}

#[test]
fn concurrency_second_intent_can_be_prepared_while_first_unconfirmed() {
    let sm = Arc::new(sm());
    let sm2 = Arc::clone(&sm);
    let (n0, _) = reserve_ok(&sm, 1);
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    // Simulate blocked broadcast: intent stays Submitted while we prepare next.
    let handle = std::thread::spawn(move || {
        let (n1, _) = reserve_ok(&sm2, 2);
        sm2.begin_prepare(n1).unwrap();
        sm2.record_submission(&signed(n1, 2, candidate(2))).unwrap();
        n1
    });
    let n1 = handle.join().unwrap();
    assert!(sm.intent(n0).unwrap().is_some());
    assert!(sm.intent(n1).unwrap().is_some());
    // Reconcile during "blocked broadcast" must not release n0.
    let released = sm
        .reconcile(ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 2,
        })
        .unwrap();
    assert!(released.is_empty());
}

#[test]
fn reserve_requires_ready_snapshot_matching_candidate() {
    let sm = sm();
    let err = sm
        .reserve(candidate(1), &SnapshotStatus::Syncing, fee_ctx(1))
        .unwrap_err();
    assert!(matches!(err, IntentError::SnapshotNotReady));

    let err = sm
        .reserve(candidate(1), &ready_status(2), fee_ctx(1))
        .unwrap_err();
    assert!(matches!(
        err,
        IntentError::StaleCandidate {
            attempt,
            current
        } if attempt.block_number == 1 && current.block_number == 2
    ));
}

#[test]
fn reserve_fails_closed_without_pool_universe_fingerprint() {
    let sm = sm();
    let cand = candidate(1);
    let status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        cand.snapshot_id,
        cand.header,
        HashMap::new(),
        ProtocolCoverage::default(),
    )));
    let error = sm.reserve(cand, &status, fee_ctx(1)).unwrap_err();
    assert!(matches!(
        error,
        IntentError::StaleTopology { current: None, .. }
    ));
}

#[test]
fn same_block_topology_promotion_rejects_old_and_accepts_new() {
    let sm = sm();
    let old = candidate(1);
    let mut promoted = old.clone();
    promoted.pool_universe_fingerprint = B256::repeat_byte(0xAA);
    let mut coverage = ProtocolCoverage::default();
    coverage.pool_universe_fingerprint = Some(promoted.pool_universe_fingerprint);
    let status = SnapshotStatus::Ready(Arc::new(MarketSnapshot::new(
        old.snapshot_id,
        old.header,
        HashMap::new(),
        coverage,
    )));
    assert!(matches!(
        sm.reserve(old, &status, fee_ctx(1)).unwrap_err(),
        IntentError::StaleTopology { .. }
    ));
    assert!(sm.reserve(promoted, &status, fee_ctx(1)).is_ok());
}

#[test]
fn replace_rejects_stale_same_candidate_after_broadcast() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    sm.record_submission(&signed(nonce, 1, cand.clone()))
        .unwrap();
    let err = sm
        .replace(nonce, cand.clone(), &ready_status(1), fee_ctx(1), true)
        .unwrap_err();
    assert!(matches!(err, IntentError::StaleReplacement));
}

#[test]
fn replace_unprofitable_without_broadcast_releases() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    let err = sm
        .replace(nonce, candidate(2), &ready_status(2), fee_ctx(2), false)
        .unwrap_err();
    assert!(matches!(err, IntentError::CancelWithoutBroadcast));
    assert!(sm.intent(nonce).unwrap().is_none());
}

#[test]
fn execute_success_emits_gas_profile_requalification_when_hot() {
    let sm = sm();
    let cand = candidate(10);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(10), fee_ctx(10))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let mut s = signed(nonce, 3, cand);
    // Force utilization over 95% of gas_limit 200_000.
    s.fee_plan.gas_limit = 200_000;
    s.fee_plan.expected_gas_used = 150_000;
    sm.record_submission(&s).unwrap();

    let mut canonical = HashMap::new();
    canonical.insert(10, B256::from([10; 32]));
    let outcome = ReceiptOutcome {
        success: true,
        block_number: 10,
        block_hash: B256::from([10; 32]),
        gas_used: 195_000,
        effective_gas_price: 50_000_000_000,
        l1_fee: None,
        execution_layer_only: true,
    };
    let mut receipts = HashMap::new();
    receipts.insert(s.tx_hash, Some(outcome));
    let events = sm
        .on_new_block(
            CanonicalBlock {
                number: 11,
                hash: B256::from([11; 32]),
            },
            &canonical,
            &receipts,
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 1,
                pending_nonce: 1,
            },
        )
        .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        IntentEvent::GasProfileRequalification {
            gas_used: 195_000,
            ..
        }
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, IntentEvent::Finalized { success: true, .. })));
}

#[test]
fn operator_recover_extra_cancel_attempts_frees_budget() {
    let mut p = policy();
    p.max_cancel_attempts = 1;
    let sm = IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        p,
        false,
    )
    .unwrap();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    sm.record_submission(&signed(nonce, 1, cand.clone()))
        .unwrap();
    let cancel = SignedSubmission {
        raw: Bytes::from(vec![9]),
        tx_hash: B256::from([9; 32]),
        fee_plan: FeePlan {
            block_fee_context: fee_ctx(1),
            gas_limit: 21_000,
            expected_gas_used: 21_000,
            expected_gas_cost: U256::from(21_000u64),
            max_fee_per_gas: 60_000_000_000,
            max_priority_fee_per_gas: 112_500,
            profile_identity: FeePlan::CANCEL_PROFILE_IDENTITY.into(),
        },
        payload: PreparedPayload::Cancel {
            to: Address::ZERO,
            gas_limit: 21_000,
        },
        calldata_digest: B256::ZERO,
        nonce,
        submitted_at: cand.snapshot_id,
    };
    sm.record_submission(&cancel).unwrap();
    assert!(sm.cancel_budget_exhausted(nonce).unwrap());
    sm.mark_needs_operator(nonce, NeedsOperatorReason::CancelBudgetExhausted)
        .unwrap();
    sm.operator_recover(nonce, &[cancel.tx_hash], 1).unwrap();
    assert!(!sm.cancel_budget_exhausted(nonce).unwrap());
}

#[test]
fn cancel_fee_cap_validation_uses_ceil_not_floor() {
    // floor(100 * 10050 / 10000) = 100; ceil = 101. Spec requires ceil.
    assert_eq!(bump_fee_value(100, 50).unwrap(), 100);
    assert_eq!(bump_fee_value_ceil(100, 50).unwrap(), 101);

    let mut p = policy();
    p.max_fee_cap_wei = 100;
    p.fee_bump_bps = 50;
    p.cancel_fee_cap_wei = 100; // equals floor, below ceil
    assert!(p.validate().is_err());
    p.cancel_fee_cap_wei = 101;
    assert!(p.validate().is_ok());
}

#[test]
fn deadline_helpers_share_header_timestamp_math() {
    use crate::execution::deadline_from_header_timestamp;
    let sm = sm();
    let h = header(1_000);
    assert_eq!(sm.deadline_for_header(&h).unwrap(), U256::from(1_060u64));
    assert_eq!(
        deadline_from_header_timestamp(h.block_timestamp, 60).unwrap(),
        U256::from(1_060u64)
    );
    assert!(deadline_from_header_timestamp(u64::MAX, 1).is_err());
}

#[test]
fn needs_operator_entry_and_operator_recover_exit() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    sm.record_submission(&signed(nonce, 1, cand)).unwrap();
    sm.mark_needs_operator(nonce, NeedsOperatorReason::CancelBudgetExhausted)
        .unwrap();
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(
        intent.state,
        IntentState::NeedsOperator {
            reason: NeedsOperatorReason::CancelBudgetExhausted
        }
    ));
    let hash = intent.attempts[0].tx_hash;
    sm.operator_recover(nonce, &[hash], 0).unwrap();
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::Submitted
    ));
}

#[test]
fn cancel_signs_while_halted_for_live_submitted_intent() {
    let mut p = policy();
    p.reorg_track_blocks = 1;
    p.confirmation_depth = 1; // include stays unconfirmed until conf >= 1
    let sm = IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        p,
        false,
    )
    .unwrap();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 2, cand.clone());
    sm.record_submission(&s).unwrap();
    let mut canonical = HashMap::new();
    canonical.insert(1, B256::from([1; 32]));
    let mut receipts = HashMap::new();
    receipts.insert(
        s.tx_hash,
        Some(ReceiptOutcome {
            success: true,
            block_number: 1,
            block_hash: B256::from([1; 32]),
            gas_used: 100,
            effective_gas_price: 1,
            l1_fee: None,
            execution_layer_only: true,
        }),
    );
    // head == inclusion block => conf 0 < confirmation_depth 1 => IncludedUnconfirmed
    sm.on_new_block(
        CanonicalBlock {
            number: 1,
            hash: B256::from([1; 32]),
        },
        &canonical,
        &receipts,
        &HashMap::new(),
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        },
    )
    .unwrap();
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::IncludedUnconfirmed { .. }
    ));
    // Depth beyond reorg window + diverged inclusion hash => halt.
    canonical.insert(1, B256::from([99; 32]));
    sm.on_new_block(
        CanonicalBlock {
            number: 5,
            hash: B256::from([5; 32]),
        },
        &canonical,
        &HashMap::new(),
        &HashMap::new(),
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 1,
        },
    )
    .unwrap();
    assert!(sm.is_halted().unwrap());
    // Spec: cancel may still be signed while Halted for a live broadcast intent.
    // Deep halt leaves IncludedUnconfirmed; route through NeedsOperator so
    // record_submission can accept a cancel without a fresh Execute prepare.
    sm.mark_needs_operator(nonce, NeedsOperatorReason::DeepReorgHalt)
        .expect("IncludedUnconfirmed -> NeedsOperator");
    let cancel = SignedSubmission {
        raw: Bytes::from(vec![9]),
        tx_hash: B256::from([9; 32]),
        fee_plan: FeePlan {
            block_fee_context: fee_ctx(1),
            gas_limit: 21_000,
            expected_gas_used: 21_000,
            expected_gas_cost: U256::from(21_000u64),
            max_fee_per_gas: 60_000_000_000,
            max_priority_fee_per_gas: 112_500,
            profile_identity: FeePlan::CANCEL_PROFILE_IDENTITY.into(),
        },
        payload: PreparedPayload::Cancel {
            to: Address::ZERO,
            gas_limit: 21_000,
        },
        calldata_digest: B256::ZERO,
        nonce,
        submitted_at: cand.snapshot_id,
    };
    sm.record_submission(&cancel).unwrap();
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::Submitted
    ));
    // Execute record while Halted is rejected.
    let err = sm
        .record_submission(&signed(nonce, 8, candidate(1)))
        .unwrap_err();
    assert!(matches!(err, IntentError::Halted(_)));
}

#[test]
fn stuck_detection_triggers_after_configured_blocks() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let s = signed(nonce, 8, cand);
    sm.record_submission(&s).unwrap();
    assert!(!sm.is_stuck(nonce).unwrap());
    for n in 2..=4 {
        sm.on_new_block(
            CanonicalBlock {
                number: n,
                hash: B256::from([n as u8; 32]),
            },
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            ChainNonceView {
                latest_nonce: 0,
                pending_nonce: 1,
            },
        )
        .unwrap();
    }
    // policy stuck_after_blocks = 3, so after 3 increments pending_blocks >= 3
    assert!(sm.is_stuck(nonce).unwrap());
    // Replacement with a fresh candidate after stuck is accepted.
    let fresh = candidate(5);
    let permit = sm
        .replace(nonce, fresh.clone(), &ready_status(5), fee_ctx(5), true)
        .unwrap();
    assert_eq!(permit.snapshot_id(), fresh.snapshot_id);
}

#[test]
fn nonce_manager_is_module_private_surface() {
    // Compile-time contract: NonceManager is not re-exported from execution.
    // This test documents the acceptance visibility rule; the type is only
    // reachable as `pub(super)` inside the intent module (see nonce.rs).
    let sm = sm();
    assert_eq!(sm.peek_next_nonce().unwrap(), 0);
    let _ = reserve_ok(&sm, 1);
    assert_eq!(sm.peek_next_nonce().unwrap(), 1);
}

#[test]
fn base_fee_drop_rearms_exactly_one_fee_recovery_cancel() {
    // Spec: unpriceable cancel enters NeedsOperator; a later cheaper base-fee
    // context re-arms exactly one fee-recovery retry. Budget exhaustion never
    // auto-exits.
    //
    // High base fee forces max_fee up to base+priority and past cancel cap;
    // after the base-fee drop the prior max fits under the cap again.
    let mut p = policy();
    p.max_fee_cap_wei = 100;
    p.cancel_fee_cap_wei = 200;
    p.fee_bump_bps = 0;
    let sm = IntentStateMachine::new(
        Address::ZERO,
        ChainNonceView {
            latest_nonce: 0,
            pending_nonce: 0,
        },
        p.clone(),
        false,
    )
    .unwrap();
    let cand = candidate(1);
    let (nonce, _) = sm
        .reserve(cand.clone(), &ready_status(1), fee_ctx(1))
        .unwrap();
    sm.begin_prepare(nonce).unwrap();
    let mut s = signed(nonce, 2, cand.clone());
    s.fee_plan.max_fee_per_gas = 100;
    s.fee_plan.max_priority_fee_per_gas = 1;
    sm.record_submission(&s).unwrap();

    let high_base = crate::execution::BlockFeeContext {
        block_number: 2,
        block_hash: B256::from([2; 32]),
        base_fee_per_gas: 1_000,
        block_gas_limit: 60_000_000,
    };
    let prior = PriorFees::new(1, 100);
    let err = FeePlan::for_cancel(
        p.cancel_gas_limit,
        prior,
        p.fee_bump_bps,
        p.cancel_fee_cap_wei,
        &high_base,
        1,
    )
    .unwrap_err();
    assert!(matches!(
        err,
        crate::execution::FeePlanError::CancelFeeCapExceeded { .. }
    ));
    sm.mark_needs_operator(nonce, NeedsOperatorReason::CancelUnpriceable)
        .unwrap();
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::NeedsOperator {
            reason: NeedsOperatorReason::CancelUnpriceable
        }
    ));

    // Still unpriceable under the high base fee → no re-arm.
    assert!(sm
        .try_rearm_fee_recovery_cancel(nonce, &high_base, 1)
        .unwrap()
        .is_none());
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::NeedsOperator {
            reason: NeedsOperatorReason::CancelUnpriceable
        }
    ));

    let low_base = crate::execution::BlockFeeContext {
        block_number: 3,
        block_hash: B256::from([3; 32]),
        base_fee_per_gas: 50,
        block_gas_limit: 60_000_000,
    };
    let plan = sm
        .try_rearm_fee_recovery_cancel(nonce, &low_base, 1)
        .unwrap()
        .expect("base-fee drop should re-arm exactly one cancel");
    assert_eq!(plan.profile_identity, FeePlan::CANCEL_PROFILE_IDENTITY);
    assert_eq!(plan.max_fee_per_gas, 100);
    assert!(matches!(
        sm.intent(nonce).unwrap().unwrap().state,
        IntentState::Submitted
    ));
    assert!(
        !sm.intent(nonce)
            .unwrap()
            .unwrap()
            .fee_recovery_retry_available
    );

    // Second automatic re-arm is refused (flag already spent).
    sm.mark_needs_operator(nonce, NeedsOperatorReason::CancelUnpriceable)
        .unwrap();
    assert!(sm
        .try_rearm_fee_recovery_cancel(nonce, &low_base, 1)
        .unwrap()
        .is_none());

    // Budget-exhaustion quarantine never auto-exits via fee recovery.
    // Return to Submitted first so we can re-enter with a different reason.
    let hash = sm.intent(nonce).unwrap().unwrap().attempts[0].tx_hash;
    sm.operator_recover(nonce, &[hash], 0).unwrap();
    sm.mark_needs_operator(nonce, NeedsOperatorReason::CancelBudgetExhausted)
        .unwrap();
    let err = sm
        .try_rearm_fee_recovery_cancel(nonce, &low_base, 1)
        .unwrap_err();
    assert!(matches!(
        err,
        IntentError::NeedsOperator(NeedsOperatorReason::CancelBudgetExhausted)
    ));
}
