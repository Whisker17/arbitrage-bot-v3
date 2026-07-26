//! WHI-556: `RuntimeGasProfile::from_artifact_with_identity` — the identity-parameterized
//! loader that both the mainnet path and a future non-mainnet deployment (WHI-525) route
//! through. Confirms the mainnet wrapper is unaffected by the refactor, that the
//! compile-time mainnet identity matches the committed `config/executor_identity.json`,
//! and that every identity-related check fails closed under tampering.

use alloy::primitives::Address;
use amms::execution::gas_profile::{
    compute_content_digest, load_artifact, GasProfileError, ProtocolKind, RouteKey,
    TickCrossingBucket, WHI501_EXECUTOR_CODEHASH,
};
use amms::execution::runtime_identity::{
    resolve_immutable_plan, verify_deployed_runtime, BuildEvidence, ImmutableInputs,
};
use amms::execution::{
    ExecutorIdentity, MarginPolicy, RuntimeGasProfile, RuntimeGasProfileError,
    RuntimeProfileConfig, MANTLE_MAINNET_CHAIN_ID, WHI551_MAINNET_IDENTITY_DIGEST,
};
use std::path::PathBuf;

const MAINNET_WMNT: &str = "0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8";

fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gas_profiles/mantle_mainnet_v1.json")
}

fn approved_route() -> RouteKey {
    RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap()
}

/// Independently re-derives the real mainnet `VerifiedRuntimeIdentity` from the committed
/// artifact directory, mirroring `tests/runtime_identity.rs`'s
/// `mainnet_plan_from_the_committed_artifact` helper — deliberately not reusing
/// `gas_runtime.rs`'s private `mainnet_verified_identity()` so this test cross-checks the
/// `include_str!` embed against an out-of-band derivation rather than restating it.
fn real_mainnet_identity() -> amms::execution::runtime_identity::VerifiedRuntimeIdentity {
    let artifact_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts/executor/artifacts");
    let evidence = BuildEvidence::load(&artifact_dir)
        .expect("contracts/executor/artifacts/ArbitrageExecutor.full.json is committed");
    let wmnt: Address = MAINNET_WMNT.parse().unwrap();
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt }, MANTLE_MAINNET_CHAIN_ID)
        .expect("real mainnet artifact must resolve to exactly {WMNT: address}");
    verify_deployed_runtime(plan.patched_bytes(), &plan).expect("a plan's own bytes self-verify")
}

// ---------------------------------------------------------------------------
// Mainnet wrapper regression guard + compile-time identity self-check
// ---------------------------------------------------------------------------

#[test]
fn mainnet_wrapper_still_succeeds_against_the_real_committed_artifact() {
    let route_key = approved_route();
    let runtime = RuntimeGasProfile::load(
        &artifact_path(),
        RuntimeProfileConfig::mantle_mainnet(vec![route_key.clone()]),
    )
    .unwrap();

    let quote = runtime.quote(&route_key).unwrap();
    assert_eq!(quote.gas_limit, 187_148);
    assert_eq!(quote.expected_gas_used, 97_570);
}

/// Binds the compile-time (`include_str!`-embedded) mainnet identity used internally by
/// `gas_runtime.rs` to an out-of-band derivation from the same committed artifact
/// directory and to `config/executor_identity.json` — proving the embed reproduces
/// exactly what file-based derivation produces, and that
/// `WHI551_MAINNET_IDENTITY_DIGEST` is not stale.
#[test]
fn compile_time_mainnet_identity_matches_the_committed_identity_json() {
    let identity = real_mainnet_identity();

    let identity_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/executor_identity.json");
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&identity_path).unwrap()).unwrap();

    assert_eq!(identity.chain_id(), MANTLE_MAINNET_CHAIN_ID);
    assert_eq!(
        identity.patched_runtime_hash().to_string(),
        committed["patched_runtime_hash"].as_str().unwrap()
    );
    assert_eq!(
        identity.identity_digest().to_string(),
        committed["identity_digest"].as_str().unwrap()
    );
    assert_eq!(
        identity.identity_digest().to_string(),
        WHI551_MAINNET_IDENTITY_DIGEST
    );
}

// ---------------------------------------------------------------------------
// from_artifact_with_identity: success path
// ---------------------------------------------------------------------------

#[test]
fn from_artifact_with_identity_succeeds_for_the_real_mainnet_identity() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key.clone()],
    );

    let runtime = RuntimeGasProfile::from_artifact_with_identity(
        load_artifact(&artifact_path()).unwrap(),
        config,
        &identity,
    )
    .unwrap();

    let quote = runtime.quote(&route_key).unwrap();
    assert_eq!(quote.gas_limit, 187_148);
    assert_eq!(quote.expected_gas_used, 97_570);
}

// ---------------------------------------------------------------------------
// Fail-closed fixtures
// ---------------------------------------------------------------------------

#[test]
fn rejects_a_chain_id_mismatch_between_identity_and_artifact() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );

    // The artifact's own chain_id (5000, mainnet) no longer matches the identity's
    // chain_id once we perturb the config's claimed chain_id downstream of `identity`
    // itself — instead, perturb the artifact directly to diverge from the real identity.
    let mut artifact = load_artifact(&artifact_path()).unwrap();
    artifact.chain_id = MANTLE_MAINNET_CHAIN_ID + 1;
    artifact.content_digest = compute_content_digest(&artifact).unwrap();
    let mut config = config;
    config.expected_content_digest = artifact.content_digest.clone();

    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity { field: "chain_id", .. }
    ));
}

#[test]
fn rejects_an_executor_code_hash_mismatch_via_the_identity_parameterized_path() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );

    let mut artifact = load_artifact(&artifact_path()).unwrap();
    artifact.executor_code_hash = "0xdeadbeef".into();
    artifact.content_digest = compute_content_digest(&artifact).unwrap();
    let mut config = config;
    config.expected_content_digest = artifact.content_digest.clone();

    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "executor_code_hash",
            ..
        }
    ));
}

#[test]
fn rejects_a_wrong_identity_digest_even_when_everything_else_matches() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let mut config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );
    config.expected_identity_digest = "0xdeadbeef".into();

    let error = RuntimeGasProfile::from_artifact_with_identity(
        load_artifact(&artifact_path()).unwrap(),
        config,
        &identity,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "identity_digest",
            ..
        }
    ));
}

/// An artifact that edits its own `chain_id` and `executor_code_hash` and recomputes its
/// `content_digest` to stay internally consistent must still fail: the validator checks
/// the artifact's self-reported fields against the caller-supplied `identity` and
/// build-provenance constants, never against the artifact's own claims about itself.
#[test]
fn rejects_an_artifact_that_recomputes_its_own_digest_to_self_report_a_different_identity() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );

    let mut artifact = load_artifact(&artifact_path()).unwrap();
    artifact.chain_id = 99999;
    artifact.executor_code_hash = "0xbadc0de".into();
    artifact.content_digest = compute_content_digest(&artifact).unwrap();
    // expected_content_digest intentionally left pointing at the original mainnet
    // profile digest, so this also exercises the content_digest fail-closed check —
    // whichever check trips first, the artifact's self-consistent tampering must not
    // be accepted.
    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::Identity { .. }));
}

/// Isolates the `content_digest` check specifically: an otherwise-untampered, correctly
/// self-consistent artifact whose digest simply doesn't match the config's pinned
/// expectation must fail on that field alone, not on chain_id/codehash/identity_digest.
#[test]
fn rejects_a_content_digest_mismatch_against_the_configs_pinned_expectation() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let mut config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );
    config.expected_content_digest = "0xdeadbeef".into();

    let error = RuntimeGasProfile::from_artifact_with_identity(
        load_artifact(&artifact_path()).unwrap(),
        config,
        &identity,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "content_digest",
            ..
        }
    ));
}

#[test]
fn rejects_an_unqualified_profile_below_the_minimum_sample_count() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let mut artifact = load_artifact(&artifact_path()).unwrap();
    let profile = artifact
        .profiles
        .iter_mut()
        .find(|profile| profile.route_key == route_key)
        .unwrap();
    profile.stats.as_mut().unwrap().sample_count = 0;
    artifact.content_digest = compute_content_digest(&artifact).unwrap();
    let expected_content_digest = artifact.content_digest.clone();

    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        expected_content_digest,
        MarginPolicy::default(),
        vec![route_key],
    );

    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::InsufficientRouteSamples { .. }
    ));
}

#[test]
fn rejects_an_unsupported_required_route_via_the_identity_parameterized_path() {
    let identity = real_mainnet_identity();
    let unsupported_route = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Low);

    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![unsupported_route],
    );

    let error = RuntimeGasProfile::from_artifact_with_identity(
        load_artifact(&artifact_path()).unwrap(),
        config,
        &identity,
    )
    .unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::UnapprovedRoute(_)));
}

/// `ExecutorIdentity::from_verified` must never fabricate a template hash from the
/// identity itself — the template hash is a build-provenance constant, not a live
/// per-deployment value, so it always equals [`WHI501_EXECUTOR_CODEHASH`] regardless of
/// which deployment `identity` describes.
#[test]
fn executor_identity_from_verified_keeps_the_build_provenance_template_hash() {
    let identity = real_mainnet_identity();
    let executor_identity = ExecutorIdentity::from_verified(&identity);

    assert_eq!(executor_identity.template_hash, WHI501_EXECUTOR_CODEHASH);
    assert_eq!(executor_identity.chain_id, identity.chain_id());
    assert_eq!(
        executor_identity.patched_runtime_hash,
        identity.patched_runtime_hash().to_string()
    );
    assert_ne!(executor_identity.template_hash, executor_identity.patched_runtime_hash);
}

/// Sanity check that `GasProfileError` remains reachable through
/// `RuntimeGasProfileError::Artifact` for artifact-level malformation, distinct from the
/// identity-specific variants exercised above.
#[test]
fn malformed_artifact_errors_are_distinct_from_identity_errors() {
    let identity = real_mainnet_identity();
    let route_key = approved_route();
    let mut artifact = load_artifact(&artifact_path()).unwrap();
    artifact.content_digest = "0xnotreal".into();

    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        amms::execution::MANTLE_MAINNET_PROFILE_DIGEST.into(),
        MarginPolicy::default(),
        vec![route_key],
    );

    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Artifact(GasProfileError::Validation(_))
    ));
}
