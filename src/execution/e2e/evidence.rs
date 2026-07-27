//! Versioned evidence bundle for one WHI-525 Mantle Sepolia E2E arb run (M6).
//!
//! Written by `examples/e2e/e2e_run` after a successful (or operator-visible
//! failed) cycle. Same style as [`super::manifest::DeploymentManifest`]:
//! explicit `schema_version`, hex strings for addresses/digests, no floats,
//! no secrets. The bundle is the durable record that a later milestone
//! (canary, audit) can re-open without replaying the chain.
//!
//! Fields mirror the plan's required list: manifest digest, adapter/venue
//! provenance, trigger + arb tx hashes, canonical receipts, reconciliation
//! table, preflight stage, runtime/profile identities, settlement deltas,
//! and any deferred notes for this run.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::manifest::VenueProvenance;

pub const EVIDENCE_BUNDLE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum EvidenceError {
    #[error("io: {0}")]
    Io(String),
    #[error("json: {0}")]
    Json(String),
    #[error("unsupported schema_version {0}")]
    UnsupportedSchemaVersion(u32),
}

/// One on-chain receipt the E2E run observed and waited on to finality.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceReceipt {
    pub label: String,
    pub tx_hash: String,
    pub block_number: u64,
    pub block_hash: String,
    pub gas_used: u64,
    pub effective_gas_price: u128,
    pub success: bool,
    /// Depth at which the run treated this receipt as final
    /// (`harness_config.finality_depth` at write time).
    pub finality_depth: u64,
}

/// One row of the post-settlement reconciliation table: expected vs observed
/// for a single dimension the operator cares about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationRow {
    pub field: String,
    pub expected: String,
    pub observed: String,
    pub ok: bool,
}

/// Versioned record of one WHI-525 E2E arb cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceBundle {
    pub schema_version: u32,
    pub chain_id: u64,

    /// Digest of the deployment manifest this run loaded (not re-derived).
    pub manifest_digest: String,
    pub venue_provenance: VenueProvenance,
    pub adapters: Vec<String>,

    pub executor_address: String,
    pub signer_address: String,
    pub fixture_pool_v2: String,
    pub fixture_pool_agni_v3: String,

    /// Runtime identity digests observed for this run.
    pub identity_digest: String,
    pub patched_runtime_hash: String,
    pub gas_profile_content_digest: String,
    pub gas_profile_identity: String,

    /// Route that was executed (protocol sequence + crossing buckets).
    pub route_key: String,
    pub amount_in: String,
    pub expected_net_profit_mnt_wei: String,
    pub min_amount_out: String,

    /// Trigger txs that created the price imbalance (operator-supplied via
    /// `e2e_run --trigger-tx-hash`, typically the hashes printed by
    /// `e2e_trigger`). Empty when the operator did not pass any.
    pub trigger_tx_hashes: Vec<String>,
    /// Arb execute tx produced by this run.
    pub arb_tx_hash: String,
    /// Executor WMNT balance immediately before the arb broadcast.
    pub executor_wmnt_before: String,
    /// Executor WMNT balance after finality.
    pub executor_wmnt_after: String,
    /// `after - before` (saturating), the on-chain settlement delta.
    pub settlement_delta_wmnt_wei: String,
    pub receipts: Vec<EvidenceReceipt>,
    pub reconciliation: Vec<ReconciliationRow>,

    /// Preflight stage that was used (`ExecutionStage::E2e`).
    pub preflight_stage: String,
    /// Pause gate that was used (`AlwaysAllow` today — breaker not wired).
    pub pause_gate: String,

    /// Snapshot identity the candidate was built against.
    pub snapshot_block_number: u64,
    pub snapshot_block_hash: String,
    pub pool_universe_fingerprint: String,

    /// Free-form notes for anything this run intentionally left open
    /// (e.g. "breaker not wired — DI-12"). Never secrets.
    pub deferrals: Vec<String>,
}

pub fn write_evidence_bundle(path: &Path, bundle: &EvidenceBundle) -> Result<(), EvidenceError> {
    if bundle.schema_version != EVIDENCE_BUNDLE_SCHEMA_VERSION {
        return Err(EvidenceError::UnsupportedSchemaVersion(
            bundle.schema_version,
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| EvidenceError::Io(e.to_string()))?;
        }
    }
    let json = serde_json::to_string_pretty(bundle).map_err(|e| EvidenceError::Json(e.to_string()))?;
    fs::write(path, json).map_err(|e| EvidenceError::Io(e.to_string()))
}

pub fn load_evidence_bundle(path: &Path) -> Result<EvidenceBundle, EvidenceError> {
    let raw = fs::read_to_string(path).map_err(|e| EvidenceError::Io(e.to_string()))?;
    let bundle: EvidenceBundle =
        serde_json::from_str(&raw).map_err(|e| EvidenceError::Json(e.to_string()))?;
    if bundle.schema_version != EVIDENCE_BUNDLE_SCHEMA_VERSION {
        return Err(EvidenceError::UnsupportedSchemaVersion(
            bundle.schema_version,
        ));
    }
    Ok(bundle)
}

/// Canonical digest of a deployment manifest's live-comparable identity
/// fields (everything `diff_against_chain` cares about). Used as
/// `EvidenceBundle::manifest_digest` so the evidence bundle can pin which
/// deployment it ran against without embedding the full manifest.
pub fn deployment_manifest_digest(
    manifest: &super::manifest::DeploymentManifest,
) -> Result<String, EvidenceError> {
    // Stable subset: schema + chain + addresses + digests. Tx records are
    // append-only provenance and intentionally excluded so two manifests that
    // only differ in seed/config history still share a digest when their
    // live identity matches.
    let payload = serde_json::json!({
        "schema_version": manifest.schema_version,
        "chain_id": manifest.chain_id,
        "wmnt": manifest.wmnt,
        "executor_address": manifest.executor_address,
        "fixture_token": manifest.fixture_token,
        "fixture_pool_v2": manifest.fixture_pool_v2,
        "fixture_pool_agni_v3": manifest.fixture_pool_agni_v3,
        "venue_provenance": manifest.venue_provenance,
        "roles": manifest.roles,
        "template_hash": manifest.template_hash,
        "patched_runtime_hash": manifest.patched_runtime_hash,
        "immutable_values_digest": manifest.immutable_values_digest,
        "compiler_config_digest": manifest.compiler_config_digest,
        "build_info_digest": manifest.build_info_digest,
        "storage_layout_digest": manifest.storage_layout_digest,
        "plan_digest": manifest.plan_digest,
        "identity_digest": manifest.identity_digest,
        "constructor_args_digest": manifest.constructor_args_digest,
        "gas_profile_content_digest": manifest.gas_profile_content_digest,
        "e2e_config_digest": manifest.e2e_config_digest,
    });
    let bytes = serde_json::to_vec(&payload).map_err(|e| EvidenceError::Json(e.to_string()))?;
    Ok(format!("{}", alloy::primitives::keccak256(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::e2e::manifest::{DeploymentManifest, RoleHolders, VenueProvenance};

    fn sample_bundle() -> EvidenceBundle {
        EvidenceBundle {
            schema_version: EVIDENCE_BUNDLE_SCHEMA_VERSION,
            chain_id: 5003,
            manifest_digest: "0xabcd".to_string(),
            venue_provenance: VenueProvenance::Fixture,
            adapters: vec!["v2".to_string(), "agni_v3".to_string()],
            executor_address: "0x1111111111111111111111111111111111111111".to_string(),
            signer_address: "0x2222222222222222222222222222222222222222".to_string(),
            fixture_pool_v2: "0x3333333333333333333333333333333333333333".to_string(),
            fixture_pool_agni_v3: "0x4444444444444444444444444444444444444444".to_string(),
            identity_digest: "0x5555".to_string(),
            patched_runtime_hash: "0x6666".to_string(),
            gas_profile_content_digest: "0x7777".to_string(),
            gas_profile_identity: "profile-id".to_string(),
            route_key: "v2|v3:0".to_string(),
            amount_in: "1000".to_string(),
            expected_net_profit_mnt_wei: "100".to_string(),
            min_amount_out: "1000".to_string(),
            trigger_tx_hashes: vec!["0xtrigger".to_string()],
            arb_tx_hash: "0xdeadbeef".to_string(),
            executor_wmnt_before: "1000000000000000000".to_string(),
            executor_wmnt_after: "1000000000000100000".to_string(),
            settlement_delta_wmnt_wei: "100000".to_string(),
            receipts: vec![EvidenceReceipt {
                label: "arb".to_string(),
                tx_hash: "0xdeadbeef".to_string(),
                block_number: 41_790_904,
                block_hash: "0xfeed".to_string(),
                gas_used: 150_000,
                effective_gas_price: 50_000_000_000,
                success: true,
                finality_depth: 12,
            }],
            reconciliation: vec![ReconciliationRow {
                field: "receipt_success".to_string(),
                expected: "true".to_string(),
                observed: "true".to_string(),
                ok: true,
            }],
            preflight_stage: "E2e".to_string(),
            pause_gate: "AlwaysAllow".to_string(),
            snapshot_block_number: 41_790_900,
            snapshot_block_hash: "0xaaaa".to_string(),
            pool_universe_fingerprint: "0xbbbb".to_string(),
            deferrals: vec!["breaker not wired (DI-12)".to_string()],
        }
    }

    #[test]
    fn round_trips_through_json_exactly() {
        let bundle = sample_bundle();
        let json = serde_json::to_string_pretty(&bundle).unwrap();
        let decoded: EvidenceBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(bundle, decoded);
    }

    #[test]
    fn rejects_unsupported_schema_version_on_load() {
        let mut bundle = sample_bundle();
        bundle.schema_version = 99;
        let json = serde_json::to_string(&bundle).unwrap();
        let dir = tempfile_dir();
        let path = dir.join("evidence.json");
        fs::write(&path, json).unwrap();
        let err = load_evidence_bundle(&path).unwrap_err();
        assert!(matches!(err, EvidenceError::UnsupportedSchemaVersion(99)));
    }

    #[test]
    fn deployment_manifest_digest_is_stable_and_ignores_tx_records() {
        let mut a = sample_manifest();
        let mut b = sample_manifest();
        b.deploy_txs.push(crate::execution::e2e::manifest::TxRecord {
            label: "extra".to_string(),
            tx_hash: "0x1".to_string(),
            block_number: 1,
            block_hash: "0x2".to_string(),
            gas_used: 1,
        });
        assert_eq!(
            deployment_manifest_digest(&a).unwrap(),
            deployment_manifest_digest(&b).unwrap()
        );
        a.executor_address = "0x9999999999999999999999999999999999999999".to_string();
        assert_ne!(
            deployment_manifest_digest(&a).unwrap(),
            deployment_manifest_digest(&b).unwrap()
        );
    }

    fn sample_manifest() -> DeploymentManifest {
        DeploymentManifest {
            schema_version: 1,
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
            deploy_txs: vec![],
            config_txs: vec![],
            seed_txs: vec![],
        }
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "amms-evidence-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
