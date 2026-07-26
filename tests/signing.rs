use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use amms::signing::{self, scope::ExpectedScope, ssh, CanonicalEnvelope, SigningError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tempfile::TempDir;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct SamplePayload {
    action: String,
}

struct Fixture {
    _dir: TempDir,
    key_path: PathBuf,
    allowed_signers_path: PathBuf,
    revoked_keys_path: PathBuf,
}

fn generate_ed25519_keypair(dir: &Path, name: &str) -> (PathBuf, PathBuf) {
    let key_path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(&key_path)
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg("test")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("failed to spawn ssh-keygen -t ed25519");
    assert!(status.success());
    let pub_key_path = key_path.with_extension("pub");
    (key_path, pub_key_path)
}

/// Builds a fixture with a single ed25519 keypair, a principal named
/// `tester` authorized only for the `test.domain` namespace, and an empty
/// revocation list.
fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (key_path, pub_key_path) = generate_ed25519_keypair(dir.path(), "id_ed25519");
    let pub_key = fs::read_to_string(&pub_key_path).unwrap();

    let allowed_signers_path = dir.path().join("allowed_signers");
    fs::write(
        &allowed_signers_path,
        format!("tester namespaces=\"test.domain\" {pub_key}"),
    )
    .unwrap();

    let revoked_keys_path = dir.path().join("revoked_keys");
    fs::write(&revoked_keys_path, "").unwrap();

    Fixture {
        _dir: dir,
        key_path,
        allowed_signers_path,
        revoked_keys_path,
    }
}

/// Like [`build_fixture`], but the principal is authorized for TWO namespaces
/// (`a.domain,b.domain`) — the setup needed to prove that a signature is bound
/// to the namespace it was made in, independently of allowed-signers
/// authorization.
fn build_multi_namespace_fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let (key_path, pub_key_path) = generate_ed25519_keypair(dir.path(), "id_ed25519");
    let pub_key = fs::read_to_string(&pub_key_path).unwrap();

    let allowed_signers_path = dir.path().join("allowed_signers");
    fs::write(
        &allowed_signers_path,
        format!("tester namespaces=\"a.domain,b.domain\" {pub_key}"),
    )
    .unwrap();

    let revoked_keys_path = dir.path().join("revoked_keys");
    fs::write(&revoked_keys_path, "").unwrap();

    Fixture {
        _dir: dir,
        key_path,
        allowed_signers_path,
        revoked_keys_path,
    }
}

fn envelope_in_domain(domain: &str, scope: Value, payload: Value) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("schema_version".to_string(), json!("1"));
    obj.insert("domain".to_string(), json!(domain));
    obj.insert("scope".to_string(), scope);
    if let Value::Object(payload_obj) = payload {
        for (k, v) in payload_obj {
            obj.insert(k, v);
        }
    } else {
        panic!("payload must be a JSON object");
    }
    Value::Object(obj)
}

fn envelope(scope: Value, payload: Value) -> Value {
    envelope_in_domain("test.domain", scope, payload)
}

fn canonical_bytes(value: &Value) -> Vec<u8> {
    serde_json_canonicalizer::to_vec(value).unwrap()
}

fn expected_scope() -> ExpectedScope {
    ExpectedScope::new(json!({ "chain": "mantle" })).unwrap()
}

/// Happy path driven through the real producer helper (`sign_envelope`)
/// rather than a hand-built envelope, so the producer-side envelope policy
/// (`canonicalize_envelope`: no JSON numbers, no embedded signature field,
/// declared domain == signing namespace) is exercised on the sign path too.
#[test]
fn happy_path_round_trip_exposes_verified_payload() {
    let fx = build_fixture();
    let (payload_bytes, signature) = signing::sign_envelope(
        &fx.key_path,
        "test.domain",
        &CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "test.domain".to_string(),
            scope: json!({ "chain": "mantle" }),
            payload: SamplePayload {
                action: "swap".to_string(),
            },
        },
    )
    .unwrap();

    // The producer helper must emit exactly the canonical bytes the verifier
    // recomputes -- otherwise CanonicalFormMismatch would fire below.
    assert_eq!(
        payload_bytes,
        canonical_bytes(&envelope(
            json!({ "chain": "mantle" }),
            json!({ "action": "swap" })
        ))
    );

    let artifact: signing::VerifiedArtifact<SamplePayload> = signing::verify_with_paths(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap();

    assert_eq!(artifact.schema_version(), "1");
    assert_eq!(artifact.domain(), "test.domain");
    assert_eq!(
        artifact.payload(),
        &SamplePayload {
            action: "swap".to_string()
        }
    );
}

#[test]
fn tampered_payload_is_rejected() {
    let fx = build_fixture();
    let value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let mut tampered = payload_bytes.clone();
    let last = tampered.len() - 2;
    tampered[last] = b'X';

    let err = signing::verify_with_paths::<SamplePayload>(
        &tampered,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn wrong_domain_is_rejected() {
    let fx = build_fixture();
    let value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "other.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn wrong_principal_is_rejected() {
    let fx = build_fixture();
    let value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "someone-else",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn payload_declared_domain_mismatch_is_rejected() {
    // OpenSSH's own namespace check only sees the "-n" flag, never the
    // payload's own "domain" field -- so a payload can be validly signed
    // in the "test.domain" namespace (which the principal IS authorized
    // for) while its self-reported "domain" field claims something else.
    // That must be caught by the Rust-level check in verify_with_paths,
    // independent of and after the OpenSSH signature check succeeds.
    let fx = build_fixture();
    let mut value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    value["domain"] = json!("other.domain");
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    match err {
        SigningError::DomainMismatch { expected, found } => {
            assert_eq!(expected, "test.domain");
            assert_eq!(found, "other.domain");
        }
        other => panic!("expected DomainMismatch, got {other:?}"),
    }
}

#[test]
fn cross_domain_namespace_is_rejected() {
    let fx = build_fixture();
    // Sign for a namespace the principal is not authorized for in
    // allowed_signers (which only grants "test.domain").
    let value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let mut value = value;
    value["domain"] = json!("other.domain");
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "other.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "other.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn revoked_key_is_rejected() {
    let fx = build_fixture();
    let pub_key = fs::read_to_string(fx.key_path.with_extension("pub")).unwrap();
    fs::write(&fx.revoked_keys_path, &pub_key).unwrap();

    let value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    match err {
        SigningError::RevokedKey { stderr } => {
            assert!(stderr.to_lowercase().contains("revoked"));
        }
        other => panic!("expected RevokedKey, got {other:?}"),
    }
}

#[test]
fn missing_scope_field_is_rejected() {
    let fx = build_fixture();
    let value = json!({
        "schema_version": "1",
        "domain": "test.domain",
        "action": "swap",
    });
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::Json(_)));
}

#[test]
fn invalid_scope_is_rejected() {
    let fx = build_fixture();
    let value = envelope(json!("not-an-object"), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::ScopeNotObject));
}

#[test]
fn substituted_scope_is_rejected() {
    let fx = build_fixture();
    let value = envelope(json!({ "chain": "ethereum" }), json!({ "action": "swap" }));
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::ScopeMismatch));
}

#[test]
fn embedded_json_number_bypasses_signature_but_is_rejected() {
    let fx = build_fixture();
    // Bypass canonicalize_envelope entirely: hand-build a payload with a raw
    // JSON number and sign it directly. The bytes are not canonical JCS by
    // our policy but are still a validly-signed, syntactically valid JSON
    // document -- this must be caught by assert_no_numbers inside
    // verify_with_paths, not merely by producer-side discipline.
    let raw = format!(
        "{{\"schema_version\":\"1\",\"domain\":\"test.domain\",\"scope\":{{\"chain\":\"mantle\"}},\"action\":\"swap\",\"amount\":{}}}",
        42
    );
    let payload_bytes = raw.into_bytes();
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct PayloadWithAmount {
        action: String,
        amount: String,
    }

    let err = signing::verify_with_paths::<PayloadWithAmount>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::NumericValueNotAllowed { .. }));
}

#[test]
fn embedded_signature_field_is_rejected() {
    // The signature must travel alongside the payload bytes, never inside
    // them. A validly-signed payload that embeds its own top-level
    // "signature" field must still be rejected before it ever reaches T.
    let fx = build_fixture();
    let mut value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    value["signature"] = json!("deadbeef");
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SignatureFieldNotAllowed));
}

#[test]
fn non_canonical_key_order_is_rejected() {
    let fx = build_fixture();
    // Valid, well-formed JSON with all values as strings, but keys are not
    // in the RFC 8785 sorted order the canonicalizer would produce.
    let raw = "{\"scope\":{\"chain\":\"mantle\"},\"schema_version\":\"1\",\"domain\":\"test.domain\",\"action\":\"swap\"}";
    let payload_bytes = raw.as_bytes().to_vec();
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::CanonicalFormMismatch));
}

/// Cross-domain namespace fixture: the principal is authorized for BOTH
/// `a.domain` and `b.domain` in allowed_signers, so allowed-signers
/// authorization cannot be what rejects this. The artifact is signed in the
/// `a.domain` namespace and verified in `b.domain`; it must still fail,
/// proving the signature itself is namespace-bound rather than merely
/// principal-authorized.
#[test]
fn signature_signed_in_one_authorized_namespace_fails_in_the_other() {
    let fx = build_multi_namespace_fixture();

    // Sanity check: the same principal/key legitimately verifies in a.domain.
    let value_a = envelope_in_domain("a.domain", json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let bytes_a = canonical_bytes(&value_a);
    let sig_a = ssh::sign(&fx.key_path, "a.domain", &bytes_a).unwrap();
    signing::verify_with_paths::<SamplePayload>(
        &bytes_a,
        &sig_a,
        "a.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .expect("principal is authorized for a.domain");

    // ...and, separately, in b.domain when signed there.
    let value_b = envelope_in_domain("b.domain", json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    let bytes_b = canonical_bytes(&value_b);
    let sig_b = ssh::sign(&fx.key_path, "b.domain", &bytes_b).unwrap();
    signing::verify_with_paths::<SamplePayload>(
        &bytes_b,
        &sig_b,
        "b.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .expect("principal is authorized for b.domain");

    // The actual assertion: an a.domain signature replayed against a
    // b.domain verification must fail even though the principal holds both
    // namespaces.
    let err = signing::verify_with_paths::<SamplePayload>(
        &bytes_b,
        &sig_a,
        "b.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn retired_schema_version_is_rejected() {
    // A validly-signed, correctly-domained, correctly-scoped artifact whose
    // schema_version the verifier no longer honours must be a hard failure,
    // not a value silently echoed into VerifiedArtifact.
    let fx = build_fixture();
    let mut value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    value["schema_version"] = json!("0");
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let err = signing::verify_with_paths::<SamplePayload>(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1", "2"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap_err();
    match err {
        SigningError::SchemaVersionNotAccepted { accepted, found } => {
            assert_eq!(accepted, vec!["1".to_string(), "2".to_string()]);
            assert_eq!(found, "0");
        }
        other => panic!("expected SchemaVersionNotAccepted, got {other:?}"),
    }
}

#[test]
fn schema_version_from_accepted_set_is_allowed() {
    let fx = build_fixture();
    let mut value = envelope(json!({ "chain": "mantle" }), json!({ "action": "swap" }));
    value["schema_version"] = json!("2");
    let payload_bytes = canonical_bytes(&value);
    let signature = ssh::sign(&fx.key_path, "test.domain", &payload_bytes).unwrap();

    let artifact: signing::VerifiedArtifact<SamplePayload> = signing::verify_with_paths(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1", "2"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap();
    assert_eq!(artifact.schema_version(), "2");
}

/// A consumer payload with `#[serde(deny_unknown_fields)]` must be able to
/// round-trip: the verifier deserializes `CanonicalEnvelope<T>` so the
/// envelope's own keys are consumed before `T` ever sees the map.
#[test]
fn payload_with_deny_unknown_fields_round_trips() {
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct StrictPayload {
        action: String,
    }

    let fx = build_fixture();
    let (payload_bytes, signature) = signing::sign_envelope(
        &fx.key_path,
        "test.domain",
        &CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "test.domain".to_string(),
            scope: json!({ "chain": "mantle" }),
            payload: StrictPayload {
                action: "swap".to_string(),
            },
        },
    )
    .unwrap();

    let artifact: signing::VerifiedArtifact<StrictPayload> = signing::verify_with_paths(
        &payload_bytes,
        &signature,
        "test.domain",
        "tester",
        &["1"],
        &expected_scope(),
        &fx.allowed_signers_path,
        &fx.revoked_keys_path,
    )
    .unwrap();
    assert_eq!(
        artifact.payload(),
        &StrictPayload {
            action: "swap".to_string()
        }
    );
}

#[test]
fn sign_envelope_rejects_declared_domain_differing_from_signing_namespace() {
    let fx = build_fixture();
    let err = signing::sign_envelope(
        &fx.key_path,
        "test.domain",
        &CanonicalEnvelope {
            schema_version: "1".to_string(),
            domain: "other.domain".to_string(),
            scope: json!({ "chain": "mantle" }),
            payload: SamplePayload {
                action: "swap".to_string(),
            },
        },
    )
    .unwrap_err();
    match err {
        SigningError::DomainMismatch { expected, found } => {
            assert_eq!(expected, "test.domain");
            assert_eq!(found, "other.domain");
        }
        other => panic!("expected DomainMismatch, got {other:?}"),
    }
}
