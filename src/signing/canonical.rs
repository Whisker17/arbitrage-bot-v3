use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::error::SigningError;

/// The producer-side envelope shape: a uniform wrapper around a
/// consumer-defined payload, carrying only the fields this module needs to
/// authorize and verify — never the payload's own signature.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalEnvelope<T> {
    pub schema_version: String,
    pub domain: String,
    pub scope: Value,
    #[serde(flatten)]
    pub payload: T,
}

/// Serializes an arbitrary JSON value as RFC 8785 (JCS) canonical bytes.
///
/// This is the generic primitive: it allows JSON numbers through (needed for
/// RFC 8785 conformance testing). Envelope construction/verification layers on
/// top of this must separately enforce the "no floats/numbers" payload policy
/// via [`assert_no_numbers`].
pub fn canonicalize_value(value: &Value) -> Result<Vec<u8>, SigningError> {
    Ok(serde_json_canonicalizer::to_vec(value)?)
}

/// Rejects any JSON number found anywhere in `value`'s tree.
///
/// Our envelope policy requires all numeric values to be encoded as decimal
/// strings, never JSON numbers/floats. This must be enforced on both the sign
/// path and the verify path — a payload signed elsewhere could embed a raw
/// number that would otherwise canonicalize to itself undetected.
pub fn assert_no_numbers(value: &Value, path: &str) -> Result<(), SigningError> {
    match value {
        Value::Number(_) => Err(SigningError::NumericValueNotAllowed {
            path: path.to_string(),
        }),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                assert_no_numbers(item, &format!("{path}[{i}]"))?;
            }
            Ok(())
        }
        Value::Object(map) => {
            for (key, item) in map {
                assert_no_numbers(item, &format!("{path}.{key}"))?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(()),
    }
}

/// Rejects a top-level `signature` field on an envelope value.
///
/// The signature travels alongside the payload bytes (as a separate detached
/// OpenSSH signature), never embedded inside them. This only inspects the
/// top-level object -- a consumer payload's own nested fields are outside
/// this schema-agnostic module's concern.
pub fn assert_no_signature_field(value: &Value) -> Result<(), SigningError> {
    if let Value::Object(map) = value {
        if map.contains_key("signature") {
            return Err(SigningError::SignatureFieldNotAllowed);
        }
    }
    Ok(())
}

/// Serializes a [`CanonicalEnvelope`] to RFC 8785 canonical bytes, rejecting
/// any JSON number anywhere in the envelope (including inside `payload`) and
/// any top-level `signature` field.
pub fn canonicalize_envelope<T: Serialize>(
    envelope: &CanonicalEnvelope<T>,
) -> Result<Vec<u8>, SigningError> {
    let value = serde_json::to_value(envelope)?;
    assert_no_numbers(&value, "$")?;
    assert_no_signature_field(&value)?;
    canonicalize_value(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_object_keys() {
        let value: Value = serde_json::from_str(r#"{"b":true,"a":"x"}"#).unwrap();
        let out = canonicalize_value(&value).unwrap();
        assert_eq!(out, br#"{"a":"x","b":true}"#);
    }

    #[test]
    fn rejects_number_nested_in_array_in_object() {
        let value: Value = serde_json::from_str(r#"{"a":[1,"x"]}"#).unwrap();
        let err = assert_no_numbers(&value, "$").unwrap_err();
        match err {
            SigningError::NumericValueNotAllowed { path } => assert_eq!(path, "$.a[0]"),
            other => panic!("expected NumericValueNotAllowed, got {other:?}"),
        }
    }

    #[test]
    fn accepts_value_with_no_numbers() {
        let value: Value = serde_json::from_str(r#"{"a":"1","b":[true,null,"x"]}"#).unwrap();
        assert_no_numbers(&value, "$").unwrap();
    }

    /// RFC 8785 Appendix B: IEEE-754 double -> ECMAScript number-literal string.
    #[test]
    fn rfc8785_number_formatting_vectors() {
        let cases: &[(u64, &str)] = &[
            (0x0000000000000000, "0"),
            (0x8000000000000000, "0"),
            (0x0000000000000001, "5e-324"),
            (0x7fefffffffffffff, "1.7976931348623157e+308"),
            (0x4430000000000000, "295147905179352830000"),
            (0x44b52d02c7e14af6, "1e+23"),
            (0x444b1ae4d6e2ef50, "1e+21"),
            (0x3eb0c6f7a0b5ed8d, "0.000001"),
            (0x43143ff3c1cb0959, "1424953923781206.2"),
        ];
        for (bits, expected) in cases {
            let n = f64::from_bits(*bits);
            let value = serde_json::to_value(n).unwrap();
            let out = canonicalize_value(&value).unwrap();
            assert_eq!(
                String::from_utf8(out).unwrap(),
                *expected,
                "bits {bits:#018x}"
            );
        }
    }

    /// RFC 8785 §3.2.2/3.2.3 property-sorting example: object keys sort
    /// lexicographically, numbers reformat per Appendix B, and the string
    /// value's control/backslash/quote characters get canonically escaped
    /// while `/` stays unescaped.
    #[test]
    fn rfc8785_property_sorting_example() {
        // Decoded content per RFC 8785 §3.2.3, built from explicit code points
        // (rather than embedding raw escape text) to keep this source file
        // unambiguous: EURO SIGN, '$', SHIFT-IN (0x0F), LINE FEED, "A'B", '"',
        // '\\', '\\', '"', '/'.
        let control_0f = char::from_u32(0x0f).unwrap();
        let mut string_content = String::new();
        string_content.push('\u{20ac}');
        string_content.push('$');
        string_content.push(control_0f);
        string_content.push('\n');
        string_content.push_str("A'B");
        string_content.push('"');
        string_content.push('\\');
        string_content.push('\\');
        string_content.push('"');
        string_content.push('/');

        let value = serde_json::json!({
            "numbers": [333333333.33333329_f64, 1e30_f64, 4.50_f64, 2e-3_f64, 1e-27_f64],
            "string": string_content.clone(),
            "literals": [null, true, false],
        });

        let out = canonicalize_value(&value).unwrap();
        let out = String::from_utf8(out).unwrap();

        // Independently derive the expected escaping (not by calling our own
        // canonicalizer): RFC 8785 escapes '"' and '\\', uses the short '\n'
        // form for line feed, escapes other control chars as \u00xx, and
        // leaves '/' and non-ASCII characters unescaped.
        let escaped: String = string_content
            .chars()
            .map(|c| match c {
                '"' => "\\\"".to_string(),
                '\\' => "\\\\".to_string(),
                '\n' => "\\n".to_string(),
                c if (c as u32) < 0x20 => format!("\\u{:04x}", c as u32),
                c => c.to_string(),
            })
            .collect();

        let expected = format!(
            "{{\"literals\":[null,true,false],\"numbers\":[333333333.3333333,1e+30,4.5,0.002,1e-27],\"string\":\"{escaped}\"}}"
        );
        assert_eq!(out, expected);
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct SamplePayload {
        amount: String,
    }

    #[test]
    fn canonicalizes_envelope_with_sorted_keys() {
        let envelope = CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "example.domain".to_string(),
            scope: serde_json::json!({"chain_id": "5000"}),
            payload: SamplePayload {
                amount: "100".to_string(),
            },
        };
        let out = canonicalize_envelope(&envelope).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"amount":"100","domain":"example.domain","schema_version":"1","scope":{"chain_id":"5000"}}"#
        );
    }

    #[test]
    fn canonicalize_envelope_rejects_numeric_payload_field() {
        #[derive(Debug, Serialize, Deserialize)]
        struct NumericPayload {
            amount: u64,
        }
        let envelope = CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "example.domain".to_string(),
            scope: serde_json::json!({}),
            payload: NumericPayload { amount: 100 },
        };
        let err = canonicalize_envelope(&envelope).unwrap_err();
        match err {
            SigningError::NumericValueNotAllowed { path } => assert_eq!(path, "$.amount"),
            other => panic!("expected NumericValueNotAllowed, got {other:?}"),
        }
    }

    #[test]
    fn canonicalize_envelope_rejects_embedded_signature_field() {
        #[derive(Debug, Serialize, Deserialize)]
        struct PayloadWithSignature {
            action: String,
            signature: String,
        }
        let envelope = CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "example.domain".to_string(),
            scope: serde_json::json!({}),
            payload: PayloadWithSignature {
                action: "swap".to_string(),
                signature: "deadbeef".to_string(),
            },
        };
        let err = canonicalize_envelope(&envelope).unwrap_err();
        assert!(matches!(err, SigningError::SignatureFieldNotAllowed));
    }

    #[test]
    fn assert_no_signature_field_ignores_non_top_level_signature_keys() {
        // Only the top-level envelope object is inspected -- a nested object
        // happening to use the key "signature" is a consumer's own schema
        // concern, outside this schema-agnostic module's remit.
        let value = serde_json::json!({
            "scope": { "signature": "not-actually-a-signature" },
        });
        assert_no_signature_field(&value).unwrap();
    }

    /// RFC 8785 sorts object keys by UTF-16 code unit value, not by Unicode
    /// code point. A character outside the Basic Multilingual Plane is
    /// encoded in UTF-16 as a surrogate pair starting at 0xD800..=0xDBFF,
    /// which can be numerically *less* than a BMP character's single code
    /// unit even though its code point is numerically greater -- so
    /// code-unit order and code-point order disagree here, and only
    /// code-unit order is RFC 8785-conformant.
    #[test]
    fn rfc8785_sorts_object_keys_by_utf16_code_unit_not_codepoint() {
        let supplementary_key = "\u{1D11E}"; // U+1D11E, UTF-16 surrogate pair D834 DD1E
        let bmp_key = "\u{FFFF}"; // U+FFFF, single UTF-16 code unit FFFF
        assert!(0xD834_u32 < 0xFFFF_u32, "code-unit order: supplementary key sorts first");
        assert!(0x1D11E_u32 > 0xFFFF_u32, "code-point order disagrees: bmp key would sort first");

        let value = serde_json::json!({
            bmp_key: "bmp_value",
            supplementary_key: "supplementary_value",
        });
        let out = String::from_utf8(canonicalize_value(&value).unwrap()).unwrap();

        let pos_supplementary = out.find("supplementary_value").unwrap();
        let pos_bmp = out.find("bmp_value").unwrap();
        assert!(
            pos_supplementary < pos_bmp,
            "expected UTF-16 code-unit key order (supplementary before bmp), got: {out}"
        );
    }

    /// RFC 8785 conformance vector for nested objects: key sorting applies
    /// independently and recursively at every nesting level, not just the
    /// top level.
    #[test]
    fn rfc8785_sorts_nested_object_keys_recursively() {
        let value = serde_json::json!({
            "b": { "z": "1", "a": "2" },
            "a": { "y": { "b": "1", "a": "2" }, "a": "1" },
        });
        let out = canonicalize_value(&value).unwrap();
        assert_eq!(
            out,
            br#"{"a":{"a":"1","y":{"a":"2","b":"1"}},"b":{"a":"2","z":"1"}}"#.to_vec()
        );
    }
}
