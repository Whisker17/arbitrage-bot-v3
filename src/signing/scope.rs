use serde_json::Value;

use super::canonical::{assert_no_numbers, canonicalize_value};
use super::error::SigningError;

/// An opaque, consumer-schema-agnostic expected scope for verification.
///
/// Wraps a JSON object and compares against a payload's `scope` field by
/// canonical-byte equality, so this module never needs to know consumer
/// field names.
#[derive(Debug, Clone)]
pub struct ExpectedScope(Value);

impl ExpectedScope {
    /// Builds an expected scope. `scope` must be a JSON object containing no
    /// JSON numbers (our envelope policy: numerics are decimal strings).
    pub fn new(scope: Value) -> Result<Self, SigningError> {
        if !scope.is_object() {
            return Err(SigningError::ScopeNotObject);
        }
        assert_no_numbers(&scope, "$")?;
        Ok(Self(scope))
    }

    pub(crate) fn matches(&self, actual: &Value) -> Result<(), SigningError> {
        let expected_bytes = canonicalize_value(&self.0)?;
        let actual_bytes = canonicalize_value(actual)?;
        if expected_bytes == actual_bytes {
            Ok(())
        } else {
            Err(SigningError::ScopeMismatch)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_non_object_scope() {
        let err = ExpectedScope::new(serde_json::json!(["a"])).unwrap_err();
        assert!(matches!(err, SigningError::ScopeNotObject));
    }

    #[test]
    fn new_rejects_scope_with_embedded_number() {
        let err = ExpectedScope::new(serde_json::json!({"chain_id": 5000})).unwrap_err();
        assert!(matches!(err, SigningError::NumericValueNotAllowed { .. }));
    }

    #[test]
    fn matches_succeeds_on_canonically_equal_scope() {
        let scope = ExpectedScope::new(serde_json::json!({"chain_id": "5000", "pool": "a"}))
            .unwrap();
        let actual = serde_json::json!({"pool": "a", "chain_id": "5000"});
        scope.matches(&actual).unwrap();
    }

    #[test]
    fn matches_fails_on_different_scope() {
        let scope = ExpectedScope::new(serde_json::json!({"chain_id": "5000"})).unwrap();
        let actual = serde_json::json!({"chain_id": "5001"});
        let err = scope.matches(&actual).unwrap_err();
        assert!(matches!(err, SigningError::ScopeMismatch));
    }
}
