//! WHI-551: golden vector, fail-closed negative fixtures, and verification tests for
//! `amms::execution::runtime_identity`.

use alloy::primitives::{keccak256, Address};
use amms::execution::runtime_identity::{
    build_export, resolve_immutable_plan, verify_deployed_runtime, BuildEvidence, ImmutableInputs,
    RuntimeIdentityError,
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
    let template_hex = format!("0x{}", hex_of(&template_bytes()));
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

fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
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

    // A plan from a different template no longer verifies against this build's patched bytes.
    let err = verify_deployed_runtime(&patched_bytes_with(WMNT), &different_wmnt).unwrap_err();
    assert!(matches!(err, RuntimeIdentityError::RuntimeMismatch(_)));
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
fn fails_closed_on_an_extra_immutable() {
    let mut fixture = base_fixture();
    fixture["ast"]["nodes"].as_array_mut().unwrap().push(serde_json::json!({
        "id": 8,
        "nodeType": "VariableDeclaration",
        "name": "EXTRA",
        "mutability": "immutable",
        "typeDescriptions": {"typeIdentifier": "t_address", "typeString": "address"},
    }));
    // Extend the template so the extra range fits, then reference it.
    let mut bytes = template_bytes();
    bytes.extend(std::iter::repeat(0u8).take(32));
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex_of(&bytes)));
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
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex_of(&bytes)));
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
    fixture["deployedBytecode"]["object"] = serde_json::json!(format!("0x{}", hex_of(&bytes)));

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
        RuntimeIdentityError::RuntimeMismatch(mismatches) => {
            assert!(!mismatches.is_empty());
            assert!(mismatches.iter().all(|m| (28..48).contains(&m.offset)));
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
        RuntimeIdentityError::RuntimeMismatch(mismatches) => {
            assert_eq!(mismatches.len(), 1);
            assert_eq!(mismatches[0].offset, 0);
            assert_eq!(mismatches[0].expected, 0x11);
            assert_eq!(mismatches[0].observed, 0xff);
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
    assert!(!export.storage_layout_digest.is_empty());
    assert!(!export.identity_digest.is_empty());
}

// ---------------------------------------------------------------------------
// Real committed artifact sanity check
// ---------------------------------------------------------------------------

#[test]
fn real_committed_artifact_resolves_to_exactly_wmnt() {
    let artifact_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("contracts/executor/artifacts");
    let evidence = BuildEvidence::load(&artifact_dir)
        .expect("contracts/executor/artifacts/ArbitrageExecutor.full.json is committed to this repo");
    let mainnet_wmnt = "0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8"
        .parse::<Address>()
        .unwrap();

    let plan = resolve_immutable_plan(&evidence, inputs(mainnet_wmnt), CHAIN_ID)
        .expect("real ArbitrageExecutor artifact must resolve to exactly {WMNT: address}");

    assert_eq!(plan.chain_id(), CHAIN_ID);
    assert_ne!(plan.template_hash(), plan.patched_runtime_hash());
}
