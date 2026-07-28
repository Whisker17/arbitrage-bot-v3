//! Shadow-mode acceptance-threshold schema, validation, and content digest
//! (WHI-554).
//!
//! Deliberately lives outside `execution::shadow` (WHI-549): thresholds are
//! predeclared *before* a shadow run starts and are audited independently of
//! the ledger writer, so this module never depends on anything `pub(crate)`
//! inside `shadow`. All numeric fields are decimal strings, never JSON
//! numbers, matching `signing::canonical`'s envelope policy — enforced here
//! by running [`assert_no_numbers`] over the raw JSON tree before typed
//! parsing, and again per-field via [`DecimalUint`]'s own deserialization.
//!
//! The digest computed by [`validate`] is a raw `keccak256` of the exact
//! input bytes (never a re-serialized form), matching
//! `execution::shadow::digest::digest_of_bytes`'s scheme — reimplemented
//! locally here since that function lives in a private submodule and is not
//! reachable from this file (see `docs/DEFERRED_ISSUES.md` DI-27). The
//! `keccak256`-then-hex-encode step itself reuses
//! [`crate::execution::shadow_gate_plan::digest_bytes`] rather than a local
//! copy.

use std::collections::BTreeSet;
use std::str::FromStr;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

use crate::execution::shadow_gate_plan::digest_bytes;
use crate::signing::canonical::assert_no_numbers;

/// Schema version for [`ShadowThresholds`] artifacts. Only this exact value
/// is accepted by [`validate`] — there is no compatibility list because no
/// prior schema version has ever shipped.
pub const THRESHOLDS_SCHEMA_VERSION: &str = "whisker-arb/shadow-thresholds/v1";

#[derive(Debug, thiserror::Error)]
pub enum ThresholdSchemaError {
    #[error(transparent)]
    Signing(#[from] crate::signing::SigningError),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported schema_version: expected {expected:?}, found {found:?}")]
    UnsupportedSchemaVersion { expected: String, found: String },
    #[error("required_services must not be empty")]
    RequiredServicesEmpty,
    #[error("required_services contains duplicate entry: {0:?}")]
    DuplicateRequiredService(String),
    #[error(
        "invalid decimal value {value:?} (expected ASCII digits, no leading zero, and to fit in 256 bits)"
    )]
    InvalidDecimal { value: String },
    #[error(
        "rate bound at {path} is impossible: numerator {numerator} exceeds denominator {denominator}"
    )]
    ImpossibleRateBound {
        path: String,
        numerator: String,
        denominator: String,
    },
    #[error(
        "coverage_budget.min_distinct_blocks_per_service ({coverage}) must not exceed min_canonical_blocks ({canonical})"
    )]
    CoverageExceedsCanonicalBlocks { coverage: String, canonical: String },
}

/// A non-negative decimal integer, stored and serialized as a JSON string
/// (never a JSON number). Digits-only, no leading zero except the literal
/// `"0"`, and must fit in 256 bits — matching the on-chain-amount convention
/// used everywhere else in this crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecimalUint(String);

impl DecimalUint {
    pub fn parse(raw: impl Into<String>) -> Result<Self, ThresholdSchemaError> {
        let raw = raw.into();
        let is_digits_only = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit());
        let has_no_leading_zero = raw.len() == 1 || !raw.starts_with('0');
        if !is_digits_only || !has_no_leading_zero {
            return Err(ThresholdSchemaError::InvalidDecimal { value: raw });
        }
        U256::from_str(&raw).map_err(|_| ThresholdSchemaError::InvalidDecimal {
            value: raw.clone(),
        })?;
        Ok(Self(raw))
    }

    pub fn value(&self) -> &str {
        &self.0
    }

    fn as_u256(&self) -> U256 {
        U256::from_str(&self.0)
            .expect("DecimalUint is only constructed via parse(), which validates this")
    }
}

impl Serialize for DecimalUint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for DecimalUint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        DecimalUint::parse(raw).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

/// An exact fraction (`numerator / denominator`), never a float. Used as
/// either a maximum or a minimum depending on the containing field
/// (`ShadowThresholds::max_error_rate` checks `actual <= bound`;
/// `CoverageBudget::min_real_sample_block_fraction` checks `actual >=
/// bound`) — the field name at each call site carries the direction, not
/// this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateBound {
    pub numerator: DecimalUint,
    pub denominator: DecimalUint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfitDistributionCriteria {
    pub min_positive_net_profit_rows: DecimalUint,
    pub min_positive_net_profit_fraction: RateBound,
    /// Largest allowed magnitude (wei) of any single negative `net_profit`
    /// row. Never a claim about realized PnL — shadow mode never broadcasts
    /// a transaction, so every `net_profit` value is simulated.
    pub max_negative_net_profit_wei: DecimalUint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageBudget {
    /// Minimum distinct block numbers carrying candidate activity, per
    /// required service.
    pub min_distinct_blocks_per_service: DecimalUint,
    /// Minimum fraction of those blocks that must carry at least one real
    /// (`Pass`/`Revert`) preflight sample.
    pub min_real_sample_block_fraction: RateBound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuityBudget {
    /// Maximum gap between consecutive observed block numbers for a service.
    pub max_block_gap: DecimalUint,
    /// Maximum gap between consecutive `recorded_at_unix` timestamps for a
    /// service.
    pub max_wall_clock_gap_seconds: DecimalUint,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowThresholds {
    pub schema_version: String,
    pub required_services: Vec<String>,
    pub min_canonical_blocks: DecimalUint,
    pub min_runtime_seconds: DecimalUint,
    pub min_candidate_rows: DecimalUint,
    pub min_real_preflight_samples: DecimalUint,
    pub coverage_budget: CoverageBudget,
    pub continuity_budget: ContinuityBudget,
    pub max_error_rate: RateBound,
    pub max_revert_rate: RateBound,
    pub profit_distribution: ProfitDistributionCriteria,
}

/// A [`ShadowThresholds`] that has passed [`validate`], carrying the exact
/// input bytes and their `keccak256` digest alongside the parsed struct so
/// every downstream consumer (gate plan, report, decision) uses the same
/// digest value.
#[derive(Debug, Clone)]
pub struct ValidatedThresholds {
    pub thresholds: ShadowThresholds,
    pub bytes: Vec<u8>,
    pub digest: String,
}

/// Validates raw threshold-artifact bytes: rejects embedded JSON numbers,
/// parses into [`ShadowThresholds`] (which itself rejects unknown fields and
/// malformed decimal strings), checks schema version, required-service
/// shape, rate-bound sanity, and the coverage/canonical cross-check, then
/// digests the exact input bytes.
pub fn validate(bytes: &[u8]) -> Result<ValidatedThresholds, ThresholdSchemaError> {
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    assert_no_numbers(&value, "$")?;

    let thresholds: ShadowThresholds = serde_json::from_value(value)?;

    if thresholds.schema_version != THRESHOLDS_SCHEMA_VERSION {
        return Err(ThresholdSchemaError::UnsupportedSchemaVersion {
            expected: THRESHOLDS_SCHEMA_VERSION.to_string(),
            found: thresholds.schema_version.clone(),
        });
    }

    if thresholds.required_services.is_empty() {
        return Err(ThresholdSchemaError::RequiredServicesEmpty);
    }
    let mut seen = BTreeSet::new();
    for service in &thresholds.required_services {
        if !seen.insert(service.as_str()) {
            return Err(ThresholdSchemaError::DuplicateRequiredService(
                service.clone(),
            ));
        }
    }

    check_rate_bound("max_error_rate", &thresholds.max_error_rate)?;
    check_rate_bound("max_revert_rate", &thresholds.max_revert_rate)?;
    check_rate_bound(
        "coverage_budget.min_real_sample_block_fraction",
        &thresholds.coverage_budget.min_real_sample_block_fraction,
    )?;
    check_rate_bound(
        "profit_distribution.min_positive_net_profit_fraction",
        &thresholds.profit_distribution.min_positive_net_profit_fraction,
    )?;

    let coverage = thresholds
        .coverage_budget
        .min_distinct_blocks_per_service
        .as_u256();
    let canonical = thresholds.min_canonical_blocks.as_u256();
    if coverage > canonical {
        return Err(ThresholdSchemaError::CoverageExceedsCanonicalBlocks {
            coverage: thresholds
                .coverage_budget
                .min_distinct_blocks_per_service
                .value()
                .to_string(),
            canonical: thresholds.min_canonical_blocks.value().to_string(),
        });
    }

    let digest = digest_bytes(bytes);

    Ok(ValidatedThresholds {
        thresholds,
        bytes: bytes.to_vec(),
        digest,
    })
}

fn check_rate_bound(path: &str, bound: &RateBound) -> Result<(), ThresholdSchemaError> {
    if bound.numerator.as_u256() > bound.denominator.as_u256() {
        return Err(ThresholdSchemaError::ImpossibleRateBound {
            path: path.to_string(),
            numerator: bound.numerator.value().to_string(),
            denominator: bound.denominator.value().to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_thresholds_value() -> serde_json::Value {
        serde_json::json!({
            "schema_version": THRESHOLDS_SCHEMA_VERSION,
            "required_services": ["v2_monitor_executor_service", "moe_monitor_executor_service"],
            "min_canonical_blocks": "100",
            "min_runtime_seconds": "3600",
            "min_candidate_rows": "50",
            "min_real_preflight_samples": "20",
            "coverage_budget": {
                "min_distinct_blocks_per_service": "10",
                "min_real_sample_block_fraction": { "numerator": "1", "denominator": "1" }
            },
            "continuity_budget": {
                "max_block_gap": "5",
                "max_wall_clock_gap_seconds": "300"
            },
            "max_error_rate": { "numerator": "1", "denominator": "20" },
            "max_revert_rate": { "numerator": "1", "denominator": "10" },
            "profit_distribution": {
                "min_positive_net_profit_rows": "5",
                "min_positive_net_profit_fraction": { "numerator": "1", "denominator": "2" },
                "max_negative_net_profit_wei": "1000000000000000000"
            }
        })
    }

    fn valid_thresholds_bytes() -> Vec<u8> {
        serde_json::to_vec(&valid_thresholds_value()).unwrap()
    }

    #[test]
    fn validates_a_well_formed_document() {
        let validated = validate(&valid_thresholds_bytes()).unwrap();
        assert_eq!(validated.thresholds.required_services.len(), 2);
        assert!(validated.digest.starts_with("0x"));
    }

    #[test]
    fn digest_is_stable_across_repeated_validation() {
        let bytes = valid_thresholds_bytes();
        let first = validate(&bytes).unwrap();
        let second = validate(&bytes).unwrap();
        assert_eq!(first.digest, second.digest);
    }

    #[test]
    fn digest_changes_with_content() {
        let mut value = valid_thresholds_value();
        let a = validate(&serde_json::to_vec(&value).unwrap()).unwrap();
        value["min_canonical_blocks"] = serde_json::json!("101");
        let b = validate(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let mut value = valid_thresholds_value();
        value["schema_version"] = serde_json::json!("whisker-arb/shadow-thresholds/v0");
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            err,
            ThresholdSchemaError::UnsupportedSchemaVersion { .. }
        ));
    }

    #[test]
    fn rejects_empty_required_services() {
        let mut value = valid_thresholds_value();
        value["required_services"] = serde_json::json!([]);
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            err,
            ThresholdSchemaError::RequiredServicesEmpty
        ));
    }

    #[test]
    fn rejects_duplicate_required_service() {
        let mut value = valid_thresholds_value();
        value["required_services"] = serde_json::json!(["a", "a"]);
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            err,
            ThresholdSchemaError::DuplicateRequiredService(ref s) if s == "a"
        ));
    }

    #[test]
    fn rejects_impossible_rate_bound() {
        let mut value = valid_thresholds_value();
        value["max_error_rate"] = serde_json::json!({"numerator": "5", "denominator": "1"});
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            err,
            ThresholdSchemaError::ImpossibleRateBound { .. }
        ));
    }

    #[test]
    fn rejects_coverage_budget_exceeding_canonical_blocks() {
        let mut value = valid_thresholds_value();
        value["coverage_budget"]["min_distinct_blocks_per_service"] = serde_json::json!("1000");
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(
            err,
            ThresholdSchemaError::CoverageExceedsCanonicalBlocks { .. }
        ));
    }

    #[test]
    fn rejects_undeclared_field_via_deny_unknown_fields() {
        let mut value = valid_thresholds_value();
        value["unexpected_field"] = serde_json::json!("surprise");
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(err, ThresholdSchemaError::Json(_)));
    }

    #[test]
    fn rejects_embedded_json_number() {
        let mut value = valid_thresholds_value();
        value["min_canonical_blocks"] = serde_json::json!(100);
        let err = validate(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(matches!(err, ThresholdSchemaError::Signing(_)));
    }

    #[test]
    fn decimal_uint_rejects_leading_zero() {
        let err = DecimalUint::parse("007").unwrap_err();
        assert!(matches!(err, ThresholdSchemaError::InvalidDecimal { .. }));
    }

    #[test]
    fn decimal_uint_accepts_zero() {
        DecimalUint::parse("0").unwrap();
    }

    #[test]
    fn decimal_uint_rejects_non_digit_characters() {
        let err = DecimalUint::parse("12a").unwrap_err();
        assert!(matches!(err, ThresholdSchemaError::InvalidDecimal { .. }));
    }

    #[test]
    fn decimal_uint_rejects_empty_string() {
        let err = DecimalUint::parse("").unwrap_err();
        assert!(matches!(err, ThresholdSchemaError::InvalidDecimal { .. }));
    }
}
