//! Deployment manifest + harness config schema for the WHI-525 Mantle Sepolia
//! E2E harness (M3).
//!
//! Two committed, secret-free JSON schemas:
//! - [`HarnessConfig`] (`config/e2e_sepolia.json`): the static policy an E2E
//!   run is configured against — chain id, source/venue provenance policy,
//!   finality depth, and paths to the other artifacts (gas profile, executor
//!   identity, manifest). Hand-authored, checked in.
//! - [`DeploymentManifest`] (written by the M4 bootstrap example, not by
//!   this file): every address, role holder, and identity digest a live
//!   deployment produced, plus append-only tx provenance. [`diff_against_chain`]
//!   compares a manifest on disk against one freshly re-derived from live
//!   chain state, so idempotent bootstrap can detect drift instead of
//!   silently reusing (or silently redeploying over) a stale deployment.
//!
//! Both schemas mirror [`super::super::runtime_identity::ExecutorIdentityExport`]'s
//! style: explicit `schema_version`, hex strings for addresses/digests (never
//! a native `Address`/`B256`, so the file stays plain JSON), no floats, no
//! secrets.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const DEPLOYMENT_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub const HARNESS_CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("unsupported schema_version {0}")]
    UnsupportedSchemaVersion(u32),
}

/// How a venue's bytecode entered a [`DeploymentManifest`]. Only
/// [`VenueProvenance::Fixture`] exists today — WHI-525's resolved scope
/// decision is repo-owned Foundry fixture pools only (see the plan's
/// "Resolved scope decisions" #1). This is an enum rather than a free-text
/// field so that ever adding a public/canonical venue is a conscious schema
/// change, not a stringly-typed drift that could silently relabel a fixture
/// pool as a canonical deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueProvenance {
    Fixture,
}

/// One historical on-chain action a [`DeploymentManifest`] was built from —
/// a deploy, a `registerPool`/`setHotExecutor` config call, or a `seed()`/
/// funding transfer. Append-only provenance: [`diff_against_chain`] never
/// compares these, since there is no "current" tx hash for a past
/// transaction to drift against — only the identity/address/digest fields
/// below are live-comparable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxRecord {
    pub label: String,
    pub tx_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub gas_used: u64,
}

/// Executor role holders at manifest-write time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleHolders {
    pub admin: String,
    pub hot_executor: String,
}

/// Committed record of one WHI-525 Mantle Sepolia E2E bootstrap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentManifest {
    pub schema_version: u32,
    pub chain_id: u64,

    pub wmnt: String,
    pub executor_address: String,
    pub fixture_token: String,
    pub fixture_pool_v2: String,
    pub fixture_pool_agni_v3: String,
    pub venue_provenance: VenueProvenance,
    pub roles: RoleHolders,

    pub template_hash: String,
    pub patched_runtime_hash: String,
    pub immutable_values_digest: String,
    pub compiler_config_digest: String,
    pub build_info_digest: String,
    pub storage_layout_digest: String,
    pub plan_digest: String,
    pub identity_digest: String,
    pub constructor_args_digest: String,

    pub gas_profile_content_digest: String,
    pub e2e_config_digest: String,

    pub deploy_txs: Vec<TxRecord>,
    pub config_txs: Vec<TxRecord>,
    pub seed_txs: Vec<TxRecord>,
}

/// One field of live-comparable drift between a recorded manifest and a
/// freshly re-derived one. `recorded`/`observed` are rendered as strings
/// (via `Debug`/`Display` as appropriate) purely for uniform reporting —
/// this type carries no typed value of its own to compare on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestDrift {
    pub field: &'static str,
    pub recorded: String,
    pub observed: String,
}

/// Compare every live-comparable field of `recorded` (the manifest on disk)
/// against `observed` (freshly re-derived from live chain state). Tx records
/// are deliberately excluded (see [`TxRecord`]'s doc comment). Returns one
/// [`ManifestDrift`] per differing field, in a fixed field order, so a
/// caller can log or fail closed on the full list rather than just the
/// first mismatch found. An empty result means "reuse the existing
/// deployment"; any non-empty result means the M4 bootstrap example must
/// exit nonzero rather than silently redeploying or silently proceeding.
pub fn diff_against_chain(
    recorded: &DeploymentManifest,
    observed: &DeploymentManifest,
) -> Vec<ManifestDrift> {
    let mut drift = Vec::new();

    macro_rules! check {
        ($field:literal, $recorded:expr, $observed:expr) => {
            let (a, b) = (($recorded).to_string(), ($observed).to_string());
            if a != b {
                drift.push(ManifestDrift {
                    field: $field,
                    recorded: a,
                    observed: b,
                });
            }
        };
    }

    check!("schema_version", recorded.schema_version, observed.schema_version);
    check!("chain_id", recorded.chain_id, observed.chain_id);
    check!("wmnt", recorded.wmnt, observed.wmnt);
    check!(
        "executor_address",
        recorded.executor_address,
        observed.executor_address
    );
    check!(
        "fixture_token",
        recorded.fixture_token,
        observed.fixture_token
    );
    check!(
        "fixture_pool_v2",
        recorded.fixture_pool_v2,
        observed.fixture_pool_v2
    );
    check!(
        "fixture_pool_agni_v3",
        recorded.fixture_pool_agni_v3,
        observed.fixture_pool_agni_v3
    );
    check!(
        "venue_provenance",
        format!("{:?}", recorded.venue_provenance),
        format!("{:?}", observed.venue_provenance)
    );
    check!("roles.admin", recorded.roles.admin, observed.roles.admin);
    check!(
        "roles.hot_executor",
        recorded.roles.hot_executor,
        observed.roles.hot_executor
    );
    check!("template_hash", recorded.template_hash, observed.template_hash);
    check!(
        "patched_runtime_hash",
        recorded.patched_runtime_hash,
        observed.patched_runtime_hash
    );
    check!(
        "immutable_values_digest",
        recorded.immutable_values_digest,
        observed.immutable_values_digest
    );
    check!(
        "compiler_config_digest",
        recorded.compiler_config_digest,
        observed.compiler_config_digest
    );
    check!(
        "build_info_digest",
        recorded.build_info_digest,
        observed.build_info_digest
    );
    check!(
        "storage_layout_digest",
        recorded.storage_layout_digest,
        observed.storage_layout_digest
    );
    check!("plan_digest", recorded.plan_digest, observed.plan_digest);
    check!(
        "identity_digest",
        recorded.identity_digest,
        observed.identity_digest
    );
    check!(
        "constructor_args_digest",
        recorded.constructor_args_digest,
        observed.constructor_args_digest
    );
    check!(
        "gas_profile_content_digest",
        recorded.gas_profile_content_digest,
        observed.gas_profile_content_digest
    );
    check!(
        "e2e_config_digest",
        recorded.e2e_config_digest,
        observed.e2e_config_digest
    );

    drift
}

pub fn write_deployment_manifest(
    path: &Path,
    manifest: &DeploymentManifest,
) -> Result<(), ManifestError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| ManifestError::Io(e.to_string()))?;
    }
    let json =
        serde_json::to_string_pretty(manifest).map_err(|e| ManifestError::Json(e.to_string()))?;
    fs::write(path, format!("{json}\n")).map_err(|e| ManifestError::Io(e.to_string()))?;
    Ok(())
}

pub fn load_deployment_manifest(path: &Path) -> Result<DeploymentManifest, ManifestError> {
    let raw = fs::read_to_string(path).map_err(|e| ManifestError::Io(e.to_string()))?;
    let manifest: DeploymentManifest =
        serde_json::from_str(&raw).map_err(|e| ManifestError::Json(e.to_string()))?;
    if manifest.schema_version != DEPLOYMENT_MANIFEST_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion(
            manifest.schema_version,
        ));
    }
    Ok(manifest)
}

/// Static, hand-authored policy an E2E run is configured against
/// (`config/e2e_sepolia.json`). Secret-free: no private keys, no credentialed
/// RPC endpoints (those stay in the `MANTLE_SEPOLIA_E2E_*` env namespace per
/// [`super::env_guard`]), no addresses that must stay private.
///
/// `source_url` is non-secret provenance metadata (a public docs/RPC
/// documentation URL + verification date), not the live connect endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub schema_version: u32,
    pub chain_id: u64,
    pub wmnt: String,
    /// Public provenance pointer (not used for provider connect).
    pub source_url: String,
    pub verified_at: String,
    pub venue_provenance_policy: VenueProvenance,
    pub adapters: Vec<String>,
    pub finality_depth: u64,
    pub evidence_schema_version: u32,
    pub manifest_path: String,
    pub gas_profile_path: String,
    pub executor_identity_path: String,
}

pub fn load_harness_config(path: &Path) -> Result<HarnessConfig, ManifestError> {
    let raw = fs::read_to_string(path).map_err(|e| ManifestError::Io(e.to_string()))?;
    let config: HarnessConfig =
        serde_json::from_str(&raw).map_err(|e| ManifestError::Json(e.to_string()))?;
    if config.schema_version != HARNESS_CONFIG_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedSchemaVersion(
            config.schema_version,
        ));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> DeploymentManifest {
        DeploymentManifest {
            schema_version: DEPLOYMENT_MANIFEST_SCHEMA_VERSION,
            chain_id: 5003,
            wmnt: "0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF".to_string(),
            executor_address: "0x1111111111111111111111111111111111111111".to_string(),
            fixture_token: "0x2222222222222222222222222222222222222222".to_string(),
            fixture_pool_v2: "0x3333333333333333333333333333333333333333".to_string(),
            fixture_pool_agni_v3: "0x4444444444444444444444444444444444444444".to_string(),
            venue_provenance: VenueProvenance::Fixture,
            roles: RoleHolders {
                admin: "0x5555555555555555555555555555555555555555".to_string(),
                hot_executor: "0x6666666666666666666666666666666666666666".to_string(),
            },
            template_hash: "0xaaaa".to_string(),
            patched_runtime_hash: "0xbbbb".to_string(),
            immutable_values_digest: "0xcccc".to_string(),
            compiler_config_digest: "0xdddd".to_string(),
            build_info_digest: "0xeeee".to_string(),
            storage_layout_digest: "0xffff".to_string(),
            plan_digest: "0x1234".to_string(),
            identity_digest: "0x5678".to_string(),
            constructor_args_digest: "0x9abc".to_string(),
            gas_profile_content_digest: "0xdef0".to_string(),
            e2e_config_digest: "0x1357".to_string(),
            deploy_txs: vec![TxRecord {
                label: "deploy_executor".to_string(),
                tx_hash: "0xdeadbeef".to_string(),
                block_number: 41_790_904,
                block_hash: "0xfeedface".to_string(),
                gas_used: 1_500_000,
            }],
            config_txs: vec![],
            seed_txs: vec![],
        }
    }

    #[test]
    fn round_trips_through_json_exactly() {
        let manifest = sample_manifest();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let decoded: DeploymentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(manifest, decoded);
    }

    #[test]
    fn identical_manifests_have_no_drift() {
        let manifest = sample_manifest();
        assert!(diff_against_chain(&manifest, &manifest).is_empty());
    }

    #[test]
    fn detects_address_drift() {
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.executor_address = "0x9999999999999999999999999999999999999999".to_string();
        let drift = diff_against_chain(&recorded, &observed);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].field, "executor_address");
    }

    #[test]
    fn detects_digest_drift() {
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.patched_runtime_hash = "0xdeadbeef".to_string();
        let drift = diff_against_chain(&recorded, &observed);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].field, "patched_runtime_hash");
    }

    #[test]
    fn detects_role_holder_drift() {
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.roles.hot_executor = "0x7777777777777777777777777777777777777777".to_string();
        let drift = diff_against_chain(&recorded, &observed);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].field, "roles.hot_executor");
    }

    #[test]
    fn detects_venue_provenance_drift_independently() {
        // Only `Fixture` exists today, so this exercises the comparison path
        // via a mismatched chain id alongside it rather than a second enum
        // variant — proving venue_provenance participates in the same
        // per-field, non-short-circuiting diff as every other field.
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.chain_id = 5000;
        let drift = diff_against_chain(&recorded, &observed);
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].field, "chain_id");
        assert_eq!(drift[0].recorded, "5003");
        assert_eq!(drift[0].observed, "5000");
    }

    #[test]
    fn detects_multiple_independent_drifts_in_one_pass() {
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.executor_address = "0x9999999999999999999999999999999999999999".to_string();
        observed.identity_digest = "0xbadbad".to_string();
        observed.roles.admin = "0x8888888888888888888888888888888888888888".to_string();
        let drift = diff_against_chain(&recorded, &observed);
        let fields: Vec<&str> = drift.iter().map(|d| d.field).collect();
        assert_eq!(fields.len(), 3);
        assert!(fields.contains(&"executor_address"));
        assert!(fields.contains(&"identity_digest"));
        assert!(fields.contains(&"roles.admin"));
    }

    #[test]
    fn tx_records_are_never_diffed() {
        let recorded = sample_manifest();
        let mut observed = recorded.clone();
        observed.deploy_txs[0].tx_hash = "0xdifferent".to_string();
        observed.deploy_txs[0].gas_used = 999;
        assert!(diff_against_chain(&recorded, &observed).is_empty());
    }

    #[test]
    fn rejects_an_unsupported_manifest_schema_version_on_load() {
        let dir = std::env::temp_dir().join(format!(
            "whi525-manifest-test-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.json");
        let mut manifest = sample_manifest();
        manifest.schema_version = 999;
        write_deployment_manifest(&path, &manifest).unwrap();
        let err = load_deployment_manifest(&path).unwrap_err();
        assert!(matches!(err, ManifestError::UnsupportedSchemaVersion(999)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_and_loads_a_manifest_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "whi525-manifest-roundtrip-{:?}",
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.json");
        let manifest = sample_manifest();
        write_deployment_manifest(&path, &manifest).unwrap();
        let loaded = load_deployment_manifest(&path).unwrap();
        assert_eq!(manifest, loaded);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_the_committed_harness_config() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/e2e_sepolia.json");
        let config = load_harness_config(&path).expect("committed harness config must load");
        assert_eq!(config.chain_id, 5003);
        assert_eq!(config.venue_provenance_policy, VenueProvenance::Fixture);
    }
}
