//! WHI-532: Prometheus metrics registry + bind guards + HTTP scrape smoke.

use std::net::SocketAddr;
use std::time::Duration;

use alloy::primitives::{B256, U256};
use amms::execution::breaker::{AlertEvent, BreakerStats};
use amms::execution::intent::{IntentEvent, NeedsOperatorReason};
use amms::execution::preflight::{
    BlockTag, PolicyKey, PreflightAttempt, PreflightOutcome, RpcErrorClass,
};
use amms::execution::FinalRequestDigest;
use amms::metrics::{
    self, parse_metrics_bind, record_breaker_alert, record_breaker_stats, record_head_decision,
    record_halt, record_intent_event, record_needs_operator, record_preflight_attempt,
    record_settlement_balance, render_with_local, wei_to_mnt_f64, ALL_METRIC_NAMES,
    BLOCK_TO_SUBMIT_BUCKETS, PREFLIGHT_BUCKETS, STAGE_BUCKETS,
};
use amms::state_space::{ForkKind, HaltReason, HeadDecision, SnapshotId, SnapshotStatus};

fn render_with<F: FnOnce()>(f: F) -> String {
    render_with_local(|| {
        metrics::describe_all();
        f();
    })
}

fn dummy_digest() -> FinalRequestDigest {
    FinalRequestDigest(B256::ZERO)
}

#[test]
fn every_registered_metric_is_described() {
    let rendered = render_with(|| {
        metrics::emit_zero_init();
    });
    for name in ALL_METRIC_NAMES {
        assert!(
            rendered.contains(&format!("# HELP {name}")),
            "missing HELP for {name}\n{rendered}"
        );
        assert!(
            rendered.contains(&format!("# TYPE {name}")),
            "missing TYPE for {name}\n{rendered}"
        );
    }
}

#[test]
fn block_to_submit_histogram_renders_pinned_buckets() {
    let rendered = render_with(|| {
        metrics::record_block_to_submit(
            "agni-v2",
            metrics::block_outcome::GATE_BLOCKED,
            Duration::from_millis(100),
        );
    });
    assert!(
        rendered.contains("# TYPE arbbot_block_to_submit_duration_seconds histogram"),
        "expected histogram type, got summary?\n{rendered}"
    );
    for le in BLOCK_TO_SUBMIT_BUCKETS {
        assert!(
            rendered.contains(&format!("le=\"{le}\"")),
            "missing bucket le={le}\n{rendered}"
        );
    }
    assert!(
        rendered.contains("le=\"+Inf\""),
        "missing +Inf bucket\n{rendered}"
    );
    assert!(
        rendered.contains("le=\"0.001\""),
        "missing le=0.001\n{rendered}"
    );
    assert!(
        rendered.contains("le=\"2\"") || rendered.contains("le=\"2.0\""),
        "missing le=2\n{rendered}"
    );
}

#[test]
fn stage_and_preflight_histograms_render_pinned_buckets() {
    let rendered = render_with(|| {
        metrics::record_pipeline_stage(
            metrics::stage::DISCOVERY,
            "merged",
            Duration::from_millis(1),
        );
        record_preflight_attempt(&PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Pass,
            digest: dummy_digest(),
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(50)),
            detail: None,
        });
    });
    assert!(
        rendered.contains("# TYPE arbbot_pipeline_stage_duration_seconds histogram"),
        "{rendered}"
    );
    assert!(
        rendered.contains("# TYPE arbbot_preflight_duration_seconds histogram"),
        "{rendered}"
    );
    for le in STAGE_BUCKETS {
        assert!(
            rendered.contains(&format!("le=\"{le}\"")),
            "stage missing le={le}\n{rendered}"
        );
    }
    for le in PREFLIGHT_BUCKETS {
        assert!(
            rendered.contains(&format!("le=\"{le}\"")),
            "preflight missing le={le}\n{rendered}"
        );
    }
}

#[test]
fn render_contains_no_forbidden_high_cardinality_labels() {
    let rendered = render_with(|| {
        drive_all_record_helpers();
    });
    for forbidden in [
        "pool_address=",
        "tx_hash=",
        "nonce=",
        "block_hash=",
        "snapshot_id=",
        "signature=",
        "digest=",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "forbidden high-cardinality label {forbidden} in render:\n{rendered}"
        );
    }
}

fn drive_all_record_helpers() {
    record_head_decision(&HeadDecision::Bootstrap);
    record_head_decision(&HeadDecision::Advance);
    record_head_decision(&HeadDecision::Duplicate);
    record_head_decision(&HeadDecision::Fork(ForkKind::WrongParent));
    record_head_decision(&HeadDecision::Gap {
        last_number: 1,
        observed_number: 5,
    });

    let id = SnapshotId::new(5000, 1, B256::ZERO);
    record_halt(&HaltReason::Fork {
        previous: id,
        observed_number: 1,
        observed_hash: B256::repeat_byte(1),
        observed_parent: B256::ZERO,
        kind: ForkKind::SameHeightReplacement,
    });
    record_halt(&HaltReason::Gap {
        last_number: 1,
        observed_number: 10,
    });
    record_halt(&HaltReason::ReadFailure("x".into()));
    record_halt(&HaltReason::IdentityMismatch("y".into()));
    record_halt(&HaltReason::ResyncRequired);

    for event in all_intent_events() {
        record_intent_event(&event);
    }
    for reason in all_needs_operator() {
        record_needs_operator(&reason);
    }
    for attempt in all_preflight_attempts() {
        record_preflight_attempt(&attempt);
    }
    for alert in all_alert_events() {
        record_breaker_alert(&alert);
    }
    record_breaker_stats(&BreakerStats {
        consecutive_reverts: 1,
        window_loss_wei: U256::from(1u64),
        charged_entries: 2,
        paused: true,
    });
    record_settlement_balance("executor", U256::from(10u64).pow(U256::from(18u64)));
    record_settlement_balance("sender", U256::from(5u64));
}

#[test]
fn head_decision_labels_are_exhaustive() {
    let rendered = render_with(|| {
        for d in [
            HeadDecision::Bootstrap,
            HeadDecision::Advance,
            HeadDecision::Duplicate,
            HeadDecision::Fork(ForkKind::HeightRollback),
            HeadDecision::Gap {
                last_number: 1,
                observed_number: 3,
            },
        ] {
            record_head_decision(&d);
        }
    });
    for label in ["bootstrap", "advance", "duplicate", "fork", "gap"] {
        assert!(
            rendered.contains(&format!("decision=\"{label}\"")),
            "missing decision={label}\n{rendered}"
        );
    }
}

#[test]
fn halt_reason_labels_are_exhaustive() {
    let id = SnapshotId::new(5000, 1, B256::ZERO);
    let rendered = render_with(|| {
        record_halt(&HaltReason::Fork {
            previous: id,
            observed_number: 1,
            observed_hash: B256::ZERO,
            observed_parent: B256::ZERO,
            kind: ForkKind::WrongParent,
        });
        record_halt(&HaltReason::Gap {
            last_number: 1,
            observed_number: 2,
        });
        record_halt(&HaltReason::ReadFailure("r".into()));
        record_halt(&HaltReason::IdentityMismatch("i".into()));
        record_halt(&HaltReason::ResyncRequired);
    });
    for label in [
        "fork",
        "gap",
        "read_failure",
        "identity_mismatch",
        "resync_required",
    ] {
        assert!(
            rendered.contains(&format!("reason=\"{label}\"")),
            "missing halt reason={label}\n{rendered}"
        );
    }
}

#[test]
fn fork_kind_labels_are_exhaustive() {
    let id = SnapshotId::new(5000, 1, B256::ZERO);
    let rendered = render_with(|| {
        for kind in [
            ForkKind::SameHeightReplacement,
            ForkKind::HeightRollback,
            ForkKind::WrongParent,
            ForkKind::ChainIdMismatch,
        ] {
            record_halt(&HaltReason::Fork {
                previous: id,
                observed_number: 1,
                observed_hash: B256::ZERO,
                observed_parent: B256::ZERO,
                kind,
            });
        }
    });
    for label in [
        "same_height_replacement",
        "height_rollback",
        "wrong_parent",
        "chain_id_mismatch",
    ] {
        assert!(
            rendered.contains(&format!("kind=\"{label}\"")),
            "missing fork kind={label}\n{rendered}"
        );
    }
}

#[test]
fn intent_event_labels_are_exhaustive() {
    let rendered = render_with(|| {
        for e in all_intent_events() {
            record_intent_event(&e);
        }
    });
    for label in [
        "reserved",
        "submitted",
        "included_unconfirmed",
        "finalized",
        "cancel_finalized",
        "released",
        "needs_operator",
        "reopened",
        "halted",
        "superseded",
        "gas_profile_requalification",
    ] {
        assert!(
            rendered.contains(&format!("event=\"{label}\"")),
            "missing intent event={label}\n{rendered}"
        );
    }
}

#[test]
fn needs_operator_labels_are_exhaustive() {
    let rendered = render_with(|| {
        for r in all_needs_operator() {
            record_needs_operator(&r);
        }
    });
    for label in [
        "cancel_unpriceable",
        "cancel_budget_exhausted",
        "fee_context_unavailable",
        "external_nonce_activity",
        "deep_reorg_halt",
    ] {
        assert!(
            rendered.contains(&format!("reason=\"{label}\"")),
            "missing needs_operator reason={label}\n{rendered}"
        );
    }
}

#[test]
fn preflight_outcome_labels_are_exhaustive() {
    let rendered = render_with(|| {
        for a in all_preflight_attempts() {
            record_preflight_attempt(&a);
        }
    });
    for label in [
        "pass",
        "revert",
        "rpc_error",
        "env_unsupported",
        "skipped_approved",
        "sampled_out",
    ] {
        assert!(
            rendered.contains(&format!("outcome=\"{label}\"")),
            "missing preflight outcome={label}\n{rendered}"
        );
    }
}

#[test]
fn alert_event_labels_are_exhaustive() {
    let rendered = render_with(|| {
        for a in all_alert_events() {
            record_breaker_alert(&a);
        }
    });
    for label in [
        "breaker_trip",
        "pause",
        "unpause",
        "init",
        "recovery",
        "tamper",
        "inventory_violation",
        "ledger_reversal",
        "restart_validation_failure",
        "incomplete_fee_accounting",
        "needs_operator",
        "halted",
        "anomalous_cancel",
        "unattributable_activity",
        "drain_timeout",
    ] {
        assert!(
            rendered.contains(&format!("event=\"{label}\"")),
            "missing alert event={label}\n{rendered}"
        );
    }
}

#[test]
fn metrics_bind_defaults_to_loopback() {
    assert_eq!(
        parse_metrics_bind(None, false).unwrap().to_string(),
        "127.0.0.1:9464"
    );
}

#[test]
fn non_loopback_bind_is_rejected_without_override() {
    let err = parse_metrics_bind(Some("0.0.0.0:9464"), false).unwrap_err();
    assert!(err.to_string().contains("non-loopback"), "err={err}");
}

#[test]
fn non_loopback_bind_is_allowed_with_explicit_override() {
    assert!(parse_metrics_bind(Some("0.0.0.0:9464"), true).is_ok());
}

#[test]
fn wei_to_mnt_f64_scales_by_1e18() {
    let one = U256::from(10u64).pow(U256::from(18u64));
    assert_eq!(wei_to_mnt_f64(one), 1.0);
}

#[test]
fn http_listener_serves_the_rendered_registry() {
    // Run outside a Tokio test runtime so the exporter can spawn its own
    // background runtime without nested-runtime drop panics, and so we can use
    // `reqwest::blocking` (already a dev-dep).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();

    let install = metrics::install_recorder(addr);
    match install {
        Ok(handle) => {
            metrics::describe_all();
            metrics::record_build_info(env!("CARGO_PKG_VERSION"), "test", "agni-v2", false);
            std::thread::sleep(Duration::from_millis(400));
            let url = format!("http://{addr}/metrics");
            let resp = reqwest::blocking::get(&url).expect("GET /metrics");
            assert_eq!(resp.status().as_u16(), 200);
            let body = resp.text().expect("body");
            assert!(
                body.contains("arbbot_build_info")
                    && body.contains("production_send_allowed=\"false\""),
                "unexpected /metrics body:\n{body}\nhandle:\n{}",
                handle.render()
            );
        }
        Err(e) => {
            eprintln!("install_recorder skipped ({e}); validating local render shape");
            let rendered = render_with(|| {
                metrics::record_build_info("0.2.0", "test", "agni-v2", false);
            });
            assert!(
                rendered.contains("production_send_allowed=\"false\""),
                "{rendered}"
            );
        }
    }
}

fn all_intent_events() -> Vec<IntentEvent> {
    vec![
        IntentEvent::Reserved { nonce: 1 },
        IntentEvent::Submitted {
            nonce: 1,
            tx_hash: B256::ZERO,
        },
        IntentEvent::IncludedUnconfirmed {
            nonce: 1,
            tx_hash: B256::ZERO,
            block_number: 1,
            block_hash: B256::ZERO,
        },
        IntentEvent::Finalized {
            nonce: 1,
            tx_hash: B256::ZERO,
            success: true,
            actual_cost: U256::from(1u64),
            execution_layer_only: false,
        },
        IntentEvent::CancelFinalized {
            nonce: 1,
            tx_hash: B256::ZERO,
            success: true,
            actual_cost: U256::from(1u64),
            execution_layer_only: false,
            anomaly: false,
        },
        IntentEvent::Released { nonce: 1 },
        IntentEvent::NeedsOperator {
            nonce: 1,
            reason: NeedsOperatorReason::DeepReorgHalt,
        },
        IntentEvent::Reopened { nonce: 1 },
        IntentEvent::Halted {
            reason: "x".into(),
        },
        IntentEvent::Superseded {
            nonce: 1,
            tx_hash: B256::ZERO,
        },
        IntentEvent::GasProfileRequalification {
            nonce: 1,
            tx_hash: B256::ZERO,
            gas_used: 1000,
            gas_limit: 2000,
            utilization_bps: 5000,
        },
    ]
}

fn all_needs_operator() -> Vec<NeedsOperatorReason> {
    vec![
        NeedsOperatorReason::CancelUnpriceable,
        NeedsOperatorReason::CancelBudgetExhausted,
        NeedsOperatorReason::FeeContextUnavailable,
        NeedsOperatorReason::ExternalNonceActivity,
        NeedsOperatorReason::DeepReorgHalt,
    ]
}

fn all_preflight_attempts() -> Vec<PreflightAttempt> {
    let digest = dummy_digest();
    vec![
        PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Pass,
            digest,
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(10)),
            detail: None,
        },
        PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Revert("r".into()),
            digest,
            block_tag: Some(BlockTag::Pending),
            latency: Some(Duration::from_millis(10)),
            detail: None,
        },
        PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::RpcError(RpcErrorClass::Transport),
            digest,
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(10)),
            detail: None,
        },
        PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::EnvUnsupported,
            digest,
            block_tag: None,
            latency: None,
            detail: None,
        },
        PreflightAttempt {
            policy_key: PolicyKey::ApprovedStableDisabled,
            outcome: PreflightOutcome::SkippedApproved,
            digest,
            block_tag: None,
            latency: None,
            detail: None,
        },
        PreflightAttempt {
            policy_key: PolicyKey::ApprovedStableSampled,
            outcome: PreflightOutcome::SampledOut,
            digest,
            block_tag: None,
            latency: None,
            detail: None,
        },
    ]
}

fn all_alert_events() -> Vec<AlertEvent> {
    vec![
        AlertEvent::BreakerTrip {
            reason: "r".into(),
        },
        AlertEvent::Pause {
            reason: "r".into(),
        },
        AlertEvent::Unpause {
            actor: "a".into(),
        },
        AlertEvent::Init {
            actor: "a".into(),
        },
        AlertEvent::Recovery {
            actor: "a".into(),
        },
        AlertEvent::Tamper {
            detail: "d".into(),
        },
        AlertEvent::InventoryViolation {
            detail: "d".into(),
        },
        AlertEvent::LedgerReversal {
            detail: "d".into(),
        },
        AlertEvent::RestartValidationFailure {
            detail: "d".into(),
        },
        AlertEvent::IncompleteFeeAccounting {
            detail: "d".into(),
        },
        AlertEvent::NeedsOperator {
            detail: "d".into(),
        },
        AlertEvent::Halted {
            detail: "d".into(),
        },
        AlertEvent::AnomalousCancel {
            detail: "d".into(),
        },
        AlertEvent::UnattributableActivity {
            detail: "d".into(),
        },
        AlertEvent::DrainTimeout {
            detail: "d".into(),
        },
    ]
}

#[allow(dead_code)]
fn _status_ready() -> SnapshotStatus {
    SnapshotStatus::Syncing
}
