//! Signed `GatePlan` artifact (WHI-554): binds a shadow run's acceptance
//! thresholds and the environment digests in effect at plan-creation time
//! (config, gas profile, runtime identity, and the signer policy itself) into
//! one detached-signed record, so a later `shadow_report`/`shadow_decision`
//! can verify exactly what was promised before evidence was generated.
//!
//! Mirrors `execution::preflight`'s `RuntimeScope`/`ApprovalVerifier` pattern:
//! [`ShadowGateScope::to_expected_scope`] parallels
//! `RuntimeScope::to_expected_scope`, and [`GatePlanVerifier`] /
//! [`ProductionGatePlanVerifier`] parallel `ApprovalVerifier` /
//! `ProductionApprovalVerifier`, including the same
//! `#[cfg(feature = "signing-test-util")] with_verifier`-style test seam
//! (here exposed as an injectable [`GatePlanVerifier`] argument rather than a
//! builder method, since this module has no long-lived service struct to
//! attach one to).

use std::path::Path;

use alloy::primitives::keccak256;
use serde::{Deserialize, Serialize};

use crate::signing::{self, CanonicalEnvelope, ExpectedScope, SigningError, VerifiedArtifact};

/// Domain this artifact is signed/verified under. The signing namespace is
/// fixed by the WHI-554/WHI-526 artifact contract; the envelope schema may
/// advance independently as the payload gains fields.
pub const GATE_PLAN_DOMAIN: &str = "whisker-arb/gate-plan/v1";
pub const GATE_PLAN_SCHEMA_VERSION: &str = "whisker-arb/gate-plan/v2";

#[derive(Debug, thiserror::Error)]
pub enum GatePlanError {
    #[error(transparent)]
    Signing(#[from] SigningError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The scope a `GatePlan` is signed and verified against: which chain, which
/// commit of this repository produced the plan, and which services must all
/// report evidence before a decision can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowGateScope {
    pub chain_id: u64,
    pub git_commit: String,
    pub required_services: Vec<String>,
}

impl ShadowGateScope {
    /// `pub(crate)` so `shadow_decision`'s envelope builder can reuse the
    /// identical scope-to-JSON conversion, keeping the two artifact types'
    /// scope encoding from silently diverging.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "chain_id": self.chain_id.to_string(),
            "git_commit": self.git_commit,
            "required_services": self.required_services,
        })
    }

    /// Mirrors `preflight::RuntimeScope::to_expected_scope`: stringifies
    /// `chain_id` (the envelope policy forbids JSON numbers) and otherwise
    /// carries the scope fields verbatim.
    pub fn to_expected_scope(&self) -> Result<ExpectedScope, SigningError> {
        if !is_valid_git_commit(&self.git_commit) {
            return Err(SigningError::InvalidScopeValue {
                field: "git_commit".to_string(),
                value: self.git_commit.clone(),
            });
        }
        ExpectedScope::new(self.to_json())
    }
}

/// A gate plan must identify one concrete Git object. Build environments that cannot
/// resolve Git use `unknown` for ordinary build metadata, but that sentinel is never
/// acceptable as evidence identity.
pub fn is_valid_git_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The signed payload itself. `git_commit` and `required_services` duplicate
/// fields already present in [`ShadowGateScope`] — deliberately, since
/// `VerifiedArtifact<T>` never exposes the scope it was verified against
/// back to the caller, only `payload()`. Anything a downstream consumer
/// (`shadow_report`, `shadow_decision`) needs must live in the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatePlanPayload {
    pub thresholds_digest: String,
    pub git_commit: String,
    pub config_digest: String,
    pub profile_digest: String,
    pub runtime_identity_digest: String,
    pub executor_contract: String,
    pub wmnt_address: String,
    pub override_digest: String,
    /// `keccak256` of the `allowed_signers` file bytes in effect when this
    /// plan was created — "what signing policy was live at plan time".
    pub allowed_signers_digest: String,
    /// `keccak256` of the `revoked_keys` file bytes in effect when this plan
    /// was created.
    pub revoked_keys_digest: String,
    pub required_services: Vec<String>,
}

/// Builds the (unsigned) canonical envelope for a `GatePlan`. Exposed
/// separately from signing so tests can canonicalize/inspect it without
/// spawning `ssh-keygen`.
pub fn build_envelope(
    scope: &ShadowGateScope,
    payload: GatePlanPayload,
) -> CanonicalEnvelope<GatePlanPayload> {
    CanonicalEnvelope {
        schema_version: GATE_PLAN_SCHEMA_VERSION.to_string(),
        domain: GATE_PLAN_DOMAIN.to_string(),
        scope: scope.to_json(),
        payload,
    }
}

/// Signs a `GatePlan` built from `scope`/`payload`, returning the canonical
/// payload bytes and detached signature — the same pair
/// [`crate::signing::sign_envelope`] returns.
pub fn sign(
    private_key_path: &Path,
    scope: &ShadowGateScope,
    payload: GatePlanPayload,
) -> Result<(Vec<u8>, Vec<u8>), GatePlanError> {
    let envelope = build_envelope(scope, payload);
    let (payload_bytes, signature) =
        signing::sign_envelope(private_key_path, GATE_PLAN_DOMAIN, &envelope)?;
    Ok((payload_bytes, signature))
}

/// `keccak256` of `path`'s exact file bytes, hex-encoded `0x`-prefixed. Used
/// to compute `allowed_signers_digest`/`revoked_keys_digest` live at
/// `GatePlan` creation time.
pub fn digest_file_bytes(path: &Path) -> Result<String, GatePlanError> {
    let bytes = std::fs::read(path)?;
    Ok(digest_bytes(&bytes))
}

/// `keccak256` of `bytes`, hex-encoded `0x`-prefixed. Reused by
/// `shadow_thresholds`, `shadow_report`, and the `shadow_decision` CLI rather
/// than each defining its own copy — note this is *not* the crate's only
/// `to_hex0x(keccak256(...))` implementation: `gas_profile::bytes_to_hex` and
/// `breaker::coordinator::encode_hex` are pre-existing, near-duplicate
/// reimplementations of the same pattern elsewhere in the crate (see
/// `docs/DEFERRED_ISSUES.md` DI-29).
pub fn digest_bytes(bytes: &[u8]) -> String {
    to_hex0x(keccak256(bytes).as_slice())
}

fn to_hex0x(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Verification seam so tests can inject [`crate::signing::verify_with_paths`]
/// (temp fixture trust roots) instead of the production
/// [`crate::signing::verify`] entrypoint, mirroring
/// `preflight::ApprovalVerifier`.
pub trait GatePlanVerifier: Send + Sync {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<GatePlanPayload>, SigningError>;
}

/// Production verifier: always resolves the code-constant, committed trust
/// roots via [`crate::signing::verify`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionGatePlanVerifier;

impl GatePlanVerifier for ProductionGatePlanVerifier {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<GatePlanPayload>, SigningError> {
        let verified: VerifiedArtifact<GatePlanPayload> = signing::verify(
            payload_bytes,
            signature,
            GATE_PLAN_DOMAIN,
            principal,
            accepted_schema_versions,
            expected_scope,
        )?;
        if !is_valid_git_commit(&verified.payload().git_commit) {
            return Err(SigningError::InvalidScopeValue {
                field: "git_commit".to_string(),
                value: verified.payload().git_commit.clone(),
            });
        }
        Ok(verified)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use super::*;
    use crate::signing;

    struct SigningFixture {
        _dir: tempfile::TempDir,
        key_path: PathBuf,
        allowed_signers_path: PathBuf,
        revoked_keys_path: PathBuf,
    }

    fn generate_ed25519_keypair(dir: &Path) -> PathBuf {
        let key_path = dir.join("id_ed25519");
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
        key_path
    }

    fn build_fixture(principal: &str, namespace: &str) -> SigningFixture {
        let dir = tempfile::tempdir().unwrap();
        let key_path = generate_ed25519_keypair(dir.path());
        let pub_key = fs::read_to_string(key_path.with_extension("pub")).unwrap();

        let allowed_signers_path = dir.path().join("allowed_signers");
        fs::write(
            &allowed_signers_path,
            format!("{principal} namespaces=\"{namespace}\" {pub_key}"),
        )
        .unwrap();

        let revoked_keys_path = dir.path().join("revoked_keys");
        fs::write(&revoked_keys_path, "").unwrap();

        SigningFixture {
            _dir: dir,
            key_path,
            allowed_signers_path,
            revoked_keys_path,
        }
    }

    struct TestGatePlanVerifier {
        allowed_signers_path: PathBuf,
        revoked_keys_path: PathBuf,
    }

    impl GatePlanVerifier for TestGatePlanVerifier {
        fn verify(
            &self,
            payload_bytes: &[u8],
            signature: &[u8],
            principal: &str,
            accepted_schema_versions: &[&str],
            expected_scope: &ExpectedScope,
        ) -> Result<VerifiedArtifact<GatePlanPayload>, SigningError> {
            signing::verify_with_paths(
                payload_bytes,
                signature,
                GATE_PLAN_DOMAIN,
                principal,
                accepted_schema_versions,
                expected_scope,
                &self.allowed_signers_path,
                &self.revoked_keys_path,
            )
        }
    }

    fn test_scope() -> ShadowGateScope {
        ShadowGateScope {
            chain_id: 5003,
            git_commit: "0".repeat(40),
            required_services: vec![
                "v2_monitor_executor_service".to_string(),
                "moe_monitor_executor_service".to_string(),
            ],
        }
    }

    fn test_payload() -> GatePlanPayload {
        GatePlanPayload {
            thresholds_digest: "0xaa".to_string(),
            git_commit: "0".repeat(40),
            config_digest: "0xbb".to_string(),
            profile_digest: "0xcc".to_string(),
            runtime_identity_digest: "0xdd".to_string(),
            executor_contract: "0x11".to_string(),
            wmnt_address: "0x22".to_string(),
            override_digest: "0x33".to_string(),
            allowed_signers_digest: "0xee".to_string(),
            revoked_keys_digest: "0xff".to_string(),
            required_services: vec![
                "v2_monitor_executor_service".to_string(),
                "moe_monitor_executor_service".to_string(),
            ],
        }
    }

    #[test]
    fn sign_then_verify_round_trip_succeeds() {
        let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) = sign(&fx.key_path, &scope, test_payload()).unwrap();

        let verifier = TestGatePlanVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let verified = verifier
            .verify(
                &payload_bytes,
                &signature,
                "operator",
                &[GATE_PLAN_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap();
        assert_eq!(verified.payload().thresholds_digest, "0xaa");
        assert_eq!(verified.domain(), GATE_PLAN_DOMAIN);
    }

    #[test]
    fn verify_rejects_scope_mismatch() {
        let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) = sign(&fx.key_path, &scope, test_payload()).unwrap();

        let verifier = TestGatePlanVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let mut wrong_scope = scope.clone();
        wrong_scope.chain_id = 1;
        let expected_scope = wrong_scope.to_expected_scope().unwrap();
        let err = verifier
            .verify(
                &payload_bytes,
                &signature,
                "operator",
                &[GATE_PLAN_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::ScopeMismatch));
    }

    #[test]
    fn verify_rejects_signature_from_an_unauthorized_namespace() {
        // `sign` always signs in the `GATE_PLAN_DOMAIN` namespace regardless
        // of what `allowed_signers` permits -- OpenSSH only enforces the
        // namespace restriction at verify time. Here the fixture's
        // `allowed_signers` entry authorizes a *different* namespace, so
        // signing succeeds but verification must fail -- mirrors
        // tests/signing.rs's cross-namespace rejection case.
        let fx = build_fixture("operator", "whisker-arb/some-other-domain/v1");
        let scope = test_scope();
        let (payload_bytes, signature) = sign(&fx.key_path, &scope, test_payload()).unwrap();

        let verifier = TestGatePlanVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let err = verifier
            .verify(
                &payload_bytes,
                &signature,
                "operator",
                &[GATE_PLAN_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
    }

    #[test]
    fn verify_rejects_tampered_payload_bytes() {
        let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) = sign(&fx.key_path, &scope, test_payload()).unwrap();
        let mut tampered = payload_bytes.clone();
        tampered.push(b' ');

        let verifier = TestGatePlanVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let err = verifier
            .verify(
                &tampered,
                &signature,
                "operator",
                &[GATE_PLAN_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
    }

    #[test]
    fn digest_file_bytes_is_stable_and_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("allowed_signers");
        fs::write(&path, "operator namespaces=\"x\" ssh-ed25519 AAAA").unwrap();
        let a = digest_file_bytes(&path).unwrap();
        let b = digest_file_bytes(&path).unwrap();
        assert_eq!(a, b);
        assert!(a.starts_with("0x"));

        fs::write(&path, "operator namespaces=\"x\" ssh-ed25519 BBBB").unwrap();
        let c = digest_file_bytes(&path).unwrap();
        assert_ne!(a, c);
    }
}
