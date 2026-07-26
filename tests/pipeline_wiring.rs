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

use alloy::primitives::{aliases::U112, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::transports::mock::Asserter;
use alloy::sol_types::SolValue;

use amms::execution::runtime_identity::{resolve_immutable_plan, BuildEvidence, ImmutableInputs};
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

/// WHI-521: counts `SemanticCallExecutor::call` invocations rather than
/// `PreflightSlot::preflight` invocations, so tests using this double exercise the real
/// `RiskTieredPreflight` (wired the same way WHI-553's single wiring point wires it) and
/// prove it issues exactly one semantic call per candidate through the real pipeline
/// head, not just that the slot itself was invoked once.
struct CountingSemanticCallExecutor {
    count: Arc<AtomicUsize>,
}

impl SemanticCallExecutor for CountingSemanticCallExecutor {
    async fn call(&self, _request: &FinalRequest, _tag: BlockTag) -> Result<CallOutcome, SemanticCallError> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(CallOutcome::Success)
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

/// Queued *after* the three identity-check responses `from_provider` consumes. Nothing in
/// the wallet-free head may issue an RPC, so this response must still be the next one in
/// the queue when a scenario finishes — a signing/broadcast round trip would eat it.
const RPC_SENTINEL: [u8; 4] = [0x5E, 0x17, 0x11, 0x01];

struct Fixture {
    executor: Executor,
    wmnt_address: Address,
    provider: DynProvider,
}

impl Fixture {
    /// Proves zero RPC traffic since construction: the sentinel is still unconsumed.
    /// Any `eth_sendRawTransaction` (broadcast) or extra read would have popped it.
    async fn assert_no_rpc_since_startup(&self) {
        let sentinel = self
            .provider
            .get_code_at(Address::repeat_byte(0x5E))
            .await
            .expect("sentinel response must still be queued: the head must issue zero RPCs");
        assert_eq!(
            sentinel,
            Bytes::from(RPC_SENTINEL.to_vec()),
            "an unexpected RPC (e.g. a broadcast) consumed the sentinel response"
        );
    }
}

async fn build_fixture(route_keys: Vec<RouteKey>) -> Fixture {
    build_fixture_with_config(route_keys, ExecutorConfig::default()).await
}

async fn build_fixture_with_config(
    route_keys: Vec<RouteKey>,
    executor_config: ExecutorConfig,
) -> Fixture {
    let artifact_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile =
        RuntimeGasProfile::load(&artifact_path, RuntimeProfileConfig::mantle_mainnet(route_keys))
            .expect("gas profile artifact must load for the requested route keys");

    // `from_provider` pins the WMNT-patched runtime hash (WHI-551), so the mock must
    // serve patched bytes derived from the committed build evidence with the same
    // WMNT the identity export was derived with — the raw template can never match.
    let expected_chain_id = gas_profile.executor_identity().chain_id;
    let identity_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/executor_identity.json"),
        )
        .expect("checked-in executor identity export must exist"),
    )
    .expect("executor identity export must be valid JSON");
    let wmnt_address: Address = identity_json["wmnt"]
        .as_str()
        .expect("identity export must record the wmnt immutable")
        .parse()
        .expect("identity export wmnt must be a valid address");
    let evidence = BuildEvidence::load(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts/executor/artifacts"),
    )
    .expect("checked-in executor build evidence must load");
    let plan = resolve_immutable_plan(
        &evidence,
        ImmutableInputs { wmnt: wmnt_address },
        expected_chain_id,
    )
    .expect("immutable plan must resolve from the committed evidence");
    let bytecode = plan.patched_bytes().to_vec();

    let executor_contract = Address::repeat_byte(0xE0);

    let asserter = Asserter::new();
    asserter.push_success(&5000u64);
    asserter.push_success(&alloy::primitives::Bytes::from(bytecode));
    asserter.push_success(&alloy::primitives::Bytes::from(wmnt_address.abi_encode()));
    asserter.push_success(&Bytes::from(RPC_SENTINEL.to_vec()));
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);

    let fee_contexts = Arc::new(BlockFeeContextCache::default());
    fee_contexts
        .publish(fee_context())
        .expect("fee context publish must succeed");

    let context = ExecutionContext::from_provider(
        provider.clone(),
        executor_contract,
        wmnt_address,
        gas_profile,
        fee_contexts,
    )
    .await
    .expect("mocked provider responses must satisfy from_provider's identity checks");

    Fixture {
        executor: Executor::new(context, executor_config),
        wmnt_address,
        provider: provider.erased(),
    }
}

/// Counts the SM events that record cleanup and broadcast. `Released` is emitted only by
/// `reconcile` releasing a zero-broadcast reserved nonce, so it is the cleanup counter;
/// `Submitted` is emitted only by `record_submission*`, i.e. only after signing.
fn event_counts(sm: &IntentStateMachine) -> (usize, usize, usize) {
    let events = sm.drain_events().expect("event drain must succeed");
    let reserved = events
        .iter()
        .filter(|e| matches!(e, IntentEvent::Reserved { .. }))
        .count();
    let released = events
        .iter()
        .filter(|e| matches!(e, IntentEvent::Released { .. }))
        .count();
    let submitted = events
        .iter()
        .filter(|e| matches!(e, IntentEvent::Submitted { .. }))
        .count();
    (reserved, released, submitted)
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
    sm: Arc<IntentStateMachine>,
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
    let sm = Arc::new(
        IntentStateMachine::new(
            signer_address,
            chain.clone(),
            IntentPolicy::with_caps(1_000_000_000_000, 2_000_000_000_000),
            false,
        )
        .expect("valid SM construction"),
    );

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

async fn run_closed_scenario_and_assert(
    fixture: &Fixture,
    scenario: Scenario,
) -> FinalRequestDigest {
    let preflight_count = Arc::new(AtomicUsize::new(0));
    let preflight = CountingPreflight {
        count: preflight_count.clone(),
    };
    let identity_source = AlwaysValidIdentity;
    let sm = Arc::clone(&scenario.sm);

    let outcome = run_pipeline_head_closed(
        scenario.sm,
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

    // The intent was reserved (nonce 0) and must be fully released/removed by the
    // closed-send cleanup (abort_prepare -> reconcile), not merely reset to Reserved.
    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        sm.peek_next_nonce().expect("peek must succeed"),
        0,
        "a fully released zero-broadcast intent must free its nonce back up"
    );

    // Cleanup counter: exactly one reserve and exactly one release for one candidate.
    // Zero `Submitted` events proves nothing was signed (`record_submission*` is the only
    // producer, and it only runs on the sign path).
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "expected exactly one Reserved + one Released (cleanup once) and zero Submitted"
    );

    // Cleanup cannot run a second time: the intent is gone, so a repeat abort errors.
    assert!(
        sm.abort_prepare(0).is_err(),
        "cleanup already ran; a second abort_prepare must not silently succeed"
    );

    // Zero broadcast at the transport level.
    fixture.assert_no_rpc_since_startup().await;

    outcome.digest
}

/// WHI-521 wiring proof: mirrors `run_closed_scenario_and_assert`, but uses the real
/// `RiskTieredPreflight` (Shadow tier, no approval configured -- the same construction
/// `examples/protocols/intent_service_support.rs`'s single wiring point uses) instead of
/// the `CountingPreflight` test double, asserting its `SemanticCallExecutor` is invoked
/// exactly once per candidate.
async fn run_closed_scenario_with_risk_tiered_preflight_and_assert(
    fixture: &Fixture,
    scenario: Scenario,
) -> FinalRequestDigest {
    let call_count = Arc::new(AtomicUsize::new(0));
    let call_executor = CountingSemanticCallExecutor {
        count: call_count.clone(),
    };
    let preflight = RiskTieredPreflight::new(call_executor, ExecutionStage::Shadow, None);
    let identity_source = AlwaysValidIdentity;
    let sm = Arc::clone(&scenario.sm);

    let outcome = run_pipeline_head_closed(
        scenario.sm,
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

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "RiskTieredPreflight's Mandatory tier must issue exactly one semantic call"
    );

    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "expected exactly one Reserved + one Released (cleanup once) and zero Submitted"
    );
    fixture.assert_no_rpc_since_startup().await;

    outcome.digest
}

#[tokio::test]
async fn v2_route_runs_through_closed_pipeline_head_with_risk_tiered_preflight() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);
    run_closed_scenario_with_risk_tiered_preflight_and_assert(&fixture, scenario).await;
}

#[tokio::test]
async fn v3_route_runs_through_closed_pipeline_head_with_risk_tiered_preflight() {
    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None));
    let scenario = build_scenario(&fixture, route_key, 1, crossing_buckets);
    run_closed_scenario_with_risk_tiered_preflight_and_assert(&fixture, scenario).await;
}

#[tokio::test]
async fn v3_1559_route_runs_through_closed_pipeline_head_with_risk_tiered_preflight() {
    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
    let mut executor_config = ExecutorConfig::default();
    executor_config.default_priority_fee_wei = 7_000_000_000;
    executor_config.min_net_profit_mnt_wei = U256::from(1u64);
    let fixture = build_fixture_with_config(vec![route_key.clone()], executor_config).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None));
    let scenario = build_scenario(&fixture, route_key, 1, crossing_buckets);
    run_closed_scenario_with_risk_tiered_preflight_and_assert(&fixture, scenario).await;
}

#[tokio::test]
async fn moe_route_runs_through_closed_pipeline_head_with_risk_tiered_preflight() {
    let route_key = RouteKey::new(vec![ProtocolKind::Moe, ProtocolKind::Moe]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(None, Some(BinCrossingBucket::Zero)));
    let scenario = build_scenario(&fixture, route_key, 2, crossing_buckets);
    run_closed_scenario_with_risk_tiered_preflight_and_assert(&fixture, scenario).await;
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

    // Baseline: the default priority fee from `ExecutorConfig::default()`.
    assert_eq!(
        scenario.params.fee_plan.max_priority_fee_per_gas,
        ExecutorConfig::default().default_priority_fee_wei
    );
    run_closed_scenario_and_assert(&fixture, scenario).await;
}

/// The "1559" service is the same route shape as the legacy V3 service but runs with its
/// own env-derived `ExecutorConfig` (`EXECUTOR_PRIORITY_FEE_WEI` /
/// `MIN_NET_PROFIT_WEI`), which feeds the type-2 fee fields that go into the signed
/// preimage. Drive those fields explicitly so this test can fail independently of
/// `v3_route_runs_through_closed_pipeline_head`.
#[tokio::test]
async fn v3_1559_route_runs_through_closed_pipeline_head() {
    const PRIORITY_FEE_WEI: u128 = 7_000_000_000;
    assert_ne!(
        PRIORITY_FEE_WEI,
        ExecutorConfig::default().default_priority_fee_wei,
        "the 1559 fixture must differ from the default-fee V3 fixture"
    );

    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3]).unwrap();
    let mut executor_config = ExecutorConfig::default();
    executor_config.default_priority_fee_wei = PRIORITY_FEE_WEI;
    executor_config.min_net_profit_mnt_wei = U256::from(1u64);
    let fixture = build_fixture_with_config(vec![route_key.clone()], executor_config).await;

    let crossing_buckets = Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None));
    let scenario = build_scenario(&fixture, route_key.clone(), 1, crossing_buckets);

    // The 1559-specific fee fields really are what the request is built from.
    assert_eq!(
        scenario.params.fee_plan.max_priority_fee_per_gas,
        PRIORITY_FEE_WEI
    );
    assert!(
        scenario.params.fee_plan.max_fee_per_gas
            >= fee_context().base_fee_per_gas + PRIORITY_FEE_WEI
    );

    let digest_1559 = run_closed_scenario_and_assert(&fixture, scenario).await;

    // Same route shape, default fees: the sender-bound type-2 digest must differ, so the
    // two service variants cannot pass/fail as one.
    let default_fixture = build_fixture(vec![route_key.clone()]).await;
    let default_scenario = build_scenario(
        &default_fixture,
        route_key,
        1,
        Some(VerifiedCrossingBuckets::new(Some(TickCrossingBucket::Zero), None)),
    );
    let digest_default = run_closed_scenario_and_assert(&default_fixture, default_scenario).await;

    assert_ne!(
        digest_1559, digest_default,
        "raising the 1559 priority fee must change the type-2 digest preimage"
    );
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
    let sm = Arc::clone(&scenario.sm);

    let result = prepare_pipeline_head(
        scenario.sm,
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
    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        sm.peek_next_nonce().expect("peek must succeed"),
        0,
        "a pre-prepare failure must free the reserved nonce back up"
    );
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "pre-prepare cleanup must run exactly once and never sign"
    );
    assert!(
        sm.abort_prepare(0).is_err(),
        "cleanup already ran; a second abort_prepare must not silently succeed"
    );
    fixture.assert_no_rpc_since_startup().await;
}

/// A prepared head that is dropped without a continuation is a caller bug, but it must
/// not strand a `Preparing` intent on a live nonce: `Drop` runs the same best-effort
/// `abort_prepare` + `reconcile` cleanup the closed continuation would have run.
#[tokio::test]
async fn dropping_an_unconsumed_prepared_head_releases_the_nonce() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);
    let sm = Arc::clone(&scenario.sm);

    let preflight_count = Arc::new(AtomicUsize::new(0));
    let preflight = CountingPreflight {
        count: preflight_count.clone(),
    };
    let identity_source = AlwaysValidIdentity;

    let head = prepare_pipeline_head(
        scenario.sm,
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
    .expect("prepare_pipeline_head must succeed for a well-formed scenario");

    assert_eq!(
        sm.intent(0)
            .expect("intent lookup must succeed")
            .expect("intent must be live while the head is held")
            .state,
        IntentState::Preparing
    );

    drop(head);

    assert!(
        sm.intent(0).expect("intent lookup must succeed").is_none(),
        "Drop must release the leaked Preparing intent"
    );
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "Drop cleanup must run exactly once and never sign"
    );
    fixture.assert_no_rpc_since_startup().await;
}

/// `prepare_pipeline_head` alone leaves the intent `Preparing` (uncommitted). The head is
/// non-`Clone`, so the only ways forward are the public consuming continuation
/// `into_closed_outcome` (exercised here, from outside the crate) or `Drop`'s fallback.
#[tokio::test]
async fn prepare_alone_leaves_intent_uncommitted_until_a_continuation_runs() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_fixture(vec![route_key.clone()]).await;
    let scenario = build_scenario(&fixture, route_key, 0, None);
    let sm = Arc::clone(&scenario.sm);

    let preflight_count = Arc::new(AtomicUsize::new(0));
    let preflight = CountingPreflight {
        count: preflight_count.clone(),
    };
    let identity_source = AlwaysValidIdentity;

    let head = prepare_pipeline_head(
        scenario.sm,
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
    let digest: FinalRequestDigest = head.digest();

    // Uncommitted: prepare_pipeline_head never aborts/reconciles on success.
    let intent = sm
        .intent(0)
        .expect("intent lookup must succeed")
        .expect("intent must still be live after prepare alone");
    assert_eq!(intent.state, IntentState::Preparing);

    // The public consuming continuation owns cleanup, against the SM the head itself
    // carries — no caller-supplied SM handle can diverge from the one that reserved.
    let outcome = head
        .into_closed_outcome()
        .expect("the closed continuation must consume a well-formed head");
    assert_eq!(outcome.digest, digest);

    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "the continuation must clean up exactly once and never sign"
    );
    fixture.assert_no_rpc_since_startup().await;
}
