//! `ShadowOverrideManifest` — bundles every digest a shadow run pins for one config
//! generation: the compiled storage layout, the WMNT storage descriptor, the Moe
//! allowlist, and the verified on-chain runtime identity. A shadow run computes this
//! once at startup and re-derives + compares it at batch boundaries so a mid-run config
//! change (a rewritten allowlist file, a re-run compile) is detected rather than
//! silently applied to later candidates.
//!
//! `PoolProvenanceOutcome` records, per candidate, how its pool's on-chain address was
//! established: CREATE2-derived and byte-verified against a committed
//! `(factory, init_code_hash)` entry, allowlisted (Moe LB), or rejected outright.

use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use crate::execution::runtime_identity::{BuildEvidence, VerifiedRuntimeIdentity};

use super::approved_pools::{self, ApprovedPoolProtocol, ApprovedPoolsConfig, ApprovedPoolsError};
use super::digest::{digest_of, digest_of_bytes};
use super::moe_allowlist::{self, MoeAllowlist, MoeAllowlistError};
use super::wmnt_descriptor::{self, WmntDescriptor, WmntDescriptorError};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("wmnt descriptor: {0}")]
    WmntDescriptor(#[from] WmntDescriptorError),
    #[error("moe allowlist: {0}")]
    MoeAllowlist(#[from] MoeAllowlistError),
    #[error("approved pools: {0}")]
    ApprovedPools(#[from] ApprovedPoolsError),
}

/// Every pinned digest for one shadow run's config generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowOverrideManifest {
    pub storage_layout_digest: B256,
    pub wmnt_descriptor_digest: B256,
    pub moe_allowlist_digest: B256,
    pub identity_digest: B256,
    pub approved_pools_digest: B256,
    pub threshold_config_digest: B256,
}

impl ShadowOverrideManifest {
    pub fn new(
        evidence: &BuildEvidence,
        wmnt_descriptor: &WmntDescriptor,
        moe_allowlist: &MoeAllowlist,
        identity: &VerifiedRuntimeIdentity,
        approved_pools: &ApprovedPoolsConfig,
        threshold_bytes: &[u8],
    ) -> Result<Self, ManifestError> {
        Ok(Self {
            storage_layout_digest: digest_of(evidence.storage_layout()),
            wmnt_descriptor_digest: wmnt_descriptor::digest(wmnt_descriptor)?,
            moe_allowlist_digest: moe_allowlist::digest(moe_allowlist)?,
            identity_digest: identity.identity_digest(),
            approved_pools_digest: approved_pools::digest(approved_pools)?,
            threshold_config_digest: digest_of_bytes(threshold_bytes),
        })
    }

    /// Whether `other` pins the exact same config generation as `self` — used at batch
    /// boundaries to detect a mid-run config change.
    pub fn matches(&self, other: &ShadowOverrideManifest) -> bool {
        self == other
    }
}

/// The CREATE2 proof behind a [`PoolProvenanceOutcome::Verified`] outcome: which
/// committed `(factory, init_code_hash)` entry the pool matched, and the salt used to
/// derive its address — recorded in the ledger so a `Verified` row is independently
/// re-checkable, not just a bare assertion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Create2Proof {
    pub protocol: ApprovedPoolProtocol,
    pub factory: Address,
    pub init_code_hash: B256,
    pub salt: B256,
}

/// How a candidate's pool address was established against on-chain / committed
/// provenance sources.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PoolProvenanceOutcome {
    /// CREATE2-derived and matched the claimed pool address.
    Verified(Create2Proof),
    /// Matched a committed Moe LB allowlist entry (Moe pairs aren't CREATE2-derivable).
    MoeAllowlisted,
    /// Failed every applicable check.
    Rejected(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::gas_profile::MANTLE_MAINNET_CHAIN_ID;
    use crate::execution::gas_runtime::mainnet_verified_identity;
    use crate::execution::runtime_identity::{resolve_immutable_plan, ImmutableInputs};
    use alloy::primitives::address;

    const MAINNET_BUILD_EVIDENCE_JSON: &str =
        include_str!("../../../contracts/executor/artifacts/ArbitrageExecutor.full.json");

    fn mainnet_evidence() -> BuildEvidence {
        let value: serde_json::Value = serde_json::from_str(MAINNET_BUILD_EVIDENCE_JSON).unwrap();
        BuildEvidence::from_json(value).unwrap()
    }

    fn sample_wmnt_descriptor() -> WmntDescriptor {
        wmnt_descriptor::load_wmnt_descriptor(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/wmnt_descriptor.mantle_mainnet.json"),
        )
        .unwrap()
    }

    fn sample_moe_allowlist() -> MoeAllowlist {
        moe_allowlist::load_moe_allowlist(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/moe_allowlist.mantle_mainnet.json"),
        )
        .unwrap()
    }

    fn sample_approved_pools() -> ApprovedPoolsConfig {
        approved_pools::load_approved_pools(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/approved_pools.mantle_mainnet.json"),
        )
        .unwrap()
    }

    fn sample_threshold_bytes() -> Vec<u8> {
        super::super::thresholds::load_threshold_bytes(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("config/gas_profiles/shadow_thresholds.mantle_mainnet.json"),
        )
        .unwrap()
    }

    #[test]
    fn bundles_all_six_digests_for_the_mainnet_config_generation() {
        let evidence = mainnet_evidence();
        let wmnt = sample_wmnt_descriptor();
        let allowlist = sample_moe_allowlist();
        let identity = mainnet_verified_identity();
        let approved_pools = sample_approved_pools();
        let threshold_bytes = sample_threshold_bytes();

        let manifest = ShadowOverrideManifest::new(
            &evidence,
            &wmnt,
            &allowlist,
            identity,
            &approved_pools,
            &threshold_bytes,
        )
        .unwrap();

        assert_eq!(
            manifest.storage_layout_digest,
            digest_of(evidence.storage_layout())
        );
        assert_eq!(
            manifest.wmnt_descriptor_digest,
            wmnt_descriptor::digest(&wmnt).unwrap()
        );
        assert_eq!(
            manifest.moe_allowlist_digest,
            moe_allowlist::digest(&allowlist).unwrap()
        );
        assert_eq!(manifest.identity_digest, identity.identity_digest());
        assert_eq!(
            manifest.approved_pools_digest,
            approved_pools::digest(&approved_pools).unwrap()
        );
        assert_eq!(
            manifest.threshold_config_digest,
            digest_of_bytes(&threshold_bytes)
        );
    }

    #[test]
    fn matches_is_reflexive_and_detects_a_changed_allowlist() {
        let evidence = mainnet_evidence();
        let wmnt = sample_wmnt_descriptor();
        let identity = mainnet_verified_identity();
        let approved_pools = sample_approved_pools();
        let threshold_bytes = sample_threshold_bytes();

        let mut allowlist = sample_moe_allowlist();
        let baseline = ShadowOverrideManifest::new(
            &evidence,
            &wmnt,
            &allowlist,
            identity,
            &approved_pools,
            &threshold_bytes,
        )
        .unwrap();
        assert!(baseline.matches(&baseline));

        allowlist.entries[0].bin_step = 20;
        let changed = ShadowOverrideManifest::new(
            &evidence,
            &wmnt,
            &allowlist,
            identity,
            &approved_pools,
            &threshold_bytes,
        )
        .unwrap();
        assert!(!baseline.matches(&changed));
        assert_eq!(
            baseline.storage_layout_digest,
            changed.storage_layout_digest
        );
        assert_eq!(baseline.identity_digest, changed.identity_digest);
    }

    #[test]
    fn identity_digest_changes_when_the_verified_identity_differs() {
        let evidence = mainnet_evidence();
        let wmnt = sample_wmnt_descriptor();
        let allowlist = sample_moe_allowlist();
        let identity = mainnet_verified_identity();
        let approved_pools = sample_approved_pools();
        let threshold_bytes = sample_threshold_bytes();

        let baseline = ShadowOverrideManifest::new(
            &evidence,
            &wmnt,
            &allowlist,
            identity,
            &approved_pools,
            &threshold_bytes,
        )
        .unwrap();

        // A plan resolved for a different chain id yields a different identity_digest,
        // even though every other manifest input is unchanged.
        let wmnt_addr = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
        let other_plan = resolve_immutable_plan(
            &evidence,
            ImmutableInputs { wmnt: wmnt_addr },
            MANTLE_MAINNET_CHAIN_ID + 1,
        )
        .unwrap();
        let other_identity = crate::execution::runtime_identity::verify_deployed_runtime(
            other_plan.patched_bytes(),
            &other_plan,
        )
        .unwrap();

        let other_manifest = ShadowOverrideManifest::new(
            &evidence,
            &wmnt,
            &allowlist,
            &other_identity,
            &approved_pools,
            &threshold_bytes,
        )
        .unwrap();

        assert_ne!(baseline.identity_digest, other_manifest.identity_digest);
        assert_eq!(
            baseline.storage_layout_digest,
            other_manifest.storage_layout_digest
        );
    }

    fn sample_create2_proof() -> Create2Proof {
        Create2Proof {
            protocol: ApprovedPoolProtocol::UniswapV2,
            factory: address!("3333333333333333333333333333333333333333"),
            init_code_hash: B256::repeat_byte(0x44),
            salt: B256::repeat_byte(0x55),
        }
    }

    #[test]
    fn pool_provenance_outcome_variants_are_distinguishable() {
        assert_ne!(
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            PoolProvenanceOutcome::MoeAllowlisted
        );
        assert_ne!(
            PoolProvenanceOutcome::MoeAllowlisted,
            PoolProvenanceOutcome::Rejected("no match".into())
        );
        assert_eq!(
            PoolProvenanceOutcome::Rejected("x".into()),
            PoolProvenanceOutcome::Rejected("x".into())
        );
        assert_eq!(
            PoolProvenanceOutcome::Verified(sample_create2_proof()),
            PoolProvenanceOutcome::Verified(sample_create2_proof())
        );
    }
}
