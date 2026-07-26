//! WHI-521: risk-tiered exact-request semantic preflight.
//!
//! Implements [`PreflightSlot`](super::pipeline::PreflightSlot) (defined in
//! `pipeline.rs`) with a policy that performs **at most one** semantic `eth_call` on the
//! exact [`FinalRequest`] bytes: exactly one for the `Mandatory` tier (e2e/shadow/canary,
//! or production without a valid approval), zero only for a valid signed
//! `ApprovedStable` production approval record, and never any `eth_estimateGas`. This
//! module owns policy classification and attempt recording; it never validates identity
//! (that is [`super::identity`]'s job, invoked before and after this slot) and never
//! caches an outcome across invocations.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::eips::BlockId;
use alloy::providers::Provider;
use alloy::transports::{RpcError, TransportErrorKind};
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

use super::final_request::{final_request_digest, FinalRequest, FinalRequestDigest};
use super::pipeline::PreflightSlot;
use crate::signing::{self, ExpectedScope, SigningError, VerifiedArtifact};

/// Compile-time domain for signed preflight-approval records (WHI-552 scheme).
pub const PREFLIGHT_APPROVAL_DOMAIN: &str = "whisker-arb/preflight-approval/v1";

/// Deployment stage a candidate is executing under. Only `Production` may ever consult
/// an `ApprovedStable` approval record; every other stage is unconditionally
/// `Mandatory`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionStage {
    E2e,
    Shadow,
    Canary,
    Production,
}

/// Block tag a semantic call was (or would be) issued against. `Pending` is only ever
/// used when the caller has separately recorded that the RPC endpoint supports it
/// ([`PendingCapability::Supported`]) -- this module never performs its own capability
/// probe, and defaults to `Latest` otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockTag {
    Latest,
    Pending,
}

/// Whether the configured RPC endpoint is known to support the `pending` block tag.
/// Recorded once out-of-band (e.g. at service startup); this module only consults it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingCapability {
    #[default]
    Unsupported,
    Supported,
}

/// Which policy branch produced a [`PreflightAttempt`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyKey {
    Mandatory,
    ApprovedStableDisabled,
    ApprovedStableSampled,
}

/// Coarse class of a non-revert RPC failure, kept separate from [`PreflightOutcome`]'s
/// `Revert` case so a transport/timeout failure is never confused with an on-chain
/// revert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RpcErrorClass {
    Transport,
    ErrorResponse,
    Other,
}

/// Terminal result of one preflight attempt. `EnvUnsupported` is a first-class outcome
/// distinct from `Pass` -- a [`SemanticCallExecutor`] that cannot serve the call (e.g. a
/// future state-override implementation lacking a capability) must report it here
/// rather than let the slot treat an unsupported environment as a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightOutcome {
    Pass,
    Revert(String),
    RpcError(RpcErrorClass),
    EnvUnsupported,
    SkippedApproved,
    SampledOut,
}

/// One recorded preflight attempt. `block_tag`/`latency` are `None` exactly when no
/// semantic call was attempted (`SkippedApproved`/`SampledOut`).
#[derive(Debug, Clone)]
pub struct PreflightAttempt {
    pub policy_key: PolicyKey,
    pub outcome: PreflightOutcome,
    pub digest: FinalRequestDigest,
    pub block_tag: Option<BlockTag>,
    pub latency: Option<Duration>,
    /// Extra human-readable detail (RPC error message / EnvUnsupported reason) that
    /// doesn't fit the outcome enum's own shape. Never used for control flow.
    pub detail: Option<String>,
}

/// Sink for recorded preflight attempts. WHI-549's shadow ledger is the intended
/// production consumer; this module only defines and exercises the seam.
pub trait PreflightAttemptSink: Send + Sync {
    fn record(&self, attempt: PreflightAttempt);
}

/// Default sink: logs at `tracing::info!` under target `execution.preflight`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingPreflightAttemptSink;

impl PreflightAttemptSink for TracingPreflightAttemptSink {
    fn record(&self, attempt: PreflightAttempt) {
        tracing::info!(
            target: "execution.preflight",
            policy_key = ?attempt.policy_key,
            outcome = ?attempt.outcome,
            digest = ?attempt.digest.0,
            block_tag = ?attempt.block_tag,
            latency_ms = attempt.latency.map(|d| d.as_millis()),
            detail = attempt.detail.as_deref(),
            "preflight attempt"
        );
    }
}

/// Outcome of one semantic call, before policy interpretation.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    Success,
    Revert(String),
    EnvUnsupported(String),
}

/// Failure to obtain any [`CallOutcome`] at all (transport/JSON-RPC failure, as opposed
/// to a revert -- which is itself a successful RPC round trip and is a `CallOutcome`).
#[derive(Debug, Clone, thiserror::Error)]
pub enum SemanticCallError {
    #[error("semantic call RPC failure ({class:?}): {message}")]
    Rpc { class: RpcErrorClass, message: String },
}

/// Narrow seam for the one semantic call the risk-tiered policy may issue. Production
/// uses plain `eth_call` ([`ProviderSemanticCallExecutor`]); WHI-549 supplies a
/// state-override implementation. No implementation may change policy or
/// exactly-once counting -- that is entirely this module's responsibility.
#[allow(async_fn_in_trait)]
pub trait SemanticCallExecutor: Send + Sync {
    async fn call(&self, request: &FinalRequest, tag: BlockTag) -> Result<CallOutcome, SemanticCallError>;
}

/// Production [`SemanticCallExecutor`]: issues a plain `eth_call` against the exact
/// [`FinalRequest`] transaction bytes (never `eth_estimateGas`).
pub struct ProviderSemanticCallExecutor<P> {
    provider: P,
}

impl<P> ProviderSemanticCallExecutor<P> {
    pub fn new(provider: P) -> Self {
        Self { provider }
    }
}

impl<P: Provider + Send + Sync> SemanticCallExecutor for ProviderSemanticCallExecutor<P> {
    async fn call(&self, request: &FinalRequest, tag: BlockTag) -> Result<CallOutcome, SemanticCallError> {
        let block = match tag {
            BlockTag::Latest => BlockId::latest(),
            BlockTag::Pending => BlockId::pending(),
        };
        match self
            .provider
            .call(request.transaction.clone())
            .block(block)
            .await
        {
            Ok(_) => Ok(CallOutcome::Success),
            Err(err) => classify_call_error(err),
        }
    }
}

/// `code == 3` is the EIP-1474 "execution reverted" convention; a message containing
/// "revert" catches nodes that use a different code but still describe a revert.
/// Everything else is a genuine RPC failure, never conflated with a revert.
fn classify_call_error(
    err: RpcError<TransportErrorKind>,
) -> Result<CallOutcome, SemanticCallError> {
    if let Some(payload) = err.as_error_resp() {
        let message = payload.message.to_string();
        if payload.code == 3 || message.to_lowercase().contains("revert") {
            return Ok(CallOutcome::Revert(message));
        }
        return Err(SemanticCallError::Rpc {
            class: RpcErrorClass::ErrorResponse,
            message,
        });
    }
    if err.is_transport_error() {
        return Err(SemanticCallError::Rpc {
            class: RpcErrorClass::Transport,
            message: err.to_string(),
        });
    }
    Err(SemanticCallError::Rpc {
        class: RpcErrorClass::Other,
        message: err.to_string(),
    })
}

/// Runtime scope an `ApprovedStable` approval record must match field-for-field.
///
/// Digest derivation for `executor_identity_digest`/`config_digest`/`profile_digest` is
/// deliberately the caller's responsibility (e.g. service wiring or offline
/// approval-signing tooling) -- this module only defines the scope shape and enforces
/// fail-closed equality via [`crate::signing::ExpectedScope`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeScope {
    pub chain_id: u64,
    pub executor_identity_digest: String,
    pub config_digest: String,
    pub profile_digest: String,
}

impl RuntimeScope {
    pub fn to_expected_scope(&self) -> Result<ExpectedScope, SigningError> {
        ExpectedScope::new(serde_json::json!({
            "chain_id": self.chain_id.to_string(),
            "executor_identity_digest": self.executor_identity_digest,
            "config_digest": self.config_digest,
            "profile_digest": self.profile_digest,
        }))
    }
}

/// Wire form of an approval's operating mode. `Sampled` always pairs with
/// [`PreflightApprovalPayload::sample_rate_bps`]; a `Sampled` payload missing or with an
/// out-of-range rate is treated as invalid (falls back to `Mandatory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Disabled,
    Sampled,
}

/// WHI-521's `ApprovedStable` approval payload -- the `T` in WHI-552's
/// `CanonicalEnvelope<T>`. Numeric fields are decimal strings per that module's
/// envelope conventions.
///
/// Adds one field beyond the issue's literal
/// `{ profile_key, approved_by, approved_at, valid_until, evidence_digest, mode }`
/// list: `sample_rate_bps`. Without it, a `mode = "sampled"` record has no rate to
/// sample against -- the issue names `sampled` as a valid `mode` value but doesn't
/// separately spell out where its rate lives, so it is carried here as an
/// (optional, mode-gated) payload field rather than inventing a second signed
/// artifact just to hold one number.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightApprovalPayload {
    pub profile_key: String,
    pub approved_by: String,
    /// Decimal-string unix seconds.
    pub approved_at: String,
    /// Decimal-string unix seconds. Expiry is re-checked against wall-clock time on
    /// every verification, not just at construction.
    pub valid_until: String,
    pub evidence_digest: String,
    pub mode: ApprovalMode,
    /// Decimal string, `0..=10000` (basis points). Required and validated only when
    /// `mode == Sampled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_rate_bps: Option<String>,
}

/// Validated, non-expired form of [`PreflightApprovalPayload::mode`].
enum ApprovedMode {
    Disabled,
    Sampled { rate_bps: u32 },
}

/// Parses and validates an already-signature/domain/scope-verified payload: checks
/// `valid_until` against wall-clock time and, for `Sampled`, parses+bounds-checks
/// `sample_rate_bps`. Returns `None` on any invalidity -- the caller must fall back to
/// `Mandatory`.
fn approved_mode(payload: &PreflightApprovalPayload) -> Option<ApprovedMode> {
    let valid_until: u64 = payload.valid_until.parse().ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now >= valid_until {
        return None;
    }
    match payload.mode {
        ApprovalMode::Disabled => Some(ApprovedMode::Disabled),
        ApprovalMode::Sampled => {
            let rate_bps: u32 = payload.sample_rate_bps.as_deref()?.parse().ok()?;
            if rate_bps > 10_000 {
                return None;
            }
            Some(ApprovedMode::Sampled { rate_bps })
        }
    }
}

/// Deterministic sampling from the final-request digest: the digest's first two bytes
/// (big-endian `u16`), rescaled to `0..=10000`, must fall below `rate_bps`. Reusing
/// [`final_request_digest`]'s output means sampling is reproducible from the same value
/// WHI-549's ledger rows and E2E permit integration consume.
fn sampled_in(digest: FinalRequestDigest, rate_bps: u32) -> bool {
    let hi = u16::from_be_bytes([digest.0[0], digest.0[1]]) as u32;
    let scaled = (hi * 10_000) / (u16::MAX as u32 + 1);
    scaled < rate_bps
}

/// A raw signed approval record, as produced by [`crate::signing::sign_envelope`].
#[derive(Debug, Clone)]
pub struct SignedApprovalRecord {
    pub payload_bytes: Vec<u8>,
    pub signature: Vec<u8>,
    pub principal: String,
}

/// Bundles a signed record with the scope/schema versions it must be verified against.
pub struct ApprovalConfig {
    pub record: SignedApprovalRecord,
    pub scope: RuntimeScope,
    pub accepted_schema_versions: Vec<String>,
}

/// Verification seam so tests can inject [`crate::signing::verify_with_paths`] (temp
/// fixture trust roots) instead of the production [`crate::signing::verify`] entrypoint,
/// mirroring how [`super::identity::ExecutionIdentitySource`] and
/// [`super::pipeline::ExecutionRequestBuilder`] are injected elsewhere in this crate.
pub trait ApprovalVerifier: Send + Sync {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<PreflightApprovalPayload>, SigningError>;
}

/// Production verifier: always resolves the code-constant, committed trust roots via
/// [`crate::signing::verify`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionApprovalVerifier;

impl ApprovalVerifier for ProductionApprovalVerifier {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<PreflightApprovalPayload>, SigningError> {
        signing::verify(
            payload_bytes,
            signature,
            PREFLIGHT_APPROVAL_DOMAIN,
            principal,
            accepted_schema_versions,
            expected_scope,
        )
    }
}

enum Policy {
    Mandatory,
    ApprovedStableDisabled,
    ApprovedStableSampledIn,
    ApprovedStableSampledOut,
}

/// Production [`PreflightSlot`] implementation: risk-tiered, zero-or-one semantic
/// `eth_call`, never `eth_estimateGas`.
pub struct RiskTieredPreflight<C, S = TracingPreflightAttemptSink> {
    call_executor: C,
    sink: S,
    stage: ExecutionStage,
    approval: Option<ApprovalConfig>,
    pending_capability: PendingCapability,
    verifier: Box<dyn ApprovalVerifier>,
}

impl<C: SemanticCallExecutor> RiskTieredPreflight<C, TracingPreflightAttemptSink> {
    /// `approval` is only ever consulted when `stage == Production`; every other stage
    /// is unconditionally `Mandatory` regardless of what (if anything) is passed here.
    pub fn new(call_executor: C, stage: ExecutionStage, approval: Option<ApprovalConfig>) -> Self {
        Self::with_sink(call_executor, TracingPreflightAttemptSink, stage, approval)
    }
}

impl<C: SemanticCallExecutor, S: PreflightAttemptSink> RiskTieredPreflight<C, S> {
    pub fn with_sink(
        call_executor: C,
        sink: S,
        stage: ExecutionStage,
        approval: Option<ApprovalConfig>,
    ) -> Self {
        Self {
            call_executor,
            sink,
            stage,
            approval,
            pending_capability: PendingCapability::Unsupported,
            verifier: Box::new(ProductionApprovalVerifier),
        }
    }

    /// Test/tooling-only seam: inject an [`ApprovalVerifier`] other than the production
    /// [`ProductionApprovalVerifier`] (e.g. one backed by
    /// [`crate::signing::verify_with_paths`] and temp fixture trust roots).
    ///
    /// Gated behind the `signing-test-util` feature for the same reason
    /// [`crate::signing::verify_with_paths`] is: `#[doc(hidden)]` alone is not access
    /// control, so letting production code call this would let it silently swap out the
    /// committed, code-constant trust roots [`ProductionApprovalVerifier`] resolves.
    /// Only test/bench/example builds enable this feature (via the self dev-dependency
    /// in `Cargo.toml`); it is not reachable from a normal downstream build.
    #[doc(hidden)]
    #[cfg(feature = "signing-test-util")]
    pub fn with_verifier(mut self, verifier: Box<dyn ApprovalVerifier>) -> Self {
        self.verifier = verifier;
        self
    }

    pub fn with_pending_capability(mut self, pending_capability: PendingCapability) -> Self {
        self.pending_capability = pending_capability;
        self
    }

    /// `stage != Production` is unconditionally `Mandatory`. `Production` re-verifies
    /// the configured approval record fresh on every call (signature, domain, schema,
    /// scope, and revocation are all re-checked -- OpenSSH reads `revoked_keys` live on
    /// every `ssh-keygen -Y verify` invocation) so a revoked or expired approval falls
    /// back to `Mandatory` within one candidate, never a stale cached decision.
    ///
    /// The issue's Mandatory tier also lists "any unmodeled venue/hook" alongside
    /// `stage != Production`. That branch is intentionally not modeled here: every
    /// `FinalRequest` reaching this slot already carries a [`super::gas_profile::RouteKey`]
    /// built from a closed [`super::gas_profile::ProtocolKind`] enum, and any
    /// unsupported/unqualified route is already rejected pre-slot by
    /// `RuntimeGasProfile::quote` (via `identity_source.validate`/
    /// `revalidate_final_request`, both called before and after this slot) --
    /// there is no live "unmodeled venue" signal left to check by the time a request
    /// gets here. This is the extension point a future protocol addition would need,
    /// not a gap in today's policy.
    fn classify(&self, digest: FinalRequestDigest) -> Policy {
        if self.stage != ExecutionStage::Production {
            return Policy::Mandatory;
        }
        let Some(approval) = &self.approval else {
            return Policy::Mandatory;
        };
        let Ok(expected_scope) = approval.scope.to_expected_scope() else {
            return Policy::Mandatory;
        };
        let accepted_schema_versions: Vec<&str> = approval
            .accepted_schema_versions
            .iter()
            .map(String::as_str)
            .collect();
        let verified = self.verifier.verify(
            &approval.record.payload_bytes,
            &approval.record.signature,
            &approval.record.principal,
            &accepted_schema_versions,
            &expected_scope,
        );
        let Ok(verified) = verified else {
            return Policy::Mandatory;
        };
        match approved_mode(verified.payload()) {
            None => Policy::Mandatory,
            Some(ApprovedMode::Disabled) => Policy::ApprovedStableDisabled,
            Some(ApprovedMode::Sampled { rate_bps }) => {
                if sampled_in(digest, rate_bps) {
                    Policy::ApprovedStableSampledIn
                } else {
                    Policy::ApprovedStableSampledOut
                }
            }
        }
    }

    /// Issues exactly one semantic call and records the attempt. `Ok(())` only for
    /// `Pass` -- every other outcome (`Revert`/`RpcError`/`EnvUnsupported`) is a
    /// rejection, never mistaken for a pass.
    async fn run_call(
        &self,
        request: &FinalRequest,
        digest: FinalRequestDigest,
        policy_key: PolicyKey,
    ) -> Result<()> {
        let tag = match self.pending_capability {
            PendingCapability::Supported => BlockTag::Pending,
            PendingCapability::Unsupported => BlockTag::Latest,
        };
        let started = Instant::now();
        let call_result = self.call_executor.call(request, tag).await;
        let latency = started.elapsed();

        let (outcome, detail) = match call_result {
            Ok(CallOutcome::Success) => (PreflightOutcome::Pass, None),
            Ok(CallOutcome::Revert(reason)) => {
                (PreflightOutcome::Revert(reason.clone()), Some(reason))
            }
            Ok(CallOutcome::EnvUnsupported(reason)) => {
                (PreflightOutcome::EnvUnsupported, Some(reason))
            }
            Err(SemanticCallError::Rpc { class, message }) => {
                (PreflightOutcome::RpcError(class), Some(message))
            }
        };

        let pass = outcome == PreflightOutcome::Pass;
        self.sink.record(PreflightAttempt {
            policy_key,
            outcome: outcome.clone(),
            digest,
            block_tag: Some(tag),
            latency: Some(latency),
            detail,
        });

        if pass {
            Ok(())
        } else {
            Err(eyre!("preflight rejected candidate: {outcome:?}"))
        }
    }

    fn record_skip(&self, digest: FinalRequestDigest, policy_key: PolicyKey, outcome: PreflightOutcome) {
        self.sink.record(PreflightAttempt {
            policy_key,
            outcome,
            digest,
            block_tag: None,
            latency: None,
            detail: None,
        });
    }
}

impl<C: SemanticCallExecutor, S: PreflightAttemptSink> PreflightSlot for RiskTieredPreflight<C, S> {
    async fn preflight(&self, request: &FinalRequest) -> Result<()> {
        let digest = final_request_digest(request)?;
        match self.classify(digest) {
            Policy::Mandatory => self.run_call(request, digest, PolicyKey::Mandatory).await,
            Policy::ApprovedStableSampledIn => {
                self.run_call(request, digest, PolicyKey::ApprovedStableSampled)
                    .await
            }
            Policy::ApprovedStableDisabled => {
                self.record_skip(
                    digest,
                    PolicyKey::ApprovedStableDisabled,
                    PreflightOutcome::SkippedApproved,
                );
                Ok(())
            }
            Policy::ApprovedStableSampledOut => {
                self.record_skip(
                    digest,
                    PolicyKey::ApprovedStableSampled,
                    PreflightOutcome::SampledOut,
                );
                Ok(())
            }
        }
    }
}
