//! WHI-521: `RiskTieredPreflight` policy tests.
//!
//! Building a real [`FinalRequest`] requires the full reserve -> begin_prepare ->
//! build_final_request flow (an `ExecutionPermit` can only be minted by
//! `IntentStateMachine::reserve`), so this file reuses the same mocked-provider
//! `Fixture`/`Scenario` pattern `tests/pipeline_wiring.rs` (WHI-553) established:
//! drive `prepare_pipeline_head` with `NoopPreflight` to obtain a real
//! `PreparedPipelineHead`, then exercise `RiskTieredPreflight` directly against
//! `head.request()` -- entirely through public API.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alloy::primitives::{aliases::U112, Address, Bytes, B256, U256};
use alloy::providers::ProviderBuilder;
use alloy::sol_types::SolValue;
use alloy::transports::mock::Asserter;

use amms::execution::runtime_identity::{resolve_immutable_plan, BuildEvidence, ImmutableInputs};
use amms::execution::*;
use amms::signing::{self, CanonicalEnvelope, ExpectedScope, SigningError, VerifiedArtifact};
use amms::state_space::{
    BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// FinalRequest fixture (trimmed V2-only copy of tests/pipeline_wiring.rs's pattern)
// ---------------------------------------------------------------------------

struct Fixture {
    executor: Executor,
}

async fn build_fixture() -> Fixture {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let artifact_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile = RuntimeGasProfile::load(
        &artifact_path,
        RuntimeProfileConfig::mantle_mainnet(vec![route_key]),
    )
    .expect("gas profile artifact must load for the requested route key");

    let expected_chain_id = gas_profile.executor_identity().chain_id;
    let identity_json: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(
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
    asserter.push_success(&Bytes::from(bytecode));
    asserter.push_success(&Bytes::from(wmnt_address.abi_encode()));
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
        executor: Executor::new(context, ExecutorConfig::default()),
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
    let snapshot = MarketSnapshot::new(
        snapshot_id,
        header,
        std::collections::HashMap::new(),
        coverage,
    );
    SnapshotStatus::Ready(snapshot.into_arc())
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
        unimplemented!("never reached by prepare_pipeline_head")
    }
}

/// Builds a real, well-formed `PreparedPipelineHead` (via `prepare_pipeline_head` with
/// `NoopPreflight`, exactly like `tests/pipeline_wiring.rs`) so callers get a genuine
/// `&FinalRequest` to drive `RiskTieredPreflight` against directly.
async fn build_prepared_head(fixture: &Fixture) -> PreparedPipelineHead {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
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
    let wmnt = fixture.executor.context.wmnt_address();

    let params = ExecutionParams::new(
        amount_in,
        route_key.clone(),
        vec![wmnt, mid_token, wmnt],
        vec![Address::repeat_byte(0x03), Address::repeat_byte(0x04)],
        vec![0u8, 0u8],
        vec![(wmnt, mid_token), (mid_token, wmnt)],
        vec![U112::ZERO; 4],
        vec![amount_in, final_out],
        final_out,
        U256::from(1_000u64),
        None,
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

    let identity_source = AlwaysValidIdentity;
    prepare_pipeline_head(
        sm,
        candidate,
        &status,
        fee_ctx,
        &fixture.executor,
        &identity_source,
        &NoopPreflight,
        final_request_params,
        chain,
    )
    .await
    .expect("prepare_pipeline_head must succeed for a well-formed scenario")
}

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ScriptedCall {
    Success,
    Revert(String),
    EnvUnsupported(String),
    RpcError {
        class: RpcErrorClass,
        message: String,
    },
}

struct ScriptedCallExecutor {
    outcome: ScriptedCall,
    calls: Arc<AtomicUsize>,
}

impl ScriptedCallExecutor {
    fn new(outcome: ScriptedCall) -> (Self, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            Self {
                outcome,
                calls: calls.clone(),
            },
            calls,
        )
    }
}

impl SemanticCallExecutor for ScriptedCallExecutor {
    async fn call(
        &self,
        _request: &FinalRequest,
        _tag: BlockTag,
    ) -> Result<CallOutcome, SemanticCallError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.outcome.clone() {
            ScriptedCall::Success => Ok(CallOutcome::Success),
            ScriptedCall::Revert(reason) => Ok(CallOutcome::Revert(reason)),
            ScriptedCall::EnvUnsupported(reason) => Ok(CallOutcome::EnvUnsupported(reason)),
            ScriptedCall::RpcError { class, message } => {
                Err(SemanticCallError::Rpc { class, message })
            }
        }
    }
}

#[derive(Default, Clone)]
struct RecordingSink {
    attempts: Arc<Mutex<Vec<PreflightAttempt>>>,
}

impl PreflightAttemptSink for RecordingSink {
    fn record(&self, attempt: PreflightAttempt) {
        self.attempts.lock().unwrap().push(attempt);
    }
}

impl RecordingSink {
    fn attempts(&self) -> Vec<PreflightAttempt> {
        self.attempts.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------
// Signed approval-record fixtures (mirrors tests/signing.rs's ssh-keygen pattern)
// ---------------------------------------------------------------------------

struct SigningFixture {
    _dir: TempDir,
    key_path: PathBuf,
    allowed_signers_path: PathBuf,
    revoked_keys_path: PathBuf,
}

fn generate_ed25519_keypair(dir: &Path) -> PathBuf {
    let key_path = dir.join("id_ed25519");
    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(&key_path)
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg("test")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to spawn ssh-keygen -t ed25519");
    assert!(status.success());
    key_path
}

fn build_signing_fixture() -> SigningFixture {
    let dir = tempfile::tempdir().unwrap();
    let key_path = generate_ed25519_keypair(dir.path());
    let pub_key = fs::read_to_string(key_path.with_extension("pub")).unwrap();

    let allowed_signers_path = dir.path().join("allowed_signers");
    fs::write(
        &allowed_signers_path,
        format!(
            "operator namespaces=\"{}\" {pub_key}",
            PREFLIGHT_APPROVAL_DOMAIN
        ),
    )
    .unwrap();

    let revoked_keys_path = dir.path().join("revoked_keys");
    fs::write(&revoked_keys_path, "").unwrap();

    SigningFixture {
        _dir: dir,
        key_path,
        allowed_signers_path,
        revoked_keys_path,
    }
}

struct TestApprovalVerifier {
    allowed_signers_path: PathBuf,
    revoked_keys_path: PathBuf,
}

impl ApprovalVerifier for TestApprovalVerifier {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<PreflightApprovalPayload>, SigningError> {
        signing::verify_with_paths(
            payload_bytes,
            signature,
            PREFLIGHT_APPROVAL_DOMAIN,
            principal,
            accepted_schema_versions,
            expected_scope,
            &self.allowed_signers_path,
            &self.revoked_keys_path,
        )
    }
}

fn test_scope() -> RuntimeScope {
    RuntimeScope {
        chain_id: 5000,
        executor_identity_digest: "0xaa".to_string(),
        config_digest: "0xbb".to_string(),
        profile_digest: "0xcc".to_string(),
    }
}

fn unix_seconds(offset: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    (now + offset).to_string()
}

#[allow(clippy::too_many_arguments)]
fn signed_approval(
    fx: &SigningFixture,
    domain: &str,
    scope: &RuntimeScope,
    mode: ApprovalMode,
    sample_rate_bps: Option<&str>,
    valid_until_offset: i64,
) -> SignedApprovalRecord {
    let envelope = CanonicalEnvelope {
        schema_version: "1".to_string(),
        domain: domain.to_string(),
        scope: serde_json::to_value(scope_json(scope)).unwrap(),
        payload: PreflightApprovalPayload {
            profile_key: "mantle-mainnet-v1".to_string(),
            approved_by: "operator".to_string(),
            approved_at: unix_seconds(-60),
            valid_until: unix_seconds(valid_until_offset),
            evidence_digest: "0xdd".to_string(),
            mode,
            sample_rate_bps: sample_rate_bps.map(str::to_string),
        },
    };
    let (payload_bytes, signature) = signing::sign_envelope(&fx.key_path, domain, &envelope)
        .expect("sign_envelope must succeed for a well-formed envelope");
    SignedApprovalRecord {
        payload_bytes,
        signature,
        principal: "operator".to_string(),
    }
}

fn scope_json(scope: &RuntimeScope) -> serde_json::Value {
    serde_json::json!({
        "chain_id": scope.chain_id.to_string(),
        "executor_identity_digest": scope.executor_identity_digest,
        "config_digest": scope.config_digest,
        "profile_digest": scope.profile_digest,
    })
}

fn approval_config(
    fx: &SigningFixture,
    mode: ApprovalMode,
    sample_rate_bps: Option<&str>,
) -> ApprovalConfig {
    let scope = test_scope();
    let record = signed_approval(
        fx,
        PREFLIGHT_APPROVAL_DOMAIN,
        &scope,
        mode,
        sample_rate_bps,
        3600,
    );
    ApprovalConfig {
        record,
        scope,
        accepted_schema_versions: vec!["1".to_string()],
    }
}

fn verifier(fx: &SigningFixture) -> Box<dyn ApprovalVerifier> {
    Box::new(TestApprovalVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    })
}

// ---------------------------------------------------------------------------
// Mandatory tier
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mandatory_tier_makes_exactly_one_call_and_passes() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight =
        RiskTieredPreflight::with_sink(call_executor, sink.clone(), ExecutionStage::Shadow, None);

    preflight
        .preflight(head.request())
        .await
        .expect("Pass must succeed");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let attempts = sink.attempts();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].policy_key, PolicyKey::Mandatory);
    assert_eq!(attempts[0].outcome, PreflightOutcome::Pass);
    assert_eq!(attempts[0].block_tag, Some(BlockTag::Latest));
    assert!(attempts[0].latency.is_some());

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn mandatory_tier_revert_rejects_without_signing() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Revert(
        "execution reverted: INSUFFICIENT_OUTPUT".into(),
    ));
    let sink = RecordingSink::default();
    let preflight =
        RiskTieredPreflight::with_sink(call_executor, sink.clone(), ExecutionStage::E2e, None);

    let err = preflight
        .preflight(head.request())
        .await
        .expect_err("a revert must be rejected");
    assert!(err.to_string().contains("Revert"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        sink.attempts()[0].outcome,
        PreflightOutcome::Revert(_)
    ));

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn mandatory_tier_rpc_error_rejects() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::RpcError {
        class: RpcErrorClass::Transport,
        message: "connection reset".into(),
    });
    let sink = RecordingSink::default();
    let preflight =
        RiskTieredPreflight::with_sink(call_executor, sink.clone(), ExecutionStage::Canary, None);

    let _ = preflight
        .preflight(head.request())
        .await
        .expect_err("an RPC failure must be rejected");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        sink.attempts()[0].outcome,
        PreflightOutcome::RpcError(RpcErrorClass::Transport)
    );

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn env_unsupported_is_recorded_distinctly_and_rejects() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::EnvUnsupported(
        "state-override unavailable".into(),
    ));
    let sink = RecordingSink::default();
    let preflight =
        RiskTieredPreflight::with_sink(call_executor, sink.clone(), ExecutionStage::Shadow, None);

    let _ = preflight
        .preflight(head.request())
        .await
        .expect_err("EnvUnsupported must not be treated as Pass");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(sink.attempts()[0].outcome, PreflightOutcome::EnvUnsupported);
    assert_ne!(sink.attempts()[0].outcome, PreflightOutcome::Pass);

    head.into_closed_outcome().expect("cleanup must succeed");
}

// ---------------------------------------------------------------------------
// ApprovedStable tier (production only)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn approved_stable_only_applies_in_production_not_shadow() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let fx = build_signing_fixture();
    let approval = approval_config(&fx, ApprovalMode::Disabled, None);

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Shadow,
        Some(approval),
    )
    .with_verifier(verifier(&fx));

    preflight
        .preflight(head.request())
        .await
        .expect("Pass must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a Disabled approval must not apply outside Production"
    );
    assert_eq!(sink.attempts()[0].policy_key, PolicyKey::Mandatory);

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn approved_stable_disabled_skips_with_zero_calls_in_production() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let fx = build_signing_fixture();
    let approval = approval_config(&fx, ApprovalMode::Disabled, None);

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Production,
        Some(approval),
    )
    .with_verifier(verifier(&fx));

    preflight
        .preflight(head.request())
        .await
        .expect("SkippedApproved must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "disabled approval must issue zero RPC"
    );

    let attempts = sink.attempts();
    assert_eq!(attempts[0].policy_key, PolicyKey::ApprovedStableDisabled);
    assert_eq!(attempts[0].outcome, PreflightOutcome::SkippedApproved);
    assert_eq!(attempts[0].block_tag, None);
    assert_eq!(attempts[0].latency, None);

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn approved_stable_sampled_at_full_rate_calls_exactly_once() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let fx = build_signing_fixture();
    let approval = approval_config(&fx, ApprovalMode::Sampled, Some("10000"));

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Production,
        Some(approval),
    )
    .with_verifier(verifier(&fx));

    preflight
        .preflight(head.request())
        .await
        .expect("sampled-in Pass must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "100% sample rate must always call"
    );
    assert_eq!(
        sink.attempts()[0].policy_key,
        PolicyKey::ApprovedStableSampled
    );
    assert_eq!(sink.attempts()[0].outcome, PreflightOutcome::Pass);

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn approved_stable_sampled_at_zero_rate_is_sampled_out_with_zero_calls() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let fx = build_signing_fixture();
    let approval = approval_config(&fx, ApprovalMode::Sampled, Some("0"));

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Production,
        Some(approval),
    )
    .with_verifier(verifier(&fx));

    preflight
        .preflight(head.request())
        .await
        .expect("SampledOut must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "0% sample rate must never call"
    );
    assert_eq!(
        sink.attempts()[0].policy_key,
        PolicyKey::ApprovedStableSampled
    );
    assert_eq!(sink.attempts()[0].outcome, PreflightOutcome::SampledOut);
    assert_eq!(sink.attempts()[0].block_tag, None);

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn shadow_stage_never_skips_or_samples_even_with_a_zero_rate_approval() {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let fx = build_signing_fixture();
    // A 0% sample rate yields `SampledOut` with zero calls in Production (see
    // `approved_stable_sampled_at_zero_rate_is_sampled_out_with_zero_calls`). Proving
    // Shadow still forces exactly one Mandatory call against this exact approval config
    // is what rules out `classify()` ever handing Shadow a `SkippedApproved`/`SampledOut`
    // outcome -- the invariant `shadow::invariant::ShadowInvariantSink` enforces at
    // runtime.
    let approval = approval_config(&fx, ApprovalMode::Sampled, Some("0"));

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Shadow,
        Some(approval),
    )
    .with_verifier(verifier(&fx));

    preflight
        .preflight(head.request())
        .await
        .expect("Pass must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "Shadow must always issue exactly one call"
    );
    assert_eq!(sink.attempts()[0].policy_key, PolicyKey::Mandatory);
    assert_eq!(sink.attempts()[0].outcome, PreflightOutcome::Pass);
    assert_ne!(
        sink.attempts()[0].outcome,
        PreflightOutcome::SkippedApproved
    );
    assert_ne!(sink.attempts()[0].outcome, PreflightOutcome::SampledOut);

    head.into_closed_outcome().expect("cleanup must succeed");
}

// ---------------------------------------------------------------------------
// Fail-closed fallback to Mandatory
// ---------------------------------------------------------------------------

async fn assert_falls_back_to_mandatory(
    approval: ApprovalConfig,
    verifier_box: Box<dyn ApprovalVerifier>,
) {
    let fixture = build_fixture().await;
    let head = build_prepared_head(&fixture).await;

    let (call_executor, calls) = ScriptedCallExecutor::new(ScriptedCall::Success);
    let sink = RecordingSink::default();
    let preflight = RiskTieredPreflight::with_sink(
        call_executor,
        sink.clone(),
        ExecutionStage::Production,
        Some(approval),
    )
    .with_verifier(verifier_box);

    preflight
        .preflight(head.request())
        .await
        .expect("Mandatory Pass must succeed");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "an invalid approval must fall back to Mandatory"
    );
    assert_eq!(sink.attempts()[0].policy_key, PolicyKey::Mandatory);

    head.into_closed_outcome().expect("cleanup must succeed");
}

#[tokio::test]
async fn expired_approval_falls_back_to_mandatory() {
    let fx = build_signing_fixture();
    let scope = test_scope();
    let record = signed_approval(
        &fx,
        PREFLIGHT_APPROVAL_DOMAIN,
        &scope,
        ApprovalMode::Disabled,
        None,
        -60,
    );
    let approval = ApprovalConfig {
        record,
        scope,
        accepted_schema_versions: vec!["1".to_string()],
    };
    assert_falls_back_to_mandatory(approval, verifier(&fx)).await;
}

#[tokio::test]
async fn revoked_key_falls_back_to_mandatory() {
    let fx = build_signing_fixture();
    let approval = approval_config(&fx, ApprovalMode::Disabled, None);

    // Revoke the key that signed the approval after signing.
    let pub_key = fs::read_to_string(fx.key_path.with_extension("pub")).unwrap();
    fs::write(&fx.revoked_keys_path, &pub_key).unwrap();

    assert_falls_back_to_mandatory(approval, verifier(&fx)).await;
}

#[tokio::test]
async fn wrong_domain_falls_back_to_mandatory() {
    let fx = build_signing_fixture();
    let scope = test_scope();
    // Signed for a namespace the RiskTieredPreflight is not configured to trust.
    let record = signed_approval(
        &fx,
        "other-domain",
        &scope,
        ApprovalMode::Disabled,
        None,
        3600,
    );
    let approval = ApprovalConfig {
        record,
        scope,
        accepted_schema_versions: vec!["1".to_string()],
    };
    assert_falls_back_to_mandatory(approval, verifier(&fx)).await;
}

#[tokio::test]
async fn wrong_principal_falls_back_to_mandatory() {
    let fx = build_signing_fixture();
    let mut approval = approval_config(&fx, ApprovalMode::Disabled, None);
    // Validly signed and scoped, but configured to check against a principal the
    // allowed_signers file never authorized.
    approval.record.principal = "someone-else".to_string();
    assert_falls_back_to_mandatory(approval, verifier(&fx)).await;
}

#[tokio::test]
async fn scope_mismatch_falls_back_to_mandatory() {
    let fx = build_signing_fixture();
    let signing_scope = RuntimeScope {
        chain_id: 4999, // deliberately mismatched against the runtime scope below
        ..test_scope()
    };
    let record = signed_approval(
        &fx,
        PREFLIGHT_APPROVAL_DOMAIN,
        &signing_scope,
        ApprovalMode::Disabled,
        None,
        3600,
    );
    let approval = ApprovalConfig {
        record,
        scope: test_scope(), // the runtime scope RiskTieredPreflight actually checks against
        accepted_schema_versions: vec!["1".to_string()],
    };
    assert_falls_back_to_mandatory(approval, verifier(&fx)).await;
}
