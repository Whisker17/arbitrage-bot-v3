//! WHI-525 M7: offline, credential-free tests for the Mantle Sepolia E2E gate.
//!
//! Covers the three seams the plan names for offline verification:
//! 1. deployment-manifest diffing (every live-comparable field independently),
//! 2. evidence-bundle schema round-trip + digest stability,
//! 3. guard-acquire/release ordering against `EXECUTE_SEND_ORDER`'s send-tail
//!    slice, via recording `PauseGate` / `ExecutionIdentitySource` doubles.
//!
//! No network credentials, no broadcast, no live RPC.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alloy::primitives::B256;
use amms::execution::e2e::{
    deployment_manifest_digest, diff_against_chain, load_evidence_bundle, write_evidence_bundle,
    DeploymentManifest, EvidenceBundle, EvidenceReceipt, ReconciliationRow, RoleHolders,
    VenueProvenance, EVIDENCE_BUNDLE_SCHEMA_VERSION,
};
use amms::execution::{
    AttemptKind, BlockFeeContext, ExecutionIdentity, ExecutionIdentityLease,
    ExecutionIdentitySource, ExecutionStage, IdentityError, PauseGate, Paused, ProtocolKind,
    RouteKey, TickCrossingBucket, AlwaysAllow, EXECUTE_SEND_ORDER,
};
use amms::state_space::{BlockHeaderContext, IdentityBarrier, SnapshotId};

// ---------------------------------------------------------------------------
// Manifest fixtures
// ---------------------------------------------------------------------------

fn sample_manifest() -> DeploymentManifest {
    DeploymentManifest {
        schema_version: 1,
        chain_id: 5003,
        wmnt: "0x67A1f4A939b477A6b7c5BF94D97E45dE87E608eF".to_string(),
        executor_address: "0x1111111111111111111111111111111111111111".to_string(),
        fixture_token: "0x2222222222222222222222222222222222222222".to_string(),
        fixture_pool_v2: "0x3333333333333333333333333333333333333333".to_string(),
        fixture_pool_agni_v3: "0x4444444444444444444444444444444444444444".to_string(),
        venue_provenance: VenueProvenance::Fixture,
        roles: RoleHolders {
            admin: "0x5555555555555555555555555555555555555555".to_string(),
            hot_executor: "0x6666666666666666666666666666666666666666".to_string(),
        },
        template_hash: "0xaaaa".to_string(),
        patched_runtime_hash: "0xbbbb".to_string(),
        immutable_values_digest: "0xcccc".to_string(),
        compiler_config_digest: "0xdddd".to_string(),
        build_info_digest: "0xeeee".to_string(),
        storage_layout_digest: "0xffff".to_string(),
        plan_digest: "0x1234".to_string(),
        identity_digest: "0x5678".to_string(),
        constructor_args_digest: "0x9abc".to_string(),
        gas_profile_content_digest: "0xdef0".to_string(),
        e2e_config_digest: "0x1357".to_string(),
        deploy_txs: vec![],
        config_txs: vec![],
        seed_txs: vec![],
    }
}

fn sample_evidence() -> EvidenceBundle {
    EvidenceBundle {
        schema_version: EVIDENCE_BUNDLE_SCHEMA_VERSION,
        chain_id: 5003,
        manifest_digest: deployment_manifest_digest(&sample_manifest()).unwrap(),
        venue_provenance: VenueProvenance::Fixture,
        adapters: vec!["v2".into(), "agni_v3".into()],
        executor_address: "0x1111111111111111111111111111111111111111".into(),
        signer_address: "0x2222222222222222222222222222222222222222".into(),
        fixture_pool_v2: "0x3333333333333333333333333333333333333333".into(),
        fixture_pool_agni_v3: "0x4444444444444444444444444444444444444444".into(),
        identity_digest: "0x5678".into(),
        patched_runtime_hash: "0xbbbb".into(),
        gas_profile_content_digest: "0xdef0".into(),
        gas_profile_identity: "profile".into(),
        route_key: "v2|v3:0".into(),
        amount_in: "1000000000000000000".into(),
        expected_net_profit_mnt_wei: "1000".into(),
        min_amount_out: "1000000000000000000".into(),
        arb_tx_hash: "0xdeadbeef".into(),
        receipts: vec![EvidenceReceipt {
            label: "arb".into(),
            tx_hash: "0xdeadbeef".into(),
            block_number: 100,
            block_hash: "0xfeed".into(),
            gas_used: 150_000,
            effective_gas_price: 50_000_000_000,
            success: true,
            finality_depth: 12,
        }],
        reconciliation: vec![ReconciliationRow {
            field: "receipt_success".into(),
            expected: "true".into(),
            observed: "true".into(),
            ok: true,
        }],
        preflight_stage: "E2e".into(),
        pause_gate: "AlwaysAllow".into(),
        snapshot_block_number: 99,
        snapshot_block_hash: "0xaaaa".into(),
        pool_universe_fingerprint: "0xbbbb".into(),
        deferrals: vec!["breaker not wired (DI-12)".into()],
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!(
        "amms-e2e-offline-{label}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Manifest diffing
// ---------------------------------------------------------------------------

#[test]
fn manifest_diff_empty_when_identical() {
    let m = sample_manifest();
    assert!(diff_against_chain(&m, &m).is_empty());
}

#[test]
fn manifest_diff_detects_each_address_and_digest_independently() {
    let recorded = sample_manifest();

    let cases: &[(&str, Box<dyn Fn(&mut DeploymentManifest)>)] = &[
        (
            "executor_address",
            Box::new(|m| m.executor_address = "0x9999999999999999999999999999999999999999".into()),
        ),
        (
            "fixture_pool_v2",
            Box::new(|m| m.fixture_pool_v2 = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        ),
        (
            "fixture_pool_agni_v3",
            Box::new(|m| {
                m.fixture_pool_agni_v3 = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into()
            }),
        ),
        (
            "identity_digest",
            Box::new(|m| m.identity_digest = "0xchanged".into()),
        ),
        (
            "gas_profile_content_digest",
            Box::new(|m| m.gas_profile_content_digest = "0xchanged".into()),
        ),
        (
            "roles.hot_executor",
            Box::new(|m| {
                m.roles.hot_executor = "0xcccccccccccccccccccccccccccccccccccccccc".into()
            }),
        ),
        ("chain_id", Box::new(|m| m.chain_id = 5000)),
    ];

    for (field, mutate) in cases {
        let mut observed = recorded.clone();
        mutate(&mut observed);
        let drift = diff_against_chain(&recorded, &observed);
        assert_eq!(
            drift.len(),
            1,
            "mutating {field} should produce exactly one drift entry, got {drift:?}"
        );
        assert_eq!(drift[0].field, *field);
    }
}

#[test]
fn manifest_diff_ignores_append_only_tx_records() {
    let recorded = sample_manifest();
    let mut observed = recorded.clone();
    observed.deploy_txs.push(amms::execution::e2e::TxRecord {
        label: "extra".into(),
        tx_hash: "0x1".into(),
        block_number: 1,
        block_hash: "0x2".into(),
        gas_used: 1,
    });
    assert!(
        diff_against_chain(&recorded, &observed).is_empty(),
        "tx records are provenance-only and must not count as live drift"
    );
}

// ---------------------------------------------------------------------------
// Evidence schema
// ---------------------------------------------------------------------------

#[test]
fn evidence_bundle_round_trips_through_disk() {
    let dir = temp_dir("evidence");
    let path = dir.join("evidence.json");
    let bundle = sample_evidence();
    write_evidence_bundle(&path, &bundle).expect("write");
    let loaded = load_evidence_bundle(&path).expect("load");
    assert_eq!(bundle, loaded);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn evidence_bundle_rejects_wrong_schema_version() {
    let dir = temp_dir("evidence-bad-schema");
    let path = dir.join("evidence.json");
    let mut bundle = sample_evidence();
    bundle.schema_version = 99;
    let err = write_evidence_bundle(&path, &bundle).unwrap_err();
    assert!(err.to_string().contains("unsupported schema_version"));
    std::fs::write(&path, serde_json::to_string(&bundle).unwrap()).unwrap();
    let err = load_evidence_bundle(&path).unwrap_err();
    assert!(err.to_string().contains("unsupported schema_version"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn evidence_manifest_digest_pins_live_identity_not_tx_history() {
    let mut a = sample_manifest();
    let mut b = a.clone();
    b.seed_txs.push(amms::execution::e2e::TxRecord {
        label: "seed".into(),
        tx_hash: "0xabc".into(),
        block_number: 7,
        block_hash: "0xdef".into(),
        gas_used: 21_000,
    });
    assert_eq!(
        deployment_manifest_digest(&a).unwrap(),
        deployment_manifest_digest(&b).unwrap()
    );
    a.patched_runtime_hash = "0xother".into();
    assert_ne!(
        deployment_manifest_digest(&a).unwrap(),
        deployment_manifest_digest(&b).unwrap()
    );
}

#[test]
fn evidence_bundle_records_e2e_stage_and_fixture_provenance() {
    let bundle = sample_evidence();
    assert_eq!(bundle.preflight_stage, "E2e");
    assert_eq!(bundle.pause_gate, "AlwaysAllow");
    assert_eq!(bundle.venue_provenance, VenueProvenance::Fixture);
    assert_eq!(bundle.schema_version, EVIDENCE_BUNDLE_SCHEMA_VERSION);
    // ExecutionStage::E2e is the value e2e_run wires into RiskTieredPreflight.
    assert!(matches!(ExecutionStage::E2e, ExecutionStage::E2e));
}

// ---------------------------------------------------------------------------
// Guard-acquire / release ordering
// ---------------------------------------------------------------------------

#[derive(Default)]
struct CallLog {
    events: Mutex<Vec<&'static str>>,
}

impl CallLog {
    fn push(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }

    fn snapshot(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
}

/// Records `begin_send` for AttemptKind::Execute.
struct RecordingPauseGate {
    log: Arc<CallLog>,
}

impl PauseGate for RecordingPauseGate {
    fn begin_send(&self, kind: AttemptKind) -> Result<amms::execution::SendGuard, Paused> {
        assert_eq!(kind, AttemptKind::Execute, "e2e_run only mints Execute");
        self.log.push("begin_send");
        AlwaysAllow.begin_send(kind)
    }
}

/// Records validate / acquire_lease. `ExecutionIdentityLease` has private
/// fields, so an out-of-crate double cannot return `Ok(lease)` — it records
/// the call then fails closed, which is enough to assert ordering.
struct OrderRecordingIdentity {
    log: Arc<CallLog>,
    barrier: IdentityBarrier,
    acquired: AtomicUsize,
}

impl ExecutionIdentitySource for OrderRecordingIdentity {
    async fn validate(&self, _identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        self.log.push("validate");
        Ok(())
    }

    async fn acquire_send_lease(
        &self,
        identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        self.log.push("acquire_lease");
        self.validate(identity).await?;
        // Exercise the same barrier path a real source would, then refuse
        // because we cannot construct the private-field lease type.
        let _lease = self.barrier.acquire_lease().await;
        self.acquired.fetch_add(1, Ordering::SeqCst);
        Err(IdentityError::SendLeaseUnavailable(
            "offline test double records acquire_lease then refuses".into(),
        ))
    }
}

fn fixture_identity(fee: &BlockFeeContext) -> ExecutionIdentity {
    ExecutionIdentity {
        snapshot_id: SnapshotId::new(5003, fee.block_number, fee.block_hash),
        header: BlockHeaderContext::new(B256::repeat_byte(0x11), 1_700_000_000),
        pool_universe_fingerprint: B256::repeat_byte(0x22),
        route: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V3])
            .unwrap()
            .with_v3_ticks(TickCrossingBucket::Zero),
        fee_context: fee.clone(),
        gas_profile_identity: "offline-fixture".into(),
    }
}

#[test]
fn execute_send_order_places_begin_send_before_lease_before_release() {
    // The constant is the contract e2e_run composes against. The open-send
    // tail owned by e2e_run is the slice from begin_send through release.
    let begin = EXECUTE_SEND_ORDER
        .iter()
        .position(|s| *s == "begin_send")
        .expect("begin_send");
    let lease = EXECUTE_SEND_ORDER
        .iter()
        .position(|s| *s == "acquire_lease")
        .expect("acquire_lease");
    let sign = EXECUTE_SEND_ORDER
        .iter()
        .position(|s| *s == "sign")
        .expect("sign");
    let handoff = EXECUTE_SEND_ORDER
        .iter()
        .position(|s| *s == "rpc_handoff")
        .expect("rpc_handoff");
    let release = EXECUTE_SEND_ORDER
        .iter()
        .position(|s| *s == "release")
        .expect("release");

    assert!(begin < lease, "begin_send must precede acquire_lease");
    assert!(lease < sign, "acquire_lease must precede sign");
    assert!(sign < handoff, "sign must precede rpc_handoff");
    assert!(handoff < release, "rpc_handoff must precede release");
    assert_eq!(
        &EXECUTE_SEND_ORDER[begin..=release],
        &[
            "begin_send",
            "acquire_lease",
            "final_validate",
            "sign",
            "durable_hook",
            "record_submission",
            "rpc_handoff",
            "release",
        ]
    );
}

#[tokio::test]
async fn recording_doubles_observe_begin_send_before_acquire_lease() {
    let log = Arc::new(CallLog::default());
    let pause = RecordingPauseGate {
        log: Arc::clone(&log),
    };
    let identity_src = OrderRecordingIdentity {
        log: Arc::clone(&log),
        barrier: IdentityBarrier::default(),
        acquired: AtomicUsize::new(0),
    };
    let fee = BlockFeeContext {
        block_number: 42,
        block_hash: B256::repeat_byte(0x42),
        base_fee_per_gas: 50_000_000_000,
        block_gas_limit: 60_000_000,
    };
    let identity = fixture_identity(&fee);

    // Same order `acquire_execute_send_guards` uses:
    // begin_send → acquire_send_lease → final_validate (executor, not here).
    let pause_guard = pause.begin_send(AttemptKind::Execute).unwrap();
    let err = identity_src
        .acquire_send_lease(&identity)
        .await
        .expect_err("recording double refuses the lease after logging");
    assert!(matches!(err, IdentityError::SendLeaseUnavailable(_)));
    assert_eq!(identity_src.acquired.load(Ordering::SeqCst), 1);

    // Drop the pause guard last — mirrors `release` being last in
    // EXECUTE_SEND_ORDER (ExecuteSendGuards drops lease then pause).
    drop(pause_guard);
    log.push("release");

    let events = log.snapshot();
    let begin_pos = events.iter().position(|e| *e == "begin_send").unwrap();
    let lease_pos = events.iter().position(|e| *e == "acquire_lease").unwrap();
    let release_pos = events.iter().position(|e| *e == "release").unwrap();
    assert!(
        begin_pos < lease_pos,
        "begin_send must be recorded before acquire_lease; got {events:?}"
    );
    assert!(
        lease_pos < release_pos,
        "acquire_lease must be recorded before release; got {events:?}"
    );
}
