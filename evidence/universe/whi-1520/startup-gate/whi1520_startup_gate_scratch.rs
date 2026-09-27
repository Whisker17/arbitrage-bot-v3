//! WHI-1520 scratch (not committed): the bot's live startup gates, offline, on the
//! committed universe + profile against a tip observed read-only (env WHI1520_TIP).
use amms::execution::{RuntimeGasProfile, RuntimeProfileConfig};
use amms::service::{
    assert_universe_gas_profile_compatibility, enforce_universe_freshness, PoolUniverseSource,
    SelectedProtocol, UnifiedPoolUniverseSource, DEFAULT_UNIVERSE_MAX_AGE_BLOCKS,
};
use std::path::PathBuf;

#[tokio::test]
async fn whi1520_startup_gates_accept_committed_universe_at_observed_tip() {
    let tip: u64 = std::env::var("WHI1520_TIP").expect("WHI1520_TIP").parse().unwrap();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let profile = RuntimeGasProfile::load(
        &root.join("config/gas_profiles/mantle_mainnet_v1.json"),
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
    )
    .expect("load mainnet profile");
    let loaded = UnifiedPoolUniverseSource::new(root.join("data/pool_universe.csv"))
        .with_protocol_filter(SelectedProtocol::all())
        .load(5000, amms::service::fixture_settlement_asset())
        .await
        .expect("load committed universe");
    let snap = loaded.snapshot_block.expect("snapshot");
    enforce_universe_freshness("unified", loaded.snapshot_block, tip, DEFAULT_UNIVERSE_MAX_AGE_BLOCKS, "regen")
        .expect("freshness gate");
    assert_universe_gas_profile_compatibility(&loaded, &profile, 3).expect("WHI-1408 compatibility gate");
    let age = tip.saturating_sub(snap);
    println!(
        "GATES OK pools={} fingerprint={} snapshot_block={snap} tip={tip} age_blocks={age} max_age={} remaining_blocks={} profile={}",
        loaded.rows.len(), loaded.fingerprint, DEFAULT_UNIVERSE_MAX_AGE_BLOCKS,
        DEFAULT_UNIVERSE_MAX_AGE_BLOCKS - age, profile.artifact_digest()
    );
    // Negative control: the same gate at the tip where the old universe failed must reject B* + 250001.
    assert!(enforce_universe_freshness("unified", Some(snap), snap + DEFAULT_UNIVERSE_MAX_AGE_BLOCKS + 1,
        DEFAULT_UNIVERSE_MAX_AGE_BLOCKS, "regen").is_err());
}
