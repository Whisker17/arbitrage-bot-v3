//! Post-run evaluation of a completed shadow-mode ledger against a signed
//! `GatePlan`'s thresholds (WHI-554).
//!
//! Parses ledger JSONL through local wire-mirror row types rather than
//! `execution::shadow::ledger`'s real ones: those are `pub(crate)` inside a
//! private `mod ledger;` and unreachable from a sibling module (see
//! `docs/DEFERRED_ISSUES.md` DI-27). The wire mirrors intentionally omit any
//! field this module doesn't consume — serde ignores unknown JSON fields on a
//! struct without `deny_unknown_fields`, so the real ledger schema is free to
//! carry more than what's mirrored here. `ProfitBasis`, `PoolProvenanceOutcome`,
//! and `Create2Proof` are reused directly since those three are genuinely
//! `pub` from `shadow::mod`.
//!
//! Verifying the `GatePlan` signature itself is the caller's job (mirroring
//! `shadow_gate_plan`'s own verifier seam) — [`evaluate`] takes an
//! already-verified [`GatePlanPayload`] plus the exact canonical bytes it was
//! verified from, so this module's evaluation logic can be unit-tested without
//! spawning `ssh-keygen`.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

use crate::execution::shadow::{PoolProvenanceOutcome, ProfitBasis};
use crate::execution::shadow_gate_plan::{digest_bytes, GatePlanPayload, ShadowGateScope};
use crate::execution::shadow_thresholds::{DecimalUint, RateBound, ValidatedThresholds};

pub const REPORT_SCHEMA_VERSION: &str = "whisker-arb/shadow-report/v1";

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("malformed ledger row at line {line} of service ledger {path:?}: {source}")]
    MalformedRow {
        path: String,
        line: usize,
        source: serde_json::Error,
    },
    #[error("ledger file {path:?} is empty")]
    EmptyLedger { path: String },
    #[error("ledger file {path:?} does not start with a run_header row")]
    MissingRunHeader { path: String },
    #[error(
        "ledger {path:?} schema_version {found:?} unsupported (expected {expected:?})"
    )]
    UnsupportedLedgerSchemaVersion {
        path: String,
        expected: String,
        found: String,
    },
    #[error(
        "gate_plan.thresholds_digest ({gate_plan_digest}) does not match the supplied --thresholds digest ({thresholds_digest})"
    )]
    ThresholdsDigestMismatch {
        gate_plan_digest: String,
        thresholds_digest: String,
    },
    #[error(
        "ledger {path:?} for service {service:?} has threshold_config_digest {found:?}, expected {expected:?}"
    )]
    ThresholdConfigDigestMismatch {
        path: String,
        service: String,
        expected: String,
        found: String,
    },
    #[error("ledger {path:?} declares service {service:?}, which is not in the gate plan's required_services")]
    UnknownService { path: String, service: String },
    #[error("more than one ledger file was supplied for service {0:?}")]
    DuplicateService(String),
    #[error("no ledger file was supplied for required service(s): {0:?}")]
    MissingServices(Vec<String>),
    #[error(
        "ledger {path:?} for service {service:?} has chain_id {found}, expected {expected} (the gate plan's verified scope)"
    )]
    ChainIdMismatch {
        path: String,
        service: String,
        expected: u64,
        found: u64,
    },
    #[error(
        "ledger {path:?} for service {service:?} has git_commit {found:?}, expected {expected:?} (the gate plan's git_commit)"
    )]
    GitCommitMismatch {
        path: String,
        service: String,
        expected: String,
        found: String,
    },
    #[error(
        "ledger {path:?} for service {service:?} ran under gas-profile digest {found:?}, but the gate plan pinned {expected:?}"
    )]
    ProfileDigestMismatch {
        path: String,
        service: String,
        expected: String,
        found: String,
    },
    #[error(
        "ledger {path:?} for service {service:?} ran under runtime-identity digest {found:?}, but the gate plan pinned {expected:?}"
    )]
    RuntimeIdentityDigestMismatch {
        path: String,
        service: String,
        expected: String,
        found: String,
    },
    #[error("service {service:?} context row {digest:?} has a malformed net_profit value {value:?}")]
    MalformedNetProfit {
        service: String,
        digest: String,
        value: String,
    },
    #[error("ledger {path:?} has more than one {kind} row for digest {digest:?}")]
    DuplicateLedgerRow {
        path: String,
        kind: String,
        digest: String,
    },
}

/// One `--ledger <path>` input: the raw file bytes plus a caller-supplied
/// label (typically the path) used only in error messages.
pub struct LedgerInput {
    pub label: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireRpcErrorClass {
    Transport,
    ErrorResponse,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireOutcome {
    Pass,
    Revert {
        #[allow(dead_code)]
        reason: String,
    },
    RpcError {
        #[allow(dead_code)]
        class: WireRpcErrorClass,
    },
    EnvUnsupported,
    SkippedApproved,
    SampledOut,
}

#[derive(Debug, Clone, Deserialize)]
struct WireSnapshotId {
    block_number: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireExecutionIdentity {
    snapshot_id: WireSnapshotId,
}

#[derive(Debug, Clone, Deserialize)]
struct WireRunHeader {
    schema_version: String,
    service: String,
    git_commit: String,
    chain_id: u64,
    threshold_config_digest: String,
    /// `manifest.profile_digest` — the gas-profile artifact this run's
    /// fee/margin policy was actually built from. Cross-checked against the
    /// `GatePlan`'s `profile_digest`, which is the whole reason that field is
    /// pinned at plan-creation time.
    profile_digest: String,
    /// `manifest.identity_digest` — i.e.
    /// `runtime_identity::VerifiedRuntimeIdentity::identity_digest()`, the
    /// verified runtime identity this run started against. Cross-checked
    /// against the `GatePlan`'s `runtime_identity_digest` (the ledger header
    /// spells the same value `identity_digest`).
    identity_digest: String,
    started_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireCandidateRow {
    digest: String,
    outcome: WireOutcome,
    recorded_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireContextRow {
    digest: String,
    identity: WireExecutionIdentity,
    net_profit: String,
    #[allow(dead_code)]
    profit_basis: ProfitBasis,
}

#[derive(Debug, Clone, Deserialize)]
struct WireProvenanceRow {
    digest: String,
    outcome: PoolProvenanceOutcome,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "row_type", rename_all = "snake_case")]
enum WireLedgerRow {
    RunHeader(WireRunHeader),
    Candidate(WireCandidateRow),
    Context(WireContextRow),
    Provenance(WireProvenanceRow),
}

const LEDGER_SCHEMA_VERSION: &str = "whisker-arb/shadow-ledger/v1";

struct ParsedLedger {
    header: WireRunHeader,
    candidates: Vec<WireCandidateRow>,
    contexts: BTreeMap<String, WireContextRow>,
    provenances: BTreeMap<String, WireProvenanceRow>,
}

fn parse_ledger_jsonl(label: &str, bytes: &[u8]) -> Result<ParsedLedger, ReportError> {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());

    let (first_idx, first_line) = lines.next().ok_or_else(|| ReportError::EmptyLedger {
        path: label.to_string(),
    })?;
    let first_row: WireLedgerRow =
        serde_json::from_str(first_line).map_err(|source| ReportError::MalformedRow {
            path: label.to_string(),
            line: first_idx + 1,
            source,
        })?;
    let header = match first_row {
        WireLedgerRow::RunHeader(h) => h,
        _ => {
            return Err(ReportError::MissingRunHeader {
                path: label.to_string(),
            })
        }
    };
    if header.schema_version != LEDGER_SCHEMA_VERSION {
        return Err(ReportError::UnsupportedLedgerSchemaVersion {
            path: label.to_string(),
            expected: LEDGER_SCHEMA_VERSION.to_string(),
            found: header.schema_version.clone(),
        });
    }

    let mut candidates = Vec::new();
    let mut contexts = BTreeMap::new();
    let mut provenances = BTreeMap::new();
    for (idx, line) in lines {
        let row: WireLedgerRow =
            serde_json::from_str(line).map_err(|source| ReportError::MalformedRow {
                path: label.to_string(),
                line: idx + 1,
                source,
            })?;
        match row {
            WireLedgerRow::RunHeader(_) => {
                return Err(ReportError::MalformedRow {
                    path: label.to_string(),
                    line: idx + 1,
                    source: serde_json::Error::io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "unexpected second run_header row",
                    )),
                })
            }
            WireLedgerRow::Candidate(c) => candidates.push(c),
            WireLedgerRow::Context(c) => {
                if contexts.contains_key(&c.digest) {
                    return Err(ReportError::DuplicateLedgerRow {
                        path: label.to_string(),
                        kind: "context".to_string(),
                        digest: c.digest,
                    });
                }
                contexts.insert(c.digest.clone(), c);
            }
            WireLedgerRow::Provenance(p) => {
                if provenances.contains_key(&p.digest) {
                    return Err(ReportError::DuplicateLedgerRow {
                        path: label.to_string(),
                        kind: "provenance".to_string(),
                        digest: p.digest,
                    });
                }
                provenances.insert(p.digest.clone(), p);
            }
        }
    }

    Ok(ParsedLedger {
        header,
        candidates,
        contexts,
        provenances,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvariantViolation {
    pub service: String,
    pub digest: String,
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceEvaluation {
    pub distinct_blocks: String,
    pub real_sample_blocks: String,
    pub candidate_rows: String,
    pub real_preflight_samples: String,
    pub error_count: String,
    pub revert_count: String,
    pub max_block_gap: String,
    pub max_wall_clock_gap_seconds: String,
    pub failure_reasons: Vec<String>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverallEvaluation {
    pub total_canonical_blocks: String,
    pub runtime_seconds: String,
    pub total_candidate_rows: String,
    pub total_real_preflight_samples: String,
    pub total_context_rows: String,
    pub positive_net_profit_rows: String,
    pub max_single_negative_net_profit_wei: String,
    pub failure_reasons: Vec<String>,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowReport {
    pub schema_version: String,
    pub gate_plan_digest: String,
    pub thresholds_digest: String,
    pub ledger_digest: String,
    pub scope: serde_json::Value,
    pub per_service: BTreeMap<String, ServiceEvaluation>,
    pub overall: OverallEvaluation,
    pub invariant_violations: Vec<InvariantViolation>,
    pub env_unsupported_count: BTreeMap<String, String>,
    pub verdict_eligible: bool,
}

/// `keccak256` over sorted `(service, raw_bytes)` pairs, each entry encoded
/// as `service_bytes ++ 0x00 ++ file_bytes` and concatenated in service-name
/// order — `shadow_decision create` recomputes this independently from its
/// own `--ledger` inputs and compares against the value embedded here.
pub fn ledger_digest(ledgers: &BTreeMap<String, Vec<u8>>) -> String {
    let mut buf = Vec::new();
    for (service, bytes) in ledgers {
        buf.extend_from_slice(service.as_bytes());
        buf.push(0u8);
        buf.extend_from_slice(bytes);
    }
    digest_bytes(&buf)
}

/// Parses `bytes` as a ledger JSONL file and returns just its
/// `run_header.service` — lets `shadow_decision` determine which required
/// service a `--ledger` file covers without the private `ParsedLedger`/
/// `WireRunHeader` types being exposed themselves. `pub` (not `pub(crate)`)
/// since `examples/shadow_decision.rs` compiles as a separate crate and can't
/// see crate-private items.
pub fn ledger_header_service(label: &str, bytes: &[u8]) -> Result<String, ReportError> {
    parse_ledger_jsonl(label, bytes).map(|parsed| parsed.header.service)
}

fn rate_bound_u256(bound: &RateBound) -> (U256, U256) {
    (
        U256::from_str(bound.numerator.value()).expect("RateBound already validated"),
        U256::from_str(bound.denominator.value()).expect("RateBound already validated"),
    )
}

/// `actual_num / actual_den >= bound.numerator / bound.denominator`,
/// via cross-multiplication (never floats). `false` when `actual_den == 0`
/// (a fraction with no observations can't satisfy a minimum).
fn at_least(actual_num: U256, actual_den: U256, bound: &RateBound) -> bool {
    if actual_den.is_zero() {
        return false;
    }
    let (bn, bd) = rate_bound_u256(bound);
    actual_num * bd >= bn * actual_den
}

/// `actual_num / actual_den <= bound.numerator / bound.denominator`.
/// `true` when `actual_den == 0` (no samples means no violation either).
fn at_most(actual_num: U256, actual_den: U256, bound: &RateBound) -> bool {
    if actual_den.is_zero() {
        return true;
    }
    let (bn, bd) = rate_bound_u256(bound);
    actual_num * bd <= bn * actual_den
}

/// Parses a `net_profit` wire value into `(is_negative, magnitude)`. At most
/// one leading `-` is accepted; the remainder must satisfy
/// [`DecimalUint`]'s own convention (digits-only, no leading zero except the
/// literal `"0"`, fits in 256 bits) — rejecting malformed values like
/// `"--5"`, `"0x5"`, or `"05"` that a bare `trim_start_matches('-')` +
/// `U256::from_str` would silently accept.
fn parse_net_profit(value: &str) -> Result<(bool, U256), ()> {
    let (is_negative, magnitude_str) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let magnitude = DecimalUint::parse(magnitude_str).map_err(|_| ())?;
    let magnitude = U256::from_str(magnitude.value()).expect("DecimalUint already validated");
    Ok((is_negative, magnitude))
}

fn decimal_to_u64(value: &crate::execution::shadow_thresholds::DecimalUint) -> u64 {
    // ValidatedThresholds already guarantees `value()` is digits-only and
    // fits in 256 bits; a threshold count that doesn't fit u64 is not a
    // realistic deployment, so this is asserted rather than propagated as a
    // recoverable error.
    value
        .value()
        .parse::<u64>()
        .expect("threshold count value out of u64 range")
}

/// Verifies and evaluates a completed shadow run's ledgers against
/// `thresholds`, given an already-verified `gate_plan` (verification is the
/// caller's responsibility — see the module doc). `expected_chain_id` is the
/// chain ID the `GatePlan`'s scope was verified against — `GatePlanPayload`
/// itself doesn't carry `chain_id` (see `shadow_gate_plan`'s module doc on
/// why `VerifiedArtifact` never exposes the scope back), so the caller
/// passes the same value it used to build that scope.
pub fn evaluate(
    gate_plan_bytes: &[u8],
    gate_plan: &GatePlanPayload,
    thresholds: &ValidatedThresholds,
    ledgers: &[LedgerInput],
    expected_chain_id: u64,
) -> Result<ShadowReport, ReportError> {
    if thresholds.digest != gate_plan.thresholds_digest {
        return Err(ReportError::ThresholdsDigestMismatch {
            gate_plan_digest: gate_plan.thresholds_digest.clone(),
            thresholds_digest: thresholds.digest.clone(),
        });
    }

    let mut parsed_by_service: BTreeMap<String, ParsedLedger> = BTreeMap::new();
    let mut raw_by_service: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for input in ledgers {
        let parsed = parse_ledger_jsonl(&input.label, &input.bytes)?;
        let service = parsed.header.service.clone();
        if !gate_plan.required_services.iter().any(|s| s == &service) {
            return Err(ReportError::UnknownService {
                path: input.label.clone(),
                service,
            });
        }
        if parsed.header.threshold_config_digest != thresholds.digest {
            return Err(ReportError::ThresholdConfigDigestMismatch {
                path: input.label.clone(),
                service,
                expected: thresholds.digest.clone(),
                found: parsed.header.threshold_config_digest.clone(),
            });
        }
        if parsed.header.chain_id != expected_chain_id {
            return Err(ReportError::ChainIdMismatch {
                path: input.label.clone(),
                service,
                expected: expected_chain_id,
                found: parsed.header.chain_id,
            });
        }
        if parsed.header.git_commit != gate_plan.git_commit {
            return Err(ReportError::GitCommitMismatch {
                path: input.label.clone(),
                service,
                expected: gate_plan.git_commit.clone(),
                found: parsed.header.git_commit.clone(),
            });
        }
        // Environment-drift detection: the `GatePlan` pins the gas profile and
        // runtime identity that were in effect when the plan was created, so a
        // shadow run that actually executed under a different profile or a
        // re-derived runtime identity is caught here rather than silently
        // accepted as evidence for a plan it doesn't correspond to.
        if parsed.header.profile_digest != gate_plan.profile_digest {
            return Err(ReportError::ProfileDigestMismatch {
                path: input.label.clone(),
                service,
                expected: gate_plan.profile_digest.clone(),
                found: parsed.header.profile_digest.clone(),
            });
        }
        if parsed.header.identity_digest != gate_plan.runtime_identity_digest {
            return Err(ReportError::RuntimeIdentityDigestMismatch {
                path: input.label.clone(),
                service,
                expected: gate_plan.runtime_identity_digest.clone(),
                found: parsed.header.identity_digest.clone(),
            });
        }
        if parsed_by_service.contains_key(&service) {
            return Err(ReportError::DuplicateService(service));
        }
        raw_by_service.insert(service.clone(), input.bytes.clone());
        parsed_by_service.insert(service, parsed);
    }

    let missing: Vec<String> = gate_plan
        .required_services
        .iter()
        .filter(|s| !parsed_by_service.contains_key(s.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(ReportError::MissingServices(missing));
    }

    let thr = &thresholds.thresholds;
    let mut invariant_violations = Vec::new();
    let mut env_unsupported_count = BTreeMap::new();
    let mut per_service = BTreeMap::new();

    let mut all_blocks: BTreeSet<u64> = BTreeSet::new();
    let mut earliest_started_at = u64::MAX;
    let mut latest_recorded_at = 0u64;
    let mut total_candidate_rows: u64 = 0;
    let mut total_real_preflight_samples: u64 = 0;
    let mut total_context_rows: u64 = 0;
    let mut positive_net_profit_rows: u64 = 0;
    let mut max_single_negative_net_profit_wei = U256::ZERO;

    for (service, parsed) in &parsed_by_service {
        earliest_started_at = earliest_started_at.min(parsed.header.started_at_unix);

        let mut env_unsupported = 0u64;
        let mut error_count = 0u64;
        let mut revert_count = 0u64;
        let mut real_samples = 0u64;
        let mut attempted_samples = 0u64;
        let mut failure_reasons = Vec::new();
        let mut service_blocks: BTreeSet<u64> = BTreeSet::new();
        let mut real_sample_blocks: BTreeSet<u64> = BTreeSet::new();

        for candidate in &parsed.candidates {
            latest_recorded_at = latest_recorded_at.max(candidate.recorded_at_unix);
            total_candidate_rows += 1;

            let context = parsed.contexts.get(&candidate.digest);
            let provenance = parsed.provenances.get(&candidate.digest);
            if context.is_none() || provenance.is_none() {
                invariant_violations.push(InvariantViolation {
                    service: service.clone(),
                    digest: candidate.digest.clone(),
                    kind: "incomplete_row_group".to_string(),
                    detail: format!(
                        "context present={} provenance present={}",
                        context.is_some(),
                        provenance.is_some()
                    ),
                });
                continue;
            }
            let context = context.unwrap();
            let provenance = provenance.unwrap();

            let block = context.identity.snapshot_id.block_number;
            service_blocks.insert(block);
            all_blocks.insert(block);

            if let PoolProvenanceOutcome::Rejected(reason) = &provenance.outcome {
                invariant_violations.push(InvariantViolation {
                    service: service.clone(),
                    digest: candidate.digest.clone(),
                    kind: "rejected_provenance".to_string(),
                    detail: reason.clone(),
                });
            }

            match &candidate.outcome {
                WireOutcome::Pass => {
                    real_samples += 1;
                    attempted_samples += 1;
                    real_sample_blocks.insert(block);
                }
                WireOutcome::Revert { .. } => {
                    real_samples += 1;
                    attempted_samples += 1;
                    real_sample_blocks.insert(block);
                    revert_count += 1;
                }
                WireOutcome::RpcError { .. } => {
                    attempted_samples += 1;
                    error_count += 1;
                }
                WireOutcome::EnvUnsupported => {
                    env_unsupported += 1;
                }
                WireOutcome::SkippedApproved => {
                    invariant_violations.push(InvariantViolation {
                        service: service.clone(),
                        digest: candidate.digest.clone(),
                        kind: "skipped_approved".to_string(),
                        detail: "SkippedApproved outcome recorded in a completed shadow run"
                            .to_string(),
                    });
                }
                WireOutcome::SampledOut => {
                    invariant_violations.push(InvariantViolation {
                        service: service.clone(),
                        digest: candidate.digest.clone(),
                        kind: "sampled_out".to_string(),
                        detail: "SampledOut outcome recorded in a completed shadow run"
                            .to_string(),
                    });
                }
            }
        }

        let candidate_digests: BTreeSet<&str> =
            parsed.candidates.iter().map(|c| c.digest.as_str()).collect();

        for (digest, context) in &parsed.contexts {
            if !candidate_digests.contains(digest.as_str()) {
                invariant_violations.push(InvariantViolation {
                    service: service.clone(),
                    digest: digest.clone(),
                    kind: "orphan_context_row".to_string(),
                    detail: "context row has no matching candidate row".to_string(),
                });
                continue;
            }
            total_context_rows += 1;
            let (is_negative, magnitude) =
                parse_net_profit(&context.net_profit).map_err(|_| ReportError::MalformedNetProfit {
                    service: service.clone(),
                    digest: context.digest.clone(),
                    value: context.net_profit.clone(),
                })?;
            if is_negative {
                if magnitude > max_single_negative_net_profit_wei {
                    max_single_negative_net_profit_wei = magnitude;
                }
            } else if !magnitude.is_zero() {
                positive_net_profit_rows += 1;
            }
        }

        for digest in parsed.provenances.keys() {
            if !candidate_digests.contains(digest.as_str()) {
                invariant_violations.push(InvariantViolation {
                    service: service.clone(),
                    digest: digest.clone(),
                    kind: "orphan_provenance_row".to_string(),
                    detail: "provenance row has no matching candidate row".to_string(),
                });
            }
        }

        env_unsupported_count.insert(service.clone(), env_unsupported.to_string());
        total_real_preflight_samples += real_samples;

        let distinct_blocks = service_blocks.len() as u64;
        let real_sample_block_count = real_sample_blocks.len() as u64;
        let sample_pool = attempted_samples; // Pass + Revert + RpcError

        // Unconditional: a required service that contributed zero real
        // (Pass/Revert) preflight samples gives no evidence to evaluate at
        // all, regardless of how permissive the coverage-budget thresholds
        // are configured (e.g. a min_real_sample_block_fraction of 0/1 would
        // otherwise let this service "pass" on paper while another service
        // alone satisfies the global min_real_preflight_samples minimum).
        if real_samples == 0 {
            failure_reasons
                .push("service has zero real (Pass/Revert) preflight samples".to_string());
        }

        if distinct_blocks < decimal_to_u64(&thr.coverage_budget.min_distinct_blocks_per_service) {
            failure_reasons.push(format!(
                "distinct_blocks {distinct_blocks} < min_distinct_blocks_per_service {}",
                thr.coverage_budget.min_distinct_blocks_per_service.value()
            ));
        }
        if !at_least(
            U256::from(real_sample_block_count),
            U256::from(distinct_blocks),
            &thr.coverage_budget.min_real_sample_block_fraction,
        ) {
            failure_reasons.push(format!(
                "real_sample_blocks/distinct_blocks {real_sample_block_count}/{distinct_blocks} below min_real_sample_block_fraction {}/{}",
                thr.coverage_budget.min_real_sample_block_fraction.numerator.value(),
                thr.coverage_budget.min_real_sample_block_fraction.denominator.value()
            ));
        }

        let sorted_blocks: Vec<u64> = service_blocks.iter().copied().collect();
        let max_block_gap = sorted_blocks
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0);
        if max_block_gap > decimal_to_u64(&thr.continuity_budget.max_block_gap) {
            failure_reasons.push(format!(
                "max_block_gap {max_block_gap} > continuity_budget.max_block_gap {}",
                thr.continuity_budget.max_block_gap.value()
            ));
        }

        let mut recorded_ats: Vec<u64> = parsed.candidates.iter().map(|c| c.recorded_at_unix).collect();
        recorded_ats.sort_unstable();
        let max_wall_clock_gap = recorded_ats
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or(0);
        if max_wall_clock_gap > decimal_to_u64(&thr.continuity_budget.max_wall_clock_gap_seconds) {
            failure_reasons.push(format!(
                "max_wall_clock_gap_seconds {max_wall_clock_gap} > continuity_budget.max_wall_clock_gap_seconds {}",
                thr.continuity_budget.max_wall_clock_gap_seconds.value()
            ));
        }

        if !at_most(U256::from(error_count), U256::from(sample_pool), &thr.max_error_rate) {
            failure_reasons.push(format!(
                "error_rate {error_count}/{sample_pool} exceeds max_error_rate {}/{}",
                thr.max_error_rate.numerator.value(),
                thr.max_error_rate.denominator.value()
            ));
        }
        if !at_most(U256::from(revert_count), U256::from(sample_pool), &thr.max_revert_rate) {
            failure_reasons.push(format!(
                "revert_rate {revert_count}/{sample_pool} exceeds max_revert_rate {}/{}",
                thr.max_revert_rate.numerator.value(),
                thr.max_revert_rate.denominator.value()
            ));
        }

        let has_invariant_violation = invariant_violations.iter().any(|v| &v.service == service);
        if has_invariant_violation {
            failure_reasons.push("service has one or more invariant violations".to_string());
        }

        per_service.insert(
            service.clone(),
            ServiceEvaluation {
                distinct_blocks: distinct_blocks.to_string(),
                real_sample_blocks: real_sample_block_count.to_string(),
                candidate_rows: parsed.candidates.len().to_string(),
                real_preflight_samples: real_samples.to_string(),
                error_count: error_count.to_string(),
                revert_count: revert_count.to_string(),
                max_block_gap: max_block_gap.to_string(),
                max_wall_clock_gap_seconds: max_wall_clock_gap.to_string(),
                passed: failure_reasons.is_empty(),
                failure_reasons,
            },
        );
    }

    let total_canonical_blocks = all_blocks.len() as u64;
    let runtime_seconds = latest_recorded_at.saturating_sub(earliest_started_at);

    let mut overall_failure_reasons = Vec::new();
    if total_canonical_blocks < decimal_to_u64(&thr.min_canonical_blocks) {
        overall_failure_reasons.push(format!(
            "total_canonical_blocks {total_canonical_blocks} < min_canonical_blocks {}",
            thr.min_canonical_blocks.value()
        ));
    }
    if runtime_seconds < decimal_to_u64(&thr.min_runtime_seconds) {
        overall_failure_reasons.push(format!(
            "runtime_seconds {runtime_seconds} < min_runtime_seconds {}",
            thr.min_runtime_seconds.value()
        ));
    }
    if total_candidate_rows < decimal_to_u64(&thr.min_candidate_rows) {
        overall_failure_reasons.push(format!(
            "total_candidate_rows {total_candidate_rows} < min_candidate_rows {}",
            thr.min_candidate_rows.value()
        ));
    }
    if total_real_preflight_samples < decimal_to_u64(&thr.min_real_preflight_samples) {
        overall_failure_reasons.push(format!(
            "total_real_preflight_samples {total_real_preflight_samples} < min_real_preflight_samples {}",
            thr.min_real_preflight_samples.value()
        ));
    }
    if positive_net_profit_rows < decimal_to_u64(&thr.profit_distribution.min_positive_net_profit_rows)
    {
        overall_failure_reasons.push(format!(
            "positive_net_profit_rows {positive_net_profit_rows} < min_positive_net_profit_rows {}",
            thr.profit_distribution.min_positive_net_profit_rows.value()
        ));
    }
    if !at_least(
        U256::from(positive_net_profit_rows),
        U256::from(total_context_rows),
        &thr.profit_distribution.min_positive_net_profit_fraction,
    ) {
        overall_failure_reasons.push(format!(
            "positive_net_profit_rows/total_context_rows {positive_net_profit_rows}/{total_context_rows} below min_positive_net_profit_fraction {}/{}",
            thr.profit_distribution.min_positive_net_profit_fraction.numerator.value(),
            thr.profit_distribution.min_positive_net_profit_fraction.denominator.value()
        ));
    }
    let max_negative_cap = U256::from_str(thr.profit_distribution.max_negative_net_profit_wei.value())
        .expect("threshold DecimalUint already validated");
    if max_single_negative_net_profit_wei > max_negative_cap {
        overall_failure_reasons.push(format!(
            "max single negative net_profit magnitude {max_single_negative_net_profit_wei} exceeds max_negative_net_profit_wei {}",
            thr.profit_distribution.max_negative_net_profit_wei.value()
        ));
    }

    let overall = OverallEvaluation {
        total_canonical_blocks: total_canonical_blocks.to_string(),
        runtime_seconds: runtime_seconds.to_string(),
        total_candidate_rows: total_candidate_rows.to_string(),
        total_real_preflight_samples: total_real_preflight_samples.to_string(),
        total_context_rows: total_context_rows.to_string(),
        positive_net_profit_rows: positive_net_profit_rows.to_string(),
        max_single_negative_net_profit_wei: max_single_negative_net_profit_wei.to_string(),
        passed: overall_failure_reasons.is_empty(),
        failure_reasons: overall_failure_reasons,
    };

    let verdict_eligible = per_service.values().all(|s| s.passed)
        && overall.passed
        && invariant_violations.is_empty();

    let scope = ShadowGateScope {
        chain_id: expected_chain_id,
        git_commit: gate_plan.git_commit.clone(),
        required_services: gate_plan.required_services.clone(),
    }
    .to_json();

    Ok(ShadowReport {
        schema_version: REPORT_SCHEMA_VERSION.to_string(),
        gate_plan_digest: digest_bytes(gate_plan_bytes),
        thresholds_digest: thresholds.digest.clone(),
        ledger_digest: ledger_digest(&raw_by_service),
        scope,
        per_service,
        overall,
        invariant_violations,
        env_unsupported_count,
        verdict_eligible,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::shadow_thresholds;

    fn thresholds_bytes() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema_version": shadow_thresholds::THRESHOLDS_SCHEMA_VERSION,
            "required_services": ["svc_a"],
            "min_canonical_blocks": "1",
            "min_runtime_seconds": "0",
            "min_candidate_rows": "1",
            "min_real_preflight_samples": "1",
            "coverage_budget": {
                "min_distinct_blocks_per_service": "1",
                "min_real_sample_block_fraction": { "numerator": "1", "denominator": "1" }
            },
            "continuity_budget": {
                "max_block_gap": "1000",
                "max_wall_clock_gap_seconds": "1000000"
            },
            "max_error_rate": { "numerator": "1", "denominator": "1" },
            "max_revert_rate": { "numerator": "1", "denominator": "1" },
            "profit_distribution": {
                "min_positive_net_profit_rows": "1",
                "min_positive_net_profit_fraction": { "numerator": "1", "denominator": "1" },
                "max_negative_net_profit_wei": "1000000000000000000"
            }
        }))
        .unwrap()
    }

    fn gate_plan_payload(thresholds_digest: &str) -> GatePlanPayload {
        GatePlanPayload {
            thresholds_digest: thresholds_digest.to_string(),
            git_commit: "0".repeat(40),
            config_digest: "0xaa".to_string(),
            profile_digest: "0xbb".to_string(),
            runtime_identity_digest: "0xcc".to_string(),
            allowed_signers_digest: "0xdd".to_string(),
            revoked_keys_digest: "0xee".to_string(),
            required_services: vec!["svc_a".to_string()],
        }
    }

    fn ledger_row(json: serde_json::Value) -> String {
        serde_json::to_string(&json).unwrap()
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
            "outcome": { "verified": { "protocol": "uniswap_v2", "factory": "0x0000000000000000000000000000000000000001", "init_code_hash": format!("0x{}", "11".repeat(32)), "salt": format!("0x{}", "22".repeat(32)) } },
        })
    }

    fn header_json(threshold_digest: &str, started_at: u64) -> serde_json::Value {
        serde_json::json!({
            "row_type": "run_header",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "run_id": "run-1",
            "git_commit": "0".repeat(40),
            "chain_id": 5000,
            "service": "svc_a",
            "executor_contract": "0x0000000000000000000000000000000000000002",
            "wmnt_address": "0x0000000000000000000000000000000000000003",
            "storage_layout_digest": "0x00",
            "wmnt_descriptor_digest": "0x00",
            "moe_allowlist_digest": "0x00",
            // Must match `gate_plan_payload`'s `runtime_identity_digest` /
            // `profile_digest`: `evaluate` cross-checks both.
            "identity_digest": "0xcc",
            "approved_pools_digest": "0x00",
            "threshold_config_digest": threshold_digest,
            "profile_digest": "0xbb",
            "override_digest": "0x00",
            "start_identity": null,
            "started_at_unix": started_at,
        })
    }

    fn passing_ledger(thresholds_digest: &str) -> Vec<u8> {
        let mut lines = vec![ledger_row(header_json(thresholds_digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        lines.join("\n").into_bytes()
    }

    #[test]
    fn evaluates_a_clean_run_as_eligible() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let ledger = passing_ledger(&validated.digest);
        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(report.verdict_eligible, "{:?}", report.per_service);
        assert!(report.invariant_violations.is_empty());
        assert_eq!(report.per_service["svc_a"].candidate_rows, "1");
    }

    #[test]
    fn rejects_thresholds_digest_mismatch() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload("0xdeadbeef");
        let err = evaluate(b"gate-plan-bytes", &gate_plan, &validated, &[], 5000).unwrap_err();
        assert!(matches!(err, ReportError::ThresholdsDigestMismatch { .. }));
    }

    #[test]
    fn rejects_missing_required_service() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let err = evaluate(b"gate-plan-bytes", &gate_plan, &validated, &[], 5000).unwrap_err();
        assert!(matches!(err, ReportError::MissingServices(_)));
    }

    #[test]
    fn rejects_duplicate_service_ledgers() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let ledger = passing_ledger(&validated.digest);
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[
                LedgerInput {
                    label: "a.jsonl".to_string(),
                    bytes: ledger.clone(),
                },
                LedgerInput {
                    label: "b.jsonl".to_string(),
                    bytes: ledger,
                },
            ],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::DuplicateService(_)));
    }

    #[test]
    fn rejects_threshold_config_digest_mismatch_in_ledger_header() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let ledger = passing_ledger("0xstaledigest");
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::ThresholdConfigDigestMismatch { .. }));
    }

    #[test]
    fn rejects_a_chain_id_mismatch_against_the_gate_plan_scope() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let ledger = passing_ledger(&validated.digest);
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            // Differs from the ledger header's fixture chain_id (5000).
            1,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::ChainIdMismatch { .. }));
    }

    #[test]
    fn rejects_a_git_commit_mismatch_against_the_gate_plan() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut header = header_json(&validated.digest, 1_000);
        header["git_commit"] = serde_json::json!("1".repeat(40));
        let mut lines = vec![ledger_row(header)];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::GitCommitMismatch { .. }));
    }

    /// Builds `passing_ledger`'s row set with one `run_header` field replaced,
    /// for the header cross-check rejection tests.
    fn ledger_with_header_field(
        thresholds_digest: &str,
        field: &str,
        value: serde_json::Value,
    ) -> Vec<u8> {
        let mut header = header_json(thresholds_digest, 1_000);
        header[field] = value;
        let lines = vec![
            ledger_row(header),
            ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)),
            ledger_row(context_json("d1", 100, "5")),
            ledger_row(provenance_json("d1")),
        ];
        lines.join("\n").into_bytes()
    }

    #[test]
    fn rejects_a_profile_digest_mismatch_against_the_gate_plan() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        // The gate plan pinned "0xbb"; this run executed under a different
        // gas-profile artifact.
        let ledger = ledger_with_header_field(
            &validated.digest,
            "profile_digest",
            serde_json::json!("0xdeadbeef"),
        );
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::ProfileDigestMismatch { .. }));
    }

    #[test]
    fn rejects_a_runtime_identity_digest_mismatch_against_the_gate_plan() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        // The gate plan pinned "0xcc"; this run started against a different
        // verified runtime identity.
        let ledger = ledger_with_header_field(
            &validated.digest,
            "identity_digest",
            serde_json::json!("0xdeadbeef"),
        );
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ReportError::RuntimeIdentityDigestMismatch { .. }
        ));
    }

    /// A `run_header` missing `profile_digest`/`identity_digest` altogether is
    /// not a real WHI-549 ledger (`LedgerRunHeader` writes both unconditionally),
    /// so it must fail to parse rather than skip the cross-check.
    #[test]
    fn rejects_a_run_header_missing_the_environment_digests() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut header = header_json(&validated.digest, 1_000);
        header
            .as_object_mut()
            .unwrap()
            .remove("profile_digest")
            .unwrap();
        let ledger = vec![ledger_row(header)].join("\n").into_bytes();
        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::MalformedRow { .. }));
    }

    #[test]
    fn rejects_a_malformed_net_profit_value_instead_of_defaulting_to_zero() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "not-a-number")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::MalformedNetProfit { .. }));
    }

    #[test]
    fn skipped_approved_outcome_is_an_invariant_violation_and_fails_service() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "skipped_approved"}),
            1_000,
        )));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(!report.verdict_eligible);
        assert_eq!(report.invariant_violations.len(), 1);
        assert_eq!(report.invariant_violations[0].kind, "skipped_approved");
        assert!(!report.per_service["svc_a"].passed);
    }

    #[test]
    fn incomplete_row_group_is_an_invariant_violation() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        // No matching context/provenance row for "d1".
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(!report.verdict_eligible);
        assert_eq!(report.invariant_violations[0].kind, "incomplete_row_group");
    }

    #[test]
    fn byte_reproducible_across_two_generations() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let ledger = passing_ledger(&validated.digest);
        let inputs = |b: Vec<u8>| {
            vec![LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: b,
            }]
        };
        let a = evaluate(b"gate-plan-bytes", &gate_plan, &validated, &inputs(ledger.clone()), 5000).unwrap();
        let b = evaluate(b"gate-plan-bytes", &gate_plan, &validated, &inputs(ledger), 5000).unwrap();

        let a_bytes = crate::signing::canonical::canonicalize_value(&serde_json::to_value(&a).unwrap()).unwrap();
        let b_bytes = crate::signing::canonical::canonicalize_value(&serde_json::to_value(&b).unwrap()).unwrap();
        assert_eq!(a_bytes, b_bytes);
    }

    #[test]
    fn zero_real_samples_fails_min_real_preflight_samples() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "env_unsupported"}),
            1_000,
        )));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(!report.verdict_eligible);
        assert!(report
            .overall
            .failure_reasons
            .iter()
            .any(|r| r.contains("min_real_preflight_samples")));
    }

    #[test]
    fn rpc_error_outcome_is_not_counted_as_a_real_preflight_sample() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "rpc_error", "class": "transport"}),
            1_000,
        )));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        // A run with zero Pass/Revert outcomes must fail min_real_preflight_samples
        // even though it produced an RpcError candidate row — RpcError is an
        // attempted sample, not a "real" one (see shadow_thresholds.rs's
        // `min_real_sample_block_fraction` doc).
        assert_eq!(report.per_service["svc_a"].real_preflight_samples, "0");
        assert!(!report.verdict_eligible);
        assert!(report
            .overall
            .failure_reasons
            .iter()
            .any(|r| r.contains("min_real_preflight_samples")));
    }

    #[test]
    fn zero_real_samples_service_fails_even_with_permissive_coverage_thresholds() {
        let lenient_thresholds = serde_json::to_vec(&serde_json::json!({
            "schema_version": shadow_thresholds::THRESHOLDS_SCHEMA_VERSION,
            "required_services": ["svc_a"],
            "min_canonical_blocks": "0",
            "min_runtime_seconds": "0",
            "min_candidate_rows": "0",
            "min_real_preflight_samples": "0",
            "coverage_budget": {
                "min_distinct_blocks_per_service": "0",
                "min_real_sample_block_fraction": { "numerator": "0", "denominator": "1" }
            },
            "continuity_budget": {
                "max_block_gap": "1000",
                "max_wall_clock_gap_seconds": "1000000"
            },
            "max_error_rate": { "numerator": "1", "denominator": "1" },
            "max_revert_rate": { "numerator": "1", "denominator": "1" },
            "profit_distribution": {
                "min_positive_net_profit_rows": "0",
                "min_positive_net_profit_fraction": { "numerator": "0", "denominator": "1" },
                "max_negative_net_profit_wei": "1000000000000000000"
            }
        }))
        .unwrap();
        let validated = shadow_thresholds::validate(&lenient_thresholds).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json(
            "d1",
            serde_json::json!({"kind": "env_unsupported"}),
            1_000,
        )));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        // Every configured threshold is deliberately lenient enough to pass on
        // its own — only the unconditional zero-real-samples guard should fail.
        assert!(report.overall.passed, "{:?}", report.overall.failure_reasons);
        assert!(!report.verdict_eligible);
        assert!(!report.per_service["svc_a"].passed);
        assert!(report.per_service["svc_a"]
            .failure_reasons
            .iter()
            .any(|r| r.contains("zero real")));
    }

    #[test]
    fn orphan_context_row_without_matching_candidate_is_an_invariant_violation() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        // Orphan: no candidate row references "d2".
        lines.push(ledger_row(context_json("d2", 100, "5")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(!report.verdict_eligible);
        assert!(report
            .invariant_violations
            .iter()
            .any(|v| v.kind == "orphan_context_row" && v.digest == "d2"));
    }

    #[test]
    fn orphan_provenance_row_without_matching_candidate_is_an_invariant_violation() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        // Orphan: no candidate row references "d2".
        lines.push(ledger_row(provenance_json("d2")));
        let ledger = lines.join("\n").into_bytes();

        let report = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap();

        assert!(!report.verdict_eligible);
        assert!(report
            .invariant_violations
            .iter()
            .any(|v| v.kind == "orphan_provenance_row" && v.digest == "d2"));
    }

    #[test]
    fn rejects_duplicate_context_row_for_the_same_digest() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::DuplicateLedgerRow { ref kind, .. } if kind == "context"));
    }

    #[test]
    fn rejects_duplicate_provenance_row_for_the_same_digest() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "5")));
        lines.push(ledger_row(provenance_json("d1")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::DuplicateLedgerRow { ref kind, .. } if kind == "provenance"));
    }

    #[test]
    fn rejects_net_profit_with_multiple_leading_dashes() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "--5")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::MalformedNetProfit { .. }));
    }

    #[test]
    fn rejects_net_profit_with_a_leading_zero() {
        let validated = shadow_thresholds::validate(&thresholds_bytes()).unwrap();
        let gate_plan = gate_plan_payload(&validated.digest);
        let mut lines = vec![ledger_row(header_json(&validated.digest, 1_000))];
        lines.push(ledger_row(candidate_json("d1", serde_json::json!({"kind": "pass"}), 1_000)));
        lines.push(ledger_row(context_json("d1", 100, "05")));
        lines.push(ledger_row(provenance_json("d1")));
        let ledger = lines.join("\n").into_bytes();

        let err = evaluate(
            b"gate-plan-bytes",
            &gate_plan,
            &validated,
            &[LedgerInput {
                label: "svc_a.jsonl".to_string(),
                bytes: ledger,
            }],
            5000,
        )
        .unwrap_err();
        assert!(matches!(err, ReportError::MalformedNetProfit { .. }));
    }
}
