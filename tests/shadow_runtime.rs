//! WHI-549: end-to-end shadow-runtime wiring tests.
//!
//! Exercises `ShadowExecutionContext::build_preflight` + `run_pipeline_head_closed`
//! against a real `Executor`/`ExecutionContext` (mocked provider, real checked-in gas
//! profile artifact, contract bytecode, WMNT descriptor, and Moe allowlist), proving:
//! the real `eth_call` outcome (pass/revert) drives the pipeline result, every
//! candidate is recorded to the ledger in the header/provenance/candidate row order,
//! the ledger content matches the outcome, and zero broadcast ever occurs. Mirrors
//! `tests/pipeline_wiring.rs`'s fixture pattern but swaps its `Executor`-only fixture
//! for a full `ShadowExecutionContext`, and swaps the no-op/counting preflight doubles
//! for the real `RiskTieredPreflight` built via `build_preflight`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use alloy::primitives::{aliases::U112, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::transports::mock::Asserter;

use amms::execution::runtime_identity::BuildEvidence;
use amms::execution::*;
use amms::state_space::{
    BlockHeaderContext, MarketSnapshot, PoolProtocol, ProtocolCoverage, SnapshotId, SnapshotStatus,
};

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

/// Queued *after* the one shadow `eth_call` response — `ShadowExecutionContext::new`
/// itself issues zero RPC calls, so the only real network traffic in a scenario is the
/// shadow preflight's `eth_call`. Nothing in the wallet-free head may issue an RPC, so
/// this response must still be the next one in the queue when a scenario finishes.
const RPC_SENTINEL: [u8; 4] = [0x5E, 0x17, 0x11, 0x01];

/// Controls the 1st mocked response: the shadow preflight's real `eth_call`.
enum CallResponse {
    Success,
    Revert(&'static str),
}

struct ShadowFixture {
    context: ShadowExecutionContext,
    wmnt_address: Address,
    executor_contract: Address,
    provider: DynProvider,
    ledger_path: PathBuf,
    _ledger_dir: tempfile::TempDir,
}

impl ShadowFixture {
    /// Proves zero RPC traffic since construction beyond the one scripted `eth_call`:
    /// the sentinel is still unconsumed. Any `eth_sendRawTransaction` (broadcast) or
    /// extra read would have popped it.
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

    /// Reads every ledger row written so far as plain JSON — the ledger's row types are
    /// `pub(crate)`, so this is the only way an external test crate can assert on ledger
    /// content, mirroring `ledger.rs`'s and `call_executor.rs`'s own unit tests.
    fn ledger_rows(&self) -> Vec<serde_json::Value> {
        let content =
            std::fs::read_to_string(&self.ledger_path).expect("ledger file must be readable");
        content
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("each ledger line must be valid JSON"))
            .collect()
    }
}

async fn build_shadow_fixture(
    route_keys: Vec<RouteKey>,
    call_response: CallResponse,
) -> ShadowFixture {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    let profile_path = manifest_dir.join("config/gas_profiles/mantle_mainnet_v1.json");
    let artifact = load_artifact(&profile_path).expect("gas profile artifact must load");
    let identity = mainnet_verified_identity();
    let profile_config = RuntimeProfileConfig::mantle_mainnet(route_keys);

    let identity_json: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(manifest_dir.join("config/executor_identity.json"))
            .expect("checked-in executor identity export must exist"),
    )
    .expect("executor identity export must be valid JSON");
    let wmnt_address: Address = identity_json["wmnt"]
        .as_str()
        .expect("identity export must record the wmnt immutable")
        .parse()
        .expect("identity export wmnt must be a valid address");

    let evidence = BuildEvidence::load(&manifest_dir.join("contracts/executor/artifacts"))
        .expect("checked-in executor build evidence must load");

    let wmnt_descriptor = load_wmnt_descriptor(
        &manifest_dir.join("config/gas_profiles/wmnt_descriptor.mantle_mainnet.json"),
    )
    .expect("checked-in WMNT descriptor must load");
    let moe_allowlist = load_moe_allowlist(
        &manifest_dir.join("config/gas_profiles/moe_allowlist.mantle_mainnet.json"),
    )
    .expect("checked-in Moe allowlist must load");
    let approved_pools = approved_pools_fixture();
    let threshold_bytes = load_threshold_bytes(
        &manifest_dir.join("config/gas_profiles/shadow_thresholds.mantle_mainnet.json"),
    )
    .expect("checked-in shadow threshold config must load");

    let manifest = ShadowOverrideManifest::new(
        &evidence,
        &wmnt_descriptor,
        &moe_allowlist,
        identity,
        &approved_pools,
        &threshold_bytes,
    )
    .expect("shadow override manifest must build from checked-in inputs");

    let executor_contract = Address::repeat_byte(0xE0);

    let asserter = Asserter::new();
    match call_response {
        CallResponse::Success => {
            asserter.push_success(&Bytes::new());
        }
        CallResponse::Revert(reason) => {
            asserter.push_failure_msg(reason);
        }
    }
    asserter.push_success(&Bytes::from(RPC_SENTINEL.to_vec()));
    let provider = ProviderBuilder::new()
        .connect_mocked_client(asserter)
        .erased();

    let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
    block_fee_contexts
        .publish(fee_context())
        .expect("fee context publish must succeed");

    let ledger_dir = tempfile::tempdir().expect("temp dir for the ledger must be creatable");
    let ledger_path = ledger_dir.path().join("shadow.jsonl");

    let context = ShadowExecutionContext::new(
        provider.clone(),
        executor_contract,
        wmnt_address,
        artifact,
        profile_config,
        &evidence,
        identity,
        block_fee_contexts,
        ExecutorConfig::default(),
        wmnt_descriptor,
        manifest,
        moe_allowlist,
        approved_pools,
        &ledger_path,
        1_700_000_000,
    )
    .expect("mocked provider responses must satisfy ShadowExecutionContext::new's checks");

    ShadowFixture {
        context,
        wmnt_address,
        executor_contract,
        provider,
        ledger_path,
        _ledger_dir: ledger_dir,
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

fn ready_status(
    snapshot_id: SnapshotId,
    header: BlockHeaderContext,
    fingerprint: B256,
) -> SnapshotStatus {
    let coverage = ProtocolCoverage {
        fingerprint: Some(fingerprint),
        pool_universe_fingerprint: Some(fingerprint),
    };
    let snapshot = MarketSnapshot::new(snapshot_id, header, HashMap::new(), coverage);
    SnapshotStatus::Ready(snapshot.into_arc())
}

/// Counts the SM events that record cleanup and broadcast, exactly as
/// `tests/pipeline_wiring.rs::event_counts` does.
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

/// On-chain poolType byte per `ProtocolKind`, matching `executor::pool_type_byte`'s
/// Solidity-constant mapping (0=V2, 1=V3, 2=MoeLB).
fn pool_type_for_protocol(protocol: ProtocolKind) -> u8 {
    match protocol {
        ProtocolKind::V2 => 0,
        ProtocolKind::V3 => 1,
        ProtocolKind::Moe => 2,
    }
}

/// Loads the checked-in mainnet approved-pools config as the CREATE2 registration
/// fixture — reused by `scenario_pool_addresses`/`shadow_inputs_for_scenario` so this
/// test's pool addresses are genuinely CREATE2-verifiable, not placeholders.
fn approved_pools_fixture() -> ApprovedPoolsConfig {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    load_approved_pools(&manifest_dir.join("config/gas_profiles/approved_pools.mantle_mainnet.json"))
        .expect("checked-in approved pools config must load")
}

fn pool_protocol_for(protocol: ProtocolKind) -> PoolProtocol {
    match protocol {
        ProtocolKind::V2 => PoolProtocol::UniswapV2,
        ProtocolKind::V3 => PoolProtocol::UniswapV3,
        ProtocolKind::Moe => PoolProtocol::MoeLb,
    }
}

/// Deterministic, per-hop token pair — distinct across hops so a multi-hop route's
/// CREATE2 addresses don't collide.
fn hop_tokens(hop: usize) -> (Address, Address) {
    (
        Address::repeat_byte(0x10 + hop as u8),
        Address::repeat_byte(0x20 + hop as u8),
    )
}

const HOP_FEE: u32 = 3000;

/// Genuinely CREATE2-verifiable pool addresses for `protocols`, derived against
/// [`approved_pools_fixture`] — shared between `build_scenario`'s `ExecutionParams` and
/// the matching `ShadowOverrideInputs`, since `build_preflight` derives its
/// `StateOverride` from the latter while the pipeline head decodes calldata built from
/// the former.
fn scenario_pool_addresses(protocols: &[ProtocolKind]) -> Vec<Address> {
    let approved_pools = approved_pools_fixture();
    protocols
        .iter()
        .copied()
        .enumerate()
        .map(|(hop, protocol)| {
            let pool_protocol = pool_protocol_for(protocol);
            let (token0, token1) = hop_tokens(hop);
            let entry = approved_entry_for(&approved_pools, pool_protocol).unwrap_or_else(|| {
                panic!("approved pools fixture must cover {pool_protocol:?}")
            });
            expected_pool_address(
                pool_protocol,
                entry.factory,
                token0,
                token1,
                HOP_FEE,
                entry.init_code_hash,
            )
            .unwrap_or_else(|| panic!("{pool_protocol:?} must be CREATE2-derivable"))
        })
        .collect()
}

struct Scenario {
    sm: Arc<IntentStateMachine>,
    candidate: CandidateRef,
    status: SnapshotStatus,
    fee_ctx: BlockFeeContext,
    params: FinalRequestParams,
    chain: ChainNonceView,
}

fn build_scenario(
    fixture: &ShadowFixture,
    route_key: RouteKey,
    crossing_buckets: Option<VerifiedCrossingBuckets>,
) -> Scenario {
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
        .context
        .gas_profile()
        .quote(&route_key)
        .expect("route key must be approved in the gas profile fixture");
    let fee_plan = FeePolicy::new(
        fixture.context.config().default_priority_fee_wei,
        fixture.context.config().block_gas_limit_reserve,
    )
    .build(&quote, &fee_ctx)
    .expect("fee plan build must succeed against the published fee context");

    let final_out = amount_in + fee_plan.expected_gas_cost + U256::from(1_000u64);
    let mid_token = Address::repeat_byte(0x55);
    let wmnt = fixture.wmnt_address;

    let pool_types = route_key
        .protocols
        .iter()
        .copied()
        .map(pool_type_for_protocol)
        .collect::<Vec<_>>();
    let params = ExecutionParams::new(
        amount_in,
        route_key.clone(),
        vec![wmnt, mid_token, wmnt],
        scenario_pool_addresses(&route_key.protocols),
        pool_types.clone(),
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

/// Mirrors `shadow_override_inputs_from_pools`
/// (`examples/protocols/intent_service_support.rs`) but hand-constructed from the fixed
/// addresses `build_scenario` used, since this test crate cannot call a non-`pub`
/// example-binary function. Each hop's pool address is genuinely CREATE2-verifiable
/// against `approved_pools_fixture` — see `scenario_pool_addresses`.
fn shadow_inputs_for_scenario(
    fixture: &ShadowFixture,
    protocols: &[ProtocolKind],
) -> ShadowOverrideInputs {
    let pools = protocols
        .iter()
        .copied()
        .zip(scenario_pool_addresses(protocols))
        .enumerate()
        .map(|(hop, (protocol, pool))| {
            let (token0, token1) = hop_tokens(hop);
            ShadowPoolOverrideInputs {
                pool,
                protocol: pool_protocol_for(protocol),
                token0,
                token1,
                fee: HOP_FEE,
            }
        })
        .collect();

    ShadowOverrideInputs {
        executor: fixture.executor_contract,
        caller: Address::repeat_byte(0x77),
        pools,
        wmnt_funding_amount: U256::from(10_000_000_000_000_000_000_000u128),
    }
}

fn requires_shadow_capability(_capability: NoSend) {}

#[tokio::test]
async fn shadow_v2_route_pass_records_ledger_rows_and_returns_a_head_outcome() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let fixture = build_shadow_fixture(vec![route_key.clone()], CallResponse::Success).await;
    let scenario = build_scenario(&fixture, route_key, None);
    let sm = Arc::clone(&scenario.sm);

    let shadow_inputs = shadow_inputs_for_scenario(&fixture, &scenario.candidate.route_key.protocols);
    let preflight = fixture
        .context
        .build_preflight(&shadow_inputs)
        .expect("build_preflight must succeed for a well-formed candidate");
    let identity_source = AlwaysValidIdentity;

    let outcome = run_pipeline_head_closed(
        scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.context,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain,
    )
    .await
    .expect("a passing eth_call must let the closed pipeline head succeed");

    let rows = fixture.ledger_rows();
    assert_eq!(
        rows.len(),
        4,
        "expected header + provenance + context + candidate rows"
    );
    assert_eq!(rows[0]["row_type"], "run_header");
    assert_eq!(rows[1]["row_type"], "provenance");
    assert_eq!(rows[2]["row_type"], "context");
    assert_eq!(rows[3]["row_type"], "candidate");

    let digest = outcome.digest.0.to_string();
    assert_eq!(
        rows[1]["digest"], digest,
        "provenance row must key on the candidate's digest"
    );
    assert_eq!(
        rows[2]["digest"], digest,
        "context row must key on the same digest"
    );
    assert_eq!(
        rows[3]["digest"], digest,
        "candidate row must key on the same digest"
    );
    assert!(
        rows[1]["outcome"]["verified"].is_object(),
        "every hop's pool address is genuinely CREATE2-derived and matches its claimed address: {:?}",
        rows[1]["outcome"]
    );
    assert_eq!(rows[3]["outcome"]["kind"], "pass");

    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "expected exactly one Reserved + one Released (cleanup once) and zero Submitted"
    );
    fixture.assert_no_rpc_since_startup().await;

    requires_shadow_capability(fixture.context.capability());
}

#[tokio::test]
async fn shadow_v3_route_pass_wires_crossing_buckets_through_the_real_preflight() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3]).unwrap();
    let fixture = build_shadow_fixture(vec![route_key.clone()], CallResponse::Success).await;
    let crossing_buckets = Some(VerifiedCrossingBuckets::new(
        Some(TickCrossingBucket::Zero),
        None,
    ));
    let scenario = build_scenario(&fixture, route_key, crossing_buckets);
    let sm = Arc::clone(&scenario.sm);

    let shadow_inputs = shadow_inputs_for_scenario(&fixture, &scenario.candidate.route_key.protocols);
    let preflight = fixture
        .context
        .build_preflight(&shadow_inputs)
        .expect("build_preflight must succeed for a well-formed candidate");
    let identity_source = AlwaysValidIdentity;

    run_pipeline_head_closed(
        scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.context,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain,
    )
    .await
    .expect("a passing eth_call must let the closed pipeline head succeed for a V3 route");

    let rows = fixture.ledger_rows();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[3]["outcome"]["kind"], "pass");

    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(event_counts(&sm), (1, 1, 0));
    fixture.assert_no_rpc_since_startup().await;
}

#[tokio::test]
async fn shadow_revert_rejects_the_candidate_and_still_records_its_ledger_rows() {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let revert_reason = "execution reverted: insufficient liquidity";
    let fixture =
        build_shadow_fixture(vec![route_key.clone()], CallResponse::Revert(revert_reason)).await;
    let scenario = build_scenario(&fixture, route_key, None);
    let sm = Arc::clone(&scenario.sm);

    let shadow_inputs = shadow_inputs_for_scenario(&fixture, &scenario.candidate.route_key.protocols);
    let preflight = fixture
        .context
        .build_preflight(&shadow_inputs)
        .expect("build_preflight must succeed for a well-formed candidate");
    let identity_source = AlwaysValidIdentity;

    let result = run_pipeline_head_closed(
        scenario.sm,
        scenario.candidate,
        &scenario.status,
        scenario.fee_ctx,
        &fixture.context,
        &identity_source,
        &preflight,
        scenario.params,
        scenario.chain,
    )
    .await;

    assert!(
        result.is_err(),
        "a reverting eth_call must reject the candidate through the real preflight"
    );

    let rows = fixture.ledger_rows();
    assert_eq!(
        rows.len(),
        4,
        "the candidate row must still be recorded even though the pipeline errored"
    );
    assert_eq!(rows[0]["row_type"], "run_header");
    assert_eq!(rows[1]["row_type"], "provenance");
    assert_eq!(rows[2]["row_type"], "context");
    assert_eq!(rows[3]["row_type"], "candidate");
    assert_eq!(rows[1]["digest"], rows[3]["digest"]);
    assert_eq!(rows[2]["digest"], rows[3]["digest"]);
    assert_eq!(rows[3]["outcome"]["kind"], "revert");
    assert!(
        rows[3]["outcome"]["reason"]
            .as_str()
            .expect("revert row must carry a reason")
            .contains("insufficient liquidity"),
        "ledger must preserve the on-chain revert reason"
    );

    // A preflight rejection runs the same pre-prepare cleanup path as any other
    // pre-prepare failure (`prepare_pipeline_head`'s `Err` branch): the reserved nonce
    // is fully released, not merely left `Reserved`.
    assert!(sm.intent(0).expect("intent lookup must succeed").is_none());
    assert_eq!(
        sm.peek_next_nonce().expect("peek must succeed"),
        0,
        "a preflight rejection must free the reserved nonce back up"
    );
    assert_eq!(
        event_counts(&sm),
        (1, 1, 0),
        "expected exactly one Reserved + one Released (cleanup once) and zero Submitted"
    );
    fixture.assert_no_rpc_since_startup().await;
}
