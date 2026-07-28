//! WHI-554 end-to-end fixtures: exercises the full
//! `shadow_thresholds` -> `shadow_gate_plan` -> `shadow_report` ->
//! `shadow_decision` chain through the library API (the examples' CLI-only
//! logic -- the sign-overwrite guard and the cross-artifact-digest
//! recompute-and-compare in `examples/shadow_decision.rs`'s `cmd_create` --
//! is intentionally not re-driven here as a subprocess; this file targets
//! what's reachable as library calls, mirroring `tests/signing.rs`'s fixture
//! idiom). Per-module unit tests already cover most schema/evaluation
//! rejection paths in isolation; this file covers what only shows up once
//! the modules are chained together: fresh-reverification discipline,
//! cross-domain principal separation, and digest-substitution detection.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use amms::execution::shadow_decision::{
    self, check_approve_eligibility, DecisionPayload, DecisionVerifier, Verdict,
    GATE_DECISION_DOMAIN, GATE_DECISION_SCHEMA_VERSION,
};
use amms::execution::shadow_gate_plan::{
    self, digest_bytes, GatePlanPayload, GatePlanVerifier, ShadowGateScope, GATE_PLAN_DOMAIN,
    GATE_PLAN_SCHEMA_VERSION,
};
use amms::execution::shadow_report::{evaluate, ledger_digest, LedgerInput};
use amms::execution::shadow_thresholds;
use amms::signing::{self, ExpectedScope, SigningError, VerifiedArtifact};

struct KeyFixture {
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

fn build_fixture(principal: &str, namespaces: &str) -> KeyFixture {
    let dir = tempfile::tempdir().unwrap();
    let key_path = generate_ed25519_keypair(dir.path());
    let pub_key = fs::read_to_string(key_path.with_extension("pub")).unwrap();

    let allowed_signers_path = dir.path().join("allowed_signers");
    fs::write(
        &allowed_signers_path,
        format!("{principal} namespaces=\"{namespaces}\" {pub_key}"),
    )
    .unwrap();

    let revoked_keys_path = dir.path().join("revoked_keys");
    fs::write(&revoked_keys_path, "").unwrap();

    KeyFixture {
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
        chain_id: 5000,
        git_commit: "0".repeat(40),
        required_services: vec!["svc_a".to_string()],
    }
}

/// Lenient-but-nonzero threshold set: every fraction bound is satisfiable by
/// the small hand-built ledgers below (denominator 2, so a single non-real
/// or non-positive row alongside one real/positive row still clears it).
fn thresholds_bytes(required_services: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema_version": shadow_thresholds::THRESHOLDS_SCHEMA_VERSION,
        "required_services": required_services,
        "min_canonical_blocks": "1",
        "min_runtime_seconds": "0",
        "min_candidate_rows": "1",
        "min_real_preflight_samples": "1",
        "coverage_budget": {
            "min_distinct_blocks_per_service": "1",
            "min_real_sample_block_fraction": { "numerator": "1", "denominator": "2" }
        },
        "continuity_budget": {
            "max_block_gap": "1000",
            "max_wall_clock_gap_seconds": "1000000"
        },
        "max_error_rate": { "numerator": "1", "denominator": "1" },
        "max_revert_rate": { "numerator": "1", "denominator": "1" },
        "profit_distribution": {
            "min_positive_net_profit_rows": "1",
            "min_positive_net_profit_fraction": { "numerator": "1", "denominator": "2" },
            "max_negative_net_profit_wei": "1000000000000000000"
        }
    }))
    .unwrap()
}

fn ledger_row(json: serde_json::Value) -> String {
    serde_json::to_string(&json).unwrap()
}

fn header_json(service: &str, threshold_digest: &str, started_at: u64) -> serde_json::Value {
    serde_json::json!({
        "row_type": "run_header",
        "schema_version": "whisker-arb/shadow-ledger/v1",
        "run_id": "run-1",
        "git_commit": "0".repeat(40),
        "chain_id": 5000,
        "service": service,
        "executor_contract": "0x0000000000000000000000000000000000000002",
        "wmnt_address": "0x0000000000000000000000000000000000000003",
        "storage_layout_digest": "0x00",
        "wmnt_descriptor_digest": "0x00",
        "moe_allowlist_digest": "0x00",
        "identity_digest": "0x00",
        "approved_pools_digest": "0x00",
        "threshold_config_digest": threshold_digest,
        "profile_digest": "0x00",
        "override_digest": "0x00",
        "start_identity": null,
        "started_at_unix": started_at,
    })
}

fn candidate_json(digest: &str, outcome: serde_json::Value, recorded_at: u64) -> serde_json::Value {
    serde_json::json!({
        "row_type": "candidate",
        "digest": digest,
        "outcome": outcome,
        "recorded_at_unix": recorded_at,
    })
}

fn context_json(digest: &str, block: u64, net_profit: &str) -> serde_json::Value {
    serde_json::json!({
        "row_type": "context",
        "digest": digest,
        "identity": { "snapshot_id": { "block_number": block } },
        "net_profit": net_profit,
        "profit_basis": "simulated",
    })
}

fn provenance_json(digest: &str) -> serde_json::Value {
    serde_json::json!({
        "row_type": "provenance",
        "digest": digest,
        "outcome": { "verified": {
            "protocol": "uniswap_v2",
            "factory": "0x0000000000000000000000000000000000000001",
            "init_code_hash": format!("0x{}", "11".repeat(32)),
            "salt": format!("0x{}", "22".repeat(32)),
        } },
    })
}

/// One passing row-group (`Pass`, positive `net_profit`) for `svc_a` at
/// block 100 -- satisfies every threshold in [`thresholds_bytes`] on its own.
fn passing_ledger(threshold_digest: &str) -> Vec<u8> {
    let lines = vec![
        ledger_row(header_json("svc_a", threshold_digest, 1_000)),
        ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)),
        ledger_row(context_json("d1", 100, "5")),
        ledger_row(provenance_json("d1")),
    ];
    lines.join("\n").into_bytes()
}

fn sign_and_verify_gate_plan(
    fx: &KeyFixture,
    scope: &ShadowGateScope,
    thresholds_digest: &str,
) -> (Vec<u8>, GatePlanPayload) {
    let payload = GatePlanPayload {
        thresholds_digest: thresholds_digest.to_string(),
        git_commit: scope.git_commit.clone(),
        config_digest: "0xaa".to_string(),
        profile_digest: "0xbb".to_string(),
        runtime_identity_digest: "0xcc".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
        required_services: scope.required_services.clone(),
    };
    let (payload_bytes, signature) = shadow_gate_plan::sign(&fx.key_path, scope, payload).unwrap();

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
    (payload_bytes, verified.payload().clone())
}

#[test]
fn full_chain_gate_plan_report_decision_approve_round_trip() {
    let fx = build_fixture(
        "operator",
        &format!("{GATE_PLAN_DOMAIN},{GATE_DECISION_DOMAIN}"),
    );
    let scope = test_scope();

    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let ledger = passing_ledger(&validated.digest);
    let report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: ledger,
        }],
        scope.chain_id,
    )
    .unwrap();
    assert!(report.verdict_eligible, "{:?}", report.per_service);

    let report_bytes = serde_json::to_vec(&report).unwrap();
    check_approve_eligibility(Verdict::Approve, report.verdict_eligible).unwrap();

    let decision_payload = DecisionPayload {
        gate_plan_digest: report.gate_plan_digest.clone(),
        ledger_digest: report.ledger_digest.clone(),
        report_digest: digest_bytes(&report_bytes),
        verdict: Verdict::Approve,
        decision_principal: "operator".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
    };
    let (decision_bytes, decision_sig) =
        shadow_decision::sign(&fx.key_path, &scope, decision_payload).unwrap();

    let verifier = TestDecisionVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    };
    let expected_scope = scope.to_expected_scope().unwrap();
    let verified = verifier
        .verify(
            &decision_bytes,
            &decision_sig,
            "operator",
            &[GATE_DECISION_SCHEMA_VERSION],
            &expected_scope,
        )
        .unwrap();
    assert_eq!(verified.payload().verdict, Verdict::Approve);
    assert_eq!(verified.payload().gate_plan_digest, report.gate_plan_digest);
    assert_eq!(verified.payload().ledger_digest, report.ledger_digest);
}

#[test]
fn reject_verdict_is_always_signable_even_when_report_is_ineligible() {
    let fx = build_fixture(
        "operator",
        &format!("{GATE_PLAN_DOMAIN},{GATE_DECISION_DOMAIN}"),
    );
    let scope = test_scope();

    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    // A ledger declaring a different required service than the gate plan's
    // `svc_a` is missing entirely -> `MissingServices`, so build a ledger for
    // "svc_a" whose only row is `sampled_out` instead, which is always an
    // invariant violation and forces `verdict_eligible = false`.
    let lines = vec![
        ledger_row(header_json("svc_a", &validated.digest, 1_000)),
        ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "sampled_out"}),
            1_000,
        )),
        ledger_row(context_json("d1", 100, "5")),
        ledger_row(provenance_json("d1")),
    ];
    let ledger = lines.join("\n").into_bytes();

    let report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: ledger,
        }],
        scope.chain_id,
    )
    .unwrap();
    assert!(!report.verdict_eligible);

    check_approve_eligibility(Verdict::Approve, report.verdict_eligible).unwrap_err();
    check_approve_eligibility(Verdict::Reject, report.verdict_eligible).unwrap();

    let report_bytes = serde_json::to_vec(&report).unwrap();
    let decision_payload = DecisionPayload {
        gate_plan_digest: report.gate_plan_digest.clone(),
        ledger_digest: report.ledger_digest.clone(),
        report_digest: digest_bytes(&report_bytes),
        verdict: Verdict::Reject,
        decision_principal: "operator".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
    };
    let (decision_bytes, decision_sig) =
        shadow_decision::sign(&fx.key_path, &scope, decision_payload).unwrap();

    let verifier = TestDecisionVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    };
    let expected_scope = scope.to_expected_scope().unwrap();
    let verified = verifier
        .verify(
            &decision_bytes,
            &decision_sig,
            "operator",
            &[GATE_DECISION_SCHEMA_VERSION],
            &expected_scope,
        )
        .unwrap();
    assert_eq!(verified.payload().verdict, Verdict::Reject);
}

#[test]
fn sampled_out_outcome_is_an_invariant_violation_and_fails_its_service() {
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();
    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let lines = vec![
        ledger_row(header_json("svc_a", &validated.digest, 1_000)),
        ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "sampled_out"}),
            1_000,
        )),
        ledger_row(context_json("d1", 100, "5")),
        ledger_row(provenance_json("d1")),
    ];
    let ledger = lines.join("\n").into_bytes();

    let report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: ledger,
        }],
        scope.chain_id,
    )
    .unwrap();

    assert!(!report.verdict_eligible);
    assert_eq!(report.invariant_violations.len(), 1);
    assert_eq!(report.invariant_violations[0].kind, "sampled_out");
    assert!(!report.per_service["svc_a"].passed);
}

#[test]
fn env_unsupported_outcome_is_reported_but_does_not_block_an_otherwise_passing_run() {
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();
    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let lines = vec![
        ledger_row(header_json("svc_a", &validated.digest, 1_000)),
        ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)),
        ledger_row(context_json("d1", 100, "5")),
        ledger_row(provenance_json("d1")),
        ledger_row(candidate_json(
            "d2",
            serde_json::json!({"kind": "env_unsupported"}),
            1_000,
        )),
        ledger_row(context_json("d2", 101, "0")),
        ledger_row(provenance_json("d2")),
    ];
    let ledger = lines.join("\n").into_bytes();

    let report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: ledger,
        }],
        scope.chain_id,
    )
    .unwrap();

    assert!(report.verdict_eligible, "{:?}", report.per_service);
    assert!(report.invariant_violations.is_empty());
    assert_eq!(report.env_unsupported_count["svc_a"], "1");
    assert_eq!(report.per_service["svc_a"].real_preflight_samples, "1");
}

#[test]
fn substituted_ledger_is_detectable_via_ledger_digest_mismatch() {
    // Mirrors the cross-check `examples/shadow_decision.rs`'s `cmd_create`
    // performs: it recomputes `ledger_digest` from its own `--ledger` inputs
    // and compares against the value already embedded in the trusted
    // `--report`. A substituted ledger file must produce a different digest.
    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let genuine_ledger = passing_ledger(&validated.digest);
    let substituted_ledger = {
        let lines = vec![
            ledger_row(header_json("svc_a", &validated.digest, 1_000)),
            ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)),
            ledger_row(context_json("d1", 100, "999")),
            ledger_row(provenance_json("d1")),
        ];
        lines.join("\n").into_bytes()
    };
    assert_ne!(genuine_ledger, substituted_ledger);

    let genuine_digest =
        ledger_digest(&std::collections::BTreeMap::from([("svc_a".to_string(), genuine_ledger.clone())]));
    let substituted_digest = ledger_digest(&std::collections::BTreeMap::from([(
        "svc_a".to_string(),
        substituted_ledger.clone(),
    )]));
    assert_ne!(genuine_digest, substituted_digest);

    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);
    let report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: genuine_ledger,
        }],
        scope.chain_id,
    )
    .unwrap();

    assert_eq!(report.ledger_digest, genuine_digest);
    assert_ne!(
        report.ledger_digest, substituted_digest,
        "a substituted ledger must be rejected by the recompute-and-compare check"
    );
}

#[test]
fn substituted_report_is_detectable_via_report_digest_mismatch() {
    // Mirrors the second half of `cmd_create`'s cross-check: `report_digest`
    // is computed from the *canonical* form of the `--report` bytes seen at
    // decision-creation time (not the raw file bytes, which need only parse
    // as a `ShadowReport` and may differ in key order/whitespace for the same
    // semantic report), so a report swapped out after the fact hashes
    // differently.
    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();
    let (gate_plan_bytes, gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let genuine_report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: passing_ledger(&validated.digest),
        }],
        scope.chain_id,
    )
    .unwrap();

    let substituted_ledger = {
        let lines = vec![
            ledger_row(header_json("svc_a", &validated.digest, 1_000)),
            ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)),
            ledger_row(context_json("d1", 100, "999")),
            ledger_row(provenance_json("d1")),
        ];
        lines.join("\n").into_bytes()
    };
    let substituted_report = evaluate(
        &gate_plan_bytes,
        &gate_plan,
        &validated,
        &[LedgerInput {
            label: "svc_a.jsonl".to_string(),
            bytes: substituted_ledger,
        }],
        scope.chain_id,
    )
    .unwrap();

    let genuine_digest = digest_bytes(&serde_json::to_vec(&genuine_report).unwrap());
    let substituted_digest = digest_bytes(&serde_json::to_vec(&substituted_report).unwrap());
    assert_ne!(
        genuine_digest, substituted_digest,
        "a substituted report must be rejected by the recompute-and-compare check"
    );
}

#[test]
fn stale_gate_plan_signature_is_rejected_on_fresh_reverification() {
    // Proves `shadow_decision create`'s discipline of never trusting a prior
    // `shadow_gate_plan verify` run: a signature produced over an earlier
    // plan's bytes must not verify against a newer plan's bytes, even when
    // both share the same key, principal, and scope.
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();
    let validated_v1 = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let mut v2_value: serde_json::Value =
        serde_json::from_slice(&thresholds_bytes(&["svc_a"])).unwrap();
    v2_value["min_canonical_blocks"] = serde_json::json!("2");
    let validated_v2 = shadow_thresholds::validate(&serde_json::to_vec(&v2_value).unwrap()).unwrap();
    assert_ne!(validated_v1.digest, validated_v2.digest);

    let payload_v1 = GatePlanPayload {
        thresholds_digest: validated_v1.digest.clone(),
        git_commit: scope.git_commit.clone(),
        config_digest: "0xaa".to_string(),
        profile_digest: "0xbb".to_string(),
        runtime_identity_digest: "0xcc".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
        required_services: scope.required_services.clone(),
    };
    let (_stale_payload_bytes, stale_signature) =
        shadow_gate_plan::sign(&fx.key_path, &scope, payload_v1).unwrap();

    let payload_v2 = GatePlanPayload {
        thresholds_digest: validated_v2.digest.clone(),
        git_commit: scope.git_commit.clone(),
        config_digest: "0xaa".to_string(),
        profile_digest: "0xbb".to_string(),
        runtime_identity_digest: "0xcc".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
        required_services: scope.required_services.clone(),
    };
    let (fresh_payload_bytes, _fresh_signature) =
        shadow_gate_plan::sign(&fx.key_path, &scope, payload_v2).unwrap();

    let verifier = TestGatePlanVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    };
    let expected_scope = scope.to_expected_scope().unwrap();
    let err = verifier
        .verify(
            &fresh_payload_bytes,
            &stale_signature,
            "operator",
            &[GATE_PLAN_SCHEMA_VERSION],
            &expected_scope,
        )
        .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn gate_plan_verify_rejects_a_revoked_key() {
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let pub_key = fs::read_to_string(fx.key_path.with_extension("pub")).unwrap();
    fs::write(&fx.revoked_keys_path, &pub_key).unwrap();

    let scope = test_scope();
    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let payload = GatePlanPayload {
        thresholds_digest: validated.digest.clone(),
        git_commit: scope.git_commit.clone(),
        config_digest: "0xaa".to_string(),
        profile_digest: "0xbb".to_string(),
        runtime_identity_digest: "0xcc".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
        required_services: scope.required_services.clone(),
    };
    let (payload_bytes, signature) = shadow_gate_plan::sign(&fx.key_path, &scope, payload).unwrap();

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
    assert!(matches!(err, SigningError::RevokedKey { .. }));
}

#[test]
fn decision_verify_rejects_a_revoked_key() {
    let fx = build_fixture("operator", GATE_DECISION_DOMAIN);
    let pub_key = fs::read_to_string(fx.key_path.with_extension("pub")).unwrap();
    fs::write(&fx.revoked_keys_path, &pub_key).unwrap();

    let scope = test_scope();
    let payload = DecisionPayload {
        gate_plan_digest: "0xaa".to_string(),
        ledger_digest: "0xbb".to_string(),
        report_digest: "0xcc".to_string(),
        verdict: Verdict::Reject,
        decision_principal: "operator".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
    };
    let (payload_bytes, signature) =
        shadow_decision::sign(&fx.key_path, &scope, payload).unwrap();

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
    assert!(matches!(err, SigningError::RevokedKey { .. }));
}

#[test]
fn principal_authorized_for_gate_plan_domain_only_cannot_verify_a_decision() {
    // `sign` never checks `allowed_signers` -- OpenSSH only enforces the
    // namespace restriction at verify time -- so signing a Decision with a
    // gate-plan-only key succeeds, but verifying it in the Decision domain
    // must fail.
    let fx = build_fixture("operator", GATE_PLAN_DOMAIN);
    let scope = test_scope();

    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (_gate_plan_bytes, _gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let decision_payload = DecisionPayload {
        gate_plan_digest: "0xaa".to_string(),
        ledger_digest: "0xbb".to_string(),
        report_digest: "0xcc".to_string(),
        verdict: Verdict::Approve,
        decision_principal: "operator".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
    };
    let (decision_bytes, decision_sig) =
        shadow_decision::sign(&fx.key_path, &scope, decision_payload).unwrap();

    let verifier = TestDecisionVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    };
    let expected_scope = scope.to_expected_scope().unwrap();
    let err = verifier
        .verify(
            &decision_bytes,
            &decision_sig,
            "operator",
            &[GATE_DECISION_SCHEMA_VERSION],
            &expected_scope,
        )
        .unwrap_err();
    assert!(matches!(err, SigningError::SshVerifyFailed { .. }));
}

#[test]
fn principal_authorized_for_both_domains_can_sign_and_verify_both_artifacts() {
    let fx = build_fixture(
        "operator",
        &format!("{GATE_PLAN_DOMAIN},{GATE_DECISION_DOMAIN}"),
    );
    let scope = test_scope();

    let validated = shadow_thresholds::validate(&thresholds_bytes(&["svc_a"])).unwrap();
    let (_gate_plan_bytes, _gate_plan) =
        sign_and_verify_gate_plan(&fx, &scope, &validated.digest);

    let decision_payload = DecisionPayload {
        gate_plan_digest: "0xaa".to_string(),
        ledger_digest: "0xbb".to_string(),
        report_digest: "0xcc".to_string(),
        verdict: Verdict::Approve,
        decision_principal: "operator".to_string(),
        allowed_signers_digest: "0xdd".to_string(),
        revoked_keys_digest: "0xee".to_string(),
    };
    let (decision_bytes, decision_sig) =
        shadow_decision::sign(&fx.key_path, &scope, decision_payload).unwrap();

    let verifier = TestDecisionVerifier {
        allowed_signers_path: fx.allowed_signers_path.clone(),
        revoked_keys_path: fx.revoked_keys_path.clone(),
    };
    let expected_scope = scope.to_expected_scope().unwrap();
    let verified = verifier
        .verify(
            &decision_bytes,
            &decision_sig,
            "operator",
            &[GATE_DECISION_SCHEMA_VERSION],
            &expected_scope,
        )
        .unwrap();
    assert_eq!(verified.payload().verdict, Verdict::Approve);
}
