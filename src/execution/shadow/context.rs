//! `ShadowExecutionContext` — signerless shadow mode's wallet-free, RPC-free execution
//! context: wraps a plain [`ExecutionContext`] (built via a direct struct literal, never
//! [`ExecutionContext::from_provider`]'s live chain-id/codehash/WMNT RPC checks) plus the
//! override engine's approved-registration/allowlist/storage-layout config and the ledger.
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

use alloy::primitives::Address;
use alloy::providers::{DynProvider, Provider};
use serde_json::Value;

use crate::execution::executor::{build_final_request_impl, revalidate_final_request_impl};
use crate::execution::fee_context::BlockFeeContextCache;
use crate::execution::final_request::{FinalRequest, FinalRequestParams};
use crate::execution::gas_profile::GasProfileArtifact;
use crate::execution::gas_runtime::{
    mainnet_verified_identity, RuntimeGasProfile, RuntimeGasProfileError, RuntimeProfileConfig,
};
use crate::execution::pipeline::ExecutionRequestBuilder;
use crate::execution::preflight::{ExecutionStage, RiskTieredPreflight};
use crate::execution::runtime_identity::{
    resolve_immutable_plan, BuildEvidence, ImmutableInputs, RuntimeIdentityError,
    VerifiedRuntimeIdentity,
};
use crate::execution::types::{ExecutionContext, ExecutionContextView, ExecutionPermit, ExecutorConfig};

use super::approved_pools::{self, ApprovedPoolsConfig, ApprovedPoolsError};
use super::call_executor::ShadowSemanticCallExecutor;
use super::invariant::ShadowInvariantSink;
use super::ledger::{LedgerError, LedgerRunHeader, ShadowLedgerWriter};
use super::manifest::{ManifestError, ShadowOverrideManifest};
use super::moe_allowlist::{self, MoeAllowlist, MoeAllowlistError};
use super::overrides::{build_shadow_state_override, check_route_provenance, ShadowOverrideInputs};
use super::slots::SlotsError;
use super::thresholds::{self, ThresholdError};
use super::wmnt_descriptor::{self, WmntDescriptor, WmntDescriptorError};

/// Every batch boundary (see [`MANIFEST_RECHECK_BATCH_SIZE`]), [`ShadowExecutionContext`]
/// re-reads these same five sources from disk and re-derives a fresh
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
}

/// How many [`ShadowExecutionContext::build_preflight`] calls make up one batch: every
/// `MANIFEST_RECHECK_BATCH_SIZE`th call re-reads every config source from disk and aborts
/// (via [`ShadowContextError::ManifestDrift`]) if any of them no longer matches the
/// manifest pinned at construction — catching a mid-run rewrite of the allowlist,
/// approved-pools, WMNT descriptor, threshold, or build-evidence files.
const MANIFEST_RECHECK_BATCH_SIZE: u64 = 100;

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
    #[error("shadow manifest: {0}")]
    Manifest(#[from] ManifestError),
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
/// `Executor`. Holds an [`ExecutionContext`] directly, and the override engine's inputs
/// (storage layout, patched runtime bytes, WMNT descriptor, Moe allowlist, approved
/// CREATE2 registrations, the pinned config-generation manifest) plus the ledger.
pub struct ShadowExecutionContext {
    context: ExecutionContext,
    config: ExecutorConfig,
    storage_layout: Value,
    patched_runtime: Vec<u8>,
    wmnt_descriptor: WmntDescriptor,
    manifest: ShadowOverrideManifest,
    moe_allowlist: MoeAllowlist,
    approved_pools: ApprovedPoolsConfig,
    config_paths: ShadowConfigPaths,
    call_count: AtomicU64,
    ledger: Arc<ShadowLedgerWriter>,
}

impl ShadowExecutionContext {
    /// Builds a shadow context with **zero RPC calls**: `provider` is erased and stored
    /// for `eth_call` use only, never queried for chain id / deployed code / a live WMNT
    /// read (that's `ExecutionContext::from_provider`'s job for the production path).
    /// Instead, the executor's patched runtime bytes come from re-deriving the immutable
    /// plan from `evidence` (for
    /// [`crate::execution::runtime_identity::ValidatedImmutablePlan::patched_bytes`],
    /// injected into the shadow `eth_call`'s state override), while the gas profile is
    /// built from the caller-supplied, already-pinned `verified_identity` — matching what
    /// `manifest` was built from, with no redundant re-verification.
    #[allow(clippy::too_many_arguments)]
    pub fn new<P: Provider + Clone + 'static>(
        provider: P,
        executor_contract: Address,
        wmnt_address: Address,
        artifact: GasProfileArtifact,
        profile_config: RuntimeProfileConfig,
        evidence: &BuildEvidence,
        verified_identity: &VerifiedRuntimeIdentity,
        block_fee_contexts: Arc<BlockFeeContextCache>,
        executor_config: ExecutorConfig,
        wmnt_descriptor: WmntDescriptor,
        manifest: ShadowOverrideManifest,
        moe_allowlist: MoeAllowlist,
        approved_pools: ApprovedPoolsConfig,
        config_paths: ShadowConfigPaths,
        ledger_path: &Path,
        started_at_unix: u64,
    ) -> Result<Self, ShadowContextError> {
        wmnt_descriptor::check_wmnt_balance_slot_drift(&wmnt_descriptor)?;

        let plan = resolve_immutable_plan(
            evidence,
            ImmutableInputs { wmnt: wmnt_address },
            executor_config.chain_id,
        )?;

        let gas_profile = RuntimeGasProfile::from_artifact_with_identity(
            artifact,
            profile_config,
            verified_identity,
        )?;

        let context = ExecutionContext {
            provider: provider.erased(),
            executor_contract,
            wmnt_address,
            gas_profile,
            block_fee_contexts,
        };

        let header = LedgerRunHeader::from_manifest(&manifest, started_at_unix);
        let ledger = Arc::new(ShadowLedgerWriter::open(ledger_path, header)?);

        Ok(Self {
            context,
            config: executor_config,
            storage_layout: evidence.storage_layout().clone(),
            patched_runtime: plan.patched_bytes().to_vec(),
            wmnt_descriptor,
            manifest,
            moe_allowlist,
            approved_pools,
            config_paths,
            call_count: AtomicU64::new(0),
            ledger,
        })
    }

    pub fn manifest(&self) -> &ShadowOverrideManifest {
        &self.manifest
    }

    /// The executor config this context was built with — needed by call sites (e.g.
    /// [`crate::execution::pipeline::run_pipeline_head_closed`] wiring) that must read
    /// fee-policy fields (`default_priority_fee_wei`, `block_gas_limit_reserve`,
    /// `execution_deadline_secs`) without an owned `Executor` to read them from.
    pub fn config(&self) -> &ExecutorConfig {
        &self.config
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
        RiskTieredPreflight<ShadowSemanticCallExecutor<DynProvider>, ShadowInvariantSink<Arc<ShadowLedgerWriter>>>,
        ShadowContextError,
    > {
        self.recheck_manifest_at_batch_boundary()?;
        let state_override = build_shadow_state_override(
            self.context.wmnt_address(),
            self.wmnt_descriptor.storage_shape,
            &self.storage_layout,
            &self.patched_runtime,
            inputs,
        )?;
        let provider = self.context.provider();
        let provenance = check_route_provenance(
            &inputs.pools,
            &self.moe_allowlist,
            &self.approved_pools,
            &provider,
        )
        .await;
        let call_executor = ShadowSemanticCallExecutor::new(
            provider,
            state_override,
            Arc::clone(&self.ledger),
            provenance,
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
        if count % MANIFEST_RECHECK_BATCH_SIZE != 0 {
            return Ok(());
        }
        self.recheck_manifest()
    }

    /// Reloads every config source [`ShadowOverrideManifest::new`] was built from and
    /// compares the fresh result against `self.manifest`. Uses
    /// [`mainnet_verified_identity`] for the identity input, exactly as the construction-time
    /// call site does (`intent_service_support.rs::build_shadow_execution_context`) — that
    /// value is a compile-time-embedded constant, not itself re-derived from disk, so a
    /// mid-run edit to the build evidence is instead caught via `storage_layout_digest`.
    fn recheck_manifest(&self) -> Result<(), ShadowContextError> {
        let evidence = BuildEvidence::load(&self.config_paths.artifact_dir)?;
        let wmnt_descriptor = wmnt_descriptor::load_wmnt_descriptor(&self.config_paths.wmnt_descriptor_path)?;
        let moe_allowlist = moe_allowlist::load_moe_allowlist(&self.config_paths.moe_allowlist_path)?;
        let approved_pools = approved_pools::load_approved_pools(&self.config_paths.approved_pools_path)?;
        let threshold_bytes = thresholds::load_threshold_bytes(&self.config_paths.threshold_config_path)?;

        let fresh_manifest = ShadowOverrideManifest::new(
            &evidence,
            &wmnt_descriptor,
            &moe_allowlist,
            mainnet_verified_identity(),
            &approved_pools,
            &threshold_bytes,
        )?;

        if self.manifest.matches(&fresh_manifest) {
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
        build_final_request_impl(&self.context, &self.config, params, permit)
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

    use crate::execution::gas_profile::load_artifact;
    use crate::execution::types::ExecutorConfig;

    use super::*;

    /// Copies the five checked-in mainnet shadow config sources into a fresh temp dir
    /// (so a test can mutate one without touching the real checked-in files) and builds
    /// a [`ShadowExecutionContext`] pinned against those copies via [`ShadowConfigPaths`].
    /// `new` issues zero RPC calls, so the mocked provider never needs a queued response.
    fn build_test_context(config_dir: &Path) -> ShadowExecutionContext {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

        let source_artifact_dir = manifest_dir.join("contracts/executor/artifacts");
        let artifact_dir = config_dir.join("artifacts");
        copy_dir(&source_artifact_dir, &artifact_dir);

        let wmnt_descriptor_path = config_dir.join("wmnt_descriptor.json");
        fs::copy(
            manifest_dir.join("config/gas_profiles/wmnt_descriptor.mantle_mainnet.json"),
            &wmnt_descriptor_path,
        )
        .expect("wmnt descriptor fixture must copy");

        let moe_allowlist_path = config_dir.join("moe_allowlist.json");
        fs::copy(
            manifest_dir.join("config/gas_profiles/moe_allowlist.mantle_mainnet.json"),
            &moe_allowlist_path,
        )
        .expect("moe allowlist fixture must copy");

        let approved_pools_path = config_dir.join("approved_pools.json");
        fs::copy(
            manifest_dir.join("config/gas_profiles/approved_pools.mantle_mainnet.json"),
            &approved_pools_path,
        )
        .expect("approved pools fixture must copy");

        let threshold_config_path = config_dir.join("shadow_thresholds.json");
        fs::copy(
            manifest_dir.join("config/gas_profiles/shadow_thresholds.mantle_mainnet.json"),
            &threshold_config_path,
        )
        .expect("threshold config fixture must copy");

        let evidence = BuildEvidence::load(&artifact_dir).expect("build evidence must load");
        let wmnt_descriptor = wmnt_descriptor::load_wmnt_descriptor(&wmnt_descriptor_path)
            .expect("wmnt descriptor must load");
        let moe_allowlist =
            moe_allowlist::load_moe_allowlist(&moe_allowlist_path).expect("moe allowlist must load");
        let approved_pools = approved_pools::load_approved_pools(&approved_pools_path)
            .expect("approved pools must load");
        let threshold_bytes = thresholds::load_threshold_bytes(&threshold_config_path)
            .expect("threshold bytes must load");
        let identity = mainnet_verified_identity();

        let manifest = ShadowOverrideManifest::new(
            &evidence,
            &wmnt_descriptor,
            &moe_allowlist,
            identity,
            &approved_pools,
            &threshold_bytes,
        )
        .expect("manifest must build from the copied fixtures");

        let identity_json: Value = serde_json::from_str(
            &fs::read_to_string(manifest_dir.join("config/executor_identity.json"))
                .expect("checked-in executor identity export must exist"),
        )
        .expect("executor identity export must be valid JSON");
        let wmnt_address: Address = identity_json["wmnt"]
            .as_str()
            .expect("identity export must record the wmnt immutable")
            .parse()
            .expect("identity export wmnt must be a valid address");

        let profile_path = manifest_dir.join("config/gas_profiles/mantle_mainnet_v1.json");
        let artifact = load_artifact(&profile_path).expect("gas profile artifact must load");
        let profile_config = RuntimeProfileConfig::mantle_mainnet(Vec::new());

        let provider = ProviderBuilder::new()
            .connect_mocked_client(Asserter::new())
            .erased();
        let block_fee_contexts = Arc::new(BlockFeeContextCache::default());

        let ledger_path = config_dir.join("shadow.jsonl");

        let config_paths = ShadowConfigPaths {
            artifact_dir,
            wmnt_descriptor_path,
            moe_allowlist_path,
            approved_pools_path,
            threshold_config_path,
        };

        ShadowExecutionContext::new(
            provider,
            Address::repeat_byte(0xE0),
            wmnt_address,
            artifact,
            profile_config,
            &evidence,
            identity,
            block_fee_contexts,
            ExecutorConfig::default(),
            wmnt_descriptor,
            manifest,
            moe_allowlist,
            approved_pools,
            config_paths,
            &ledger_path,
            1_700_000_000,
        )
        .expect("context must build from the copied fixtures")
    }

    fn copy_dir(src: &Path, dst: &Path) {
        fs::create_dir_all(dst).expect("artifact dir copy target must be creatable");
        for entry in fs::read_dir(src).expect("source artifact dir must be readable") {
            let entry = entry.expect("artifact dir entry must be readable");
            let dest_path = dst.join(entry.file_name());
            if entry.file_type().expect("file type must be readable").is_dir() {
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

        fs::write(config_dir.path().join("moe_allowlist.json"), b"not valid json")
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
}
