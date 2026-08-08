use super::{
    fee_plan_cost, BlockFeeContext, BlockFeeContextCache, FeePlanError, FeePolicy, FeeScoreKey,
    GasQuote,
};
use alloy::primitives::{B256, U256};

#[test]
fn fee_plan_prices_profitability_with_expected_gas_not_transaction_limit() {
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 200,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };
    let fee_plan = FeePolicy::new(10, 100).build(&quote, &context).unwrap();

    assert_eq!(fee_plan.gas_limit, 200);
    assert_eq!(fee_plan.expected_gas_used, 100);
    assert_eq!(
        fee_plan.expected_gas_cost,
        alloy::primitives::U256::from(6_000u64)
    );
}

#[test]
fn block_fee_context_cache_rejects_a_stale_candidate_context() {
    let cache = BlockFeeContextCache::default();
    let current = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };
    cache.publish(current).unwrap();
    let stale = BlockFeeContext {
        block_number: 41,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };

    let error = cache.matching(&stale).unwrap_err();

    assert!(matches!(error, FeePlanError::StaleBlockFeeContext));
}

#[test]
fn block_fee_context_cache_publishes_live_header_fields() {
    let cache = BlockFeeContextCache::default();
    let mut header = alloy::rpc::types::eth::Header::<alloy::consensus::Header>::default();
    header.hash = B256::from([1u8; 32]);
    header.inner.number = 42;
    header.inner.gas_limit = 1_000;
    header.inner.base_fee_per_gas = Some(50);

    cache.publish_header(&header).unwrap();

    let context = cache
        .matching(&BlockFeeContext {
            block_number: 42,
            block_hash: B256::from([1u8; 32]),
            base_fee_per_gas: 50,
            block_gas_limit: 1_000,
        })
        .unwrap();
    assert_eq!(context.block_number, 42);
}

#[test]
fn fee_plan_rejects_a_gas_limit_inside_the_block_reserve() {
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 900,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };

    let error = FeePolicy::new(10, 100).build(&quote, &context).unwrap_err();

    assert!(matches!(
        error,
        FeePlanError::GasLimitExceedsBlockReserve { .. }
    ));
}

#[test]
fn fee_plan_reports_priority_fee_overflow() {
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 200,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: u128::MAX,
        block_gas_limit: 1_000,
    };

    let error = FeePolicy::new(1, 100).build(&quote, &context).unwrap_err();

    assert!(matches!(error, FeePlanError::Overflow));
}

#[test]
fn fee_plan_cost_matches_fee_policy_build_including_priority_fee() {
    // Non-zero priority fee: cost = expected_gas_used × (base_fee + priority).
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 200,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };
    let policy = FeePolicy::new(17, 100);

    let via_cost = fee_plan_cost(&quote, &context, policy).unwrap();
    let via_build = policy.build(&quote, &context).unwrap().expected_gas_cost;

    assert_eq!(via_cost, via_build);
    assert_eq!(via_cost, U256::from(100u64 * (50 + 17)));
}

#[test]
fn fee_plan_cost_rejects_gas_limit_inside_block_reserve() {
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 900,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };

    let error = fee_plan_cost(&quote, &context, FeePolicy::new(10, 100)).unwrap_err();
    assert!(matches!(
        error,
        FeePlanError::GasLimitExceedsBlockReserve { .. }
    ));
}

#[test]
fn fee_score_key_changes_when_priority_policy_only_changes() {
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 30_000_000,
    };
    let a = FeeScoreKey::from_policy_and_context(FeePolicy::new(100_000, 1), &context);
    let b = FeeScoreKey::from_policy_and_context(FeePolicy::new(200_000, 1), &context);
    assert_ne!(
        a, b,
        "priority-only policy change must invalidate gas re-score key"
    );
    assert_eq!(a.base_fee_per_gas, b.base_fee_per_gas);
}

#[test]
fn receipt_gas_at_the_utilization_threshold_requires_requalification() {
    let quote = GasQuote {
        route_key: crate::execution::RouteKey::new(vec![crate::execution::ProtocolKind::V2])
            .unwrap(),
        gas_limit: 200,
        expected_gas_used: 100,
        profile_identity: "profile".into(),
    };
    let context = BlockFeeContext {
        block_number: 42,
        block_hash: B256::ZERO,
        base_fee_per_gas: 50,
        block_gas_limit: 1_000,
    };
    let fee_plan = FeePolicy::new(10, 100).build(&quote, &context).unwrap();

    let error = fee_plan.qualify_receipt_gas(190, 9_500).unwrap_err();

    assert!(matches!(
        error,
        FeePlanError::ReceiptGasThresholdExceeded {
            gas_used: 190,
            threshold: 190,
            gas_limit: 200,
        }
    ));
}
