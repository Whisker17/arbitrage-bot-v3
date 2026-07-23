//! WHI-553: end-to-end wallet-free pipeline-head wiring tests.
//!
//! Exercises `prepare_pipeline_head` / `run_pipeline_head_closed` against a real
//! `Executor`/`ExecutionContext` (mocked provider, real checked-in gas-profile artifact
//! and contract bytecode) for each of the three route shapes the four monitor services
//! use (V2-only, V3, Moe). Everything here goes through public API only: permits are
//! minted via a real `IntentStateMachine::reserve`, never fabricated.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use alloy::primitives::{aliases::U112, Address, B256, U256};
use alloy::providers::ProviderBuilder;
use alloy::transports::mock::Asserter;
use alloy::sol_types::SolValue;

use amms::execution::*;
use amms::state_space::{
    BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
};

struct CountingPreflight {
    count: Arc<AtomicUsize>,
}

impl PreflightSlot for CountingPreflight {
    async fn preflight(&self, _request: &FinalRequest) -> eyre::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct AlwaysFailingPreflight;

impl PreflightSlot for AlwaysFailingPreflight {
    async fn preflight(&self, _request: &FinalRequest) -> eyre::Result<()> {
        eyre::bail!("synthetic preflight failure for pre-prepare-failure cleanup test")
    }
}

struct AlwaysValidIdentity;

impl ExecutionIdentitySource for AlwaysValidIdentity {
    async fn validate(&self, _identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        unimplemented!("never reached by prepare_pipeline_head/run_pipeline_head_closed")
    }
}

struct Fixture {
    executor: Executor,
    wmnt_address: Address,
}

async fn build_fixture(route_keys: Vec<RouteKey>) -> Fixture {
    let artifact_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile =
        RuntimeGasProfile::load(&artifact_path, RuntimeProfileConfig::mantle_mainnet(route_keys))
            .expect("gas profile artifact must load for the requested route keys");

    let bytecode_hex = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("contracts/executor/artifacts/ArbitrageExecutor.deployed.hex"),
    )
    .expect("checked-in executor bytecode artifact must exist");
    let bytecode = hex::decode(bytecode_hex.trim()).expect("bytecode artifact must be valid hex");

    let executor_contract = Address::repeat_byte(0xE0);
    let wmnt_address = Address::repeat_byte(0xC0);

    let asserter = Asserter::new();
    asserter.push_success(&5000u64);
    asserter.push_success(&alloy::primitives::Bytes::from(bytecode));
    asserter.push_success(&alloy::primitives::Bytes::from(wmnt_address.abi_encode()));
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);

    let fee_contexts = Arc::new(BlockFeeContextCache::default());
    fee_contexts
        .publish(fee_context())
        .expect("fee context publish must succeed");

    let context = ExecutionContext::from_provider(
        provider,
        executor_contract,
        wmnt_address,
        gas_profile,
        fee_contexts,
    )
    .await
    .expect("mocked provider responses must satisfy from_provider's identity checks");

    Fixture {
        executor: Executor::new(context, ExecutorConfig::default()),
        wmnt_address,
    }
}

fn fee_context() -> BlockFeeContext {
    BlockFeeContext {
        block_number: 42,
        block_hash: B256::repeat_byte(0x42),
        base_fee_per_gas: 50_000_000_000,
        block_gas_limit: 60_000_000,
    }
}

fn ready_status(snapshot_id: SnapshotId, header: BlockHeaderContext, fingerprint: B256) -> SnapshotStatus {
    let coverage = ProtocolCoverage {
        fingerprint: Some(fingerprint),
        pool_universe_fingerprint: Some(fingerprint),
    };
    let snapshot = MarketSnapshot::new(snapshot_id, header, HashMap::new(), coverage);
    SnapshotStatus::Ready(snapshot.into_arc())
}

/// One SM + candidate + params bundle for a single `run_pipeline_head_closed`/
/// `prepare_pipeline_head` call.
struct Scenario {
    sm: IntentStateMachine,
    candidate: CandidateRef,
    status: SnapshotStatus,
    fee_ctx: BlockFeeContext,
    params: FinalRequestParams,
    chain: ChainNonceView,
}

fn build_scenario(fixture: &Fixture, route_key: RouteKey, pool_type: u8, crossing_buckets: Option<VerifiedCrossingBuckets>) -> Scenario {
    let signer_address = Address::repeat_byte(0x77);
    let chain = ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    };
    let sm = IntentStateMachine::new(
        signer_address,
        chain.clone(),
        IntentPolicy::with_caps(1_000_000_000_000, 2_000_000_000_000),
        false,
    )
    .expect("valid SM construction");

    let snapshot_id = SnapshotId::new(5000, 42, B256::repeat_byte(0x42));
    let header = BlockHeaderContext::new(B256::ZERO, 1_700_000_000);
    let fingerprint = B256::repeat_byte(0x99);
    let fee_ctx = fee_context();

    let amount_in = U256::from(1_000_000_000_000_000_000u128);
    let quote = fixture
        .executor
        .context
        .gas_profile()
        .quote(&route_key)
        .expect("route key must be approved in the gas profile fixture");
    let fee_plan = FeePolicy::new(
        fixture.executor.config.default_priority_fee_wei,
        fixture.executor.config.block_gas_limit_reserve,
    )
    .build(&quote, &fee_ctx)
    .expect("fee plan build must succeed against the published fee context");

    let final_out = amount_in + fee_plan.expected_gas_cost + U256::from(1_000u64);
    let mid_token = Address::repeat_byte(0x55);
    let wmnt = fixture.wmnt_address;

    let params = ExecutionParams::new(
        amount_in,
        route_key.clone(),
        vec![wmnt, mid_token, wmnt],
        vec![Address::repeat_byte(0x03), Address::repeat_byte(0x04)],
        vec![pool_type, pool_type],
        vec![(wmnt, mid_token), (mid_token, wmnt)],
        vec![U112::ZERO; 4],
        vec![amount_in, final_out],
        final_out,
        U256::from(1_000u64),
        crossing_buckets,
    )
    .expect("ExecutionParams::new must succeed for a structurally valid route");

    let candidate = CandidateRef {
        snapshot_id,
        header,
        pool_universe_fingerprint: fingerprint,
        route_key,
        amount_in,
    };

    let status = ready_status(snapshot_id, header, fingerprint);

    let final_request_params = FinalRequestParams {
        params,
        candidate: candidate.clone(),
        fee_plan,
        deadline: U256::from(1_700_000_060u64),
    };

    Scenario {
        sm,
        candidate,
        status,
        fee_ctx,
        params: final_request_params,
        chain,
    }
}

async fn run_closed_scenario_and_assert(fixture: &Fixture, scenario: Scenario) {
    let preflight_count = Arc::new(AtomicUsize::new(0));
    let preflight = CountingPreflight {
        count: preflight_count.clone(),
    };
    let identity_source = AlwaysValidIdentity;

    let outcome = run_pipeline_head_closed(
        &scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.executor,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain,
    )
    .await
    .expect("run_pipeline_head_closed must succeed for a well-formed scenario");

    assert_eq!(preflight_count.load(Ordering::SeqCst), 1);
    let _: FinalRequestDigest = outcome.digest;

    // The intent was reserved (nonce 0) and must be fully released/removed by the
    // closed-send cleanup (abort_prepare -> reconcile), not merely reset to Reserved.
    assert!(scenario
        .sm
        .intent(0)
        .expect("intent lookup must succeed")
        .is_none());
    assert_eq!(
        scenario.sm.peek_next_nonce().expect("peek must succeed"),
        0,
        "a fully released zero-broadcast intent must free its nonce back up"
    );
}

#[tokio::test]
async fn v2_route_runs_through_closed_pipeline_head() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);
    run_closed_scenario_and_assert(&fixture, scenario).await;
}

#[tokio::test]
async fn v3_route_runs_through_closed_pipeline_head() {
    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None));
    let scenario = build_scenario(&fixture, route_key, 1, crossing_buckets);
    run_closed_scenario_and_assert(&fixture, scenario).await;
}

/// `v3_monitor_executor_service_1559` shares the exact same wallet-free
/// request-building path as the legacy V3 service: `build_final_request` always
/// constructs a type-2 (EIP-1559) `TransactionRequest`, so there is no structurally
/// distinct pipeline-head behavior to cover for the "1559" service beyond the V3 route
/// shape already exercised above.
#[tokio::test]
async fn v3_1559_route_runs_through_closed_pipeline_head() {
    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None));
    let scenario = build_scenario(&fixture, route_key, 1, crossing_buckets);
    run_closed_scenario_and_assert(&fixture, scenario).await;
}

#[tokio::test]
async fn moe_route_runs_through_closed_pipeline_head() {
    let route_key = RouteKey::new(vec![ProtocolKind::Moe, ProtocolKind::Moe]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(None, Some(BinCrossingBucket::Zero)));
    let scenario = build_scenario(&fixture, route_key, 2, crossing_buckets);
    run_closed_scenario_and_assert(&fixture, scenario).await;
}

/// A failure *inside* `prepare_pipeline_head` (here: the preflight slot rejecting the
/// built request) must run cleanup (`abort_prepare` -> `reconcile`) exactly once and
/// fully release the reserved nonce, mirroring the closed-continuation cleanup path
/// exercised by `run_closed_scenario_and_assert` above but triggered from a pre-prepare
/// failure rather than success.
#[tokio::test]
async fn preflight_failure_inside_prepare_pipeline_head_cleans_up_exactly_once() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);

    let identity_source = AlwaysValidIdentity;
    let preflight = AlwaysFailingPreflight;

    let result = prepare_pipeline_head(
        &scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.executor,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain.clone(),
    )
    .await;

    assert!(
        result.is_err(),
        "a preflight rejection must propagate as an error from prepare_pipeline_head"
    );

    // The reserved nonce must be fully released/removed by the pre-prepare-failure
    // cleanup (abort_prepare -> reconcile), not merely reset to Reserved.
    assert!(scenario
        .sm
        .intent(0)
        .expect("intent lookup must succeed")
        .is_none());
    assert_eq!(
        scenario.sm.peek_next_nonce().expect("peek must succeed"),
        0,
        "a pre-prepare failure must free the reserved nonce back up"
    );
}

/// `prepare_pipeline_head` alone leaves the intent `Preparing` (uncommitted): only a
/// consuming continuation such as `run_pipeline_head_closed` (whose `into_closed_outcome`
/// is `pub(crate)` and thus uncallable from this external test binary) may abort+
/// reconcile. This demonstrates the continuation-ownership invariant behaviorally,
/// since the compiler already enforces it structurally for any code outside the crate.
#[tokio::test]
async fn prepare_alone_leaves_intent_uncommitted_until_a_continuation_runs() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);

    let preflight_count = Arc::new(AtomicUsize::new(0));
    let preflight = CountingPreflight {
        count: preflight_count.clone(),
    };
    let identity_source = AlwaysValidIdentity;

    let head = prepare_pipeline_head(
        &scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.executor,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain.clone(),
    )
    .await
    .expect("prepare_pipeline_head must succeed for a well-formed scenario");

    assert_eq!(preflight_count.load(Ordering::SeqCst), 1);
    assert_eq!(head.nonce(), 0);
    let _: &FinalRequest = head.request();
    let _: FinalRequestDigest = head.digest();

    // Uncommitted: prepare_pipeline_head never aborts/reconciles on success.
    let intent = scenario
        .sm
        .intent(0)
        .expect("intent lookup must succeed")
        .expect("intent must still be live after prepare alone");
    assert_eq!(intent.state, IntentState::Preparing);

    // A separately-authorized continuation (here: driving the same public
    // abort_prepare/reconcile calls `into_closed_outcome` performs internally) is the
    // only way to release it. `PreparedPipelineHead::into_closed_outcome` itself is
    // pub(crate) and therefore uncallable from this file.
    scenario
        .sm
        .abort_prepare(head.nonce())
        .expect("abort_prepare must succeed from Preparing");
    scenario
        .sm
        .reconcile(scenario.chain)
        .expect("reconcile must succeed");

    assert!(scenario
        .sm
        .intent(0)
        .expect("intent lookup must succeed")
        .is_none());
}
