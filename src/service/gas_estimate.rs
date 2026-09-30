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
#[derive(Debug)]
pub struct DiscoveryGasEstimator {
    artifact: EstimatorArtifact,
    digest: Arc<str>,
    policy: HashMap<RouteKey, (WithholdCategory, EstimationDisposition)>,
    venue_factories: HashSet<Address>,
    qualified: HashSet<(Address, RouteKey)>,
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
#[derive(Debug, Clone, Default)]
pub struct PoolVenueMap {
    by_pool: HashMap<Address, Address>,
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
