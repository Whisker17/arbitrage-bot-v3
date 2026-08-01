//! Machine-readable probe report types and serialization (WHI-744).

use crate::rpc_probe::thresholds;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level probe report written to `--out`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProbeReport {
    pub probe_version: String,
    pub git_commit: String,
    pub started_at: String,
    pub ended_at: String,
    pub http_endpoint_fingerprint: String,
    pub ws_endpoint_fingerprint: String,
    pub checks: BTreeMap<String, CheckResult>,
    pub qualified: bool,
}

/// Per-check result with measured values vs thresholds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub name: String,
    pub passed: bool,
    /// Distinct failure reason when `passed` is false (stable snake_case id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// Human-readable detail (must not contain endpoint URLs or secrets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Measured scalars for this check.
    pub measured: BTreeMap<String, serde_json::Value>,
    /// Thresholds compared against (copied from the const block for diffability).
    pub thresholds: BTreeMap<String, serde_json::Value>,
}

impl CheckResult {
    pub fn pass(
        name: impl Into<String>,
        measured: BTreeMap<String, serde_json::Value>,
        thresholds_map: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            passed: true,
            failure_reason: None,
            detail: None,
            measured,
            thresholds: thresholds_map,
        }
    }

    pub fn fail(
        name: impl Into<String>,
        reason: impl Into<String>,
        detail: String,
        measured: BTreeMap<String, serde_json::Value>,
        thresholds_map: BTreeMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            name: name.into(),
            passed: false,
            failure_reason: Some(reason.into()),
            detail: Some(detail),
            measured,
            thresholds: thresholds_map,
        }
    }
}

/// Named check identifiers (stable in reports and failure reasons).
pub mod check_id {
    pub const MULTI_ADDRESS_GET_LOGS: &str = "A_multi_address_get_logs";
    pub const RECEIPT_0X7E: &str = "B_receipt_0x7e_decode";
    pub const BLOCK_CONTINUITY: &str = "C_block_continuity";
    pub const HEADER_COMPLETENESS: &str = "D_header_completeness";
    pub const WS_STABILITY: &str = "E_ws_stability";
}

/// Distinct failure reason identifiers (one per independent failure mode).
pub mod failure_reason {
    pub const MULTI_ADDRESS_REJECTED: &str = "multi_address_get_logs_rejected";
    pub const RATE_LIMITED_429: &str = "http_429_rate_limited";
    pub const PAYLOAD_TOO_LARGE_413: &str = "http_413_payload_too_large";
    pub const RECEIPT_TYPE_0X7E_DECODE: &str = "receipt_type_0x7e_decode";
    pub const RECEIPT_FETCH_ERROR: &str = "receipt_fetch_error";
    pub const NO_TYPE_0X7E_OBSERVED: &str = "no_type_0x7e_receipts_observed";
    pub const CONTINUITY_GAP: &str = "block_continuity_gap";
    pub const HTTP_WS_DISAGREEMENT: &str = "http_ws_header_disagreement";
    pub const INCOMPLETE_HEADER: &str = "incomplete_block_header";
    pub const WS_DISCONNECT: &str = "ws_disconnect";
    pub const WS_STALL: &str = "ws_silent_stall";
    pub const MISSING_ENDPOINT: &str = "missing_endpoint";
    pub const POOL_UNIVERSE_LOAD: &str = "pool_universe_load_failed";
    pub const PROVIDER_CONNECT: &str = "provider_connect_failed";
}

/// Aggregate qualification: every check must pass.
pub fn compute_qualified(checks: &BTreeMap<String, CheckResult>) -> bool {
    !checks.is_empty() && checks.values().all(|c| c.passed)
}

/// Compare a measured count against a maximum-allowed threshold.
pub fn within_max(measured: u64, max_allowed: u64) -> bool {
    measured <= max_allowed
}

/// Compare a measured ratio against a minimum-required threshold.
pub fn meets_min_ratio(measured: f64, min_required: f64) -> bool {
    measured + f64::EPSILON >= min_required
}

/// Serialize the report to pretty JSON.
pub fn serialize_report(report: &ProbeReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

/// Human-readable one-screen summary (no secrets).
pub fn format_summary(report: &ProbeReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "rpc_probe {}  commit={}  qualified={}",
        report.probe_version, report.git_commit, report.qualified
    ));
    lines.push(format!(
        "http_fp={}  ws_fp={}",
        report.http_endpoint_fingerprint, report.ws_endpoint_fingerprint
    ));
    lines.push(format!(
        "window={} → {}",
        report.started_at, report.ended_at
    ));
    for (id, check) in &report.checks {
        let status = if check.passed { "PASS" } else { "FAIL" };
        let reason = check
            .failure_reason
            .as_deref()
            .unwrap_or("-");
        lines.push(format!("  [{status}] {id}  reason={reason}"));
        if let Some(detail) = &check.detail {
            lines.push(format!("         {detail}"));
        }
    }
    lines.push(format!(
        "thresholds_defaults: blocks≥{} duration≥{}s stall≤{}s max_429={}",
        thresholds::DEFAULT_BLOCKS,
        thresholds::DEFAULT_DURATION_SECS,
        thresholds::WS_STALL_THRESHOLD_SECS,
        thresholds::MAX_HTTP_429
    ));
    lines.join("\n")
}

/// Build a skeleton report shell (checks filled by the runner).
pub fn new_report(
    http_fp: String,
    ws_fp: String,
    git_commit: String,
    started_at: String,
    ended_at: String,
    checks: BTreeMap<String, CheckResult>,
) -> ProbeReport {
    let qualified = compute_qualified(&checks);
    ProbeReport {
        probe_version: thresholds::PROBE_VERSION.to_string(),
        git_commit,
        started_at,
        ended_at,
        http_endpoint_fingerprint: http_fp,
        ws_endpoint_fingerprint: ws_fp,
        checks,
        qualified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc_probe::fingerprint::{endpoint_fingerprint, text_leaks_endpoint};

    fn sample_check(pass: bool) -> CheckResult {
        let mut measured = BTreeMap::new();
        measured.insert("count".into(), serde_json::json!(1));
        let mut th = BTreeMap::new();
        th.insert("max".into(), serde_json::json!(0));
        if pass {
            CheckResult::pass("t", measured, th)
        } else {
            CheckResult::fail("t", "rate_limited", "got 429".to_string(), measured, th)
        }
    }

    #[test]
    fn qualified_requires_all_checks_pass() {
        let mut checks = BTreeMap::new();
        checks.insert("A".into(), sample_check(true));
        checks.insert("B".into(), sample_check(true));
        assert!(compute_qualified(&checks));
        checks.insert("C".into(), sample_check(false));
        assert!(!compute_qualified(&checks));
        assert!(!compute_qualified(&BTreeMap::new()));
    }

    #[test]
    fn threshold_helpers() {
        assert!(within_max(0, 0));
        assert!(!within_max(1, 0));
        assert!(meets_min_ratio(1.0, 1.0));
        assert!(!meets_min_ratio(0.99, 1.0));
    }

    #[test]
    fn serializer_never_emits_endpoint_url_or_api_key() {
        let http = "https://user:pass@rpc.vendor.com/v1/sk_live_SECRET123?api_key=abc";
        let ws = "wss://rpc.vendor.com/ws?token=xyz";
        let mut checks = BTreeMap::new();
        checks.insert(
            check_id::MULTI_ADDRESS_GET_LOGS.into(),
            CheckResult::fail(
                check_id::MULTI_ADDRESS_GET_LOGS,
                failure_reason::RATE_LIMITED_429,
                "provider returned HTTP 429 on multi-address eth_getLogs".to_string(),
                {
                    let mut m = BTreeMap::new();
                    m.insert("address_count".into(), serde_json::json!(200));
                    m.insert("http_429_count".into(), serde_json::json!(1));
                    m
                },
                {
                    let mut t = BTreeMap::new();
                    t.insert("max_http_429".into(), serde_json::json!(0));
                    t
                },
            ),
        );
        let report = new_report(
            endpoint_fingerprint(http),
            endpoint_fingerprint(ws),
            "deadbeef".into(),
            "2026-01-01T00:00:00Z".into(),
            "2026-01-01T00:05:00Z".into(),
            checks,
        );
        let json = serialize_report(&report).expect("serialize");
        let summary = format_summary(&report);
        for blob in [&json, &summary] {
            assert!(
                !text_leaks_endpoint(blob, http, ws),
                "leak in output: {blob}"
            );
            assert!(!blob.contains("sk_live"));
            assert!(!blob.contains("SECRET"));
            assert!(!blob.contains("rpc.vendor.com"));
            assert!(!blob.contains("api_key=abc"));
        }
        // Fingerprints are present and non-empty.
        assert!(json.contains(&report.http_endpoint_fingerprint));
        assert!(json.contains("\"qualified\": false"));
    }

    #[test]
    fn independent_failure_reasons_are_distinct() {
        let reasons = [
            failure_reason::RATE_LIMITED_429,
            failure_reason::RECEIPT_TYPE_0X7E_DECODE,
            failure_reason::CONTINUITY_GAP,
            failure_reason::INCOMPLETE_HEADER,
            failure_reason::WS_STALL,
        ];
        let set: std::collections::BTreeSet<_> = reasons.iter().collect();
        assert_eq!(set.len(), reasons.len());
    }
}
