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
    build_route_profile, generate_artifact, load_artifact, load_generator_config,
    load_samples_jsonl,
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

/// WHI-1422 fix round 1 (review PR108-F2 / PR108-F3): the committed mainnet profile
/// may approve a class only if
/// - it has no V3 hop, or it was already Approved before WHI-1422 (`h2:v2+v3:ticks=0`):
///   RouteKey has no factory axis, so a new V3 approval would also price the
///   unmeasured non-Agni V3 pools (DI-50); and
/// - every WHI-1422 campaign sample in its qualification set used the `v2_boost`
///   lever (V3/Moe hops on unmodified pool state); `inflate` / `displace` rewrite
///   V3/Moe state and are not shown to upper-bound canonical gas. Non-campaign fork
///   samples (WHI-557) back only the classes approved before WHI-1422.
#[test]
fn committed_profile_approvals_respect_pr108_factory_and_lever_fences() {
    const PRE_WHI_1422_APPROVED: [&str; 3] = ["h2:v2+v2", "h2:v2+v3:ticks=0", "h2:v2+moe:bins=0"];
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifact = load_artifact(&root.join("config/gas_profiles/mantle_mainnet_v1.json")).unwrap();
    let samples =
        load_samples_jsonl(&root.join("config/gas_profiles/pinned/samples.jsonl")).unwrap();
    let approved: Vec<&RouteKey> = artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Approved)
        .map(|p| &p.route_key)
        .collect();
    assert!(!approved.is_empty(), "fixture premise: the profile approves something");
    for key in approved {
        let name = key.key_string();
        let pre_existing = PRE_WHI_1422_APPROVED.contains(&name.as_str());
        assert!(
            pre_existing || !key.protocols.contains(&ProtocolKind::V3),
            "{name}: new V3-hop approval while pricing has no factory axis (PR108-F2, DI-50)"
        );
        let mut qualification = 0usize;
        for s in samples
            .iter()
            .filter(|s| s.source == SampleSource::ForkReplay && &s.route_key == key)
        {
            qualification += 1;
            let notes = s.notes.as_deref().unwrap_or("");
            // WHI-1413's Moe V1 campaign runs carry `[whi-1413]` / `[whi-1413-b]`,
            // WHI-1520's re-qualification runs `[whi-1520]` / `[whi-1520-b]`; all are
            // held to the same lever fence.
            if ["[whi-1422]", "[whi-1413]", "[whi-1413-b]", "[whi-1520]", "[whi-1520-b]"]
                .iter()
                .any(|tag| notes.starts_with(tag))
            {
                assert!(
                    notes.contains(" lever=v2_boost "),
                    "{name}: approved on a lever that rewrites V3/Moe state (PR108-F3): {notes}"
                );
            } else {
                assert!(pre_existing, "{name}: non-campaign fork sample backs a new approval: {notes}");
            }
        }
        assert!(qualification > 0, "{name}: approved without fork samples");
    }
}

/// WHI-1413: RouteKey has no factory axis, so every Approved class with a V2 hop
/// also prices the Merchant Moe V1 classic pools of the committed universe. When the
/// committed universe holds Moe V1 rows, each such class must carry fork samples
/// whose route touches a Moe V1 pool, and every one of them must sit below the
/// class's gas limit, so the approved bound is shown conservative for the new venue.
/// (Stricter than reachability: a V2 class with no Moe V1 cycle would also need
/// samples, which fails closed.) With no Moe V1 rows the check is vacuous by design.
#[test]
fn committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools() {
    const MIN_MOE_V1_SAMPLES: usize = 2;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let moe_v1 = amms::service::MOE_V1.factory;
    let rows = amms::service::read_unified_csv(&root.join("data/pool_universe.csv")).unwrap();
    let moe_v1_pools: std::collections::HashSet<String> = rows
        .iter()
        .filter(|r| r.protocol == "agni-v2" && r.factory == moe_v1)
        .map(|r| format!("{:#x}", r.pool))
        .collect();
    if moe_v1_pools.is_empty() {
        return;
    }
    let artifact = load_artifact(&root.join("config/gas_profiles/mantle_mainnet_v1.json")).unwrap();
    let samples =
        load_samples_jsonl(&root.join("config/gas_profiles/pinned/samples.jsonl")).unwrap();
    for p in artifact
        .profiles
        .iter()
        .filter(|p| p.status == ProfileStatus::Approved && p.route_key.protocols.contains(&ProtocolKind::V2))
    {
        let name = p.route_key.key_string();
        let limit = p.gas_limit.expect("approved class has a gas limit");
        let on_moe_v1: Vec<u64> = samples
            .iter()
            .filter(|s| s.source == SampleSource::ForkReplay && s.route_key == p.route_key)
            .filter(|s| {
                s.venues
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|h| moe_v1_pools.contains(&h.pool.to_ascii_lowercase())))
            })
            .map(|s| s.gas_used)
            .collect();
        assert!(
            on_moe_v1.len() >= MIN_MOE_V1_SAMPLES,
            "{name}: Approved and it prices Moe V1 pools, but has only {} Moe V1 fork samples",
            on_moe_v1.len()
        );
        let max = on_moe_v1.iter().copied().max().unwrap();
        assert!(max <= limit, "{name}: a Moe V1 sample ({max}) exceeds the approved gas limit {limit}");
    }
}

// ── PR109-F1: every admitted V2 venue a class prices is measured on that class ──

/// Admitted V2 venue label for a factory (`service::v2_venues`), else the address.
fn v2_venue_label(factory: alloy::primitives::Address) -> String {
    amms::service::v2_venue_by_factory(factory)
        .map_or_else(|| format!("{factory:#x}"), |v| v.label.to_string())
}

/// WMNT settlement cycles (2..=3 hops) of a universe, as pool sequences. Same rules
/// as the production `PathFinder` (no immediate same-pool reversal, no repeated
/// intermediate token); the caller cross-checks the count against
/// `service::count_settlement_cycles`, which runs the production enumerator.
fn universe_cycles(
    rows: &[amms::service::CandidatePool],
    settlement: alloy::primitives::Address,
) -> Vec<Vec<usize>> {
    use alloy::primitives::Address;
    use std::collections::HashMap;
    let mut adj: HashMap<Address, Vec<(Address, usize)>> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if r.token0 == Address::ZERO || r.token1 == Address::ZERO || r.token0 == r.token1 {
            continue;
        }
        adj.entry(r.token0).or_default().push((r.token1, i));
        adj.entry(r.token1).or_default().push((r.token0, i));
    }
    fn dfs(
        adj: &HashMap<Address, Vec<(Address, usize)>>,
        settlement: Address,
        tok: Address,
        path: &mut Vec<usize>,
        seen: &mut Vec<Address>,
        out: &mut Vec<Vec<usize>>,
    ) {
        if path.len() >= 3 {
            return;
        }
        for &(next, pool) in adj.get(&tok).map(Vec::as_slice).unwrap_or(&[]) {
            if path.last() == Some(&pool) {
                continue;
            }
            if next == settlement {
                if !path.is_empty() {
                    let mut c = path.clone();
                    c.push(pool);
                    out.push(c);
                }
                continue;
            }
            if seen.contains(&next) {
                continue;
            }
            path.push(pool);
            seen.push(next);
            dfs(adj, settlement, next, path, seen, out);
            path.pop();
            seen.pop();
        }
    }
    let mut out = Vec::new();
    dfs(&adj, settlement, settlement, &mut Vec::new(), &mut vec![settlement], &mut out);
    out
}

fn universe_protocol_kind(label: &str) -> ProtocolKind {
    match label {
        "agni-v2" => ProtocolKind::V2,
        "agni-v3" => ProtocolKind::V3,
        "moe" => ProtocolKind::Moe,
        other => panic!("unexpected universe protocol label {other}"),
    }
}

/// PR109-F1 rule: for every Approved class, every admitted V2 venue (factory) that
/// occurs on the universe's cycles of the class's topology needs at least
/// `min_samples` fork samples of the **exact** class (same topology and crossing
/// buckets) touching one of that venue's pools, all within the class's gas limit.
/// Samples of another bucket or another venue never count. Returns one line per
/// (class, venue) gap.
fn approved_class_venue_gaps(
    artifact: &amms::execution::gas_profile::GasProfileArtifact,
    samples: &[GasSample],
    rows: &[amms::service::CandidatePool],
    cycles: &[Vec<usize>],
    min_samples: usize,
) -> Vec<String> {
    use std::collections::{BTreeMap, HashMap};
    let pool_factory: HashMap<String, alloy::primitives::Address> = rows
        .iter()
        .filter(|r| r.protocol == "agni-v2")
        .map(|r| (format!("{:#x}", r.pool), r.factory))
        .collect();
    // topology -> V2 factory -> cycles containing it
    let mut venues: HashMap<Vec<ProtocolKind>, BTreeMap<alloy::primitives::Address, usize>> =
        HashMap::new();
    for c in cycles {
        let topo: Vec<ProtocolKind> = c.iter().map(|&i| universe_protocol_kind(&rows[i].protocol)).collect();
        let mut fs: Vec<_> = c.iter().filter(|&&i| rows[i].protocol == "agni-v2").map(|&i| rows[i].factory).collect();
        fs.sort();
        fs.dedup();
        let entry = venues.entry(topo).or_default();
        for f in fs {
            *entry.entry(f).or_default() += 1;
        }
    }
    let mut gaps = Vec::new();
    for p in artifact.profiles.iter().filter(|p| p.status == ProfileStatus::Approved) {
        let name = p.route_key.key_string();
        let limit = p.gas_limit.expect("approved class has a gas limit");
        let Some(by_factory) = venues.get(&p.route_key.protocols) else {
            continue;
        };
        for (&factory, &n_cycles) in by_factory {
            let gas: Vec<u64> = samples
                .iter()
                .filter(|s| {
                    s.source == SampleSource::ForkReplay
                        && s.outcome != Some(SampleOutcome::Reverted)
                        && s.gas_used > 0
                        && s.route_key == p.route_key
                })
                .filter(|s| {
                    s.venues.as_ref().is_some_and(|v| {
                        v.iter().any(|h| {
                            h.protocol == ProtocolKind::V2
                                && pool_factory.get(&h.pool.to_ascii_lowercase()) == Some(&factory)
                        })
                    })
                })
                .map(|s| s.gas_used)
                .collect();
            let venue = v2_venue_label(factory);
            if gas.len() < min_samples {
                gaps.push(format!(
                    "{name}: prices {venue} ({n_cycles} universe cycles) on {} exact-class fork samples (< {min_samples})",
                    gas.len()
                ));
            } else if let Some(&max) = gas.iter().max().filter(|&&m| m > limit) {
                gaps.push(format!("{name}: a {venue} sample ({max}) exceeds the approved gas limit {limit}"));
            }
        }
    }
    gaps
}

struct CommittedGasInputs {
    artifact: amms::execution::gas_profile::GasProfileArtifact,
    samples: Vec<GasSample>,
    rows: Vec<amms::service::CandidatePool>,
    cycles: Vec<Vec<usize>>,
}

fn committed_gas_inputs() -> CommittedGasInputs {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let artifact = load_artifact(&root.join("config/gas_profiles/mantle_mainnet_v1.json")).unwrap();
    let samples = load_samples_jsonl(&root.join("config/gas_profiles/pinned/samples.jsonl")).unwrap();
    let rows = amms::service::read_unified_csv(&root.join("data/pool_universe.csv")).unwrap();
    let settlement = amms::service::DEFAULT_WMNT;
    let cycles = universe_cycles(&rows, settlement);
    // Tie this enumerator to the production PathFinder.
    let tokens: Vec<amms::service::PoolTokens> = rows
        .iter()
        .map(|r| amms::service::PoolTokens { pool: r.pool, token0: r.token0, token1: r.token1 })
        .collect();
    assert_eq!(
        cycles.len(),
        amms::service::count_settlement_cycles(&tokens, settlement, 3),
        "test cycle enumerator drifted from the production PathFinder"
    );
    CommittedGasInputs { artifact, samples, rows, cycles }
}

/// PR109-F1 / DI-54: RouteKey has no venue axis, so an Approved class prices every
/// admitted V2 venue on the committed universe's cycles of its topology. Each such
/// venue must carry >= 2 fork samples of the exact class, all within its limit.
/// A class that cannot meet this is withheld (evidence/gas/whi-1413/withhold.py).
#[test]
fn committed_approved_classes_are_measured_on_every_v2_venue_they_price() {
    let c = committed_gas_inputs();
    // Not vacuous: the universe has cycles and both admitted V2 venues occur on them.
    assert!(c.cycles.len() > 1000, "{}", c.cycles.len());
    let gaps = approved_class_venue_gaps(&c.artifact, &c.samples, &c.rows, &c.cycles, 2);
    assert!(gaps.is_empty(), "Approved classes lack exact-class venue samples:\n{}", gaps.join("\n"));
}

/// Non-vacuity of the PR109-F1 guard: the 45c0bd9 profile differed from the committed
/// one only in approving `h3:v2+moe+v2:bins=0` (limit 561348) on 12 Moe V1/Moe V1
/// samples. Restoring that entry in memory must trip the guard on FusionX V2, and
/// only there.
#[test]
fn venue_guard_rejects_the_45c0bd9_approval_of_h3_v2_moe_v2_bins_0() {
    use amms::execution::gas_profile::BinCrossingBucket;
    let mut c = committed_gas_inputs();
    let key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::Moe, ProtocolKind::V2])
        .unwrap()
        .with_moe_bins(BinCrossingBucket::Zero);
    let p = c.artifact.profiles.iter_mut().find(|p| p.route_key == key).unwrap();
    assert_eq!(p.status, ProfileStatus::Unsupported, "fixture premise: the class is withheld");
    assert!(
        p.reason.as_deref().is_some_and(|r| r.contains("PR109-F1")),
        "withheld under PR109-F1: {:?}",
        p.reason
    );
    p.status = ProfileStatus::Approved;
    p.gas_limit = Some(561_348);
    let gaps = approved_class_venue_gaps(&c.artifact, &c.samples, &c.rows, &c.cycles, 2);
    assert_eq!(
        gaps,
        vec!["h3:v2+moe+v2:bins=0: prices fusionx-v2 (4 universe cycles) on 0 exact-class fork samples (< 2)".to_string()]
    );
}
