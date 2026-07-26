//! WHI-551: golden vector, fail-closed negative fixtures, and verification tests for
//! `amms::execution::runtime_identity`.

use alloy::hex;
use alloy::primitives::{keccak256, Address};
use amms::execution::runtime_identity::{
    build_export, resolve_immutable_plan, verify_deployed_runtime, BuildEvidence, ImmutableInputs,
    RuntimeIdentityError, MAX_REPORTED_MISMATCHES,
};
use std::path::PathBuf;

const AST_ID: u64 = 7;
const AST_ID_STR: &str = "7";
const WMNT: Address = Address::repeat_byte(0xAA);
const OTHER_WMNT: Address = Address::repeat_byte(0xBB);
const CHAIN_ID: u64 = 5000;

/// 64-byte synthetic template: 16 marker bytes, a 32-byte zero-filled immutable slot
/// at offset 16, then 16 more marker bytes.
fn template_bytes() -> Vec<u8> {
    let mut bytes = vec![0x11u8; 16];
    bytes.extend(std::iter::repeat(0u8).take(32));
    bytes.extend(std::iter::repeat(0x22u8).take(16));
    bytes
}

fn base_fixture() -> serde_json::Value {
    let template_hex = format!("0x{}", hex::encode(template_bytes()));
    serde_json::json!({
        "deployedBytecode": {
            "object": template_hex,
            "immutableReferences": {
                AST_ID_STR: [{"start": 16, "length": 32}],
            },
        },
        "ast": {
            "id": 1,
            "nodeType": "SourceUnit",
            "absolutePath": "Fixture.sol",
            "nodes": [
                {
                    "id": AST_ID,
                    "nodeType": "VariableDeclaration",
                    "name": "WMNT",
                    "mutability": "immutable",
                    "stateVariable": true,
                    "typeDescriptions": {"typeIdentifier": "t_address", "typeString": "address"},
                }
            ],
        },
        "storageLayout": {"storage": []},
        "metadata": {
            "language": "Solidity",
            "compiler": {"version": "0.8.26+commit.fixture"},
            "settings": {
                "remappings": ["forge-std/=/Users/someone/checkout/contracts/lib/forge-std/src/"],
                "compilationTarget": {"Fixture.sol": "Fixture"},
            },
        },
    })
}

fn evidence(fixture: serde_json::Value) -> BuildEvidence {
    BuildEvidence::from_json(fixture).expect("fixture should parse")
}

fn inputs(wmnt: Address) -> ImmutableInputs {
    ImmutableInputs { wmnt }
}

/// The expected patched form of [`template_bytes`] with `wmnt` written into the
/// `[16, 48)` immutable slot, constructed independently of the module's own patch
/// loop so tests cross-check it rather than restate it.
fn patched_bytes_with(wmnt: Address) -> Vec<u8> {
    let mut bytes = vec![0x11u8; 16];
    bytes.extend(std::iter::repeat(0u8).take(12));
    bytes.extend_from_slice(wmnt.as_slice());
    bytes.extend(std::iter::repeat(0x22u8).take(16));
    bytes
}

// ---------------------------------------------------------------------------
// Golden vector
// ---------------------------------------------------------------------------

#[test]
fn golden_vector_patches_bytes_and_derives_expected_digests() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let expected_patched = patched_bytes_with(WMNT);
    assert_eq!(expected_patched.len(), 64);

    assert_eq!(plan.template_hash(), keccak256(template_bytes()));
    assert_eq!(plan.template_length(), 64);
    assert_eq!(plan.patched_runtime_hash(), keccak256(&expected_patched));

    // plan_digest re-derived from the spec's domain-separated preimage, independently
    // of resolve_immutable_plan's internals.
    let mut preimage = Vec::new();
    preimage.extend_from_slice(b"whisker-arb/immutable-plan/v1");
    preimage.push(0x00);
    preimage.extend_from_slice(&CHAIN_ID.to_be_bytes());
    preimage.extend_from_slice(plan.template_hash().as_slice());
    preimage.extend_from_slice(&64u64.to_be_bytes());
    preimage.extend_from_slice(plan.build_info_digest().as_slice());
    preimage.extend_from_slice(plan.compiler_config_digest().as_slice());
    preimage.extend_from_slice(plan.immutable_values_digest().as_slice());
    preimage.extend_from_slice(plan.patched_runtime_hash().as_slice());
    assert_eq!(plan.plan_digest(), keccak256(preimage));

    // identity_digest is only mintable via verify_deployed_runtime.
    let verified = verify_deployed_runtime(&expected_patched, &plan).unwrap();
    let mut identity_preimage = Vec::new();
    identity_preimage.extend_from_slice(b"whisker-arb/executor-runtime-identity/v1");
    identity_preimage.push(0x00);
    identity_preimage.extend_from_slice(&CHAIN_ID.to_be_bytes());
    identity_preimage.extend_from_slice(plan.patched_runtime_hash().as_slice());
    identity_preimage.extend_from_slice(plan.plan_digest().as_slice());
    assert_eq!(verified.identity_digest(), keccak256(identity_preimage));
    assert_eq!(verified.chain_id(), CHAIN_ID);
    assert_eq!(verified.patched_runtime_hash(), plan.patched_runtime_hash());
    assert_eq!(verified.plan_digest(), plan.plan_digest());
}

#[test]
fn golden_vector_checkout_path_normalization_is_reproducible() {
    // Same settings, different absolute checkout prefix -> identical digest.
    let mut alt_fixture = base_fixture();
    alt_fixture["metadata"]["settings"]["remappings"] =
        serde_json::json!(["forge-std/=/home/other-user/elsewhere/contracts/lib/forge-std/src/"]);

    let plan_a = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let plan_b = resolve_immutable_plan(&evidence(alt_fixture), inputs(WMNT), CHAIN_ID).unwrap();

    assert_eq!(plan_a.compiler_config_digest(), plan_b.compiler_config_digest());
    assert_eq!(plan_a.plan_digest(), plan_b.plan_digest());
}

#[test]
fn changing_any_bound_field_changes_the_corresponding_digest() {
    let base = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();

    let different_chain =
        resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID + 1).unwrap();
    assert_ne!(base.plan_digest(), different_chain.plan_digest());

    let different_wmnt =
        resolve_immutable_plan(&evidence(base_fixture()), inputs(OTHER_WMNT), CHAIN_ID).unwrap();
    assert_ne!(base.immutable_values_digest(), different_wmnt.immutable_values_digest());
    assert_ne!(base.patched_runtime_hash(), different_wmnt.patched_runtime_hash());
    assert_ne!(base.plan_digest(), different_wmnt.plan_digest());

    let mut different_build = base_fixture();
    different_build["metadata"]["compiler"]["version"] = serde_json::json!("0.8.26+commit.other");
    let different_build_plan =
        resolve_immutable_plan(&evidence(different_build), inputs(WMNT), CHAIN_ID).unwrap();
    assert_ne!(base.build_info_digest(), different_build_plan.build_info_digest());
    assert_ne!(base.plan_digest(), different_build_plan.plan_digest());

    let mut different_settings = base_fixture();
    different_settings["metadata"]["settings"]["optimizer"] = serde_json::json!({"runs": 999});
    let different_settings_plan =
        resolve_immutable_plan(&evidence(different_settings), inputs(WMNT), CHAIN_ID).unwrap();
    assert_ne!(
        base.compiler_config_digest(),
        different_settings_plan.compiler_config_digest()
    );
    assert_ne!(base.plan_digest(), different_settings_plan.plan_digest());

    let mut different_template = base_fixture();
    let mut altered_bytes = template_bytes();
    altered_bytes[0] = 0x33; // outside the immutable range; still a legitimately different build
    different_template["deployedBytecode"]["object"] =
        serde_json::json!(format!("0x{}", hex::encode(&altered_bytes)));
    let different_template_plan =
        resolve_immutable_plan(&evidence(different_template), inputs(WMNT), CHAIN_ID).unwrap();
    assert_ne!(base.template_hash(), different_template_plan.template_hash());
    assert_ne!(
        base.patched_runtime_hash(),
        different_template_plan.patched_runtime_hash()
    );
    assert_ne!(base.plan_digest(), different_template_plan.plan_digest());

    // A plan from a different template no longer verifies against this build's patched
    // bytes: `different_template_plan` expects 0x33 at offset 0, this build has 0x11.
    let err =
        verify_deployed_runtime(&patched_bytes_with(WMNT), &different_template_plan).unwrap_err();
    match err {
        RuntimeIdentityError::RuntimeMismatch { total, offsets } => {
            assert_eq!(total, 1);
            assert_eq!(offsets[0].offset, 0);
            assert_eq!(offsets[0].expected, 0x33);
            assert_eq!(offsets[0].observed, 0x11);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }

    // ...and neither does a plan bound to a different immutable value.
    let err = verify_deployed_runtime(&patched_bytes_with(WMNT), &different_wmnt).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::RuntimeMismatch { .. }
    ));
}

// ---------------------------------------------------------------------------
// Fail-closed negative fixtures
// ---------------------------------------------------------------------------

#[test]
fn fails_closed_on_missing_build_evidence() {
    let err = BuildEvidence::load(&PathBuf::from("/nonexistent/whi-551-fixture-dir")).unwrap_err();
    assert!(matches!(err, RuntimeIdentityError::MissingEvidence(_)));
}

#[test]
fn fails_closed_on_unknown_ast_id() {
    let mut fixture = base_fixture();
    fixture["deployedBytecode"]["immutableReferences"] =
        serde_json::json!({ "999999": [{"start": 16, "length": 32}] });

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::UnknownAstId { ast_id: 999999 }
    ));
}

#[test]
fn fails_closed_on_an_io_error_reading_the_evidence_file() {
    // A path that exists but cannot be read as a file (here: a directory standing where
    // ArbitrageExecutor.full.json should be) is an IO failure, not "no evidence
    // committed" — the two must not be conflated.
    let dir = std::env::temp_dir().join(format!("whi-551-io-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("ArbitrageExecutor.full.json")).unwrap();

    let err = BuildEvidence::load(&dir).unwrap_err();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        matches!(err, RuntimeIdentityError::EvidenceRead { .. }),
        "expected EvidenceRead, got {err:?}"
    );
}

#[test]
fn fails_closed_on_a_zero_valued_immutable() {
    // wmnt = 0x0 would patch zeroes over an already zero-filled template range, making
    // the "patched" runtime the bare template — i.e. declaring the unpatched template a
    // valid live identity.
    let err =
        resolve_immutable_plan(&evidence(base_fixture()), inputs(Address::ZERO), CHAIN_ID)
            .unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::ZeroImmutableValue { .. }
    ));
}

#[test]
fn a_valid_plan_never_declares_the_template_a_live_identity() {
    // The backstop behind the zero guard: no plan may ever be minted whose patched
    // runtime hash equals its template hash.
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    assert_ne!(plan.template_hash(), plan.patched_runtime_hash());
    assert!(verify_deployed_runtime(&template_bytes(), &plan).is_err());
}

#[test]
fn fails_closed_on_an_ast_node_that_is_not_an_immutable_state_variable() {
    for (field, value) in [
        ("nodeType", serde_json::json!("FunctionDefinition")),
        ("mutability", serde_json::json!("mutable")),
        ("stateVariable", serde_json::json!(false)),
    ] {
        let mut fixture = base_fixture();
        fixture["ast"]["nodes"][0][field] = value;

        let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
        assert!(
            matches!(err, RuntimeIdentityError::NotAnImmutableStateVariable { ast_id: AST_ID, .. }),
            "expected NotAnImmutableStateVariable for {field}, got {err:?}"
        );
    }
}

#[test]
fn fails_closed_on_an_ast_node_without_a_name() {
    let mut fixture = base_fixture();
    fixture["ast"]["nodes"][0].as_object_mut().unwrap().remove("name");

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::AstNodeMissingField {
            ast_id: AST_ID,
            field: "name"
        }
    ));
}

#[test]
fn fails_closed_on_an_extra_immutable() {
    let mut fixture = base_fixture();
    fixture["ast"]["nodes"].as_array_mut().unwrap().push(serde_json::json!({
        "id": 8,
        "nodeType": "VariableDeclaration",
        "name": "EXTRA",
        "mutability": "immutable",
        "stateVariable": true,
        "typeDescriptions": {"typeIdentifier": "t_address", "typeString": "address"},
    }));
    // Extend the template so the extra range fits, then reference it.
    let mut bytes = template_bytes();
    bytes.extend(std::iter::repeat(0u8).take(32));
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex::encode(&bytes)));
    fixture["deployedBytecode"]["immutableReferences"]["8"] =
        serde_json::json!([{"start": 64, "length": 32}]);

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::UnexpectedImmutableSet { .. }
    ));
}

#[test]
fn fails_closed_on_a_wrong_immutable_name() {
    let mut fixture = base_fixture();
    fixture["ast"]["nodes"][0]["name"] = serde_json::json!("NOT_WMNT");

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::UnexpectedImmutableSet { .. }
    ));
}

#[test]
fn fails_closed_on_a_solidity_type_mismatch() {
    let mut fixture = base_fixture();
    fixture["ast"]["nodes"][0]["typeDescriptions"]["typeIdentifier"] =
        serde_json::json!("t_uint256");

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::UnsupportedImmutableType { .. }
    ));
}

#[test]
fn fails_closed_on_a_range_length_mismatch() {
    let mut fixture = base_fixture();
    fixture["deployedBytecode"]["immutableReferences"][AST_ID_STR] =
        serde_json::json!([{"start": 16, "length": 16}]);

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::ImmutableRangeLengthMismatch { .. }
    ));
}

#[test]
fn fails_closed_on_an_out_of_bounds_range() {
    let mut fixture = base_fixture();
    fixture["deployedBytecode"]["immutableReferences"][AST_ID_STR] =
        serde_json::json!([{"start": 48, "length": 32}]); // template is only 64 bytes

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::RangeOutOfBounds { .. }
    ));
}

#[test]
fn fails_closed_on_overlapping_ranges() {
    let mut fixture = base_fixture();
    // Extend the template and give the single WMNT AST id two overlapping ranges.
    let mut bytes = template_bytes();
    bytes.extend(std::iter::repeat(0u8).take(32));
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex::encode(&bytes)));
    fixture["deployedBytecode"]["immutableReferences"][AST_ID_STR] = serde_json::json!([
        {"start": 16, "length": 32},
        {"start": 40, "length": 32},
    ]);

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::OverlappingRanges { .. }
    ));
}

#[test]
fn fails_closed_on_a_non_zero_template_range() {
    let mut bytes = template_bytes();
    bytes[20] = 0x01; // inside the [16, 48) immutable range
    let mut fixture = base_fixture();
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex::encode(&bytes)));

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::NonZeroTemplateRange { offset: 16 }
    ));
}

#[test]
fn fails_closed_on_a_non_array_ranges_value() {
    let mut fixture = base_fixture();
    // A non-array value where a range list is expected (e.g. a corrupted/hand-edited
    // artifact) must fail closed at parse time, not silently resolve to "no ranges".
    fixture["deployedBytecode"]["immutableReferences"][AST_ID_STR] = serde_json::json!("oops");

    let err = BuildEvidence::from_json(fixture).unwrap_err();
    assert!(matches!(err, RuntimeIdentityError::Json(_)));
}

#[test]
fn fails_closed_on_an_empty_ranges_array() {
    let mut fixture = base_fixture();
    // Syntactically valid JSON, but a declared immutable with zero ranges patches
    // nothing — must fail closed rather than silently produce a no-op plan.
    fixture["deployedBytecode"]["immutableReferences"][AST_ID_STR] = serde_json::json!([]);

    let err = resolve_immutable_plan(&evidence(fixture), inputs(WMNT), CHAIN_ID).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::NoRangesForImmutable { .. }
    ));
}

// ---------------------------------------------------------------------------
// verify_deployed_runtime
// ---------------------------------------------------------------------------

#[test]
fn verify_deployed_runtime_accepts_the_correct_patch() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let patched = patched_bytes_with(WMNT);

    let verified = verify_deployed_runtime(&patched, &plan).unwrap();
    assert_eq!(verified.patched_runtime_hash(), plan.patched_runtime_hash());
}

#[test]
fn verify_deployed_runtime_reports_offsets_for_a_wrong_wmnt() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let wrong = patched_bytes_with(OTHER_WMNT);

    let err = verify_deployed_runtime(&wrong, &plan).unwrap_err();
    match err {
        RuntimeIdentityError::RuntimeMismatch { total, offsets } => {
            assert_eq!(total, 20); // the 20 address bytes of the patched word
            assert_eq!(offsets.len(), MAX_REPORTED_MISMATCHES);
            assert!(offsets.iter().all(|m| (28..48).contains(&m.offset)));
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }
}

#[test]
fn verify_deployed_runtime_reports_an_arbitrary_byte_flip_outside_the_immutable() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let mut patched = patched_bytes_with(WMNT);
    patched[0] = 0xff; // corrupt a byte outside the immutable range

    let err = verify_deployed_runtime(&patched, &plan).unwrap_err();
    match err {
        RuntimeIdentityError::RuntimeMismatch { total, offsets } => {
            assert_eq!(total, 1);
            assert_eq!(offsets.len(), 1);
            assert_eq!(offsets[0].offset, 0);
            assert_eq!(offsets[0].expected, 0x11);
            assert_eq!(offsets[0].observed, 0xff);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }
}

#[test]
fn verify_deployed_runtime_caps_reported_offsets_for_an_unrelated_runtime() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    // Same length, unrelated contract: every one of the 64 bytes diverges (no byte of
    // the patched runtime is 0xff). The total is exact; the reported list stays bounded.
    let unrelated = vec![0xffu8; 64];

    let err = verify_deployed_runtime(&unrelated, &plan).unwrap_err();
    match err {
        RuntimeIdentityError::RuntimeMismatch { total, offsets } => {
            assert_eq!(total, 64);
            assert_eq!(offsets.len(), MAX_REPORTED_MISMATCHES);
            assert_eq!(offsets[0].offset, 0);
        }
        other => panic!("expected RuntimeMismatch, got {other:?}"),
    }
}

#[test]
fn verify_deployed_runtime_rejects_a_length_mismatch() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let too_short = vec![0u8; 10];

    let err = verify_deployed_runtime(&too_short, &plan).unwrap_err();
    assert!(matches!(
        err,
        RuntimeIdentityError::LengthMismatch {
            expected: 64,
            observed: 10
        }
    ));
}

// ---------------------------------------------------------------------------
// Export schema
// ---------------------------------------------------------------------------

#[test]
fn build_export_reports_all_digests_without_constructing_opaque_types() {
    let plan = resolve_immutable_plan(&evidence(base_fixture()), inputs(WMNT), CHAIN_ID).unwrap();
    let export = build_export(&plan);

    assert_eq!(export.schema_version, 1);
    assert_eq!(export.chain_id, CHAIN_ID);
    assert_eq!(export.template_hash, plan.template_hash().to_string());
    assert_eq!(
        export.patched_runtime_hash,
        plan.patched_runtime_hash().to_string()
    );
    assert_eq!(export.wmnt, WMNT.to_string());
    assert_eq!(export.plan_digest, plan.plan_digest().to_string());

    // storage_layout_digest is the digest of the fixture's storageLayout under the
    // module's canonical-JSON rule (no path-like strings here, so normalization and
    // key sorting are identities).
    let expected_storage_layout_digest =
        keccak256(serde_json::to_vec(&serde_json::json!({"storage": []})).unwrap());
    assert_eq!(
        export.storage_layout_digest,
        expected_storage_layout_digest.to_string()
    );

    // identity_digest re-derived from the spec's preimage, not merely "non-empty".
    let mut identity_preimage = Vec::new();
    identity_preimage.extend_from_slice(b"whisker-arb/executor-runtime-identity/v1");
    identity_preimage.push(0x00);
    identity_preimage.extend_from_slice(&CHAIN_ID.to_be_bytes());
    identity_preimage.extend_from_slice(plan.patched_runtime_hash().as_slice());
    identity_preimage.extend_from_slice(plan.plan_digest().as_slice());
    assert_eq!(
        export.identity_digest,
        keccak256(identity_preimage).to_string()
    );
    assert_eq!(
        export.identity_digest,
        verify_deployed_runtime(&patched_bytes_with(WMNT), &plan)
            .unwrap()
            .identity_digest()
            .to_string()
    );
}

// ---------------------------------------------------------------------------
// Real committed artifact sanity check
// ---------------------------------------------------------------------------

const MAINNET_WMNT: &str = "0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8";

fn mainnet_plan_from_the_committed_artifact(
) -> amms::execution::runtime_identity::ValidatedImmutablePlan {
    let artifact_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts/executor/artifacts");
    let evidence = BuildEvidence::load(&artifact_dir)
        .expect("contracts/executor/artifacts/ArbitrageExecutor.full.json is committed to this repo");
    let mainnet_wmnt = MAINNET_WMNT.parse::<Address>().unwrap();

    resolve_immutable_plan(&evidence, inputs(mainnet_wmnt), CHAIN_ID)
        .expect("real ArbitrageExecutor artifact must resolve to exactly {WMNT: address}")
}

#[test]
fn real_committed_artifact_resolves_to_exactly_wmnt() {
    let plan = mainnet_plan_from_the_committed_artifact();

    assert_eq!(plan.chain_id(), CHAIN_ID);
    assert_ne!(plan.template_hash(), plan.patched_runtime_hash());
}

/// Binds the committed build evidence to the committed identity record: re-deriving the
/// plan from `contracts/executor/artifacts/` must reproduce every field of
/// `config/executor_identity.json` byte for byte. Regenerating artifacts without
/// re-running `cargo run --example derive_runtime_identity` therefore fails CI instead
/// of silently drifting away from `WHI501_EXECUTOR_PATCHED_RUNTIME_HASH`, which
/// `gas_runtime.rs` pins to that same file.
#[test]
fn committed_identity_json_matches_a_fresh_derivation_from_the_committed_artifact() {
    let export = build_export(&mainnet_plan_from_the_committed_artifact());

    let identity_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/executor_identity.json");
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&identity_path).expect(
            "config/executor_identity.json is committed to this repo",
        ))
        .expect("config/executor_identity.json is valid JSON");

    assert_eq!(
        serde_json::to_value(&export).unwrap(),
        committed,
        "config/executor_identity.json is stale — re-run \
         `cargo run --example derive_runtime_identity -- --artifact contracts/executor/artifacts/ \
         --wmnt {MAINNET_WMNT} --chain-id {CHAIN_ID} --out config/executor_identity.json`"
    );
}
