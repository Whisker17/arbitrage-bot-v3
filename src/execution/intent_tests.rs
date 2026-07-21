use super::*;
use crate::execution::gas_profile::{ProtocolKind, RouteKey};
use crate::execution::types::IntentPolicy;
use crate::state_space::{BlockHeaderContext, SnapshotId};
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
        route_key: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap(),
        amount_in: U256::from(1_000_000u64),
    }
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
    let (nonce, permit) = sm.reserve(candidate(1)).unwrap();
    assert_eq!(nonce, 0);
    assert_eq!(permit.nonce(), 0);
    assert_eq!(permit.snapshot_id(), snap(1));
}

#[test]
fn preparing_is_not_released_by_reconcile() {
    let sm = sm();
    let (nonce, _) = sm.reserve(candidate(1)).unwrap();
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
    let (nonce, _) = sm.reserve(candidate(1)).unwrap();
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
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
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
    assert!(released.is_empty(), "ambiguous/broadcast attempt must not release");
}

#[test]
fn broadcast_order_rejects_gap() {
    let sm = sm();
    let (n0, _) = sm.reserve(candidate(1)).unwrap();
    let (n1, _) = sm.reserve(candidate(2)).unwrap();
    // Try to submit n1 while n0 still Reserved zero-broadcast.
    sm.begin_prepare(n1).unwrap();
    let err = sm
        .record_submission(&signed(n1, 9, candidate(2)))
        .unwrap_err();
    assert!(matches!(err, IntentError::BroadcastOrder { nonce: 1, blocker: 0 }));
    // After n0 submits, n1 may submit.
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    sm.record_submission(&signed(n1, 2, candidate(2))).unwrap();
}

#[test]
fn reconcile_releases_contiguous_reserved_suffix_only() {
    let sm = sm();
    let (n0, _) = sm.reserve(candidate(1)).unwrap();
    let (n1, _) = sm.reserve(candidate(2)).unwrap();
    let (n2, _) = sm.reserve(candidate(3)).unwrap();
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
    let (n0, _) = sm.reserve(candidate(1)).unwrap();
    let (n1, _) = sm.reserve(candidate(2)).unwrap();
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
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
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
    assert!(events.iter().any(|e| matches!(e, IntentEvent::Finalized { success: true, .. })));
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::Finalized));
    assert_eq!(outcome.actual_cost(), U256::from(180_000u64) * U256::from(50_000_000_000u64) + U256::from(1000u64));
}

#[test]
fn cancel_receipt_never_qualifies_gas_and_is_terminal_even_on_failure() {
    let sm = sm();
    let cand = candidate(5);
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
    sm.begin_prepare(nonce).unwrap();
    // First an execute attempt so cancel baseline exists.
    sm.record_submission(&signed(nonce, 1, cand.clone())).unwrap();
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
    assert!(events.iter().any(|e| matches!(
        e,
        IntentEvent::CancelFinalized {
            anomaly: true,
            ..
        }
    )));
    let intent = sm.intent(nonce).unwrap().unwrap();
    assert!(matches!(intent.state, IntentState::CancelFinalized));
}

#[test]
fn reorg_reopens_when_inclusion_hash_diverges_even_if_receipt_none() {
    let sm = sm();
    let cand = candidate(20);
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
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
    assert!(events.iter().any(|e| matches!(e, IntentEvent::Reopened { .. })));
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
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
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
    assert!(events.iter().any(|e| matches!(e, IntentEvent::Halted { .. })));
    assert!(sm.is_halted().unwrap());
}

#[test]
fn drop_detection_is_debounced() {
    let sm = sm();
    let cand = candidate(1);
    let (nonce, _) = sm.reserve(cand.clone()).unwrap();
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
    let prior = (100_000u128, 190_000_000_000u128);
    let plan = FeePlan::for_cancel(
        21_000,
        prior,
        1_250,
        400_000_000_000,
        &ctx,
        1,
    )
    .unwrap();
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
    assert!(matches!(err, crate::execution::FeePlanError::InvalidGasQuote));
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
    let err = sm.reserve(candidate(1)).unwrap_err();
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
fn concurrency_second_intent_can_be_prepared_while_first_unconfirmed() {
    let sm = Arc::new(sm());
    let sm2 = Arc::clone(&sm);
    let (n0, _) = sm.reserve(candidate(1)).unwrap();
    sm.begin_prepare(n0).unwrap();
    sm.record_submission(&signed(n0, 1, candidate(1))).unwrap();
    // Simulate blocked broadcast: intent stays Submitted while we prepare next.
    let handle = std::thread::spawn(move || {
        let (n1, _) = sm2.reserve(candidate(2)).unwrap();
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
