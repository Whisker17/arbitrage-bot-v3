use super::gas_profile::{
    compute_content_digest, load_artifact, ProtocolKind, RouteKey, TickCrossingBucket,
};
use super::{ExecutorIdentity, RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig};
use std::{fs, path::PathBuf};

fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gas_profiles/mantle_mainnet_v1.json")
}

fn approved_route() -> RouteKey {
    RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap()
}

#[test]
fn runtime_profile_returns_the_approved_quote_for_a_pinned_route() {
    let route_key = approved_route();
    let runtime = RuntimeGasProfile::from_artifact(
        load_artifact(&artifact_path()).unwrap(),
        RuntimeProfileConfig::mantle_mainnet(vec![route_key.clone()]),
    )
    .unwrap();

    let quote = runtime.quote(&route_key).unwrap();

    assert_eq!(quote.gas_limit, 187_148);
    assert_eq!(quote.expected_gas_used, 97_570);
}

#[test]
fn runtime_profile_rejects_an_executor_code_hash_mismatch() {
    let route_key = approved_route();
    let mut artifact = load_artifact(&artifact_path()).unwrap();
    artifact.executor_code_hash = "0xdeadbeef".into();
    artifact.content_digest = compute_content_digest(&artifact).unwrap();

    let error = RuntimeGasProfile::from_artifact(
        artifact,
        RuntimeProfileConfig::mantle_mainnet(vec![route_key]),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "executor_code_hash",
            ..
        }
    ));
}

#[test]
fn runtime_profile_rejects_a_mismatched_runtime_executor_identity() {
    let route_key = approved_route();
    let artifact = load_artifact(&artifact_path()).unwrap();
    let mut config = RuntimeProfileConfig::mantle_mainnet(vec![route_key]);
    config.executor_identity = ExecutorIdentity {
        chain_id: 5000,
        code_hash: "0xdeadbeef".into(),
        abi_digest: config.executor_identity.abi_digest.clone(),
    };

    let error = RuntimeGasProfile::from_artifact(artifact, config).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::Identity {
            field: "runtime executor_code_hash",
            ..
        }
    ));
}

#[test]
fn runtime_profile_rejects_an_unsupported_required_route() {
    let route_key = RouteKey::new(vec![ProtocolKind::V3, ProtocolKind::V3])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Low);
    let artifact = load_artifact(&artifact_path()).unwrap();

    let error = RuntimeGasProfile::from_artifact(
        artifact,
        RuntimeProfileConfig::mantle_mainnet(vec![route_key]),
    )
    .unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::UnapprovedRoute(_)));
}

#[test]
fn runtime_profile_fails_closed_for_an_unknown_route() {
    let runtime = RuntimeGasProfile::load(
        &artifact_path(),
        RuntimeProfileConfig::mantle_mainnet(vec![approved_route()]),
    )
    .unwrap();
    let unknown = RouteKey::new(vec![ProtocolKind::V2]).unwrap();

    let error = runtime.quote(&unknown).unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::UnknownRoute(_)));
}

#[test]
fn runtime_profile_invalidates_a_route_after_receipt_qualification_breach() {
    let route_key = approved_route();
    let runtime = RuntimeGasProfile::from_artifact(
        load_artifact(&artifact_path()).unwrap(),
        RuntimeProfileConfig::mantle_mainnet(vec![route_key.clone()]),
    )
    .unwrap();

    runtime.invalidate(&route_key).unwrap();

    let error = runtime.quote(&route_key).unwrap_err();

    assert!(matches!(error, RuntimeGasProfileError::UnapprovedRoute(_)));
}

#[test]
fn runtime_profile_reconciles_a_newer_temp_invalidation_state() {
    let route_key = approved_route();
    let path = std::env::temp_dir().join(format!(
        "whi-502-profile-{}.json",
        std::process::id()
    ));
    let sidecar = path.with_extension("invalidated.json");
    let temp = path.with_extension("invalidated.tmp");
    fs::copy(artifact_path(), &path).unwrap();
    fs::write(
        &sidecar,
        serde_json::json!({
            "content_digest": super::gas_runtime::MANTLE_MAINNET_PROFILE_DIGEST,
            "routes": []
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        &temp,
        serde_json::json!({
            "content_digest": super::gas_runtime::MANTLE_MAINNET_PROFILE_DIGEST,
            "routes": [route_key]
        })
        .to_string(),
    )
    .unwrap();

    let runtime = RuntimeGasProfile::load(
        &path,
        RuntimeProfileConfig::mantle_mainnet(vec![approved_route()]),
    )
    .unwrap();

    assert!(matches!(
        runtime.quote(&approved_route()),
        Err(RuntimeGasProfileError::UnapprovedRoute(_))
    ));
    let _ = fs::remove_file(path);
    let _ = fs::remove_file(sidecar);
    let _ = fs::remove_file(temp);
}

#[test]
fn runtime_profile_requires_route_local_qualification_samples() {
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
    let mut config = RuntimeProfileConfig::mantle_mainnet(vec![route_key]);
    config.expected_content_digest = expected_content_digest;

    let error = RuntimeGasProfile::from_artifact(artifact, config).unwrap_err();

    assert!(matches!(
        error,
        RuntimeGasProfileError::InsufficientRouteSamples { .. }
    ));
}
