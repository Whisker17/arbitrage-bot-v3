//! WHI-525 M2: loading `config/gas_profiles/mantle_sepolia_e2e_v1.json` through the
//! generic, non-mainnet-hardcoded path (`RuntimeProfileConfig::from_verified_identity` +
//! `RuntimeGasProfile::from_artifact_with_identity`, WHI-556) against a real
//! `VerifiedRuntimeIdentity` for the Mantle Sepolia deployment.
use super::gas_profile::{
    load_artifact, MarginPolicy, ProtocolKind, RouteKey, TickCrossingBucket, MANTLE_MAINNET_CHAIN_ID,
};
use super::runtime_identity::{
    resolve_immutable_plan, verify_deployed_runtime, BuildEvidence, ImmutableInputs,
    VerifiedRuntimeIdentity,
};
use super::{ExecutorIdentity, RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig};
use alloy::primitives::Address;
use std::path::PathBuf;

const CHAIN_ID_MANTLE_SEPOLIA: u64 = 5003;
const SEPOLIA_WMNT: &str = "0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF";
const BUILD_EVIDENCE_JSON: &str =
    include_str!("../../contracts/executor/artifacts/ArbitrageExecutor.full.json");

/// Same WHI-551 build evidence as `gas_runtime::mainnet_verified_identity`, patched
/// against the Sepolia WMNT instead — the compiled template is chain-independent, only
/// the immutable + chain id differ per deployment.
fn sepolia_verified_identity() -> VerifiedRuntimeIdentity {
    let value: serde_json::Value =
        serde_json::from_str(BUILD_EVIDENCE_JSON).expect("embedded build evidence must be JSON");
    let evidence =
        BuildEvidence::from_json(value).expect("embedded build evidence must parse");
    let wmnt: Address = SEPOLIA_WMNT.parse().expect("SEPOLIA_WMNT must be a valid address");
    let plan = resolve_immutable_plan(&evidence, ImmutableInputs { wmnt }, CHAIN_ID_MANTLE_SEPOLIA)
        .expect("build evidence must resolve to the Sepolia WMNT-patched plan");
    verify_deployed_runtime(plan.patched_bytes(), &plan)
        .expect("a plan's own patched bytes must self-verify")
}

fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_sepolia_e2e_v1.json")
}

fn v2_v3_route() -> RouteKey {
    RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Zero)
}

fn v3_v2_route() -> RouteKey {
    RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V2])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Zero)
}

#[test]
fn sepolia_runtime_profile_resolves_both_fixture_routes_via_verified_identity() {
    let identity = sepolia_verified_identity();
    let artifact = load_artifact(&artifact_path()).unwrap();
    let content_digest = artifact.content_digest.clone();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        content_digest,
        MarginPolicy::default(),
        vec![v2_v3_route(), v3_v2_route()],
    );

    let runtime = RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity)
        .expect("both fork-replay-qualified fixture routes must load and resolve");

    let v2_v3 = runtime.quote(&v2_v3_route()).unwrap();
    assert_eq!(v2_v3.gas_limit, 185_899);
    assert_eq!(v2_v3.expected_gas_used, 113_239);

    let v3_v2 = runtime.quote(&v3_v2_route()).unwrap();
    assert_eq!(v3_v2.gas_limit, 184_934);
    assert_eq!(v3_v2.expected_gas_used, 112_435);

    assert_eq!(runtime.executor_identity().chain_id, CHAIN_ID_MANTLE_SEPOLIA);
}

#[test]
fn sepolia_runtime_profile_fails_closed_against_the_mainnet_chain_id() {
    let identity = sepolia_verified_identity();
    let artifact = load_artifact(&artifact_path()).unwrap();
    let mut config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        artifact.content_digest.clone(),
        MarginPolicy::default(),
        vec![],
    );
    config.executor_identity.chain_id = MANTLE_MAINNET_CHAIN_ID;

    let error = RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity)
        .unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "runtime chain_id",
            ..
        }
    ));
}

#[test]
fn sepolia_runtime_profile_fails_closed_against_an_unverified_identity() {
    // A "verified" identity for a *different* WMNT (still chain 5003) has a distinct
    // patched_runtime_hash from the one config/gas_profiles/mantle_sepolia_e2e_v1.json's
    // artifact was actually qualified against — the runtime loader must reject it rather
    // than silently trusting an identity that never matched real deployed bytecode.
    let real_identity = sepolia_verified_identity();
    let artifact = load_artifact(&artifact_path()).unwrap();

    let value: serde_json::Value = serde_json::from_str(BUILD_EVIDENCE_JSON).unwrap();
    let evidence = BuildEvidence::from_json(value).unwrap();
    let other_wmnt: Address = "0x0000000000000000000000000000000000000001"
        .parse()
        .unwrap();
    let other_plan = resolve_immutable_plan(
        &evidence,
        ImmutableInputs { wmnt: other_wmnt },
        CHAIN_ID_MANTLE_SEPOLIA,
    )
    .unwrap();
    let other_identity = verify_deployed_runtime(other_plan.patched_bytes(), &other_plan).unwrap();

    let config = RuntimeProfileConfig::from_verified_identity(
        &real_identity,
        artifact.content_digest.clone(),
        MarginPolicy::default(),
        vec![],
    );

    let error =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &other_identity)
            .unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "runtime executor_patched_runtime_hash",
            ..
        }
    ));
}

#[test]
fn sepolia_runtime_profile_rejects_an_unsupported_route() {
    let identity = sepolia_verified_identity();
    let artifact = load_artifact(&artifact_path()).unwrap();
    let config = RuntimeProfileConfig::from_verified_identity(
        &identity,
        artifact.content_digest.clone(),
        MarginPolicy::default(),
        vec![],
    );
    let runtime =
        RuntimeGasProfile::from_artifact_with_identity(artifact, config, &identity).unwrap();

    let unsupported = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
    let error = runtime.quote(&unsupported).unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::UnknownRoute(_)));
}
