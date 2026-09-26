//! WHI-557: pure, network-free provenance fixtures for the gas-profile pipeline.
//!
//! Guards two things that must never regress:
//! - Foundry-mock samples (synthetic pools) can never qualify a production route class,
//!   no matter how good their `gas_used` looks, and are counted separately from real
//!   fork-replay evidence.
//! - The checked-in `samples.jsonl` never claims `source: fork_replay` without carrying
//!   the provenance fields that back that claim (venues + block_hash), so fabricated
//!   fork-replay data can't silently reappear.

use amms::execution::gas_profile::{
    build_route_profile, generate_artifact, load_generator_config, load_samples_jsonl,
    FeeAnalysis, FeeObservation, GasSample, GeneratorConfig, MarginPolicy, ProfileStatus,
    ProtocolKind, RouteKey, SampleOutcome, SampleSource, SamplingPolicy, VenueRef,
    GAS_PROFILE_SCHEMA_VERSION, GAS_PROFILE_TOOL_VERSION, MANTLE_MAINNET_CHAIN_ID,
    WHI501_EXECUTOR_CODEHASH,
};
use std::path::Path;

fn codehash() -> String {
    WHI501_EXECUTOR_CODEHASH.into()
}

fn v2_key(hops: u8) -> RouteKey {
    RouteKey::new(vec![ProtocolKind::V2; hops as usize]).unwrap()
}

fn base_sample(route: RouteKey, gas: u64, block: u64, source: SampleSource) -> GasSample {
    GasSample {
        route_key: route,
        gas_used: gas,
        source,
        executor_code_hash: codehash(),
        chain_id: MANTLE_MAINNET_CHAIN_ID,
        block_number: block,
        block_hash: Some(format!("0x{:064x}", block)),
        tx_hash: Some(format!("0x{:064x}", block * 7 + gas)),
        effective_gas_price_wei: Some(50_000_000_000 + 1_000_000),
        base_fee_wei: Some(50_000_000_000),
        block_gas_limit: Some(60_000_000),
        inclusion_latency_blocks: Some(1),
        notes: Some("fixture".into()),
        venues: None,
        calldata_digest: None,
        outcome: Some(SampleOutcome::Success),
    }
}

fn fee_analysis() -> FeeAnalysis {
    FeeAnalysis {
        start_block: 97_158_262,
        end_block: 98_158_262,
        start_block_hash: Some("0x1".into()),
        end_block_hash: Some("0x2".into()),
        observation_count: 1,
        min_base_fee_wei: 50_000_000_000,
        max_base_fee_wei: 50_000_000_000,
        min_block_gas_limit: 60_000_000,
        max_block_gas_limit: 60_000_000,
        notes: "fixture: not a permanent protocol constant.".into(),
        observations: vec![FeeObservation {
            block_number: 98_121_659,
            block_hash: Some("0x1".into()),
            base_fee_wei: 50_000_000_000,
            block_gas_limit: 60_000_000,
            effective_priority_fee_wei: None,
            inclusion_latency_blocks: None,
        }],
    }
}

fn policy() -> MarginPolicy {
    MarginPolicy {
        name: "test_tail_20pct".into(),
        expected_percentile: 50,
        margin_bps: 2000,
        absolute_overhead: 50_000,
        min_samples: 10,
        holdout_fraction_bps: 2000,
    }
}

fn base_config(routes: Vec<RouteKey>) -> GeneratorConfig {
    GeneratorConfig {
        chain_id: MANTLE_MAINNET_CHAIN_ID,
        schema_version: GAS_PROFILE_SCHEMA_VERSION,
        tool_version: GAS_PROFILE_TOOL_VERSION.into(),
        executor_code_hash: codehash(),
        executor_abi_digest: "0x".to_owned() + &"ab".repeat(32),
        margin_policy: policy(),
        sampling_policy: SamplingPolicy {
            description: "fixture sampling".into(),
            qualification_executor_code_hash: codehash(),
            qualification_source: SampleSource::ForkReplay,
            research_sources_excluded_from_limits: vec![
                SampleSource::ResearchHistorical,
                SampleSource::ResearchRevert,
                SampleSource::FoundryMock,
            ],
        },
        active_route_classes: routes,
        fee_analysis: fee_analysis(),
        replacement_overhead_notes: None,
        unsupported_route_classes: Vec::new(),
    }
}

fn mock_samples(route: &RouteKey, gases: &[u64]) -> Vec<GasSample> {
    gases
        .iter()
        .enumerate()
        .map(|(i, g)| {
            base_sample(
                route.clone(),
                *g,
                98_000_000 + i as u64,
                SampleSource::FoundryMock,
            )
        })
        .collect()
}

fn fork_samples(route: &RouteKey, gases: &[u64]) -> Vec<GasSample> {
    gases
        .iter()
        .enumerate()
        .map(|(i, g)| {
            base_sample(
                route.clone(),
                *g,
                98_000_000 + i as u64,
                SampleSource::ForkReplay,
            )
        })
        .collect()
}

#[test]
fn foundry_mock_samples_never_qualify_regardless_of_gas_used() {
    let route = v2_key(2);
    // Excellent-looking gas numbers, but sourced from a Foundry mock — must still be
    // Unsupported, never Approved.
    let samples = mock_samples(&route, &[100_000; 12]);
    let profile = build_route_profile(&route, &samples, &[], &policy(), 60_000_000, &codehash())
        .unwrap();
    assert_eq!(profile.status, ProfileStatus::Unsupported);
    assert!(profile
        .reason
        .as_deref()
        .unwrap_or("")
        .contains("fork_replay"));
}

#[test]
fn generate_artifact_counts_foundry_mock_samples_without_affecting_qualification() {
    let route = v2_key(2);
    let mut mocks = mock_samples(&route, &[100_000; 20]);
    let forks = fork_samples(&route, &[150_000; 12]);
    let mut samples = fork_samples(&route, &[150_000; 12]);
    samples.append(&mut mocks);
    // sanity: make sure both sets ended up in `samples` (avoid unused warnings if refactored).
    assert_eq!(samples.len(), 12 + 20);
    let _ = forks;

    let cfg = base_config(vec![route.clone()]);
    let artifact = generate_artifact(&cfg, &samples).unwrap();

    assert_eq!(artifact.foundry_mock_sample_count, 20);
    assert_eq!(artifact.qualification_sample_count, 12);
    let p = artifact
        .profiles
        .iter()
        .find(|p| p.route_key == route)
        .unwrap();
    assert_eq!(p.status, ProfileStatus::Approved);
    assert_eq!(p.stats.as_ref().unwrap().sample_count, 12);
}

#[test]
fn fork_replay_sample_with_wrong_executor_code_hash_is_rejected() {
    let route = v2_key(2);
    let mut samples = fork_samples(&route, &[150_000; 12]);
    for s in &mut samples {
        s.executor_code_hash = "0xdeadbeef".into();
    }
    let profile = build_route_profile(&route, &samples, &[], &policy(), 60_000_000, &codehash())
        .unwrap();
    assert_eq!(profile.status, ProfileStatus::Unsupported);
    assert!(profile
        .reason
        .as_deref()
        .unwrap_or("")
        .contains("executor_code_hash"));
}

#[test]
fn reverted_outcome_is_rejected_even_with_nonzero_gas_used() {
    let route = v2_key(2);
    let mut samples = fork_samples(&route, &[150_000; 12]);
    samples[0].outcome = Some(SampleOutcome::Reverted);
    let profile = build_route_profile(&route, &samples, &[], &policy(), 60_000_000, &codehash())
        .unwrap();
    assert_eq!(profile.status, ProfileStatus::Unsupported);
    assert!(profile
        .reason
        .as_deref()
        .unwrap_or("")
        .contains("reverted"));
}

#[test]
fn gas_sample_optional_fields_round_trip_through_json() {
    let route = v2_key(2);
    let mut sample = base_sample(route, 150_000, 98_000_000, SampleSource::ForkReplay);
    sample.venues = Some(vec![
        VenueRef {
            protocol: ProtocolKind::V2,
            pool: "0x3e5922cd0cec71dc2d60ec8b36aa4c05b7c1672f".into(),
        },
        VenueRef {
            protocol: ProtocolKind::V2,
            pool: "0x3e5922cd0cec71dc2d60ec8b36aa4c05b7c1672f".into(),
        },
    ]);
    sample.calldata_digest = Some("0xabc123".into());
    sample.outcome = Some(SampleOutcome::Success);

    let json = serde_json::to_string(&sample).unwrap();
    let round_tripped: GasSample = serde_json::from_str(&json).unwrap();
    assert_eq!(sample, round_tripped);
    assert_eq!(round_tripped.venues.unwrap().len(), 2);
}

#[test]
fn gas_sample_without_optional_fields_still_deserializes() {
    // Legacy-shaped line (no venues/calldata_digest/outcome) must still parse — these
    // fields are additive, no schema version bump.
    let legacy = r#"{"route_key":{"protocols":["v2","v2"],"hop_count":2},"gas_used":100000,
        "source":"fork_replay","executor_code_hash":"0xabc","chain_id":5000,"block_number":1}"#;
    let sample: GasSample = serde_json::from_str(legacy).unwrap();
    assert!(sample.venues.is_none());
    assert!(sample.calldata_digest.is_none());
    assert!(sample.outcome.is_none());
}

/// Regression guard: no committed sample may claim `source: fork_replay` without also
/// carrying the provenance fields (`venues`, `block_hash`) that back a real on-chain
/// measurement. This is exactly the bug WHI-557 fixes (91 Foundry-mock samples were
/// mislabeled `fork_replay`); it must never reappear silently.
#[test]
fn committed_samples_never_claim_fork_replay_without_provenance() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let samples_path = root.join("config/gas_profiles/pinned/samples.jsonl");
    if !samples_path.exists() {
        return;
    }
    let samples = load_samples_jsonl(&samples_path).unwrap();
    for (i, s) in samples.iter().enumerate() {
        if s.source == SampleSource::ForkReplay {
            assert!(
                s.venues.is_some() && s.block_hash.is_some(),
                "sample #{i} claims fork_replay without venues/block_hash provenance \
                 (route={}, block={})",
                s.route_key.key_string(),
                s.block_number
            );
        }
    }
}

/// Sanity check that the checked-in generator config + samples still parse and that
/// the pinned executor code hash used for qualification matches the frozen constant
/// (DI-17): no drift between the two.
#[test]
fn pinned_generator_config_code_hash_matches_frozen_constant() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config_path = root.join("config/gas_profiles/pinned/generator_config.json");
    if !config_path.exists() {
        return;
    }
    let config = load_generator_config(&config_path).unwrap();
    assert_eq!(config.executor_code_hash, WHI501_EXECUTOR_CODEHASH);
    assert_eq!(
        config.sampling_policy.qualification_executor_code_hash,
        WHI501_EXECUTOR_CODEHASH
    );
}
