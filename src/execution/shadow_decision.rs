//! Signed `Decision` artifact (WHI-554): the human go/no-go verdict on a
//! completed shadow run, binding together the exact `GatePlan` that was
//! promised, the exact per-service ledgers that were produced, and the exact
//! evaluated `ShadowReport` — so a later audit can verify precisely what was
//! approved (or rejected) and by whom, without trusting anything held in
//! memory across process boundaries.
//!
//! Shares [`crate::execution::shadow_gate_plan::ShadowGateScope`] with
//! `GatePlan` rather than defining a second scope type (see the WHI-554
//! plan's design decision #2). Mirrors `shadow_gate_plan`'s
//! sign/verify/`*Verifier` shape exactly, fixed to the Decision domain and
//! schema version.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::execution::shadow_gate_plan::ShadowGateScope;
use crate::signing::{self, CanonicalEnvelope, ExpectedScope, SigningError, VerifiedArtifact};

/// Domain this artifact is signed/verified under, and also its
/// `schema_version` — see `shadow_gate_plan::GATE_PLAN_DOMAIN` for the same
/// one-constant-per-artifact convention.
pub const GATE_DECISION_DOMAIN: &str = "whisker-arb/gate-decision/v1";
pub const GATE_DECISION_SCHEMA_VERSION: &str = "whisker-arb/gate-decision/v1";

#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error(transparent)]
    Signing(#[from] SigningError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("refusing to record an Approve decision: the supplied report is not verdict_eligible")]
    ApproveIneligible,
    #[error(
        "decision principal {found:?} does not match the verified signer principal {expected:?}"
    )]
    PrincipalMismatch { expected: String, found: String },
    #[error("decision unlock criteria do not match the verdict")]
    CriteriaMismatch,
}

/// The human operator's go/no-go verdict. `Reject` is always permitted;
/// `Approve` is refused by [`check_approve_eligibility`] unless the report
/// being approved is itself `verdict_eligible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnlockCriteria {
    ApproveUnlocksGoLiveAndM3ForExistingEntrypointsOnly,
    RejectBlocksGoLiveAndM3,
}

impl UnlockCriteria {
    pub fn for_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Approve => Self::ApproveUnlocksGoLiveAndM3ForExistingEntrypointsOnly,
            Verdict::Reject => Self::RejectBlocksGoLiveAndM3,
        }
    }
}

/// The signed payload itself. `gate_plan_digest`/`ledger_digest`/
/// `report_digest` pin the exact artifacts this verdict was reached over —
/// each is recomputed independently by the CLI from its own inputs and
/// compared before this payload is ever built, so a substituted ledger,
/// gate plan, or report is caught before signing (or, on the `verify` side,
/// before the verdict is trusted).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionPayload {
    pub gate_plan_digest: String,
    pub ledger_digest: String,
    pub report_digest: String,
    pub verdict: Verdict,
    pub unlock_criteria: UnlockCriteria,
    pub decision_principal: String,
    /// `keccak256` of the `allowed_signers` file bytes in effect when this
    /// decision was created — "what signing policy was live at decision
    /// time", recorded independently of `GatePlanPayload::allowed_signers_digest`
    /// since signer policy may have changed between plan creation and
    /// decision time.
    pub allowed_signers_digest: String,
    /// `keccak256` of the `revoked_keys` file bytes in effect when this
    /// decision was created.
    pub revoked_keys_digest: String,
}

/// Refuses `Verdict::Approve` when the report it would be approving is not
/// `verdict_eligible`. `Reject` is always permitted regardless of eligibility
/// — rejecting an ineligible (or eligible) run is never itself a problem.
pub fn check_approve_eligibility(
    verdict: Verdict,
    report_verdict_eligible: bool,
) -> Result<(), DecisionError> {
    match verdict {
        Verdict::Approve if !report_verdict_eligible => Err(DecisionError::ApproveIneligible),
        _ => Ok(()),
    }
}

pub fn check_decision_principal(
    payload: &DecisionPayload,
    verified_principal: &str,
) -> Result<(), DecisionError> {
    if payload.decision_principal == verified_principal {
        Ok(())
    } else {
        Err(DecisionError::PrincipalMismatch {
            expected: verified_principal.to_string(),
            found: payload.decision_principal.clone(),
        })
    }
}

pub fn check_unlock_criteria(payload: &DecisionPayload) -> Result<(), DecisionError> {
    if payload.unlock_criteria == UnlockCriteria::for_verdict(payload.verdict) {
        Ok(())
    } else {
        Err(DecisionError::CriteriaMismatch)
    }
}

/// Builds the (unsigned) canonical envelope for a `Decision`. Exposed
/// separately from signing so tests can canonicalize/inspect it without
/// spawning `ssh-keygen`.
pub fn build_envelope(
    scope: &ShadowGateScope,
    payload: DecisionPayload,
) -> CanonicalEnvelope<DecisionPayload> {
    CanonicalEnvelope {
        schema_version: GATE_DECISION_SCHEMA_VERSION.to_string(),
        domain: GATE_DECISION_DOMAIN.to_string(),
        scope: scope.to_json(),
        payload,
    }
}

/// Signs a `Decision` built from `scope`/`payload`, returning the canonical
/// payload bytes and detached signature — the same pair
/// [`crate::signing::sign_envelope`] returns.
pub fn sign(
    private_key_path: &Path,
    scope: &ShadowGateScope,
    payload: DecisionPayload,
) -> Result<(Vec<u8>, Vec<u8>), DecisionError> {
    check_unlock_criteria(&payload)?;
    let envelope = build_envelope(scope, payload);
    let (payload_bytes, signature) =
        signing::sign_envelope(private_key_path, GATE_DECISION_DOMAIN, &envelope)?;
    Ok((payload_bytes, signature))
}

/// Verification seam so tests can inject [`crate::signing::verify_with_paths`]
/// (temp fixture trust roots) instead of the production
/// [`crate::signing::verify`] entrypoint, mirroring
/// `shadow_gate_plan::GatePlanVerifier`.
pub trait DecisionVerifier: Send + Sync {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<DecisionPayload>, SigningError>;
}

/// Production verifier: always resolves the code-constant, committed trust
/// roots via [`crate::signing::verify`].
#[derive(Debug, Default, Clone, Copy)]
pub struct ProductionDecisionVerifier;

impl DecisionVerifier for ProductionDecisionVerifier {
    fn verify(
        &self,
        payload_bytes: &[u8],
        signature: &[u8],
        principal: &str,
        accepted_schema_versions: &[&str],
        expected_scope: &ExpectedScope,
    ) -> Result<VerifiedArtifact<DecisionPayload>, SigningError> {
        signing::verify(
            payload_bytes,
            signature,
            GATE_DECISION_DOMAIN,
            principal,
            accepted_schema_versions,
            expected_scope,
        )
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

    struct TestDecisionVerifier {
        allowed_signers_path: PathBuf,
        revoked_keys_path: PathBuf,
    }

    impl DecisionVerifier for TestDecisionVerifier {
        fn verify(
            &self,
            payload_bytes: &[u8],
            signature: &[u8],
            principal: &str,
            accepted_schema_versions: &[&str],
            expected_scope: &ExpectedScope,
        ) -> Result<VerifiedArtifact<DecisionPayload>, SigningError> {
            signing::verify_with_paths(
                payload_bytes,
                signature,
                GATE_DECISION_DOMAIN,
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

    fn test_payload(verdict: Verdict) -> DecisionPayload {
        DecisionPayload {
            gate_plan_digest: "0xaa".to_string(),
            ledger_digest: "0xbb".to_string(),
            report_digest: "0xcc".to_string(),
            verdict,
            unlock_criteria: UnlockCriteria::for_verdict(verdict),
            decision_principal: "operator".to_string(),
            allowed_signers_digest: "0xdd".to_string(),
            revoked_keys_digest: "0xee".to_string(),
        }
    }

    #[test]
    fn decision_principal_must_match_verified_signer() {
        let mut payload = test_payload(Verdict::Reject);
        payload.decision_principal = "other-operator".to_string();

        let error = check_decision_principal(&payload, "operator").unwrap_err();
        assert!(matches!(error, DecisionError::PrincipalMismatch { .. }));
    }

    #[test]
    fn unlock_criteria_must_match_verdict() {
        let mut payload = test_payload(Verdict::Reject);
        payload.unlock_criteria =
            UnlockCriteria::ApproveUnlocksGoLiveAndM3ForExistingEntrypointsOnly;

        let error = check_unlock_criteria(&payload).unwrap_err();
        assert!(matches!(error, DecisionError::CriteriaMismatch));
    }

    #[test]
    fn sign_then_verify_round_trip_succeeds() {
        let fx = build_fixture("operator", GATE_DECISION_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) =
            sign(&fx.key_path, &scope, test_payload(Verdict::Reject)).unwrap();

        let verifier = TestDecisionVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let verified = verifier
            .verify(
                &payload_bytes,
                &signature,
                "operator",
                &[GATE_DECISION_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap();
        assert_eq!(verified.payload().verdict, Verdict::Reject);
        assert_eq!(verified.domain(), GATE_DECISION_DOMAIN);
    }

    #[test]
    fn verify_rejects_scope_mismatch() {
        let fx = build_fixture("operator", GATE_DECISION_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) =
            sign(&fx.key_path, &scope, test_payload(Verdict::Approve)).unwrap();

        let verifier = TestDecisionVerifier {
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
                &[GATE_DECISION_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::ScopeMismatch));
    }

    #[test]
    fn verify_rejects_signature_from_an_unauthorized_namespace() {
        // Mirrors `shadow_gate_plan`'s cross-namespace rejection test: `sign`
        // always signs in `GATE_DECISION_DOMAIN`; OpenSSH only enforces the
        // namespace restriction at verify time, so a fixture authorizing a
        // different namespace must still fail verification.
        let fx = build_fixture("operator", "whisker-arb/some-other-domain/v1");
        let scope = test_scope();
        let (payload_bytes, signature) =
            sign(&fx.key_path, &scope, test_payload(Verdict::Reject)).unwrap();

        let verifier = TestDecisionVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let err = verifier
            .verify(
                &payload_bytes,
                &signature,
                "operator",
                &[GATE_DECISION_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
    }

    #[test]
    fn verify_rejects_tampered_payload_bytes() {
        let fx = build_fixture("operator", GATE_DECISION_DOMAIN);
        let scope = test_scope();
        let (payload_bytes, signature) =
            sign(&fx.key_path, &scope, test_payload(Verdict::Reject)).unwrap();
        let mut tampered = payload_bytes.clone();
        tampered.push(b' ');

        let verifier = TestDecisionVerifier {
            allowed_signers_path: fx.allowed_signers_path.clone(),
            revoked_keys_path: fx.revoked_keys_path.clone(),
        };
        let expected_scope = scope.to_expected_scope().unwrap();
        let err = verifier
            .verify(
                &tampered,
                &signature,
                "operator",
                &[GATE_DECISION_SCHEMA_VERSION],
                &expected_scope,
            )
            .unwrap_err();
        assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
    }

    #[test]
    fn check_approve_eligibility_refuses_ineligible_approve() {
        let err = check_approve_eligibility(Verdict::Approve, false).unwrap_err();
        assert!(matches!(err, DecisionError::ApproveIneligible));
    }

    #[test]
    fn check_approve_eligibility_allows_eligible_approve() {
        assert!(check_approve_eligibility(Verdict::Approve, true).is_ok());
    }

    #[test]
    fn check_approve_eligibility_always_allows_reject() {
        assert!(check_approve_eligibility(Verdict::Reject, false).is_ok());
        assert!(check_approve_eligibility(Verdict::Reject, true).is_ok());
    }
}
