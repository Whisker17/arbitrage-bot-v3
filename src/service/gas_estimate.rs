//! Discovery-only gas estimation for route classes without a measured profile
//! (WHI-1572).
//!
//! Signerless shadow discovery used to reject every candidate whose route class
//! had no `Approved` measured gas profile. This module prices those candidates
//! with a pinned, offline-fitted additive model instead, while the measured send
//! path stays exactly as it was:
//!
//! * An estimate never becomes a [`GasQuote`]; [`FeePolicy::build`] and
//!   `RuntimeGasProfile::quote` are not touched. An `Estimated` candidate is
//!   statically send-ineligible in every configuration
//!   ([`crate::service::eligibility`]).
//! * Only declared cases estimate: a structurally valid route absent from the
//!   measured profile (`Unknown`), or a static `Unsupported` entry whose audited
//!   withhold category in the estimator artifact is estimation-eligible. Runtime
//!   invalidation, `ResearchOnly`, a poisoned profile, a malformed key, an
//!   unmapped withhold, invalid estimates and arithmetic failures are rejected and
//!   never fall back ([`resolve_discovery_gas`]).
//! * The artifact is pinned by the keccak256 of its bytes, identity-checked
//!   against the loaded measured profile (chain, executor code hash, ABI digest,
//!   feature schema, measured-profile content digest) and immutable for the
//!   process lifetime.
//!
//! Net profit priced here is **L2-gas-only modeled net**: Mantle operator and L1
//! fees are not modeled (separate issue).

use crate::execution::{
    fee_plan_cost, BinCrossingBucket, BlockFeeContext, FeePlanError, FeePolicy, GasQuote,
    MeasuredRouteStatus, ProtocolKind, RouteKey, RuntimeGasProfile, TickCrossingBucket,
};
use crate::metrics::reject_reason;
use alloy::primitives::{keccak256, Address, U256};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

pub const ESTIMATOR_SCHEMA: &str = "whisker-arb/discovery-gas-estimator/v1";
pub const FEATURE_SCHEMA: &str =
    "additive-v1:intercept,n_v2,n_v3,n_moe,sum_v3_ticks,sum_moe_bins";
/// Repo-relative path of the committed mainnet estimator artifact.
pub const MAINNET_ESTIMATOR_REL_PATH: &str =
    "config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json";
/// keccak256 of the committed mainnet artifact's bytes (the pin). Regenerate with
/// `python3 scripts/gas_estimate/fit_discovery_estimator.py fit`.
pub const MAINNET_ESTIMATOR_DIGEST: &str =
    "0x3cc808afe0331f2f6c3d4a2f3f20013b10ff0dd10ba6fc88068d45a8c2d634a2";

const BPS: i128 = 10_000;

// ---------------------------------------------------------------------------
// Artifact
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdditiveModel {
    pub b0: i64,
    pub b_v2: i64,
    pub b_v3: i64,
    pub b_moe: i64,
    pub s_tick: i64,
    pub s_bin: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimateMargins {
    pub expected_multiplier_bps: u64,
    pub limit_multiplier_bps: u64,
    pub limit_overhead_gas: u64,
    /// Always `false`: these are new, unvalidated discovery-only parameters.
    pub validated: bool,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingEnvelope {
    pub hop_counts: Vec<u8>,
    pub max_v3_ticks_per_hop: u32,
    pub max_moe_bins_per_hop: u32,
    pub max_sum_v3_ticks: u32,
    pub max_sum_moe_bins: u32,
    pub v2_free_rows: u64,
    pub venue_factories: Vec<Address>,
}

/// Audited category of a static `Unsupported` measured entry (the measured
/// artifact records these only as prose). An unknown category string fails the
/// artifact load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WithholdCategory {
    InsufficientSamples,
    OpenEndedBucket,
    FailedLimitGate,
    VenueWithholdFactoryAxis,
    VenueWithholdVenueAxis,
    ScopeWithhold,
}

impl WithholdCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InsufficientSamples => "insufficient_samples",
            Self::OpenEndedBucket => "open_ended_bucket",
            Self::FailedLimitGate => "failed_limit_gate",
            Self::VenueWithholdFactoryAxis => "venue_withhold_factory_axis",
            Self::VenueWithholdVenueAxis => "venue_withhold_venue_axis",
            Self::ScopeWithhold => "scope_withhold",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EstimationDisposition {
    Eligible,
    Ineligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WithholdEntry {
    pub route_key: RouteKey,
    pub category: WithholdCategory,
    pub estimation: EstimationDisposition,
}

/// Explicit exact-class/venue evidence that qualifies a venue positively.
/// Without such an entry a venue is `unverified` (binding D).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VenueQualificationEvidence {
    pub factory: Address,
    pub route_key: RouteKey,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EstimatorArtifact {
    pub schema: String,
    pub feature_schema: String,
    pub chain_id: u64,
    pub executor_code_hash: String,
    pub executor_abi_digest: String,
    pub measured_profile_digest: String,
    pub model: AdditiveModel,
    pub margins: EstimateMargins,
    pub training_envelope: TrainingEnvelope,
    pub withhold_policy: Vec<WithholdEntry>,
    #[serde(default)]
    pub venue_qualification_evidence: Vec<VenueQualificationEvidence>,
    /// Free-form provenance; not interpreted.
    pub provenance: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum EstimatorLoadError {
    #[error("discovery gas estimator artifact {path} unreadable: {detail}")]
    Io { path: String, detail: String },
    #[error("discovery gas estimator digest mismatch: pinned {expected}, file {observed}")]
    Digest { expected: String, observed: String },
    #[error("discovery gas estimator artifact is malformed: {0}")]
    Malformed(String),
    #[error("discovery gas estimator {field} mismatch: expected {expected}, artifact {observed}")]
    Identity {
        field: &'static str,
        expected: String,
        observed: String,
    },
    #[error("discovery gas estimator withhold policy invalid: {0}")]
    Policy(String),
}

// ---------------------------------------------------------------------------
// Features, tiers, resolutions
// ---------------------------------------------------------------------------

/// Raw per-hop simulation evidence: protocol and exact crossing count per hop,
/// from the same simulation that priced the amount (no second simulation).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct GasFeatures {
    pub protocols: Vec<ProtocolKind>,
    pub crossings: Vec<u32>,
}

impl GasFeatures {
    pub fn new(protocols: Vec<ProtocolKind>, crossings: Vec<u32>) -> Self {
        Self {
            protocols,
            crossings,
        }
    }

    fn count(&self, kind: ProtocolKind) -> u32 {
        self.protocols.iter().filter(|p| **p == kind).count() as u32
    }

    fn per_hop(&self, kind: ProtocolKind) -> impl Iterator<Item = u32> + '_ {
        self.protocols
            .iter()
            .zip(&self.crossings)
            .filter(move |(p, _)| **p == kind)
            .map(|(_, c)| *c)
    }

    fn sum(&self, kind: ProtocolKind) -> u64 {
        self.per_hop(kind).map(u64::from).sum()
    }

    /// True when these features could have produced `route_key` under the
    /// existing max-per-hop bucket semantics.
    pub fn consistent_with(&self, route_key: &RouteKey) -> bool {
        if self.protocols != route_key.protocols || self.crossings.len() != self.protocols.len() {
            return false;
        }
        let v3 = self
            .per_hop(ProtocolKind::V3)
            .map(TickCrossingBucket::from_crossings)
            .max();
        let moe = self
            .per_hop(ProtocolKind::Moe)
            .map(BinCrossingBucket::from_crossings)
            .max();
        v3 == route_key.v3_tick_crossings && moe == route_key.moe_bin_crossings
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GasTier {
    Measured,
    Estimated,
}

impl GasTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Measured => "measured",
            Self::Estimated => "estimated",
        }
    }
}

/// Labels for an estimate outside the training envelope. Extrapolation alone
/// never rejects; it is carried to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash, Serialize)]
pub struct ExtrapolationFlags {
    pub v2_free: bool,
    pub hop_count: bool,
    pub v3_ticks: bool,
    pub moe_bins: bool,
    /// Set at materialization, where pool factories are known.
    pub venue_family: bool,
}

impl ExtrapolationFlags {
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (on, label) in [
            (self.v2_free, "v2_free_topology"),
            (self.hop_count, "hop_count_out_of_training"),
            (self.v3_ticks, "v3_ticks_out_of_training"),
            (self.moe_bins, "moe_bins_out_of_training"),
            (self.venue_family, "unseen_venue_family"),
        ] {
            if on {
                out.push(label);
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstimatedGas {
    /// Conservative expected gas (calibration p90 multiplier), not unbiased.
    pub expected_gas_used: u64,
    pub limit_envelope: u64,
    pub model_digest: Arc<str>,
    pub extrapolated: ExtrapolationFlags,
    pub features: GasFeatures,
}

/// Why a discovery gas resolution failed. None of these falls back to an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GasRejectReason {
    /// Measured class invalidated at runtime (never downgraded to Estimated).
    Invalidated,
    ResearchOnly,
    /// Poisoned runtime profile state.
    Poisoned,
    /// Malformed route key or features inconsistent with it.
    Structural,
    /// Static `Unsupported` entry absent from the audited withhold map.
    UnmappedWithhold,
    /// Static `Unsupported` entry the audited map declares estimation-ineligible.
    IneligibleWithhold,
    /// `expected <= 0` or `limit <= expected`.
    InvalidEstimate,
    /// Checked arithmetic overflow.
    Arithmetic,
    /// Limit does not fit the current available block gas.
    GasReserve,
    /// Measured `FeePolicy::build` rejected the quote for another reason.
    MeasuredFeePlan,
}

impl GasRejectReason {
    /// Discovery reject-reason label (bounded; no pool/address labels).
    pub fn label(self) -> &'static str {
        match self {
            Self::Invalidated => reject_reason::ROUTE_INVALIDATED,
            Self::ResearchOnly => reject_reason::RESEARCH_ONLY,
            Self::Poisoned => reject_reason::GAS_SCREEN,
            Self::Structural => reject_reason::ROUTE_KEY_CONSTRUCTION_ERROR,
            Self::UnmappedWithhold => reject_reason::UNMAPPED_WITHHOLD,
            Self::IneligibleWithhold => reject_reason::INELIGIBLE_WITHHOLD,
            Self::InvalidEstimate => reject_reason::INVALID_ESTIMATE,
            Self::Arithmetic => reject_reason::GAS_ARITHMETIC,
            Self::GasReserve => reject_reason::GAS_RESERVE,
            Self::MeasuredFeePlan => reject_reason::GAS_SCREEN,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryGasResolution {
    /// Existing Approved lookup; priced by the unchanged `fee_plan_cost`.
    Measured(GasQuote),
    Estimated(EstimatedGas),
    Rejected(GasRejectReason),
}

/// A priced resolution: the cost the optimizer and materialization both use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricedGas {
    pub tier: GasTier,
    pub expected_gas_used: u64,
    /// Measured `gas_limit`, or the estimate's limit envelope.
    pub gas_limit: u64,
    pub cost: U256,
    pub model_digest: Option<Arc<str>>,
    pub extrapolated: ExtrapolationFlags,
}

// ---------------------------------------------------------------------------
// Estimator
// ---------------------------------------------------------------------------

/// Loaded, identity-checked, immutable estimator + audited withhold policy.
pub struct DiscoveryGasEstimator {
    artifact: EstimatorArtifact,
    digest: Arc<str>,
    policy: HashMap<RouteKey, (WithholdCategory, EstimationDisposition)>,
    venue_factories: HashSet<Address>,
    qualified: HashSet<(Address, RouteKey)>,
}

/// Identity only: the artifact (301 policy entries) never floods a `Debug` log.
impl std::fmt::Debug for DiscoveryGasEstimator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryGasEstimator")
            .field("model_digest", &self.digest)
            .field("withhold_entries", &self.policy.len())
            .finish()
    }
}

impl DiscoveryGasEstimator {
    /// Load the committed mainnet artifact under `repo_root`, pinned by
    /// [`MAINNET_ESTIMATOR_DIGEST`].
    pub fn load_mainnet(
        repo_root: &Path,
        profile: &RuntimeGasProfile,
    ) -> Result<Self, EstimatorLoadError> {
        Self::load(
            &repo_root.join(MAINNET_ESTIMATOR_REL_PATH),
            MAINNET_ESTIMATOR_DIGEST,
            profile,
        )
    }

    pub fn load(
        path: &Path,
        pinned_digest: &str,
        profile: &RuntimeGasProfile,
    ) -> Result<Self, EstimatorLoadError> {
        let bytes = std::fs::read(path).map_err(|e| EstimatorLoadError::Io {
            path: path.display().to_string(),
            detail: e.to_string(),
        })?;
        Self::from_bytes(&bytes, pinned_digest, profile)
    }

    pub fn from_bytes(
        bytes: &[u8],
        pinned_digest: &str,
        profile: &RuntimeGasProfile,
    ) -> Result<Self, EstimatorLoadError> {
        let observed = format!("{}", keccak256(bytes));
        if !observed.eq_ignore_ascii_case(pinned_digest) {
            return Err(EstimatorLoadError::Digest {
                expected: pinned_digest.to_string(),
                observed,
            });
        }
        let artifact: EstimatorArtifact = serde_json::from_slice(bytes)
            .map_err(|e| EstimatorLoadError::Malformed(e.to_string()))?;
        Self::from_artifact(artifact, observed, profile)
    }

    fn from_artifact(
        artifact: EstimatorArtifact,
        digest: String,
        profile: &RuntimeGasProfile,
    ) -> Result<Self, EstimatorLoadError> {
        let identity = profile.executor_identity();
        let checks: [(&'static str, String, String); 6] = [
            ("schema", ESTIMATOR_SCHEMA.into(), artifact.schema.clone()),
            (
                "feature_schema",
                FEATURE_SCHEMA.into(),
                artifact.feature_schema.clone(),
            ),
            (
                "chain_id",
                identity.chain_id.to_string(),
                artifact.chain_id.to_string(),
            ),
            (
                "executor_code_hash",
                identity.template_hash.clone(),
                artifact.executor_code_hash.clone(),
            ),
            (
                "executor_abi_digest",
                identity.abi_digest.clone(),
                artifact.executor_abi_digest.clone(),
            ),
            (
                "measured_profile_digest",
                profile.artifact_digest().to_string(),
                artifact.measured_profile_digest.clone(),
            ),
        ];
        for (field, expected, observed) in checks {
            if !expected.eq_ignore_ascii_case(&observed) {
                return Err(EstimatorLoadError::Identity {
                    field,
                    expected,
                    observed,
                });
            }
        }
        let m = &artifact.margins;
        if m.validated
            || m.expected_multiplier_bps < BPS as u64
            || m.limit_multiplier_bps <= m.expected_multiplier_bps
        {
            return Err(EstimatorLoadError::Malformed(format!(
                "margins must be unvalidated with limit > expected >= 1.0: {m:?}"
            )));
        }

        let mut policy = HashMap::with_capacity(artifact.withhold_policy.len());
        for entry in &artifact.withhold_policy {
            entry
                .route_key
                .validate_structure()
                .map_err(|e| EstimatorLoadError::Policy(e.to_string()))?;
            if policy
                .insert(entry.route_key.clone(), (entry.category, entry.estimation))
                .is_some()
            {
                return Err(EstimatorLoadError::Policy(format!(
                    "duplicate entry {}",
                    entry.route_key.key_string()
                )));
            }
        }
        // The map must cover exactly the measured profile's static Unsupported set.
        let unsupported: HashSet<RouteKey> = profile.unsupported_route_keys().into_iter().collect();
        if let Some(missing) = unsupported.iter().find(|k| !policy.contains_key(*k)) {
            return Err(EstimatorLoadError::Policy(format!(
                "Unsupported measured entry {} is unmapped",
                missing.key_string()
            )));
        }
        if let Some(extra) = policy.keys().find(|k| !unsupported.contains(*k)) {
            return Err(EstimatorLoadError::Policy(format!(
                "entry {} is not a static Unsupported measured entry",
                extra.key_string()
            )));
        }

        let venue_factories = artifact
            .training_envelope
            .venue_factories
            .iter()
            .copied()
            .collect();
        let qualified = artifact
            .venue_qualification_evidence
            .iter()
            .map(|e| (e.factory, e.route_key.clone()))
            .collect();
        Ok(Self {
            artifact,
            digest: Arc::from(digest),
            policy,
            venue_factories,
            qualified,
        })
    }

    /// Bind the estimator to the chain the process actually connected to
    /// (WHI-1572 PR-F2): the profile check alone cannot see a mainnet profile
    /// loaded for a non-mainnet `--chain-id`.
    pub fn require_chain(&self, observed_chain_id: u64) -> Result<(), EstimatorLoadError> {
        if self.artifact.chain_id == observed_chain_id {
            Ok(())
        } else {
            Err(EstimatorLoadError::Identity {
                field: "connected chain_id",
                expected: observed_chain_id.to_string(),
                observed: self.artifact.chain_id.to_string(),
            })
        }
    }

    /// keccak256 of the artifact bytes (model identity recorded on candidates).
    pub fn model_digest(&self) -> &str {
        &self.digest
    }

    pub fn artifact(&self) -> &EstimatorArtifact {
        &self.artifact
    }

    /// Whether `key`, given its current checked measured status, may be estimated:
    /// `Unknown`, or a static `Unsupported` entry the audited map declares eligible.
    pub fn is_estimable(&self, status: &MeasuredRouteStatus, key: &RouteKey) -> bool {
        match status {
            MeasuredRouteStatus::Unknown => true,
            MeasuredRouteStatus::Unsupported => matches!(
                self.policy.get(key),
                Some((_, EstimationDisposition::Eligible))
            ),
            _ => false,
        }
    }

    fn extrapolation(&self, features: &GasFeatures) -> ExtrapolationFlags {
        let env = &self.artifact.training_envelope;
        ExtrapolationFlags {
            v2_free: env.v2_free_rows == 0 && features.count(ProtocolKind::V2) == 0,
            hop_count: !env.hop_counts.contains(&(features.protocols.len() as u8)),
            v3_ticks: features
                .per_hop(ProtocolKind::V3)
                .any(|c| c > env.max_v3_ticks_per_hop)
                || features.sum(ProtocolKind::V3) > u64::from(env.max_sum_v3_ticks),
            moe_bins: features
                .per_hop(ProtocolKind::Moe)
                .any(|c| c > env.max_moe_bins_per_hop)
                || features.sum(ProtocolKind::Moe) > u64::from(env.max_sum_moe_bins),
            venue_family: false,
        }
    }

    /// Deployed integer model with ceiling margins and checked arithmetic.
    pub fn estimate(&self, features: &GasFeatures) -> Result<EstimatedGas, GasRejectReason> {
        let m = &self.artifact.model;
        let terms: [(i64, i128); 6] = [
            (m.b0, 1),
            (m.b_v2, features.count(ProtocolKind::V2).into()),
            (m.b_v3, features.count(ProtocolKind::V3).into()),
            (m.b_moe, features.count(ProtocolKind::Moe).into()),
            (m.s_tick, features.sum(ProtocolKind::V3).into()),
            (m.s_bin, features.sum(ProtocolKind::Moe).into()),
        ];
        let mut pred: i128 = 0;
        for (coef, x) in terms {
            pred = i128::from(coef)
                .checked_mul(x)
                .and_then(|t| pred.checked_add(t))
                .ok_or(GasRejectReason::Arithmetic)?;
        }
        if pred <= 0 {
            return Err(GasRejectReason::InvalidEstimate);
        }
        let margins = &self.artifact.margins;
        let scaled = |bps: u64| -> Result<i128, GasRejectReason> {
            let num = pred
                .checked_mul(i128::from(bps))
                .ok_or(GasRejectReason::Arithmetic)?;
            // Ceiling division of a positive numerator.
            num.checked_add(BPS - 1)
                .map(|n| n / BPS)
                .ok_or(GasRejectReason::Arithmetic)
        };
        let expected = scaled(margins.expected_multiplier_bps)?;
        let limit = scaled(margins.limit_multiplier_bps)?
            .checked_add(i128::from(margins.limit_overhead_gas))
            .ok_or(GasRejectReason::Arithmetic)?;
        let expected = u64::try_from(expected).map_err(|_| GasRejectReason::Arithmetic)?;
        let limit = u64::try_from(limit).map_err(|_| GasRejectReason::Arithmetic)?;
        if expected == 0 || limit <= expected {
            return Err(GasRejectReason::InvalidEstimate);
        }
        Ok(EstimatedGas {
            expected_gas_used: expected,
            limit_envelope: limit,
            model_digest: Arc::clone(&self.digest),
            extrapolated: self.extrapolation(features),
            features: features.clone(),
        })
    }

    /// True when `factory` is inside the training venue families.
    pub fn venue_family_seen(&self, factory: Option<Address>) -> bool {
        factory.is_some_and(|f| self.venue_factories.contains(&f))
    }

    /// Positive qualification needs an explicit evidence entry (binding D).
    pub fn venue_qualified(&self, factory: Option<Address>, route_key: &RouteKey) -> bool {
        factory.is_some_and(|f| self.qualified.contains(&(f, route_key.clone())))
    }
}

/// Typed discovery resolution (binding A). Reads the **current** checked profile
/// status on every call, so a cached class invalidated later is rejected, never
/// downgraded to an estimate.
pub fn resolve_discovery_gas(
    profile: &RuntimeGasProfile,
    estimator: &DiscoveryGasEstimator,
    route_key: &RouteKey,
    features: &GasFeatures,
) -> DiscoveryGasResolution {
    use DiscoveryGasResolution::Rejected;
    if !features.consistent_with(route_key) {
        return Rejected(GasRejectReason::Structural);
    }
    let status = match profile.checked_route_status(route_key) {
        Ok(status) => status,
        Err(crate::execution::RuntimeGasProfileError::ProfileStatePoisoned) => {
            return Rejected(GasRejectReason::Poisoned)
        }
        Err(_) => return Rejected(GasRejectReason::Structural),
    };
    match status {
        MeasuredRouteStatus::Approved(quote) => DiscoveryGasResolution::Measured(quote),
        MeasuredRouteStatus::Invalidated => Rejected(GasRejectReason::Invalidated),
        MeasuredRouteStatus::ResearchOnly => Rejected(GasRejectReason::ResearchOnly),
        MeasuredRouteStatus::Unsupported => match estimator.policy.get(route_key) {
            None => Rejected(GasRejectReason::UnmappedWithhold),
            Some((_, EstimationDisposition::Ineligible)) => {
                Rejected(GasRejectReason::IneligibleWithhold)
            }
            Some((_, EstimationDisposition::Eligible)) => estimate_or_reject(estimator, features),
        },
        MeasuredRouteStatus::Unknown => estimate_or_reject(estimator, features),
    }
}

fn estimate_or_reject(
    estimator: &DiscoveryGasEstimator,
    features: &GasFeatures,
) -> DiscoveryGasResolution {
    match estimator.estimate(features) {
        Ok(est) => DiscoveryGasResolution::Estimated(est),
        Err(reason) => DiscoveryGasResolution::Rejected(reason),
    }
}

/// L2-gas-only cost of an estimate under the current fee context. Mirrors the
/// `FeePolicy::build` checks without constructing a `GasQuote`/`FeePlan`.
pub fn estimated_fee_cost(
    est: &EstimatedGas,
    context: &BlockFeeContext,
    policy: FeePolicy,
) -> Result<U256, GasRejectReason> {
    if est.expected_gas_used == 0 || est.limit_envelope <= est.expected_gas_used {
        return Err(GasRejectReason::InvalidEstimate);
    }
    let available = context
        .block_gas_limit
        .checked_sub(policy.block_gas_reserve())
        .ok_or(GasRejectReason::GasReserve)?;
    if est.limit_envelope >= available {
        return Err(GasRejectReason::GasReserve);
    }
    let max_fee = context
        .base_fee_per_gas
        .checked_add(policy.priority_fee_per_gas())
        .ok_or(GasRejectReason::Arithmetic)?;
    U256::from(est.expected_gas_used)
        .checked_mul(U256::from(max_fee))
        .ok_or(GasRejectReason::Arithmetic)
}

fn measured_fee_reject(err: &FeePlanError) -> GasRejectReason {
    match err {
        FeePlanError::GasLimitExceedsBlockReserve { .. } => GasRejectReason::GasReserve,
        FeePlanError::Overflow => GasRejectReason::Arithmetic,
        _ => GasRejectReason::MeasuredFeePlan,
    }
}

/// Resolve and price one `(route_key, features)` under the current profile
/// status and fee context. Shared by optimize, materialize and fee rescoring.
pub fn price_discovery_gas(
    profile: &RuntimeGasProfile,
    estimator: &DiscoveryGasEstimator,
    route_key: &RouteKey,
    features: &GasFeatures,
    context: &BlockFeeContext,
    policy: FeePolicy,
) -> Result<PricedGas, GasRejectReason> {
    match resolve_discovery_gas(profile, estimator, route_key, features) {
        DiscoveryGasResolution::Measured(quote) => {
            let cost = fee_plan_cost(&quote, context, policy).map_err(|e| measured_fee_reject(&e))?;
            Ok(PricedGas {
                tier: GasTier::Measured,
                expected_gas_used: quote.expected_gas_used,
                gas_limit: quote.gas_limit,
                cost,
                model_digest: None,
                extrapolated: ExtrapolationFlags::default(),
            })
        }
        DiscoveryGasResolution::Estimated(est) => {
            let cost = estimated_fee_cost(&est, context, policy)?;
            Ok(PricedGas {
                tier: GasTier::Estimated,
                expected_gas_used: est.expected_gas_used,
                gas_limit: est.limit_envelope,
                cost,
                model_digest: Some(est.model_digest),
                extrapolated: est.extrapolated,
            })
        }
        DiscoveryGasResolution::Rejected(reason) => Err(reason),
    }
}

// ---------------------------------------------------------------------------
// Venue labels (binding D)
// ---------------------------------------------------------------------------

/// Pool → factory from the frozen universe. Factory identity is a label, never
/// qualification.
#[derive(Clone, Default)]
pub struct PoolVenueMap {
    by_pool: HashMap<Address, Address>,
}

impl std::fmt::Debug for PoolVenueMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PoolVenueMap").field("pools", &self.by_pool.len()).finish()
    }
}

impl PoolVenueMap {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (Address, Address)>) -> Self {
        Self {
            by_pool: pairs.into_iter().collect(),
        }
    }

    pub fn factory(&self, pool: &Address) -> Option<Address> {
        self.by_pool.get(pool).copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueQualification {
    /// Explicit exact-class/venue evidence exists.
    Qualified,
    /// No attribution: neither qualified nor proven non-executable.
    Unverified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VenueLabel {
    pub pool: Address,
    pub protocol: ProtocolKind,
    pub factory: Option<Address>,
    pub venue: &'static str,
    pub qualification: VenueQualification,
}

/// Per-pool labels for one candidate, measured or estimated.
pub fn venue_labels(
    pools: &[Address],
    protocols: &[ProtocolKind],
    route_key: &RouteKey,
    venues: Option<&PoolVenueMap>,
    estimator: Option<&DiscoveryGasEstimator>,
) -> Vec<VenueLabel> {
    pools
        .iter()
        .zip(protocols)
        .map(|(pool, protocol)| {
            let factory = venues.and_then(|v| v.factory(pool));
            let venue = match (protocol, factory) {
                (_, None) => "unattributed",
                (ProtocolKind::V2, Some(f)) => crate::service::v2_venues::v2_venue_by_factory(f)
                    .map(|v| v.label)
                    .unwrap_or("unregistered-v2"),
                (ProtocolKind::V3, Some(f)) => crate::service::v3_venues::venue_by_factory(f)
                    .map(|v| v.label)
                    .unwrap_or("unregistered-v3"),
                (ProtocolKind::Moe, Some(_)) => "moe-lb",
            };
            let qualification = if estimator.is_some_and(|e| e.venue_qualified(factory, route_key)) {
                VenueQualification::Qualified
            } else {
                VenueQualification::Unverified
            };
            VenueLabel {
                pool: *pool,
                protocol: *protocol,
                factory,
                venue,
                qualification,
            }
        })
        .collect()
}

/// Gas evidence carried on a discovered candidate (tier, identity, numbers,
/// extrapolation, venue labels). Evidence only: never send authorization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateGasEvidence {
    pub tier: GasTier,
    pub expected_gas_used: u64,
    pub gas_limit: u64,
    pub model_digest: Option<Arc<str>>,
    pub extrapolated: ExtrapolationFlags,
    pub crossings: Vec<u32>,
    pub venues: Vec<VenueLabel>,
}

impl CandidateGasEvidence {
    pub fn is_estimated(&self) -> bool {
        self.tier == GasTier::Estimated
    }

    pub fn all_venues_qualified(&self) -> bool {
        !self.venues.is_empty()
            && self
                .venues
                .iter()
                .all(|v| v.qualification == VenueQualification::Qualified)
    }
}

#[cfg(test)]
impl DiscoveryGasEstimator {
    /// Test-only: drop one withhold entry after load (the load-time coverage
    /// check makes this state unreachable in production).
    fn without_policy_entry(mut self, key: &RouteKey) -> Self {
        self.policy.remove(key);
        self
    }

    /// Test-only: build from an artifact whose identity is re-pointed at
    /// `profile` (used with deliberately modified measured profiles).
    pub(crate) fn for_profile_with(
        profile: &RuntimeGasProfile,
        mutate: impl FnOnce(&mut EstimatorArtifact),
    ) -> Result<Self, EstimatorLoadError> {
        let bytes = std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(MAINNET_ESTIMATOR_REL_PATH),
        )
        .expect("committed estimator artifact");
        let mut artifact: EstimatorArtifact = serde_json::from_slice(&bytes).expect("artifact");
        artifact.measured_profile_digest = profile.artifact_digest().to_string();
        let unsupported: HashSet<RouteKey> =
            profile.unsupported_route_keys().into_iter().collect();
        artifact
            .withhold_policy
            .retain(|e| unsupported.contains(&e.route_key));
        mutate(&mut artifact);
        Self::from_artifact(artifact, "0xtest".into(), profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::gas_profile::{load_artifact, ProfileStatus};
    use crate::execution::{RuntimeGasProfileError, RuntimeProfileConfig};
    use alloy::primitives::{address, B256};
    use ProtocolKind::{Moe, V2, V3};

    fn root() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// In-memory mainnet profile: no invalidation file, so `invalidate` in a
    /// test never writes next to the committed artifact.
    fn mainnet_profile() -> RuntimeGasProfile {
        RuntimeGasProfile::from_artifact_with_identity(
            load_artifact(&root().join("config/gas_profiles/mantle_mainnet_v1.json")).unwrap(),
            RuntimeProfileConfig::mantle_mainnet(Vec::new()),
            crate::execution::gas_runtime::mainnet_verified_identity(),
        )
        .expect("mainnet profile")
    }

    fn estimator(profile: &RuntimeGasProfile) -> DiscoveryGasEstimator {
        DiscoveryGasEstimator::load_mainnet(root(), profile).expect("mainnet estimator")
    }

    fn key(protocols: &[ProtocolKind], ticks: Option<TickCrossingBucket>, bins: Option<BinCrossingBucket>) -> RouteKey {
        let mut k = RouteKey::new(protocols.to_vec()).unwrap();
        k.v3_tick_crossings = ticks;
        k.moe_bin_crossings = bins;
        k
    }

    /// Features whose max-per-hop buckets reproduce `k`.
    fn features_for(k: &RouteKey) -> GasFeatures {
        let v3 = match k.v3_tick_crossings {
            Some(TickCrossingBucket::Zero) | None => 0,
            Some(TickCrossingBucket::Low) => 3,
            Some(TickCrossingBucket::Mid) => 10,
            Some(TickCrossingBucket::High) => 30,
        };
        let moe = match k.moe_bin_crossings {
            Some(BinCrossingBucket::Zero) | None => 0,
            Some(BinCrossingBucket::Low) => 2,
            Some(BinCrossingBucket::Mid) => 6,
            Some(BinCrossingBucket::High) => 15,
        };
        GasFeatures::new(
            k.protocols.clone(),
            k.protocols
                .iter()
                .map(|p| match p {
                    V2 => 0,
                    V3 => v3,
                    Moe => moe,
                })
                .collect(),
        )
    }

    fn ctx(base_fee: u128, block_gas_limit: u64) -> BlockFeeContext {
        BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: base_fee,
            block_gas_limit,
        }
    }

    fn artifact_bytes() -> Vec<u8> {
        std::fs::read(root().join(MAINNET_ESTIMATOR_REL_PATH)).expect("artifact bytes")
    }

    fn reencode(mutate: impl FnOnce(&mut serde_json::Value)) -> (Vec<u8>, String) {
        let mut v: serde_json::Value = serde_json::from_slice(&artifact_bytes()).unwrap();
        mutate(&mut v);
        let bytes = serde_json::to_vec_pretty(&v).unwrap();
        let digest = format!("{}", keccak256(&bytes));
        (bytes, digest)
    }

    // --- AC1: pinned, identity-checked, fail closed ----------------------------

    #[test]
    fn mainnet_artifact_is_pinned_identity_checked_and_covers_all_301_unsupported() {
        let profile = mainnet_profile();
        let est = estimator(&profile);
        assert_eq!(est.model_digest(), MAINNET_ESTIMATOR_DIGEST);
        let a = est.artifact();
        assert_eq!(a.measured_profile_digest, profile.artifact_digest());
        assert!(!a.margins.validated, "margins are new, unvalidated parameters");
        assert_eq!(profile.unsupported_route_keys().len(), 301);
        assert_eq!(a.withhold_policy.len(), 301);
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for e in &a.withhold_policy {
            *counts.entry(e.category.as_str()).or_default() += 1;
        }
        assert_eq!(counts["insufficient_samples"], 152);
        assert_eq!(counts["open_ended_bucket"], 118);
        assert_eq!(counts["venue_withhold_factory_axis"], 25);
        assert_eq!(counts["venue_withhold_venue_axis"], 2);
        assert_eq!(counts["scope_withhold"], 2);
        assert_eq!(counts["failed_limit_gate"], 2);
        // Measured identity is untouched by the estimator.
        assert_eq!(
            profile.artifact_digest(),
            crate::execution::MANTLE_MAINNET_PROFILE_DIGEST
        );
    }

    #[test]
    fn missing_corrupt_or_misidentified_artifacts_fail_closed() {
        let profile = mainnet_profile();
        let missing = DiscoveryGasEstimator::load(
            &root().join("config/gas_profiles/does-not-exist.json"),
            MAINNET_ESTIMATOR_DIGEST,
            &profile,
        );
        assert!(matches!(missing, Err(EstimatorLoadError::Io { .. })));

        let mut corrupt = artifact_bytes();
        corrupt[10] ^= 0x01;
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(&corrupt, MAINNET_ESTIMATOR_DIGEST, &profile),
            Err(EstimatorLoadError::Digest { .. })
        ));

        let truncated = &artifact_bytes()[..100];
        let digest = format!("{}", keccak256(truncated));
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(truncated, &digest, &profile),
            Err(EstimatorLoadError::Malformed(_))
        ));

        for (field, path, value) in [
            ("schema", "schema", serde_json::json!("other/v1")),
            ("feature_schema", "feature_schema", serde_json::json!("additive-v0")),
            ("chain_id", "chain_id", serde_json::json!(5003)),
            ("executor_code_hash", "executor_code_hash", serde_json::json!("0x01")),
            ("executor_abi_digest", "executor_abi_digest", serde_json::json!("0x02")),
            ("measured_profile_digest", "measured_profile_digest", serde_json::json!("0x03")),
        ] {
            let (bytes, digest) = reencode(|v| v[path] = value);
            match DiscoveryGasEstimator::from_bytes(&bytes, &digest, &profile) {
                Err(EstimatorLoadError::Identity { field: f, .. }) => assert_eq!(f, field),
                other => panic!("{field}: expected identity failure, got {other:?}"),
            }
        }

        // Connected-chain binding (PR-F2): the mainnet artifact refuses 5003.
        let est = estimator(&profile);
        est.require_chain(5000).expect("mainnet");
        assert!(matches!(
            est.require_chain(5003),
            Err(EstimatorLoadError::Identity { field: "connected chain_id", .. })
        ));

        let (bytes, digest) = reencode(|v| v["margins"]["validated"] = serde_json::json!(true));
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(&bytes, &digest, &profile),
            Err(EstimatorLoadError::Malformed(_))
        ));
    }

    // --- AC2: audited fallback policy -------------------------------------------

    #[test]
    fn unknown_category_or_incomplete_map_fails_the_load() {
        let profile = mainnet_profile();
        let (bytes, digest) =
            reencode(|v| v["withhold_policy"][0]["category"] = serde_json::json!("vibes"));
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(&bytes, &digest, &profile),
            Err(EstimatorLoadError::Malformed(_))
        ));

        let (bytes, digest) = reencode(|v| {
            v["withhold_policy"].as_array_mut().unwrap().remove(0);
        });
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(&bytes, &digest, &profile),
            Err(EstimatorLoadError::Policy(_))
        ));

        // An Approved measured class must never sit in the withhold map.
        let approved = key(&[V2, V2], None, None);
        let (bytes, digest) = reencode(|v| {
            v["withhold_policy"].as_array_mut().unwrap().push(serde_json::json!({
                "route_key": approved,
                "category": "insufficient_samples",
                "estimation": "eligible",
            }))
        });
        assert!(matches!(
            DiscoveryGasEstimator::from_bytes(&bytes, &digest, &profile),
            Err(EstimatorLoadError::Policy(_))
        ));
    }

    #[test]
    fn only_declared_cases_estimate() {
        use TickCrossingBucket as T;
        use BinCrossingBucket as B;
        let profile = mainnet_profile();
        let est = estimator(&profile);
        let cases = [
            // evidence absence
            ("insufficient_samples", key(&[Moe, Moe], None, Some(B::Zero))),
            ("open_ended_bucket", key(&[Moe, Moe], None, Some(B::High))),
            ("failed_limit_gate", key(&[V3, V3], Some(T::Low), None)),
            // venue withholds (DI-50 factory axis / DI-54 venue axis)
            ("venue_withhold_factory_axis", key(&[Moe, V3], Some(T::Zero), Some(B::Zero))),
            ("venue_withhold_venue_axis", key(&[V2, Moe], None, Some(B::Zero))),
            // WHI-1520-scope withhold
            ("scope_withhold", key(&[Moe, V2, V2], None, Some(B::Low))),
            // genuine Unknown (no entry at any bucket)
            ("unknown", key(&[V3, V3, V3, V3], Some(T::Zero), None)),
        ];
        for (label, k) in cases {
            let res = resolve_discovery_gas(&profile, &est, &k, &features_for(&k));
            let DiscoveryGasResolution::Estimated(e) = res else {
                panic!("{label} {}: expected Estimated, got {res:?}", k.key_string());
            };
            assert!(e.expected_gas_used > 0 && e.expected_gas_used < e.limit_envelope);
            assert_eq!(&*e.model_digest, MAINNET_ESTIMATOR_DIGEST);
        }
        if let Some(entry) = est
            .artifact()
            .withhold_policy
            .iter()
            .find(|e| e.route_key == key(&[Moe, V2, V2], None, Some(B::Low)))
        {
            assert_eq!(entry.category, WithholdCategory::ScopeWithhold);
        }

        // Measured stays measured, with the unchanged fee_plan_cost price.
        let measured = key(&[V2, V2], None, None);
        let res = resolve_discovery_gas(&profile, &est, &measured, &features_for(&measured));
        assert_eq!(res, DiscoveryGasResolution::Measured(profile.quote(&measured).unwrap()));
        let policy = FeePolicy::new(7, 1_000_000);
        let priced = price_discovery_gas(
            &profile,
            &est,
            &measured,
            &features_for(&measured),
            &ctx(20, 30_000_000),
            policy,
        )
        .unwrap();
        assert_eq!(priced.tier, GasTier::Measured);
        assert_eq!(
            priced.cost,
            fee_plan_cost(&profile.quote(&measured).unwrap(), &ctx(20, 30_000_000), policy).unwrap()
        );
    }

    #[test]
    fn unmapped_ineligible_research_only_invalidated_poisoned_and_malformed_never_estimate() {
        let profile = mainnet_profile();
        let unsupported = key(&[Moe, Moe], None, Some(BinCrossingBucket::Zero));

        // Unmapped (per-route defense behind the load-time coverage check).
        let est = estimator(&profile).without_policy_entry(&unsupported);
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &unsupported, &features_for(&unsupported)),
            DiscoveryGasResolution::Rejected(GasRejectReason::UnmappedWithhold)
        );

        // Declared ineligible.
        let est = DiscoveryGasEstimator::for_profile_with(&profile, |a| {
            for e in &mut a.withhold_policy {
                if e.route_key == unsupported {
                    e.estimation = EstimationDisposition::Ineligible;
                }
            }
        })
        .unwrap();
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &unsupported, &features_for(&unsupported)),
            DiscoveryGasResolution::Rejected(GasRejectReason::IneligibleWithhold)
        );

        // ResearchOnly measured entry.
        let mut artifact =
            load_artifact(&root().join("config/gas_profiles/mantle_mainnet_v1.json")).unwrap();
        for p in &mut artifact.profiles {
            if p.route_key == unsupported {
                p.status = ProfileStatus::ResearchOnly;
            }
        }
        artifact.content_digest =
            crate::execution::gas_profile::compute_content_digest(&artifact).unwrap();
        let mut config = RuntimeProfileConfig::mantle_mainnet(Vec::new());
        config.expected_content_digest = artifact.content_digest.clone();
        let research = RuntimeGasProfile::from_artifact_with_identity(
            artifact,
            config,
            crate::execution::gas_runtime::mainnet_verified_identity(),
        )
        .unwrap();
        let est = DiscoveryGasEstimator::for_profile_with(&research, |_| {}).unwrap();
        assert_eq!(
            resolve_discovery_gas(&research, &est, &unsupported, &features_for(&unsupported)),
            DiscoveryGasResolution::Rejected(GasRejectReason::ResearchOnly)
        );

        // Runtime invalidation never becomes a fallback — for an estimation-
        // eligible Unsupported class as much as for a measured one (PR-F1).
        let est = estimator(&profile);
        profile.invalidate(&unsupported).unwrap();
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &unsupported, &features_for(&unsupported)),
            DiscoveryGasResolution::Rejected(GasRejectReason::Invalidated)
        );
        let approved = key(&[V2, V2], None, None);
        profile.invalidate(&approved).unwrap();
        assert_eq!(
            profile.checked_route_status(&approved).unwrap(),
            MeasuredRouteStatus::Invalidated
        );
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &approved, &features_for(&approved)),
            DiscoveryGasResolution::Rejected(GasRejectReason::Invalidated)
        );

        // Malformed structure: features that cannot produce the key.
        let v3 = key(&[V2, V3], Some(TickCrossingBucket::Zero), None);
        let wrong = GasFeatures::new(vec![V2, V3], vec![0, 9]);
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &v3, &wrong),
            DiscoveryGasResolution::Rejected(GasRejectReason::Structural)
        );
        let mut bad = v3.clone();
        bad.hop_count = 3;
        assert_eq!(
            resolve_discovery_gas(&profile, &est, &bad, &features_for(&v3)),
            DiscoveryGasResolution::Rejected(GasRejectReason::Structural)
        );

        // Poison: the checked API errors (inspect_route would skip the lock).
        let fresh = mainnet_profile();
        let est = estimator(&fresh);
        fresh.poison_invalidation_lock_for_test();
        assert!(matches!(
            fresh.checked_route_status(&unsupported),
            Err(RuntimeGasProfileError::ProfileStatePoisoned)
        ));
        for k in [&unsupported, &approved] {
            assert_eq!(
                resolve_discovery_gas(&fresh, &est, k, &features_for(k)),
                DiscoveryGasResolution::Rejected(GasRejectReason::Poisoned)
            );
        }
    }

    // --- AC3 / AC5: deployed integer model, bounds, distinct reasons ------------

    #[test]
    fn deployed_integer_model_uses_ceiling_margins_and_distinct_failure_reasons() {
        let profile = mainnet_profile();
        let est = estimator(&profile);
        let a = est.artifact().clone();
        let f = GasFeatures::new(vec![V2, V3, Moe], vec![0, 7, 3]);
        let m = &a.model;
        let pred = i128::from(m.b0)
            + i128::from(m.b_v2)
            + i128::from(m.b_v3)
            + i128::from(m.b_moe)
            + 7 * i128::from(m.s_tick)
            + 3 * i128::from(m.s_bin);
        let ceil = |bps: u64| (pred * i128::from(bps) + 9_999) / 10_000;
        let e = est.estimate(&f).unwrap();
        assert_eq!(i128::from(e.expected_gas_used), ceil(a.margins.expected_multiplier_bps));
        assert_eq!(
            i128::from(e.limit_envelope),
            ceil(a.margins.limit_multiplier_bps) + i128::from(a.margins.limit_overhead_gas)
        );

        let policy = FeePolicy::new(3, 1_000_000);
        assert_eq!(
            estimated_fee_cost(&e, &ctx(10, 30_000_000), policy).unwrap(),
            U256::from(e.expected_gas_used) * U256::from(13u64)
        );
        // Reserve excess: no clipping, the limit is reported unchanged.
        let tight = e.limit_envelope + 1_000_000;
        assert_eq!(
            estimated_fee_cost(&e, &ctx(10, tight), policy),
            Err(GasRejectReason::GasReserve)
        );
        assert_eq!(e.limit_envelope, est.estimate(&f).unwrap().limit_envelope);
        assert_eq!(
            estimated_fee_cost(&e, &ctx(10, 10), policy),
            Err(GasRejectReason::GasReserve)
        );
        assert_eq!(
            estimated_fee_cost(&e, &ctx(u128::MAX, 30_000_000), policy),
            Err(GasRejectReason::Arithmetic)
        );
        let mut inverted = e.clone();
        inverted.limit_envelope = inverted.expected_gas_used;
        assert_eq!(
            estimated_fee_cost(&inverted, &ctx(10, 30_000_000), policy),
            Err(GasRejectReason::InvalidEstimate)
        );

        let negative = DiscoveryGasEstimator::for_profile_with(&profile, |a| {
            a.model.b0 = -10_000_000
        })
        .unwrap();
        assert_eq!(negative.estimate(&f), Err(GasRejectReason::InvalidEstimate));
        let huge = DiscoveryGasEstimator::for_profile_with(&profile, |a| {
            a.model.s_tick = i64::MAX;
            a.model.b_v3 = i64::MAX;
        })
        .unwrap();
        assert_eq!(huge.estimate(&f), Err(GasRejectReason::Arithmetic));
        assert_ne!(
            GasRejectReason::InvalidEstimate.label(),
            GasRejectReason::Arithmetic.label()
        );
        assert_ne!(GasRejectReason::Arithmetic.label(), GasRejectReason::GasReserve.label());
    }

    #[test]
    fn extrapolation_is_labeled_not_rejected() {
        let profile = mainnet_profile();
        let est = estimator(&profile);
        let v2_free = GasFeatures::new(vec![V3, V3], vec![0, 0]);
        let e = est.estimate(&v2_free).unwrap();
        assert!(e.extrapolated.v2_free, "no training row lacks V2");
        assert!(!e.extrapolated.hop_count);

        let deep = GasFeatures::new(vec![V2, V3, V3, V3], vec![0, 500, 0, 0]);
        let e = est.estimate(&deep).unwrap();
        assert!(e.extrapolated.hop_count && e.extrapolated.v3_ticks);
        assert_eq!(
            e.extrapolated.labels(),
            vec!["hop_count_out_of_training", "v3_ticks_out_of_training"]
        );

        let inside = GasFeatures::new(vec![V2, V3], vec![0, 1]);
        assert_eq!(est.estimate(&inside).unwrap().extrapolated, ExtrapolationFlags::default());
    }

    // --- AC8 binding D: venue labels ------------------------------------------

    #[test]
    fn venue_labels_need_explicit_evidence_to_qualify() {
        let profile = mainnet_profile();
        let agni_factory = address!("25780dc8fc3cfbd75f33bfdab65e969b603b2035");
        let pool_a = address!("00000000000000000000000000000000000000a1");
        let pool_b = address!("00000000000000000000000000000000000000b2");
        let k = key(&[V2, V3], Some(TickCrossingBucket::Zero), None);
        let venues = PoolVenueMap::from_pairs([(pool_b, agni_factory)]);
        let est = estimator(&profile);

        let labels = venue_labels(&[pool_a, pool_b], &[V2, V3], &k, Some(&venues), Some(&est));
        assert_eq!(labels[0].venue, "unattributed");
        assert_eq!(labels[0].factory, None);
        assert_eq!(labels[1].venue, "agni-v3");
        assert!(labels
            .iter()
            .all(|l| l.qualification == VenueQualification::Unverified));
        assert!(est.venue_family_seen(Some(agni_factory)));
        assert!(!est.venue_family_seen(None));

        let with_evidence = DiscoveryGasEstimator::for_profile_with(&profile, |a| {
            a.venue_qualification_evidence.push(VenueQualificationEvidence {
                factory: agni_factory,
                route_key: k.clone(),
                evidence: "test fixture".into(),
            })
        })
        .unwrap();
        let labels =
            venue_labels(&[pool_a, pool_b], &[V2, V3], &k, Some(&venues), Some(&with_evidence));
        assert_eq!(labels[0].qualification, VenueQualification::Unverified);
        assert_eq!(labels[1].qualification, VenueQualification::Qualified);
    }
}
