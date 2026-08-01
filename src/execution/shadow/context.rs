//! `ShadowExecutionContext` — signerless shadow mode's wallet-free, RPC-free execution
//! context: wraps a plain [`ExecutionContext`] (built via a direct struct literal, never
//! [`ExecutionContext::from_provider`]'s live chain-id/codehash/WMNT RPC checks) plus the
//! override engine's approved-registration/allowlist/storage-layout config and the ledger.
//!
//! Also home to [`ShadowPinnedConfig`], the one place that config generation is loaded
//! and digested — startup and the batch-boundary drift recheck both go through it, so
//! neither can read the committed sources differently from the other.
//!
//! There is no owned `Executor` here — `build_final_request`/`revalidate_final_request`
//! are satisfied by delegating to `executor::build_final_request_impl`/
//! `revalidate_final_request_impl`, the same free functions the production `Executor`
//! itself now delegates to, so shadow mode shares that logic without constructing a
//! wallet-adjacent production type.
//!
//! No generic provider parameter: [`ExecutionContext`] already stores a type-erased
//! `DynProvider`, and [`ExecutionContext::provider`] hands out a cheap clone of it —
//! exactly what [`ShadowExecutionContext::build_preflight`] needs to construct a
//! [`ShadowSemanticCallExecutor`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::providers::{DynProvider, Provider};
use rand::RngCore;
use serde_json::Value;

use crate::execution::executor::{build_final_request_impl, revalidate_final_request_impl};
use crate::execution::fee_context::BlockFeeContextCache;
use crate::execution::final_request::{FinalRequest, FinalRequestDigest, FinalRequestParams};
use crate::execution::gas_profile::{load_artifact, GasProfileArtifact, GasProfileError};
use crate::execution::gas_runtime::{
    mainnet_verified_identity, RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig,
};
use crate::execution::pipeline::ExecutionRequestBuilder;
use crate::execution::preflight::{
    ExecutionStage, PolicyKey, PreflightAttempt, PreflightAttemptSink, PreflightOutcome,
    RiskTieredPreflight,
};
use crate::execution::runtime_identity::{
    resolve_immutable_plan, BuildEvidence, ImmutableInputs, RuntimeIdentityError,
};
use crate::execution::shadow_thresholds as evidence_thresholds;
use crate::execution::types::{
    ExecutionContext, ExecutionContextView, ExecutionPermit, ExecutorConfig,
};
use crate::state_space::{BlockHeaderContext, SnapshotId};

use super::approved_pools::{self, ApprovedPoolsConfig, ApprovedPoolsError};
use super::call_executor::ShadowSemanticCallExecutor;
use super::invariant::ShadowInvariantSink;
use super::ledger::{LedgerError, LedgerRunHeader, RunMetadata, ShadowLedgerWriter};
use super::manifest::PoolProvenanceOutcome;
use super::manifest::{ManifestError, ShadowOverrideManifest, ShadowOverrideTarget};
use super::moe_allowlist::{self, MoeAllowlist, MoeAllowlistError};
use super::overrides::{
    build_shadow_state_override, check_route_provenance, combine_provenance_outcomes,
    ShadowOverrideInputs, ShadowRouteSummary,
};
use super::slots::SlotsError;
use super::thresholds::{self, ThresholdError};
use super::wmnt_descriptor::{self, WmntDescriptor, WmntDescriptorError};

/// Every batch boundary (see [`MANIFEST_RECHECK_BATCH_SIZE`]), [`ShadowPinnedConfig`]
/// re-reads these same six sources from disk and re-derives a fresh
/// [`ShadowOverrideManifest`] to compare against the one pinned at construction — this is
/// what it needs to find them again without re-threading a dozen loose `PathBuf`s through
/// every call site.
#[derive(Debug, Clone)]
pub struct ShadowConfigPaths {
    pub artifact_dir: PathBuf,
    pub wmnt_descriptor_path: PathBuf,
    pub moe_allowlist_path: PathBuf,
    pub approved_pools_path: PathBuf,
    pub threshold_config_path: PathBuf,
    pub gas_profile_artifact_path: PathBuf,
}

/// Where this run's ledger lives, plus the run-identity fields its header records.
/// Bundled so [`ShadowExecutionContext::new`] takes one ledger argument rather than
/// three loose ones whose order carries no type-level protection.
#[derive(Debug, Clone, Copy)]
pub struct ShadowLedgerSetup<'a> {
    pub path: &'a Path,
    pub started_at_unix: u64,
    pub service: &'a str,
}

/// One config generation, loaded from disk and pinned: every committed source a shadow
/// run's overrides depend on, plus the [`ShadowOverrideManifest`] digesting them and the
/// paths they came from.
///
/// This is the single place the load-and-digest sequence lives. Startup
/// ([`Self::load`]) and the batch-boundary drift check
/// ([`ShadowExecutionContext::recheck_manifest`], via [`Self::reload`]) run *the same*
/// code, so the fresh manifest is always derived from the same inputs, in the same
/// order, as the pinned one — a re-derivation that read one source differently would
/// report spurious drift (or, worse, miss real drift).
pub struct ShadowPinnedConfig {
    paths: ShadowConfigPaths,
    target: ShadowOverrideTarget,
    evidence: BuildEvidence,
    wmnt_descriptor: WmntDescriptor,
    moe_allowlist: MoeAllowlist,
    approved_pools: ApprovedPoolsConfig,
    gas_profile_artifact: GasProfileArtifact,
    manifest: ShadowOverrideManifest,
}

impl ShadowPinnedConfig {
    /// Reads every committed shadow config source named by `paths` and pins them into a
    /// manifest for `target`. Pure disk I/O — zero RPC.
    ///
    /// The identity input is [`mainnet_verified_identity`], a compile-time-embedded
    /// constant rather than a file read, so a mid-run edit to the build evidence is
    /// caught via `storage_layout_digest` instead.
    pub fn load(
        paths: ShadowConfigPaths,
        target: ShadowOverrideTarget,
    ) -> Result<Self, ShadowContextError> {
        let evidence = BuildEvidence::load(&paths.artifact_dir)?;
        let wmnt_descriptor = wmnt_descriptor::load_wmnt_descriptor(&paths.wmnt_descriptor_path)?;
        let moe_allowlist = moe_allowlist::load_moe_allowlist(&paths.moe_allowlist_path)?;
        let approved_pools = approved_pools::load_approved_pools(&paths.approved_pools_path)?;
        let threshold_bytes = thresholds::load_threshold_bytes(&paths.threshold_config_path)?;
        evidence_thresholds::validate(&threshold_bytes)?;
        let gas_profile_artifact = load_artifact(&paths.gas_profile_artifact_path)?;

        let manifest = ShadowOverrideManifest::new(
            &evidence,
            &wmnt_descriptor,
            &moe_allowlist,
            mainnet_verified_identity(),
            &approved_pools,
            &threshold_bytes,
            &gas_profile_artifact,
            target,
        )?;

        Ok(Self {
            paths,
            target,
            evidence,
            wmnt_descriptor,
            moe_allowlist,
            approved_pools,
            gas_profile_artifact,
            manifest,
        })
    }

    /// Re-runs [`Self::load`] against the same paths and target, for the batch-boundary
    /// drift comparison.
    fn reload(&self) -> Result<Self, ShadowContextError> {
        Self::load(self.paths.clone(), self.target)
    }

    pub fn manifest(&self) -> &ShadowOverrideManifest {
        &self.manifest
    }
}

/// How many [`ShadowExecutionContext::build_preflight`] calls make up one batch: every
/// `MANIFEST_RECHECK_BATCH_SIZE`th call re-reads every config source from disk and aborts
/// (via [`ShadowContextError::ManifestDrift`]) if any of them no longer matches the
/// manifest pinned at construction — catching a mid-run rewrite of the allowlist,
/// approved-pools, WMNT descriptor, threshold, or build-evidence files.
const MANIFEST_RECHECK_BATCH_SIZE: u64 = 100;

/// Domain-separated digest for a production-gate-blocked scaffold row (not a
/// real [`FinalRequest`] digest — no signed/broadcast body exists yet).
fn gate_blocked_digest(
    opportunity_signature: &str,
    amount_in: U256,
    min_profit: U256,
) -> FinalRequestDigest {
    const DOMAIN: &[u8] = b"whisker-arb/shadow-gate-blocked/v1";
    let mut preimage = Vec::with_capacity(
        DOMAIN.len() + 1 + opportunity_signature.len() + 1 + 32 + 1 + 32,
    );
    preimage.extend_from_slice(DOMAIN);
    preimage.push(0);
    preimage.extend_from_slice(opportunity_signature.as_bytes());
    preimage.push(0);
    preimage.extend_from_slice(&amount_in.to_be_bytes::<32>());
    preimage.push(0);
    preimage.extend_from_slice(&min_profit.to_be_bytes::<32>());
    FinalRequestDigest(keccak256(preimage))
}

/// A fresh per-run identifier, generated once at construction — distinguishes this run's
/// ledger rows from any other run writing to the same or a rotated ledger file. Follows
/// the same `rand::rng().fill_bytes` pattern already used for E2E session nonces
/// (`e2e/provider_identity.rs`, `e2e/capability.rs`), avoiding a new `uuid` dependency.
fn fresh_run_id() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    B256::from(bytes).to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum ShadowContextError {
    #[error("shadow gas profile: {0}")]
    GasProfile(#[from] RuntimeGasProfileError),
    #[error("shadow runtime identity: {0}")]
    RuntimeIdentity(#[from] RuntimeIdentityError),
    #[error("shadow wmnt descriptor: {0}")]
    WmntDescriptor(#[from] WmntDescriptorError),
    #[error("shadow moe allowlist: {0}")]
    MoeAllowlist(#[from] MoeAllowlistError),
    #[error("shadow approved pools: {0}")]
    ApprovedPools(#[from] ApprovedPoolsError),
    #[error("shadow threshold config: {0}")]
    ThresholdConfig(#[from] ThresholdError),
    #[error("shadow evidence thresholds: {0}")]
    EvidenceThresholds(#[from] evidence_thresholds::ThresholdSchemaError),
    #[error("shadow manifest: {0}")]
    Manifest(#[from] ManifestError),
    #[error("shadow gas profile artifact: {0}")]
    GasProfileArtifactLoad(#[from] GasProfileError),
    #[error("shadow ledger: {0}")]
    Ledger(#[from] LedgerError),
    #[error("shadow state override: {0}")]
    Slots(#[from] SlotsError),
    #[error(
        "shadow manifest drift: on-disk config no longer matches the manifest pinned at \
         construction — a mid-run edit to the allowlist, approved-pools, WMNT descriptor, \
         threshold, or build-evidence files was detected at a batch boundary"
    )]
    ManifestDrift,
}

/// Signerless shadow execution context (WHI-549): zero RPC at construction, no owned
/// `Executor`. Holds an [`ExecutionContext`] directly, the [`ShadowPinnedConfig`] the
/// override engine reads (WMNT descriptor, Moe allowlist, approved CREATE2
/// registrations, manifest), the two values derived from its build evidence at
/// construction (storage layout, patched runtime bytes), and the ledger.
pub struct ShadowExecutionContext {
    context: ExecutionContext,
    executor_config: ExecutorConfig,
    storage_layout: Value,
    patched_runtime: Vec<u8>,
    /// `Some` when [`wmnt_descriptor::check_wmnt_balance_slot_drift`] found a mismatch at
    /// construction. Deliberately non-fatal here — WHI-549 requires WMNT descriptor drift
    /// to surface as a per-candidate [`crate::execution::preflight::CallOutcome::EnvUnsupported`],
    /// not a hard construction-time abort, so [`Self::build_preflight`] folds this reason
    /// into a forced [`PoolProvenanceOutcome::Rejected`] instead.
    wmnt_drift_reason: Option<String>,
    pinned_config: ShadowPinnedConfig,
    call_count: AtomicU64,
    ledger: Arc<ShadowLedgerWriter>,
}

impl ShadowExecutionContext {
    /// Builds a shadow context with **zero RPC calls**: `provider` is erased and stored
    /// for `eth_call` use only, never queried for chain id / deployed code / a live WMNT
    /// read (that's `ExecutionContext::from_provider`'s job for the production path).
    /// Instead, the executor's patched runtime bytes come from re-deriving the immutable
    /// plan from `pinned_config`'s build evidence (for
    /// [`crate::execution::runtime_identity::ValidatedImmutablePlan::patched_bytes`],
    /// injected into the shadow `eth_call`'s state override), while the gas profile is
    /// built from [`mainnet_verified_identity`] — the same already-pinned constant
    /// `pinned_config`'s manifest was built from, with no redundant re-verification.
    pub fn new<P: Provider + Clone + 'static>(
        provider: P,
        pinned_config: ShadowPinnedConfig,
        profile_config: RuntimeProfileConfig,
        block_fee_contexts: Arc<BlockFeeContextCache>,
        executor_config: ExecutorConfig,
        ledger: ShadowLedgerSetup<'_>,
    ) -> Result<Self, ShadowContextError> {
        let target = pinned_config.target;
        let wmnt_drift_reason =
            wmnt_descriptor::check_wmnt_balance_slot_drift(&pinned_config.wmnt_descriptor)
                .err()
                .map(|error| error.to_string());

        let plan = resolve_immutable_plan(
            &pinned_config.evidence,
            ImmutableInputs {
                wmnt: target.wmnt_address,
            },
            executor_config.chain_id,
        )?;

        let gas_profile = RuntimeGasProfile::from_artifact_with_identity(
            pinned_config.gas_profile_artifact.clone(),
            profile_config,
            mainnet_verified_identity(),
        )?;

        let context = ExecutionContext {
            provider: provider.erased(),
            executor_contract: target.executor_contract,
            wmnt_address: target.wmnt_address,
            gas_profile,
            block_fee_contexts,
        };

        let metadata = RunMetadata {
            run_id: fresh_run_id(),
            git_commit: env!("GIT_COMMIT_HASH").to_string(),
            chain_id: executor_config.chain_id,
            service: ledger.service.to_string(),
            executor_contract: target.executor_contract,
            wmnt_address: target.wmnt_address,
            started_at_unix: ledger.started_at_unix,
        };
        let header = LedgerRunHeader::from_manifest(pinned_config.manifest(), metadata);
        let ledger = Arc::new(ShadowLedgerWriter::open(ledger.path, header)?);

        Ok(Self {
            context,
            executor_config,
            storage_layout: pinned_config.evidence.storage_layout().clone(),
            patched_runtime: plan.patched_bytes().to_vec(),
            wmnt_drift_reason,
            pinned_config,
            call_count: AtomicU64::new(0),
            ledger,
        })
    }

    pub fn manifest(&self) -> &ShadowOverrideManifest {
        self.pinned_config.manifest()
    }

    /// The executor config this context was built with — needed by call sites (e.g.
    /// [`crate::execution::pipeline::run_pipeline_head_closed`] wiring) that must read
    /// fee-policy fields (`default_priority_fee_wei`, `block_gas_limit_reserve`,
    /// `execution_deadline_secs`) without an owned `Executor` to read them from.
    pub fn config(&self) -> &ExecutorConfig {
        &self.executor_config
    }

    /// Builds one candidate's semantic preflight: a [`RiskTieredPreflight`] wired to a
    /// fresh [`ShadowSemanticCallExecutor`] carrying this candidate's `StateOverride` and
    /// baked-in pool provenance, sharing the one underlying ledger file across every
    /// candidate's attempt via a cloned `Arc<ShadowLedgerWriter>`. Always constructed
    /// with `ExecutionStage::Shadow` and no approval config, since Shadow never consults
    /// one.
    ///
    /// `async` because [`check_route_provenance`] independently confirms each hop's
    /// on-chain token getters (and, for Moe LB, its pinned runtime codehash) via the
    /// shared `DynProvider` — the CREATE2/allowlist checks alone only establish that the
    /// *address* is registration-authorized, not that the live contract at that address
    /// still reports the claimed token identity.
    pub async fn build_preflight(
        &self,
        inputs: &ShadowOverrideInputs,
    ) -> Result<
        RiskTieredPreflight<
            ShadowSemanticCallExecutor<DynProvider>,
            ShadowInvariantSink<Arc<ShadowLedgerWriter>>,
        >,
        ShadowContextError,
    > {
        self.recheck_manifest_at_batch_boundary()?;
        let state_override = build_shadow_state_override(
            self.context.wmnt_address(),
            &self.pinned_config.wmnt_descriptor.storage_shape,
            &self.storage_layout,
            &self.patched_runtime,
            inputs,
        )?;
        let provider = self.context.provider();
        let hop_outcomes = match &self.wmnt_drift_reason {
            Some(reason) => {
                vec![PoolProvenanceOutcome::Rejected(format!(
                    "wmnt descriptor drift: {reason}"
                ))]
            }
            None => {
                check_route_provenance(
                    &inputs.pools,
                    &self.pinned_config.moe_allowlist,
                    &self.pinned_config.approved_pools,
                    &provider,
                )
                .await
            }
        };
        let provenance = combine_provenance_outcomes(hop_outcomes.clone());

        let call_executor = ShadowSemanticCallExecutor::new(
            provider,
            state_override,
            Arc::clone(&self.ledger),
            provenance,
            hop_outcomes,
            ShadowRouteSummary::of(inputs),
            self.capability(),
        );
        Ok(RiskTieredPreflight::with_sink(
            call_executor,
            ShadowInvariantSink::new(Arc::clone(&self.ledger)),
            ExecutionStage::Shadow,
            None,
        ))
    }

    pub fn ledger(&self) -> &Arc<ShadowLedgerWriter> {
        &self.ledger
    }

    /// Records an accepted canonical block before discovery so evidence stays
    /// meaningful even when the block produces no executable candidate.
    pub fn record_canonical_observation(
        &self,
        snapshot_id: SnapshotId,
        header: BlockHeaderContext,
    ) -> Result<(), ShadowContextError> {
        self.ledger
            .record_canonical_observation(snapshot_id, header)?;
        Ok(())
    }

    /// Records a production-send-gate-blocked attempt as a shadow-ledger
    /// `candidate` row (WHI-739).
    ///
    /// The merged multi-protocol bot's one-shot path still terminates at the
    /// production-send gate while production send remains hard-false. That
    /// typed success is exactly the evidence a signerless shadow run collects:
    /// do not drop it. No `eth_call` is issued (there is no final request yet);
    /// the digest is domain-separated over the opportunity identity so rows
    /// stay reproducible across re-runs of the same candidate.
    pub fn record_production_gate_blocked(
        &self,
        opportunity_signature: &str,
        amount_in: U256,
        min_profit: U256,
    ) -> Result<(), ShadowContextError> {
        let digest = gate_blocked_digest(opportunity_signature, amount_in, min_profit);
        let detail = format!(
            "production_gate_blocked amount_in={amount_in} min_profit={min_profit} signature={opportunity_signature}"
        );
        self.ledger.record(PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            // No semantic call was attempted — same "no block_tag / latency" shape as
            // SampledOut / SkippedApproved, but EnvUnsupported keeps gate evaluate from
            // treating this scaffold row as a real pass/revert sample.
            outcome: PreflightOutcome::EnvUnsupported,
            digest,
            block_tag: None,
            latency: None,
            detail: Some(detail),
        });
        if let Some(failure) = self.ledger.failure() {
            return Err(ShadowContextError::Ledger(LedgerError::Io(failure)));
        }
        Ok(())
    }

    /// Proof that this call site is running in shadow mode — see [`NoSend`].
    pub fn capability(&self) -> NoSend {
        NoSend(())
    }

    /// Every [`MANIFEST_RECHECK_BATCH_SIZE`]th call, re-derives the manifest from disk and
    /// compares it against the one pinned at construction. `&self`, not `&mut self` — the
    /// counter is an `AtomicU64` purely so this can sit behind `build_preflight`'s shared
    /// reference.
    fn recheck_manifest_at_batch_boundary(&self) -> Result<(), ShadowContextError> {
        let count = self.call_count.fetch_add(1, Ordering::Relaxed) + 1;
        if !count.is_multiple_of(MANIFEST_RECHECK_BATCH_SIZE) {
            return Ok(());
        }
        self.recheck_manifest()
    }

    /// Re-reads every config source through [`ShadowPinnedConfig::reload`] — literally
    /// the same load-and-digest code startup ran, against the same paths and target — and
    /// compares the fresh manifest against the pinned one.
    fn recheck_manifest(&self) -> Result<(), ShadowContextError> {
        let fresh = self.pinned_config.reload()?;
        if self.pinned_config.manifest().matches(fresh.manifest()) {
            Ok(())
        } else {
            Err(ShadowContextError::ManifestDrift)
        }
    }
}

impl ExecutionContextView for ShadowExecutionContext {
    fn executor_contract(&self) -> Address {
        self.context.executor_contract()
    }

    fn wmnt_address(&self) -> Address {
        self.context.wmnt_address()
    }

    fn gas_profile(&self) -> &RuntimeGasProfile {
        self.context.gas_profile()
    }

    fn block_fee_contexts(&self) -> &BlockFeeContextCache {
        self.context.block_fee_contexts()
    }
}

impl ExecutionRequestBuilder for ShadowExecutionContext {
    fn build_final_request(
        &self,
        params: FinalRequestParams,
        permit: ExecutionPermit,
    ) -> eyre::Result<FinalRequest> {
        build_final_request_impl(&self.context, &self.executor_config, params, permit)
    }

    fn revalidate_final_request(&self, request: &FinalRequest) -> eyre::Result<()> {
        revalidate_final_request_impl(&self.context, request)
    }
}

/// Zero-sized proof that a call site holds no [`alloy::network::EthereumWallet`] and
/// therefore cannot reach [`crate::execution::executor::Executor::sign_final_request`]/
/// [`crate::execution::executor::Executor::prepare_submission`] — the only two `Executor`
/// methods that turn a [`FinalRequest`] into a signed, broadcastable transaction, and both
/// require a `&EthereumWallet` argument that shadow mode never constructs (no service ever
/// reads a private-key env var on the shadow branch of `main()`). This mirrors
/// `pipeline::{NoopPreflight, NoopDurableHook}`'s pattern of encoding a guarantee in the
/// type used rather than a runtime flag: the private tuple field means the only way to
/// obtain a `NoSend` is [`ShadowExecutionContext::capability`], so a `NoSend` argument in a
/// function signature is a compile-time witness that the caller is on the shadow path, not
/// the production one.
///
/// ```compile_fail
/// use amms::execution::NoSend;
/// fn forge() -> NoSend {
///     // The tuple field is private to this module: no external caller can construct one.
///     NoSend(())
/// }
/// ```
#[derive(Debug, Clone, Copy)]
pub struct NoSend(());

#[cfg(test)]
pub(crate) fn test_capability() -> NoSend {
    NoSend(())
}

#[cfg(test)]
mod capability_tests {
    use super::*;

    #[test]
    fn no_send_is_zero_sized_and_copy() {
        assert_eq!(std::mem::size_of::<NoSend>(), 0);
        let token = NoSend(());
        let _copy = token;
        let _original_still_usable = token;
    }
}

#[cfg(test)]
mod recheck_manifest_tests {
    use std::fs;

    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;

    use super::*;

    /// Copies the six checked-in mainnet shadow config sources into a fresh temp dir
    /// (so a test can mutate one without touching the real checked-in files) and builds
    /// a [`ShadowExecutionContext`] pinned against those copies via [`ShadowConfigPaths`].
    /// `new` issues zero RPC calls, so the mocked provider never needs a queued response.
    fn build_test_context(config_dir: &Path) -> ShadowExecutionContext {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let artifact_dir = config_dir.join("artifacts");
        copy_dir(
            &manifest_dir.join("contracts/executor/artifacts"),
            &artifact_dir,
        );

        let copy_fixture = |source: &str, name: &str| -> PathBuf {
            let destination = config_dir.join(name);
            fs::copy(manifest_dir.join(source), &destination)
                .unwrap_or_else(|error| panic!("{source} fixture must copy: {error}"));
            destination
        };

        let config_paths = ShadowConfigPaths {
            artifact_dir,
            wmnt_descriptor_path: copy_fixture(
                "config/gas_profiles/wmnt_descriptor.mantle_mainnet.json",
                "wmnt_descriptor.json",
            ),
            moe_allowlist_path: copy_fixture(
                "config/gas_profiles/moe_allowlist.mantle_mainnet.json",
                "moe_allowlist.json",
            ),
            approved_pools_path: copy_fixture(
                "config/gas_profiles/approved_pools.mantle_mainnet.json",
                "approved_pools.json",
            ),
            threshold_config_path: copy_fixture(
                "config/gas_profiles/shadow_thresholds_evidence.example.json",
                "shadow_thresholds.json",
            ),
            gas_profile_artifact_path: copy_fixture(
                "config/gas_profiles/mantle_mainnet_v1.json",
                "gas_profile.json",
            ),
        };

        let identity_json: Value = serde_json::from_str(
            &fs::read_to_string(manifest_dir.join("config/executor_identity.json"))
                .expect("checked-in executor identity export must exist"),
        )
        .expect("executor identity export must be valid JSON");
        let target = ShadowOverrideTarget {
            executor_contract: Address::repeat_byte(0xE0),
            wmnt_address: identity_json["wmnt"]
                .as_str()
                .expect("identity export must record the wmnt immutable")
                .parse()
                .expect("identity export wmnt must be a valid address"),
        };

        let pinned_config = ShadowPinnedConfig::load(config_paths, target)
            .expect("pinned config must load from the copied fixtures");

        let provider = ProviderBuilder::new()
            .connect_mocked_client(Asserter::new())
            .erased();
        let ledger_path = config_dir.join("shadow.jsonl");

        ShadowExecutionContext::new(
            provider,
            pinned_config,
            RuntimeProfileConfig::mantle_mainnet(Vec::new()),
            Arc::new(BlockFeeContextCache::default()),
            ExecutorConfig::default(),
            ShadowLedgerSetup {
                path: &ledger_path,
                started_at_unix: 1_700_000_000,
                service: "test-service",
            },
        )
        .expect("context must build from the copied fixtures")
    }

    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("artifact dir copy target must be creatable");
        for entry in fs::read_dir(src).expect("source artifact dir must be readable") {
            let entry = entry.expect("artifact dir entry must be readable");
            let dest_path = dst.join(entry.file_name());
            if entry
                .file_type()
                .expect("file type must be readable")
                .is_dir()
            {
                copy_dir(&entry.path(), &dest_path);
            } else {
                fs::copy(entry.path(), &dest_path).expect("artifact file must copy");
            }
        }
    }

    #[test]
    fn recheck_manifest_passes_when_the_on_disk_config_is_unchanged() {
        let config_dir = tempfile::tempdir().expect("config temp dir must be creatable");
        let context = build_test_context(config_dir.path());

        for _ in 0..MANIFEST_RECHECK_BATCH_SIZE {
            context
                .recheck_manifest_at_batch_boundary()
                .expect("an unmodified config generation must never report drift");
        }
    }

    #[test]
    fn recheck_manifest_at_batch_boundary_only_reloads_every_nth_call() {
        let config_dir = tempfile::tempdir().expect("config temp dir must be creatable");
        let context = build_test_context(config_dir.path());

        fs::write(
            config_dir.path().join("moe_allowlist.json"),
            b"not valid json",
        )
        .expect("moe allowlist fixture must be overwritable");

        for _ in 0..(MANIFEST_RECHECK_BATCH_SIZE - 1) {
            context
                .recheck_manifest_at_batch_boundary()
                .expect("calls before the batch boundary must not reload the mutated file");
        }

        let err = context
            .recheck_manifest_at_batch_boundary()
            .expect_err("the batch-boundary call must reload and reject the mutated allowlist");
        assert!(matches!(err, ShadowContextError::MoeAllowlist(_)));
    }

    #[test]
    fn recheck_manifest_reports_drift_when_the_on_disk_config_changes() {
        let config_dir = tempfile::tempdir().expect("config temp dir must be creatable");
        let context = build_test_context(config_dir.path());

        let allowlist_path = config_dir.path().join("moe_allowlist.json");
        let mut allowlist: Value = serde_json::from_str(
            &fs::read_to_string(&allowlist_path).expect("allowlist fixture must be readable"),
        )
        .expect("allowlist fixture must be valid JSON");
        allowlist["entries"]
            .as_array_mut()
            .expect("allowlist fixture must have an entries array")
            .push(serde_json::json!({
                "pool": format!("0x{:040x}", 1),
                "token_x": format!("0x{:040x}", 2),
                "token_y": format!("0x{:040x}", 3),
                "bin_step": 25,
                "runtime_codehash": format!("0x{:064x}", 4),
            }));
        fs::write(
            &allowlist_path,
            serde_json::to_string(&allowlist).expect("mutated allowlist must serialize"),
        )
        .expect("mutated allowlist must write back");

        let err = context
            .recheck_manifest()
            .expect_err("a mid-run allowlist edit must be detected as manifest drift");
        assert!(matches!(err, ShadowContextError::ManifestDrift));
    }

    #[test]
    fn record_production_gate_blocked_appends_candidate_row() {
        use alloy::primitives::U256;

        let config_dir = tempfile::tempdir().expect("config temp dir must be creatable");
        let context = build_test_context(config_dir.path());
        let ledger_path = config_dir.path().join("shadow.jsonl");

        context
            .record_production_gate_blocked("sig:v2+moe", U256::from(100u64), U256::from(5u64))
            .expect("gate-blocked row must append");

        let content = fs::read_to_string(&ledger_path).expect("ledger must be readable");
        let rows: Vec<serde_json::Value> = content
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("ledger line is JSON"))
            .collect();
        assert_eq!(rows.len(), 2, "run_header + candidate");
        assert_eq!(rows[0]["row_type"], "run_header");
        assert_eq!(rows[1]["row_type"], "candidate");
        assert_eq!(rows[1]["outcome"]["kind"], "env_unsupported");
        let detail = rows[1]["detail"].as_str().expect("detail present");
        assert!(
            detail.contains("production_gate_blocked"),
            "detail must name the gate-block outcome: {detail}"
        );
        assert!(detail.contains("sig:v2+moe"), "detail must carry signature");
    }
}
