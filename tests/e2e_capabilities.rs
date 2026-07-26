//! WHI-555: typed E2E transaction capability layer, public-API-only tests.
//!
//! Everything here goes through `amms::execution::e2e`'s public surface —
//! authorities, manifests, and permits are minted the same way a real WHI-525
//! harness would, never fabricated via crate-internal access (that white-box
//! coverage lives in `src/execution/e2e/capability.rs`'s own `#[cfg(test)]`
//! module instead).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use alloy::network::TransactionBuilder;
use alloy::primitives::{aliases::U112, Address, Bytes, B256, U256};
use alloy::providers::{DynProvider, Provider, ProviderBuilder};
use alloy::rpc::types::TransactionRequest;
use alloy::sol_types::SolValue;
use alloy::transports::mock::Asserter;

use amms::execution::e2e::{
    validate_e2e_startup, validate_e2e_startup_with_denylist, validate_provider_identity,
    BootstrapAction, E2eBootstrapAuthority, E2eCapabilityError, E2eSignAction, MapEnvSource,
    SubmissionAction, ValidatedE2eProvider, ENV_E2E_EXECUTOR_ADDRESS, ENV_E2E_PRIVATE_KEY,
    ENV_E2E_RPC_URL, FORBIDDEN_ENV_VAR_NAMES, MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH,
};
use amms::execution::runtime_identity::{resolve_immutable_plan, BuildEvidence, ImmutableInputs};
use amms::execution::*;
use amms::state_space::{
    BlockHeaderContext, MarketSnapshot, ProtocolCoverage, SnapshotId, SnapshotStatus,
};

const KEY_A: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const KEY_B: &str = "0000000000000000000000000000000000000000000000000000000000000002";

fn env_map(private_key_hex: &str, executor: Address) -> MapEnvSource {
    let mut map = BTreeMap::new();
    map.insert(
        ENV_E2E_RPC_URL.to_string(),
        "https://rpc.sepolia.mantle.xyz".to_string(),
    );
    map.insert(ENV_E2E_PRIVATE_KEY.to_string(), private_key_hex.to_string());
    map.insert(ENV_E2E_EXECUTOR_ADDRESS.to_string(), executor.to_string());
    MapEnvSource(map)
}

fn mock_provider() -> DynProvider {
    ProviderBuilder::new()
        .connect_mocked_client(Asserter::new())
        .erased()
}

/// Each call mints a fresh random provider-session nonce (exactly like a real
/// process restart or provider reconstruction would), so two authorities
/// built this way always differ on provider identity even with identical
/// chain id/genesis hash. Use this directly only when that is the dimension
/// under test; otherwise share one `ValidatedE2eProvider` via
/// `establish_authority_with_identity` so the test isolates the field it
/// actually means to vary (signer, executor, ...).
fn establish_authority(private_key_hex: &str, executor: Address) -> E2eBootstrapAuthority {
    let provider_identity =
        validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
            .expect("sepolia chain id + committed genesis hash must validate");
    establish_authority_with_identity(private_key_hex, provider_identity, executor)
}

fn establish_authority_with_identity(
    private_key_hex: &str,
    provider_identity: ValidatedE2eProvider,
    executor: Address,
) -> E2eBootstrapAuthority {
    let startup = validate_e2e_startup(&env_map(private_key_hex, executor))
        .expect("well-formed E2E env must validate");
    E2eBootstrapAuthority::establish_with_identity(startup, provider_identity, mock_provider())
}

fn fixture_tx(nonce: u64, from: Address) -> TransactionRequest {
    TransactionRequest::default()
        .with_chain_id(MANTLE_SEPOLIA_CHAIN_ID)
        .with_from(from)
        .with_nonce(nonce)
        .with_max_priority_fee_per_gas(1)
        .with_max_fee_per_gas(2)
        .with_gas_limit(21_000)
        .with_to(Address::repeat_byte(0x9))
        .with_value(U256::ZERO)
}

// ---------------------------------------------------------------------------
// Startup validation (step 1): namespace-only reads, forbidden-name presence,
// chain id 5003/5000, denylist — all before any signer/permit exists.
// ---------------------------------------------------------------------------

#[test]
fn forbidden_legacy_credential_var_blocks_startup_by_presence_alone() {
    for forbidden in FORBIDDEN_ENV_VAR_NAMES {
        let mut env = env_map(KEY_A, Address::repeat_byte(0xE2));
        env.0.insert((*forbidden).to_string(), String::new());
        let err = validate_e2e_startup(&env)
            .expect_err("forbidden legacy var presence must block startup");
        assert_eq!(err, E2eCapabilityError::ForbiddenEnvVarPresent(forbidden));
    }
}

#[test]
fn chain_5000_is_rejected_and_chain_5003_is_accepted() {
    let mainnet_err = validate_provider_identity(5000, MANTLE_SEPOLIA_GENESIS_HASH)
        .expect_err("mainnet chain id must never validate for the E2E path");
    assert_eq!(mainnet_err, E2eCapabilityError::MainnetChainIdRejected);

    validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
        .expect("sepolia chain id + committed genesis hash must validate");
}

#[test]
fn denylisted_signer_is_rejected_before_any_permit_could_exist() {
    let env = env_map(KEY_A, Address::repeat_byte(0xE2));
    let startup_ok = validate_e2e_startup(&env).expect("empty committed denylist must validate");
    let denylist = [startup_ok.signer_address()];
    let err = validate_e2e_startup_with_denylist(&env, &denylist)
        .expect_err("denylisted signer must fail");
    assert_eq!(err, E2eCapabilityError::DenylistedSigner);
}

// ---------------------------------------------------------------------------
// Redaction: secrets never surface in errors, even on the failure paths that
// touch the private key / rpc url most closely.
// ---------------------------------------------------------------------------

#[test]
fn no_e2e_capability_error_ever_renders_the_private_key_or_rpc_url() {
    let marker_key = "1111111111111111111111111111111111111111111111111111111111111x"; // invalid hex, contains a marker
    let mut env = env_map(marker_key, Address::repeat_byte(0xE2));
    env.0.insert(
        ENV_E2E_RPC_URL.to_string(),
        "https://marker-user:marker-pass@rpc.example/marker-query?x=1".to_string(),
    );
    let err = validate_e2e_startup(&env).expect_err("garbage key must fail to parse");
    let rendered = format!("{err}");
    assert!(!rendered.contains("marker"));
    assert!(!rendered.contains(marker_key));
}

// ---------------------------------------------------------------------------
// Bootstrap lifecycle (steps 3): one-shot permits, finalize hands off to a
// manifest, cross-authority permits are rejected.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bootstrap_lifecycle_mints_signs_and_finalizes() {
    let authority = establish_authority(KEY_A, Address::repeat_byte(0xE2));
    let signer = authority.signer_address();

    let deploy_permit = authority
        .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer))
        .expect("deploy permit must mint");
    let submission = authority
        .sign_bootstrap(deploy_permit)
        .await
        .expect("deploy permit must sign");
    assert_eq!(
        submission.view().action,
        SubmissionAction::Bootstrap(BootstrapAction::Deploy)
    );

    let config_permit = authority
        .mint_bootstrap_permit(BootstrapAction::Config, fixture_tx(1, signer))
        .expect("config permit must mint");
    let config_submission = authority
        .sign_bootstrap(config_permit)
        .await
        .expect("config permit must sign");
    assert_eq!(
        config_submission.view().action,
        SubmissionAction::Bootstrap(BootstrapAction::Config)
    );

    let seed_permit = authority
        .mint_bootstrap_permit(BootstrapAction::InitialSeed, fixture_tx(2, signer))
        .expect("initial-seed permit must mint");
    let seed_submission = authority
        .sign_bootstrap(seed_permit)
        .await
        .expect("initial-seed permit must sign");
    assert_eq!(
        seed_submission.view().action,
        SubmissionAction::Bootstrap(BootstrapAction::InitialSeed)
    );

    // finalize() consumes `authority` by value; no further bootstrap permit can be
    // minted from it afterward — enforced by the compiler (the binding is gone),
    // not by anything this test asserts at runtime.
    let manifest = authority.finalize();
    assert_eq!(manifest.signer_address(), signer);
}

#[tokio::test]
async fn bootstrap_permit_minted_under_one_authority_is_rejected_under_a_different_one() {
    // Share one provider identity so the only difference between the two
    // authorities is the signer (see `establish_authority`'s doc comment).
    let shared_identity =
        validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH).unwrap();
    let authority_a =
        establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE2));
    let authority_wrong_signer =
        establish_authority_with_identity(KEY_B, shared_identity, Address::repeat_byte(0xE2));
    let signer_a = authority_a.signer_address();

    let permit = authority_a
        .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer_a))
        .unwrap();
    let err = authority_wrong_signer
        .sign_bootstrap(permit)
        .await
        .expect_err("a permit minted for a different signer must be rejected");
    assert!(matches!(err, E2eCapabilityError::SignerMismatch { .. }));
}

#[tokio::test]
async fn process_restart_fixture_invalidates_previously_minted_permits() {
    let executor = Address::repeat_byte(0xE2);
    let authority_before_restart = establish_authority(KEY_A, executor);
    // Same key/executor, but a fresh `establish` call re-derives a brand new random
    // provider-session nonce — exactly what a real process restart would produce.
    let authority_after_restart = establish_authority(KEY_A, executor);

    let signer = authority_before_restart.signer_address();
    let permit = authority_before_restart
        .mint_bootstrap_permit(BootstrapAction::Deploy, fixture_tx(0, signer))
        .unwrap();

    let err = authority_after_restart
        .sign_bootstrap(permit)
        .await
        .expect_err("a permit from before a restart must not validate after");
    assert_eq!(err, E2eCapabilityError::ProviderIdentityMismatch);
}

// ---------------------------------------------------------------------------
// Post-manifest sign permits (step 4): trigger/cancel typed digests, and the
// full cross-manifest / cross-chain / cross-executor adversarial matrix.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn trigger_and_cancel_permits_sign_and_their_views_never_expose_raw_bytes() {
    let manifest = establish_authority(KEY_A, Address::repeat_byte(0xE2)).finalize();
    let signer = manifest.signer_address();

    let trigger_permit = manifest.mint_trigger_permit(fixture_tx(0, signer)).unwrap();
    let trigger_submission = manifest.sign(trigger_permit).await.unwrap();
    assert_eq!(
        trigger_submission.view().action,
        SubmissionAction::Sign(E2eSignAction::Trigger)
    );

    let cancel_permit = manifest.mint_cancel_permit(fixture_tx(1, signer)).unwrap();
    let cancel_submission = manifest.sign(cancel_permit).await.unwrap();
    assert_eq!(
        cancel_submission.view().action,
        SubmissionAction::Sign(E2eSignAction::Cancel)
    );
    assert_ne!(
        trigger_submission.view().digest,
        cancel_submission.view().digest,
        "distinct domain tags must keep trigger/cancel digests apart even for near-identical txs"
    );
}

#[tokio::test]
async fn permit_minted_under_one_manifest_is_rejected_under_a_different_manifest() {
    // Share one provider identity so the only difference between the two
    // manifests is the executor address (see `establish_authority`'s doc
    // comment for why independently-established authorities aren't suitable
    // for isolating a single field).
    let shared_identity =
        validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH).unwrap();
    let manifest_a =
        establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE2))
            .finalize();
    let manifest_b =
        establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE3))
            .finalize();
    let signer = manifest_a.signer_address();

    let permit = manifest_a
        .mint_trigger_permit(fixture_tx(0, signer))
        .unwrap();
    let err = manifest_b
        .sign(permit)
        .await
        .expect_err("a permit minted under one manifest must not sign under another");
    assert_eq!(err, E2eCapabilityError::ManifestIdentityMismatch);
}

#[tokio::test]
async fn permit_minted_for_one_signer_is_rejected_under_a_manifest_with_a_different_signer() {
    let executor = Address::repeat_byte(0xE2);
    // Share one provider identity so the only difference between the two
    // manifests is the signer (see `establish_authority`'s doc comment).
    let shared_identity =
        validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH).unwrap();
    let manifest_a = establish_authority_with_identity(KEY_A, shared_identity, executor).finalize();
    let manifest_b = establish_authority_with_identity(KEY_B, shared_identity, executor).finalize();

    // Mint against A's signer address, but only ever hand it to B's `sign` — B's
    // manifest digest already differs (it folds in B's own signer address), so this
    // also exercises "wrong manifest" together with "wrong signer".
    let permit = manifest_a
        .mint_trigger_permit(fixture_tx(0, manifest_a.signer_address()))
        .unwrap();
    let err = manifest_b
        .sign(permit)
        .await
        .expect_err("cross-signer permits must never validate");
    assert_eq!(err, E2eCapabilityError::ManifestIdentityMismatch);
}

// ---------------------------------------------------------------------------
// arb permit: the digest must come from a consumed `PreparedPipelineHead`
// (WHI-553 seam), never from raw bytes or a re-encoded `FinalRequest`.
// ---------------------------------------------------------------------------

struct AlwaysValidIdentity;

impl ExecutionIdentitySource for AlwaysValidIdentity {
    async fn validate(&self, _identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        unimplemented!("never reached before signing/broadcast, which this test never reaches")
    }
}

struct NoopCountingPreflight {
    count: Arc<AtomicUsize>,
}

impl PreflightSlot for NoopCountingPreflight {
    async fn preflight(&self, _request: &FinalRequest) -> eyre::Result<()> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Ok(())
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

/// Minimal single-route (V2-only) adaptation of `tests/pipeline_wiring.rs`'s
/// fixture, scoped down to exactly what's needed to obtain one real,
/// consumable `PreparedPipelineHead` for `mint_arb_permit`. Kept independent
/// (integration test crates cannot share code across `tests/*.rs` files
/// without a `tests/common` module) rather than reaching into that file's
/// private helpers.
async fn build_v2_executor_fixture() -> (Executor, Address, DynProvider) {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let artifact_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile = RuntimeGasProfile::load(
        &artifact_path,
        RuntimeProfileConfig::mantle_mainnet(vec![route_key]),
    )
    .expect("gas profile artifact must load for the V2 route key");

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

    (
        Executor::new(context, ExecutorConfig::default()),
        wmnt_address,
        provider.erased(),
    )
}

async fn build_v2_prepared_head(
    executor: &Executor,
    wmnt_address: Address,
    signer_address: Address,
) -> (PreparedPipelineHead, Arc<IntentStateMachine>) {
    let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
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
    let quote = executor
        .context
        .gas_profile()
        .quote(&route_key)
        .expect("route key must be approved in the gas profile fixture");
    let fee_plan = FeePolicy::new(
        executor.config.default_priority_fee_wei,
        executor.config.block_gas_limit_reserve,
    )
    .build(&quote, &fee_ctx)
    .expect("fee plan build must succeed against the published fee context");

    let final_out = amount_in + fee_plan.expected_gas_cost + U256::from(1_000u64);
    let mid_token = Address::repeat_byte(0x55);

    let params = ExecutionParams::new(
        amount_in,
        route_key.clone(),
        vec![wmnt_address, mid_token, wmnt_address],
        vec![Address::repeat_byte(0x03), Address::repeat_byte(0x04)],
        vec![0, 0],
        vec![(wmnt_address, mid_token), (mid_token, wmnt_address)],
        vec![U112::ZERO; 4],
        vec![amount_in, final_out],
        final_out,
        U256::from(1_000u64),
        None,
    )
    .expect("ExecutionParams::new must succeed for a structurally valid V2 route");

    let candidate = CandidateRef {
        snapshot_id,
        header,
        pool_universe_fingerprint: fingerprint,
        route_key,
        amount_in,
    };
    let coverage = ProtocolCoverage {
        fingerprint: Some(fingerprint),
        pool_universe_fingerprint: Some(fingerprint),
    };
    let snapshot = MarketSnapshot::new(snapshot_id, header, HashMap::new(), coverage);
    let status = SnapshotStatus::Ready(snapshot.into_arc());

    let final_request_params = FinalRequestParams {
        params,
        candidate: candidate.clone(),
        fee_plan,
        deadline: U256::from(1_700_000_060u64),
    };

    let preflight = NoopCountingPreflight {
        count: Arc::new(AtomicUsize::new(0)),
    };
    let identity_source = AlwaysValidIdentity;

    let head = prepare_pipeline_head(
        sm.clone(),
        candidate,
        &status,
        fee_ctx,
        executor,
        &identity_source,
        &preflight,
        final_request_params,
        chain,
    )
    .await
    .expect("prepare_pipeline_head must succeed for a well-formed V2 scenario");
    (head, sm)
}

#[tokio::test]
async fn arb_permit_digest_comes_from_the_consumed_pipeline_head_not_raw_bytes() {
    let (executor, wmnt_address, _provider) = build_v2_executor_fixture().await;
    let manifest = establish_authority(KEY_A, Address::repeat_byte(0xE2)).finalize();
    // Build the head's `from` to match the manifest's own signer, so the happy
    // path (rather than the signer-mismatch rejection) is what actually runs.
    let (head, sm) =
        build_v2_prepared_head(&executor, wmnt_address, manifest.signer_address()).await;
    let head_digest = head.digest();
    let nonce = head.nonce();

    let permit = manifest
        .mint_arb_permit(head)
        .expect("arb permit must mint from a consumed, well-formed PreparedPipelineHead");
    assert_eq!(permit.digest(), head_digest.0);

    // `mint_arb_permit` must not abort the intent it is about to send: the
    // reservation stays `Preparing` right up until `sign` records the
    // submission — proving the E2E path never leaves the state machine
    // thinking this nonce is free while a real transaction using it is
    // in flight.
    let intent_before_sign = sm
        .intent(nonce)
        .expect("intent lookup must succeed")
        .expect("mint_arb_permit must not abort/release the intent it is about to sign");
    assert_eq!(intent_before_sign.state, IntentState::Preparing);

    let submission = manifest.sign(permit).await.expect("arb permit must sign");
    let view = submission.view();
    assert_eq!(view.action, SubmissionAction::Sign(E2eSignAction::Arb));
    let execute_meta = view
        .execute_meta
        .expect("an arb submission's view must carry Execute metadata for the durable hook");
    // `DurableSubmissionHook::on_signed(signed, min_profit, deadline)` needs
    // both of these from the view alone — a WHI-525 caller must never need
    // crate-internal access to construct that call.
    assert_eq!(execute_meta.deadline, U256::from(1_700_000_060u64));

    // After signing, the state machine must have recorded the submission
    // (transitioning to `Submitted`), not aborted/released the nonce.
    let intent_after_sign = sm
        .intent(nonce)
        .expect("intent lookup must succeed")
        .expect("sign must record the submission, not abort the intent");
    assert_eq!(intent_after_sign.state, IntentState::Submitted);
}

#[tokio::test]
async fn arb_permit_mint_rejects_a_head_whose_signer_is_not_the_manifests_own() {
    let (executor, wmnt_address, _provider) = build_v2_executor_fixture().await;
    let manifest = establish_authority(KEY_A, Address::repeat_byte(0xE2)).finalize();
    // Deliberately a different signer than the manifest's own.
    let other_signer = Address::repeat_byte(0x77);
    assert_ne!(other_signer, manifest.signer_address());
    let (head, sm) = build_v2_prepared_head(&executor, wmnt_address, other_signer).await;
    let nonce = head.nonce();

    let err = manifest
        .mint_arb_permit(head)
        .expect_err("arb permit minted for a signer other than the manifest's own must fail");
    assert!(matches!(err, E2eCapabilityError::SignerMismatch { .. }));

    // The rejected head must have been dropped (never consumed via
    // `into_open_parts`), so `PreparedPipelineHead`'s own `Drop` impl ran its
    // best-effort abort_prepare + reconcile cleanup — the nonce must not
    // leak.
    assert!(
        sm.intent(nonce).expect("intent lookup must succeed").is_none(),
        "a rejected mint_arb_permit must not leave the intent live"
    );
}

#[tokio::test]
async fn arb_permit_dropped_without_signing_still_releases_the_intent() {
    let (executor, wmnt_address, _provider) = build_v2_executor_fixture().await;
    let manifest = establish_authority(KEY_A, Address::repeat_byte(0xE2)).finalize();
    let (head, sm) =
        build_v2_prepared_head(&executor, wmnt_address, manifest.signer_address()).await;
    let nonce = head.nonce();

    let permit = manifest
        .mint_arb_permit(head)
        .expect("arb permit must mint from a consumed, well-formed PreparedPipelineHead");
    // `E2eSignPermit` inherits the `Preparing` intent from `mint_arb_permit`'s
    // `into_open_parts` (see that method's doc comment); dropping the permit
    // here without ever calling `sign()` must not leak it, mirroring
    // `PreparedPipelineHead`'s own `Drop` safety net.
    drop(permit);

    assert!(
        sm.intent(nonce).expect("intent lookup must succeed").is_none(),
        "an arb permit dropped without sign() must not leak the Preparing intent's nonce"
    );
}

#[tokio::test]
async fn arb_permit_rejected_by_sign_still_releases_the_intent() {
    let (executor, wmnt_address, _provider) = build_v2_executor_fixture().await;
    let shared_identity =
        validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH).unwrap();
    let manifest_a =
        establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE2))
            .finalize();
    // Same signer/identity as A, but a different executor address, so its
    // manifest_digest differs — `sign()` must reject the permit.
    let manifest_b =
        establish_authority_with_identity(KEY_A, shared_identity, Address::repeat_byte(0xE3))
            .finalize();

    let (head, sm) =
        build_v2_prepared_head(&executor, wmnt_address, manifest_a.signer_address()).await;
    let nonce = head.nonce();
    let permit = manifest_a
        .mint_arb_permit(head)
        .expect("arb permit must mint from a consumed, well-formed PreparedPipelineHead");

    let err = manifest_b
        .sign(permit)
        .await
        .expect_err("a permit minted under one manifest must not sign under another");
    assert_eq!(err, E2eCapabilityError::ManifestIdentityMismatch);

    // `sign()`'s failure path must release the `Preparing` intent it took
    // over from `mint_arb_permit`, not just the not-ever-signed case a bare
    // `drop` covers.
    assert!(
        sm.intent(nonce).expect("intent lookup must succeed").is_none(),
        "sign() rejecting an arb permit must not leak the Preparing intent's nonce"
    );
}

/// `mint_arb_permit`'s only parameter is an owned `PreparedPipelineHead` — there is
/// no overload, and no other public function on `VerifiedE2eManifest` accepts a raw
/// `B256`/digest for the `arb` action. This is a compile-time property; the
/// `compile_fail` example demonstrates it directly rather than asserting it at
/// runtime.
///
/// ```compile_fail
/// use amms::execution::e2e::VerifiedE2eManifest;
/// use alloy::primitives::B256;
/// fn mint_arb_from_raw_digest(manifest: &VerifiedE2eManifest, raw_digest: B256) {
///     // `mint_arb_permit` has no such overload: this does not compile.
///     let _ = manifest.mint_arb_permit(raw_digest);
/// }
/// ```
#[test]
fn arb_permit_api_shape_is_documented_above() {}

// ---------------------------------------------------------------------------
// The real `establish` entry point: live chain-id + genesis-block read tied
// to the exact provider instance used for every subsequent send.
// ---------------------------------------------------------------------------

fn mock_genesis_block(hash: B256) -> alloy::rpc::types::Block {
    let mut inner = alloy::consensus::Header::default();
    inner.number = 0;
    inner.timestamp = 0;
    let mut header = alloy::rpc::types::Header::new(inner);
    header.hash = hash;
    alloy::rpc::types::Block::empty(header)
}

#[tokio::test]
async fn establish_performs_a_live_chain_identity_check_tied_to_the_given_provider() {
    let asserter = Asserter::new();
    asserter.push_success(&MANTLE_SEPOLIA_CHAIN_ID);
    asserter.push_success(&Some(mock_genesis_block(MANTLE_SEPOLIA_GENESIS_HASH)));
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);

    let startup = validate_e2e_startup(&env_map(KEY_A, Address::repeat_byte(0xE2)))
        .expect("well-formed E2E env must validate");
    let authority = E2eBootstrapAuthority::establish(startup, provider)
        .await
        .expect("live chain id 5003 + committed genesis hash must establish");
    assert_eq!(authority.chain_id(), MANTLE_SEPOLIA_CHAIN_ID);
}

#[tokio::test]
async fn establish_rejects_a_provider_reporting_mainnet_chain_id() {
    let asserter = Asserter::new();
    asserter.push_success(&5000u64);
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);

    let startup = validate_e2e_startup(&env_map(KEY_A, Address::repeat_byte(0xE2)))
        .expect("well-formed E2E env must validate");
    let err = E2eBootstrapAuthority::establish(startup, provider)
        .await
        .expect_err("a provider reporting mainnet chain id must never establish");
    assert_eq!(err, E2eCapabilityError::MainnetChainIdRejected);
}

#[tokio::test]
async fn establish_rejects_a_provider_whose_genesis_hash_does_not_match() {
    let asserter = Asserter::new();
    asserter.push_success(&MANTLE_SEPOLIA_CHAIN_ID);
    asserter.push_success(&Some(mock_genesis_block(B256::repeat_byte(0xAB))));
    let provider = ProviderBuilder::new().connect_mocked_client(asserter);

    let startup = validate_e2e_startup(&env_map(KEY_A, Address::repeat_byte(0xE2)))
        .expect("well-formed E2E env must validate");
    let err = E2eBootstrapAuthority::establish(startup, provider)
        .await
        .expect_err("a provider whose genesis hash doesn't match must never establish");
    assert_eq!(err, E2eCapabilityError::WrongGenesisHash);
}

// ---------------------------------------------------------------------------
// Production send gating: WHI-555 must not enable or weaken production
// sending. The intent-state-machine's own `production_send_allowed` flag is
// constructed exactly as it always was, entirely independent of anything in
// `amms::execution::e2e`.
// ---------------------------------------------------------------------------

#[test]
fn production_send_gating_is_unaffected_by_the_e2e_capability_layer() {
    let chain = ChainNonceView {
        latest_nonce: 0,
        pending_nonce: 0,
    };
    let sm = IntentStateMachine::new(
        Address::repeat_byte(0x11),
        chain,
        IntentPolicy::with_caps(1_000_000_000_000, 2_000_000_000_000),
        false,
    )
    .expect("valid SM construction");
    assert!(
        !sm.production_send_allowed(),
        "production sending must remain disabled regardless of the E2E capability layer"
    );
}
