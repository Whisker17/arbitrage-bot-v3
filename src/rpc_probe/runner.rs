//! Live RPC probe orchestration (Checks A–E).
//!
//! Network I/O lives here; pure logic stays in sibling modules so unit tests
//! never require a live endpoint.

use crate::rpc_probe::continuity::{
    detect_continuity_gaps, header_is_complete, headers_agree, SampledHeader,
};
use crate::rpc_probe::fingerprint::endpoint_fingerprint;
use crate::rpc_probe::report::{
    check_id, failure_reason, format_summary, meets_min_ratio, new_report, serialize_report,
    CheckResult, ProbeReport,
};
use crate::rpc_probe::thresholds::*;
use crate::rpc_probe::universe::{load_merged_pool_addresses, AddressSetSource};
use alloy::consensus::BlockHeader;
use alloy::eips::{BlockId, BlockNumberOrTag};
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::Ethereum;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::Filter;
use eyre::{bail, Context, Result};
use futures::StreamExt;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// CLI / programmatic configuration for one probe run.
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    pub http_url: String,
    pub ws_url: String,
    pub out: PathBuf,
    pub blocks: u64,
    pub duration_secs: u64,
    pub logs_block_window: u64,
    pub address_multiplier: f64,
    pub v2_pool_list: PathBuf,
    pub v3_pool_list: PathBuf,
    pub moe_pool_list: PathBuf,
}

impl ProbeConfig {
    pub fn validate(&self) -> Result<()> {
        if self.http_url.trim().is_empty() {
            bail!("{}: --http / MANTLE_RPC_URL is required", failure_reason::MISSING_ENDPOINT);
        }
        if self.ws_url.trim().is_empty() {
            bail!("{}: --ws / MANTLE_RPC_WS_URL is required", failure_reason::MISSING_ENDPOINT);
        }
        if self.blocks == 0 {
            bail!("--blocks must be >= 1");
        }
        if self.duration_secs == 0 {
            bail!("--duration must be >= 1");
        }
        if self.address_multiplier < 1.0 {
            bail!("--address-multiplier must be >= 1.0");
        }
        Ok(())
    }
}

/// Run the full qualification probe and write the JSON report.
///
/// Returns `(report, exit_ok)` where `exit_ok` is true only when `qualified`.
pub async fn run_probe(config: ProbeConfig) -> Result<(ProbeReport, bool)> {
    config.validate()?;
    let started = now_unix_label();
    let started_instant = Instant::now();
    let http_fp = endpoint_fingerprint(&config.http_url);
    let ws_fp = endpoint_fingerprint(&config.ws_url);
    let git_commit = resolve_git_commit();

    info!(
        target: "rpc_probe",
        http_fp = %http_fp,
        ws_fp = %ws_fp,
        blocks = config.blocks,
        duration_secs = config.duration_secs,
        "starting RPC qualification probe"
    );

    let mut checks: BTreeMap<String, CheckResult> = BTreeMap::new();

    // Load address set first (offline) so Check A can name the derivation.
    let (addresses, address_source) = match load_merged_pool_addresses(
        &config.v2_pool_list,
        &config.v3_pool_list,
        &config.moe_pool_list,
        config.address_multiplier,
    ) {
        Ok(v) => v,
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            let mut measured = BTreeMap::new();
            measured.insert("error".into(), json!(detail.clone()));
            checks.insert(
                check_id::MULTI_ADDRESS_GET_LOGS.into(),
                CheckResult::fail(
                    check_id::MULTI_ADDRESS_GET_LOGS,
                    failure_reason::POOL_UNIVERSE_LOAD,
                    detail.clone(),
                    measured,
                    BTreeMap::new(),
                ),
            );
            fill_skipped_checks(
                &mut checks,
                failure_reason::POOL_UNIVERSE_LOAD,
                &detail,
                config.duration_secs,
            );
            let report = finalize_report(
                http_fp,
                ws_fp,
                git_commit,
                started,
                checks,
                &config.out,
            )?;
            return Ok((report, false));
        }
    };

    let http = match connect_http(&config.http_url) {
        Ok(p) => p,
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            checks.insert(
                check_id::MULTI_ADDRESS_GET_LOGS.into(),
                CheckResult::fail(
                    check_id::MULTI_ADDRESS_GET_LOGS,
                    failure_reason::PROVIDER_CONNECT,
                    detail.clone(),
                    BTreeMap::new(),
                    BTreeMap::new(),
                ),
            );
            fill_skipped_checks(
                &mut checks,
                failure_reason::PROVIDER_CONNECT,
                &detail,
                config.duration_secs,
            );
            let report = finalize_report(
                http_fp,
                ws_fp,
                git_commit,
                started,
                checks,
                &config.out,
            )?;
            return Ok((report, false));
        }
    };

    // ---- Check A ----
    let check_a = run_check_a(&http, &addresses, &address_source, config.logs_block_window).await;
    checks.insert(check_id::MULTI_ADDRESS_GET_LOGS.into(), check_a);

    // Checks B–E never hard-abort the process: convert transport errors into
    // per-check failures so `--out` always receives a full A–E report.
    run_checks_b_through_e(&http, &config, &mut checks).await;

    info!(
        target: "rpc_probe",
        elapsed_ms = started_instant.elapsed().as_millis() as u64,
        "probe checks complete"
    );

    let report = finalize_report(http_fp, ws_fp, git_commit, started, checks, &config.out)?;
    let qualified = report.qualified;
    Ok((report, qualified))
}

async fn run_checks_b_through_e<P: Provider<Ethereum> + Clone>(
    http: &P,
    config: &ProbeConfig,
    checks: &mut BTreeMap<String, CheckResult>,
) {
    // ---- Check B ----
    let receipt_window = match http.get_block_number().await {
        Ok(tip) => {
            let from = tip.saturating_sub(config.blocks.saturating_sub(1));
            Some((from, tip))
        }
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            checks.insert(
                check_id::RECEIPT_0X7E.into(),
                CheckResult::fail(
                    check_id::RECEIPT_0X7E,
                    failure_reason::RECEIPT_FETCH_ERROR,
                    format!("eth_blockNumber for receipt sample failed: {detail}"),
                    BTreeMap::new(),
                    {
                        let mut th = BTreeMap::new();
                        th.insert(
                            "min_type_0x7e_receipts".into(),
                            json!(MIN_TYPE_0X7E_RECEIPTS),
                        );
                        th
                    },
                ),
            );
            fill_skipped_checks(
                checks,
                failure_reason::RECEIPT_FETCH_ERROR,
                &detail,
                config.duration_secs,
            );
            // Ensure B is not overwritten by fill (already inserted).
            None
        }
    };

    if let Some((from_block, tip)) = receipt_window {
        let check_b = run_check_b(http, from_block, tip).await;
        checks.insert(check_id::RECEIPT_0X7E.into(), check_b);
    } else {
        return;
    }

    // ---- Checks C + D (HTTP headers over a fresh fixed sample window) ----
    let header_window = match http.get_block_number().await {
        Ok(tip) => {
            let from = tip.saturating_sub(config.blocks.saturating_sub(1));
            Some((from, tip))
        }
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            checks.insert(
                check_id::BLOCK_CONTINUITY.into(),
                CheckResult::fail(
                    check_id::BLOCK_CONTINUITY,
                    failure_reason::PROVIDER_CONNECT,
                    format!("eth_blockNumber before header sample failed: {detail}"),
                    BTreeMap::new(),
                    continuity_thresholds(),
                ),
            );
            checks.insert(
                check_id::HEADER_COMPLETENESS.into(),
                CheckResult::fail(
                    check_id::HEADER_COMPLETENESS,
                    failure_reason::PROVIDER_CONNECT,
                    format!("eth_blockNumber before header sample failed: {detail}"),
                    BTreeMap::new(),
                    {
                        let mut th = BTreeMap::new();
                        th.insert(
                            "min_header_completeness_ratio".into(),
                            json!(MIN_HEADER_COMPLETENESS_RATIO),
                        );
                        th
                    },
                ),
            );
            checks.insert(
                check_id::WS_STABILITY.into(),
                CheckResult::fail(
                    check_id::WS_STABILITY,
                    failure_reason::PROVIDER_CONNECT,
                    format!("eth_blockNumber before header sample failed: {detail}"),
                    BTreeMap::new(),
                    ws_stability_thresholds(config.duration_secs),
                ),
            );
            None
        }
    };

    let Some((from_block, tip)) = header_window else {
        return;
    };

    let http_headers = match sample_http_headers(http, from_block, tip).await {
        Ok(h) => h,
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            checks.insert(
                check_id::BLOCK_CONTINUITY.into(),
                CheckResult::fail(
                    check_id::BLOCK_CONTINUITY,
                    failure_reason::PROVIDER_CONNECT,
                    format!("HTTP header sample failed: {detail}"),
                    BTreeMap::new(),
                    continuity_thresholds(),
                ),
            );
            checks.insert(
                check_id::HEADER_COMPLETENESS.into(),
                CheckResult::fail(
                    check_id::HEADER_COMPLETENESS,
                    failure_reason::PROVIDER_CONNECT,
                    format!("HTTP header sample failed: {detail}"),
                    BTreeMap::new(),
                    {
                        let mut th = BTreeMap::new();
                        th.insert(
                            "min_header_completeness_ratio".into(),
                            json!(MIN_HEADER_COMPLETENESS_RATIO),
                        );
                        th
                    },
                ),
            );
            checks.insert(
                check_id::WS_STABILITY.into(),
                CheckResult::fail(
                    check_id::WS_STABILITY,
                    failure_reason::PROVIDER_CONNECT,
                    format!("HTTP header sample failed before WS checks: {detail}"),
                    BTreeMap::new(),
                    ws_stability_thresholds(config.duration_secs),
                ),
            );
            return;
        }
    };
    let check_d_http = evaluate_header_completeness(&http_headers);

    // ---- WS: same historical range as HTTP, then sustained subscription (E) ----
    let ws_result = run_ws_checks(
        &config.ws_url,
        from_block,
        tip,
        config.duration_secs,
    )
    .await;

    let (check_c, check_d, check_e) = match ws_result {
        Ok(ws) => {
            let check_c = evaluate_continuity(&http_headers, &ws.historical_headers);
            let check_d = merge_header_completeness(check_d_http, &ws.historical_headers);
            (check_c, check_d, ws.stability)
        }
        Err(e) => {
            let detail = sanitize_error(&e.to_string());
            let check_c = CheckResult::fail(
                check_id::BLOCK_CONTINUITY,
                failure_reason::PROVIDER_CONNECT,
                format!("ws connect failed: {detail}"),
                BTreeMap::new(),
                continuity_thresholds(),
            );
            let check_e = CheckResult::fail(
                check_id::WS_STABILITY,
                failure_reason::PROVIDER_CONNECT,
                format!("ws connect failed: {detail}"),
                BTreeMap::new(),
                ws_stability_thresholds(config.duration_secs),
            );
            (check_c, check_d_http, check_e)
        }
    };
    checks.insert(check_id::BLOCK_CONTINUITY.into(), check_c);
    checks.insert(check_id::HEADER_COMPLETENESS.into(), check_d);
    checks.insert(check_id::WS_STABILITY.into(), check_e);
}

fn finalize_report(
    http_fp: String,
    ws_fp: String,
    git_commit: String,
    started: String,
    checks: BTreeMap<String, CheckResult>,
    out: &Path,
) -> Result<ProbeReport> {
    let report = new_report(
        http_fp,
        ws_fp,
        git_commit,
        started,
        now_unix_label(),
        checks,
    );
    let json = serialize_report(&report).context("serialize probe report")?;
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create report dir {}", parent.display()))?;
        }
    }
    std::fs::write(out, &json).with_context(|| format!("write report {}", out.display()))?;
    println!("{}", format_summary(&report));
    info!(
        target: "rpc_probe",
        out = %out.display(),
        qualified = report.qualified,
        "wrote probe report"
    );
    Ok(report)
}

/// When a pre-check aborts the run, still emit distinct fail rows for every
/// remaining check so A–E always appear independently in the report.
fn fill_skipped_checks(
    checks: &mut BTreeMap<String, CheckResult>,
    reason: &str,
    detail: &str,
    duration_secs: u64,
) {
    let skipped = |name: &str, th: BTreeMap<String, serde_json::Value>| {
        CheckResult::fail(
            name,
            reason,
            format!("skipped: {detail}"),
            BTreeMap::new(),
            th,
        )
    };
    checks
        .entry(check_id::RECEIPT_0X7E.into())
        .or_insert_with(|| {
            let mut th = BTreeMap::new();
            th.insert("min_type_0x7e_receipts".into(), json!(MIN_TYPE_0X7E_RECEIPTS));
            skipped(check_id::RECEIPT_0X7E, th)
        });
    checks
        .entry(check_id::BLOCK_CONTINUITY.into())
        .or_insert_with(|| skipped(check_id::BLOCK_CONTINUITY, continuity_thresholds()));
    checks
        .entry(check_id::HEADER_COMPLETENESS.into())
        .or_insert_with(|| {
            let mut th = BTreeMap::new();
            th.insert(
                "min_header_completeness_ratio".into(),
                json!(MIN_HEADER_COMPLETENESS_RATIO),
            );
            skipped(check_id::HEADER_COMPLETENESS, th)
        });
    checks
        .entry(check_id::WS_STABILITY.into())
        .or_insert_with(|| {
            skipped(
                check_id::WS_STABILITY,
                ws_stability_thresholds(duration_secs),
            )
        });
}

fn connect_http(url: &str) -> Result<impl Provider<Ethereum> + Clone> {
    let url = url.parse().context("parse HTTP RPC URL")?;
    Ok(ProviderBuilder::new().connect_http(url))
}

// ---------------------------------------------------------------------------
// Check A — multi-address eth_getLogs
// ---------------------------------------------------------------------------

async fn run_check_a<P: Provider<Ethereum>>(
    provider: &P,
    addresses: &[Address],
    source: &AddressSetSource,
    logs_window: u64,
) -> CheckResult {
    let mut measured = BTreeMap::new();
    let mut th = BTreeMap::new();
    th.insert("max_http_429".into(), json!(MAX_HTTP_429));
    th.insert("max_http_413".into(), json!(MAX_HTTP_413));
    th.insert(
        "max_multi_address_log_failures".into(),
        json!(MAX_MULTI_ADDRESS_LOG_FAILURES),
    );

    measured.insert("address_count".into(), json!(addresses.len()));
    measured.insert("unique_pool_count".into(), json!(source.unique_count));
    measured.insert("v2_row_count".into(), json!(source.v2_count));
    measured.insert("v3_row_count".into(), json!(source.v3_count));
    measured.insert("moe_row_count".into(), json!(source.moe_count));
    measured.insert(
        "address_multiplier".into(),
        json!(source.address_multiplier),
    );
    measured.insert(
        "address_set_derivation".into(),
        json!(source.derivation_summary()),
    );

    let tip = match provider.get_block_number().await {
        Ok(n) => n,
        Err(e) => {
            measured.insert("error".into(), json!(sanitize_error(&e.to_string())));
            return CheckResult::fail(
                check_id::MULTI_ADDRESS_GET_LOGS,
                failure_reason::MULTI_ADDRESS_REJECTED,
                format!("eth_blockNumber failed: {}", sanitize_error(&e.to_string())),
                measured,
                th,
            );
        }
    };
    let from = tip.saturating_sub(logs_window.saturating_sub(1));
    measured.insert("from_block".into(), json!(from));
    measured.insert("to_block".into(), json!(tip));

    let filter = Filter::new()
        .from_block(from)
        .to_block(tip)
        .address(addresses.to_vec());

    let start = Instant::now();
    let result = provider.get_logs(&filter).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    measured.insert("latency_ms".into(), json!(latency_ms));

    let mut status_dist: BTreeMap<String, u64> = BTreeMap::new();
    match result {
        Ok(logs) => {
            status_dist.insert("success".into(), 1);
            measured.insert("log_count".into(), json!(logs.len()));
            measured.insert("http_status_distribution".into(), json!(status_dist));
            measured.insert("http_429_count".into(), json!(0));
            measured.insert("http_413_count".into(), json!(0));
            measured.insert("failure_count".into(), json!(0));
            CheckResult::pass(check_id::MULTI_ADDRESS_GET_LOGS, measured, th)
        }
        Err(e) => {
            let err_s = e.to_string();
            let classified = classify_rpc_error(&err_s);
            status_dist.insert(classified.status_key.clone(), 1);
            measured.insert("http_status_distribution".into(), json!(status_dist));
            measured.insert("http_429_count".into(), json!(classified.count_429));
            measured.insert("http_413_count".into(), json!(classified.count_413));
            measured.insert("failure_count".into(), json!(1));
            measured.insert("error".into(), json!(sanitize_error(&err_s)));

            let (reason, detail) = if classified.count_429 > 0 {
                (
                    failure_reason::RATE_LIMITED_429,
                    format!(
                        "multi-address eth_getLogs rate-limited (429); address_count={}",
                        addresses.len()
                    ),
                )
            } else if classified.count_413 > 0 {
                (
                    failure_reason::PAYLOAD_TOO_LARGE_413,
                    format!(
                        "multi-address eth_getLogs rejected as too large (413); address_count={}",
                        addresses.len()
                    ),
                )
            } else {
                (
                    failure_reason::MULTI_ADDRESS_REJECTED,
                    format!(
                        "multi-address eth_getLogs failed; address_count={}: {}",
                        addresses.len(),
                        sanitize_error(&err_s)
                    ),
                )
            };
            CheckResult::fail(
                check_id::MULTI_ADDRESS_GET_LOGS,
                reason,
                detail,
                measured,
                th,
            )
        }
    }
}

#[derive(Debug)]
struct ClassifiedError {
    status_key: String,
    count_429: u64,
    count_413: u64,
}

/// Classify transport/RPC errors for Check A status distribution.
fn classify_rpc_error(err: &str) -> ClassifiedError {
    let lower = err.to_ascii_lowercase();
    if lower.contains("429") || lower.contains("too many requests") || lower.contains("rate limit")
    {
        return ClassifiedError {
            status_key: "429".into(),
            count_429: 1,
            count_413: 0,
        };
    }
    if lower.contains("413")
        || lower.contains("too large")
        || lower.contains("response size")
        || lower.contains("entity too large")
    {
        return ClassifiedError {
            status_key: "413".into(),
            count_429: 0,
            count_413: 1,
        };
    }
    if lower.contains("-32602") || lower.contains("blocked parameter") {
        return ClassifiedError {
            status_key: "rpc_-32602".into(),
            count_429: 0,
            count_413: 0,
        };
    }
    // Publicnode and similar free tiers often surface rate limits as HTTP 403
    // with a JSON-RPC body (not always a clean 429).
    if lower.contains("403") && (lower.contains("rate") || lower.contains("blocked")) {
        return ClassifiedError {
            status_key: "403".into(),
            count_429: 1,
            count_413: 0,
        };
    }
    ClassifiedError {
        status_key: "error".into(),
        count_429: 0,
        count_413: 0,
    }
}

// ---------------------------------------------------------------------------
// Check B — 0x7e receipt decoding
// ---------------------------------------------------------------------------

async fn run_check_b<P: Provider<Ethereum>>(
    provider: &P,
    from_block: u64,
    to_block: u64,
) -> CheckResult {
    let mut measured = BTreeMap::new();
    let mut th = BTreeMap::new();
    th.insert("min_type_0x7e_receipts".into(), json!(MIN_TYPE_0X7E_RECEIPTS));
    th.insert("max_receipt_failures".into(), json!(MAX_RECEIPT_FAILURES));
    measured.insert("from_block".into(), json!(from_block));
    measured.insert("to_block".into(), json!(to_block));

    let mut type_0x7e_count: u64 = 0;
    let mut receipt_count: u64 = 0;
    let mut failures: u64 = 0;
    let mut first_failure: Option<String> = None;
    let mut typed_decode_failures: u64 = 0;
    let mut raw_fetch_failures: u64 = 0;

    for number in from_block..=to_block {
        // 1) Raw JSON path — detects provider omission / RPC errors without
        //    alloy typed-decode masking the provider response.
        match provider
            .raw_request::<_, Option<serde_json::Value>>(
                std::borrow::Cow::Borrowed("eth_getBlockReceipts"),
                (BlockId::Number(BlockNumberOrTag::Number(number)),),
            )
            .await
        {
            Ok(Some(value)) => {
                if let Some(arr) = value.as_array() {
                    receipt_count += arr.len() as u64;
                    for (idx, receipt) in arr.iter().enumerate() {
                        let ty = receipt_type_u64(receipt);
                        if ty == Some(0x7e) {
                            type_0x7e_count += 1;
                            if let Err(msg) = validate_raw_receipt_fields(receipt) {
                                failures += 1;
                                let tx = receipt
                                    .get("transactionHash")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                if first_failure.is_none() {
                                    first_failure = Some(format!(
                                        "block={number} receipt_index={idx} tx={tx} type=0x7e: {msg}"
                                    ));
                                }
                            }
                        }
                    }
                } else {
                    failures += 1;
                    raw_fetch_failures += 1;
                    if first_failure.is_none() {
                        first_failure =
                            Some(format!("block={number}: eth_getBlockReceipts not an array"));
                    }
                }
            }
            Ok(None) => {
                failures += 1;
                raw_fetch_failures += 1;
                if first_failure.is_none() {
                    first_failure =
                        Some(format!("block={number}: eth_getBlockReceipts returned null"));
                }
            }
            Err(e) => {
                failures += 1;
                raw_fetch_failures += 1;
                let err_s = sanitize_error(&e.to_string());
                if first_failure.is_none() {
                    first_failure =
                        Some(format!("block={number}: eth_getBlockReceipts error: {err_s}"));
                }
            }
        }

        // 2) Typed alloy path — same shape the bot uses for TransactionReceipt.
        //    OP-stack deposit type 0x7e is the known WHI-526 failure mode.
        match provider
            .get_block_receipts(BlockId::Number(BlockNumberOrTag::Number(number)))
            .await
        {
            Ok(Some(_receipts)) => {
                // Typed decode succeeded for the whole block.
            }
            Ok(None) => {
                // null is acceptable if raw also returned null (already counted).
            }
            Err(e) => {
                typed_decode_failures += 1;
                failures += 1;
                let err_s = e.to_string();
                let is_7e = err_s.contains("7e")
                    || err_s.contains("7E")
                    || err_s.to_ascii_lowercase().contains("unexpected type")
                    || err_s.to_ascii_lowercase().contains("transaction type")
                    || err_s.to_ascii_lowercase().contains("tx type");
                // Do not overwrite a provider-side raw fetch failure with a
                // typed-decode note — failure_reason attribution depends on it.
                if first_failure.is_none() {
                    first_failure = Some(format!(
                        "block={number}: alloy typed receipt decode failed{}: {}",
                        if is_7e { " (type 0x7e)" } else { "" },
                        sanitize_error(&err_s)
                    ));
                }
            }
        }
    }

    measured.insert("receipt_count".into(), json!(receipt_count));
    measured.insert("type_0x7e_count".into(), json!(type_0x7e_count));
    measured.insert("raw_fetch_failures".into(), json!(raw_fetch_failures));
    measured.insert("typed_decode_failures".into(), json!(typed_decode_failures));
    // Provider-side failures only (raw fetch / structural). Typed alloy decode
    // of 0x7e often fails on *all* Mantle providers under Ethereum TxType —
    // that is a client/stack limitation, not an endpoint qualification signal.
    // The bot readiness path uses raw eth_getBlockReceipts for this reason
    // (see examples/protocols/legacy_service_support.rs).
    let provider_side_failures = failures.saturating_sub(typed_decode_failures);
    measured.insert(
        "provider_side_failure_count".into(),
        json!(provider_side_failures),
    );
    if let Some(ref f) = first_failure {
        measured.insert("first_failure".into(), json!(f));
    }

    // Provider omitted or errored on eth_getBlockReceipts.
    // Always `receipt_fetch_error` — do not re-attribute raw transport failures
    // to typed 0x7e decode (that mode is structural incompleteness below / flag).
    if raw_fetch_failures > MAX_RECEIPT_FAILURES {
        return CheckResult::fail(
            check_id::RECEIPT_0X7E,
            failure_reason::RECEIPT_FETCH_ERROR,
            first_failure.unwrap_or_else(|| {
                format!("{raw_fetch_failures} eth_getBlockReceipts failures")
            }),
            measured,
            th,
        );
    }

    // Structural incompleteness of a type-0x7e receipt body.
    let structural_failures = failures.saturating_sub(typed_decode_failures + raw_fetch_failures);
    if structural_failures > MAX_RECEIPT_FAILURES {
        return CheckResult::fail(
            check_id::RECEIPT_0X7E,
            failure_reason::RECEIPT_TYPE_0X7E_DECODE,
            first_failure.unwrap_or_else(|| {
                format!("{structural_failures} incomplete type 0x7e receipts")
            }),
            measured,
            th,
        );
    }

    if type_0x7e_count < MIN_TYPE_0X7E_RECEIPTS {
        return CheckResult::fail(
            check_id::RECEIPT_0X7E,
            failure_reason::NO_TYPE_0X7E_OBSERVED,
            format!(
                "observed {type_0x7e_count} type 0x7e receipts (need ≥ {MIN_TYPE_0X7E_RECEIPTS}); provider may omit deposit receipts"
            ),
            measured,
            th,
        );
    }

    measured.insert(
        "require_alloy_typed_receipt_decode".into(),
        json!(REQUIRE_ALLOY_TYPED_RECEIPT_DECODE),
    );
    th.insert(
        "require_alloy_typed_receipt_decode".into(),
        json!(REQUIRE_ALLOY_TYPED_RECEIPT_DECODE),
    );

    // Optional typed gate (off by default — see thresholds comment).
    if REQUIRE_ALLOY_TYPED_RECEIPT_DECODE && typed_decode_failures > 0 {
        return CheckResult::fail(
            check_id::RECEIPT_0X7E,
            failure_reason::RECEIPT_TYPE_0X7E_DECODE,
            first_failure.unwrap_or_else(|| {
                format!(
                    "alloy typed decode failures={typed_decode_failures} (REQUIRE_ALLOY_TYPED_RECEIPT_DECODE)"
                )
            }),
            measured,
            th,
        );
    }
    if typed_decode_failures > 0 {
        measured.insert(
            "alloy_typed_decode_note".into(),
            json!("Ethereum-typed alloy receipt decode failed (often 0x7e TxType); provider raw path OK; REQUIRE_ALLOY_TYPED_RECEIPT_DECODE=false"),
        );
    }

    CheckResult::pass(check_id::RECEIPT_0X7E, measured, th)
}

fn receipt_type_u64(receipt: &serde_json::Value) -> Option<u64> {
    let ty = receipt.get("type")?;
    if let Some(s) = ty.as_str() {
        let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
        return u64::from_str_radix(s, 16).ok();
    }
    ty.as_u64()
}

fn validate_raw_receipt_fields(receipt: &serde_json::Value) -> Result<(), String> {
    for key in ["transactionHash", "blockHash", "blockNumber", "type"] {
        if receipt.get(key).is_none() {
            return Err(format!("missing field `{key}`"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Checks C + D
// ---------------------------------------------------------------------------

async fn sample_http_headers<P: Provider<Ethereum>>(
    provider: &P,
    from_block: u64,
    to_block: u64,
) -> Result<Vec<SampledHeader>> {
    sample_headers_range(provider, from_block, to_block, true).await
}

/// Sample headers over `[from_block, to_block]` via either transport.
/// When `strict` is true, missing blocks are hard errors; when false, gaps are skipped.
async fn sample_headers_range<P: Provider<Ethereum>>(
    provider: &P,
    from_block: u64,
    to_block: u64,
    strict: bool,
) -> Result<Vec<SampledHeader>> {
    let mut out = Vec::new();
    for number in from_block..=to_block {
        match provider
            .get_block_by_number(BlockNumberOrTag::Number(number))
            .await
        {
            Ok(Some(block)) => {
                let header = block.header();
                out.push(SampledHeader {
                    number: header.number(),
                    hash: header.hash(),
                    parent_hash: header.parent_hash(),
                    timestamp: header.timestamp(),
                });
            }
            Ok(None) if strict => bail!("missing block {number}"),
            Ok(None) => {
                warn!(target: "rpc_probe", number, "missing block in sample");
            }
            Err(e) if strict => {
                return Err(e).with_context(|| format!("get_block_by_number {number}"));
            }
            Err(e) => {
                warn!(
                    target: "rpc_probe",
                    number,
                    error = %sanitize_error(&e.to_string()),
                    "get_block failed in sample"
                );
            }
        }
    }
    Ok(out)
}

fn continuity_thresholds() -> BTreeMap<String, serde_json::Value> {
    let mut th = BTreeMap::new();
    th.insert("max_continuity_gaps".into(), json!(MAX_CONTINUITY_GAPS));
    th.insert(
        "min_http_ws_hash_agreement_ratio".into(),
        json!(MIN_HTTP_WS_HASH_AGREEMENT_RATIO),
    );
    th
}

fn evaluate_continuity(
    http_headers: &[SampledHeader],
    ws_headers: &[SampledHeader],
) -> CheckResult {
    let mut measured = BTreeMap::new();
    let th = continuity_thresholds();

    let http_report = detect_continuity_gaps(http_headers);
    let ws_report = detect_continuity_gaps(ws_headers);
    measured.insert("http_samples".into(), json!(http_report.samples));
    measured.insert("ws_samples".into(), json!(ws_report.samples));
    measured.insert("http_gap_count".into(), json!(http_report.gap_count()));
    measured.insert("ws_gap_count".into(), json!(ws_report.gap_count()));

    if http_report.gap_count() > MAX_CONTINUITY_GAPS {
        return CheckResult::fail(
            check_id::BLOCK_CONTINUITY,
            failure_reason::CONTINUITY_GAP,
            format!(
                "HTTP continuity gaps={} detail={:?}",
                http_report.gap_count(),
                http_report.gaps.first().map(|g| &g.detail)
            ),
            measured,
            th,
        );
    }
    if ws_report.gap_count() > MAX_CONTINUITY_GAPS {
        return CheckResult::fail(
            check_id::BLOCK_CONTINUITY,
            failure_reason::CONTINUITY_GAP,
            format!(
                "WS continuity gaps={} detail={:?}",
                ws_report.gap_count(),
                ws_report.gaps.first().map(|g| &g.detail)
            ),
            measured,
            th,
        );
    }

    // Cross-transport agreement at shared heights.
    let http_by_num: BTreeMap<u64, &SampledHeader> =
        http_headers.iter().map(|h| (h.number, h)).collect();
    let mut compared = 0u64;
    let mut agreed = 0u64;
    let mut first_disagreement: Option<String> = None;
    for ws_h in ws_headers {
        if let Some(http_h) = http_by_num.get(&ws_h.number) {
            compared += 1;
            if headers_agree(http_h, ws_h) {
                agreed += 1;
            } else if first_disagreement.is_none() {
                first_disagreement = Some(format!(
                    "height {}: http_hash={:?} ws_hash={:?}",
                    ws_h.number, http_h.hash, ws_h.hash
                ));
            }
        }
    }
    let ratio = if compared == 0 {
        0.0
    } else {
        agreed as f64 / compared as f64
    };
    measured.insert("cross_compared".into(), json!(compared));
    measured.insert("cross_agreed".into(), json!(agreed));
    measured.insert("cross_agreement_ratio".into(), json!(ratio));

    if compared == 0 {
        return CheckResult::fail(
            check_id::BLOCK_CONTINUITY,
            failure_reason::HTTP_WS_DISAGREEMENT,
            "no overlapping heights between HTTP sample and WS sample".to_string(),
            measured,
            th,
        );
    }
    if !meets_min_ratio(ratio, MIN_HTTP_WS_HASH_AGREEMENT_RATIO) {
        return CheckResult::fail(
            check_id::BLOCK_CONTINUITY,
            failure_reason::HTTP_WS_DISAGREEMENT,
            first_disagreement.unwrap_or_else(|| {
                format!("HTTP/WS agreement ratio {ratio} < {MIN_HTTP_WS_HASH_AGREEMENT_RATIO}")
            }),
            measured,
            th,
        );
    }

    CheckResult::pass(check_id::BLOCK_CONTINUITY, measured, th)
}

fn evaluate_header_completeness(headers: &[SampledHeader]) -> CheckResult {
    let mut measured = BTreeMap::new();
    let mut th = BTreeMap::new();
    th.insert(
        "min_header_completeness_ratio".into(),
        json!(MIN_HEADER_COMPLETENESS_RATIO),
    );
    let total = headers.len() as u64;
    let complete = headers.iter().filter(|h| header_is_complete(h)).count() as u64;
    let ratio = if total == 0 {
        0.0
    } else {
        complete as f64 / total as f64
    };
    measured.insert("samples".into(), json!(total));
    measured.insert("complete".into(), json!(complete));
    measured.insert("completeness_ratio".into(), json!(ratio));

    if total == 0 || !meets_min_ratio(ratio, MIN_HEADER_COMPLETENESS_RATIO) {
        let incomplete = headers.iter().find(|h| !header_is_complete(h));
        let detail = match incomplete {
            Some(h) => format!(
                "incomplete header at #{} hash={:?} parent={:?} ts={}",
                h.number, h.hash, h.parent_hash, h.timestamp
            ),
            None => "no headers sampled".into(),
        };
        return CheckResult::fail(
            check_id::HEADER_COMPLETENESS,
            failure_reason::INCOMPLETE_HEADER,
            detail,
            measured,
            th,
        );
    }
    CheckResult::pass(check_id::HEADER_COMPLETENESS, measured, th)
}

fn merge_header_completeness(
    http_result: CheckResult,
    ws_headers: &[SampledHeader],
) -> CheckResult {
    if !http_result.passed {
        return http_result;
    }
    let ws_result = evaluate_header_completeness(ws_headers);
    if !ws_result.passed {
        return ws_result;
    }
    // Merge measured maps for the report.
    let mut measured = http_result.measured;
    for (k, v) in ws_result.measured {
        measured.insert(format!("ws_{k}"), v);
    }
    CheckResult::pass(
        check_id::HEADER_COMPLETENESS,
        measured,
        http_result.thresholds,
    )
}

// ---------------------------------------------------------------------------
// Check E (+ WS historical sample for C/D)
// ---------------------------------------------------------------------------

struct WsCheckOutput {
    historical_headers: Vec<SampledHeader>,
    stability: CheckResult,
}

fn ws_stability_thresholds(duration_secs: u64) -> BTreeMap<String, serde_json::Value> {
    let mut th = BTreeMap::new();
    th.insert("duration_secs".into(), json!(duration_secs));
    th.insert("max_ws_disconnects".into(), json!(MAX_WS_DISCONNECTS));
    th.insert("max_ws_stalls".into(), json!(MAX_WS_STALLS));
    th.insert(
        "stall_threshold_secs".into(),
        json!(WS_STALL_THRESHOLD_SECS),
    );
    th
}

async fn run_ws_checks(
    ws_url: &str,
    from_block: u64,
    to_block: u64,
    duration_secs: u64,
) -> Result<WsCheckOutput> {
    let ws = WsConnect::new(ws_url);
    let provider = ProviderBuilder::new()
        .connect_ws(ws)
        .await
        .context("connect websocket provider")?;

    // Historical sample via WS transport over the **same** heights as HTTP so
    // cross-transport agreement is meaningful (not tip-drift noise).
    let historical = sample_headers_range(&provider, from_block, to_block, false).await?;

    // Sustained subscription with one reconnect attempt on stream end.
    let mut measured = BTreeMap::new();
    let th = ws_stability_thresholds(duration_secs);
    measured.insert("duration_secs_requested".into(), json!(duration_secs));

    let deadline = Instant::now() + Duration::from_secs(duration_secs);
    let mut heads: u64 = 0;
    let mut stalls: u64 = 0;
    let mut tip_number_gaps: u64 = 0;
    let mut disconnects: u64 = 0;
    let mut reconnect_attempts: u64 = 0;
    let mut reconnect_successes: u64 = 0;
    let mut last_head = Instant::now();
    let mut last_number: Option<u64> = None;

    let mut stream = match provider.subscribe_blocks().await {
        Ok(s) => s.into_stream(),
        Err(e) => {
            return Ok(WsCheckOutput {
                historical_headers: historical,
                stability: CheckResult::fail(
                    check_id::WS_STABILITY,
                    failure_reason::WS_DISCONNECT,
                    format!(
                        "subscribe_blocks failed: {}",
                        sanitize_error(&e.to_string())
                    ),
                    measured,
                    th,
                ),
            });
        }
    };

    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let remaining = deadline.saturating_duration_since(now);
        let wait = remaining.min(Duration::from_secs(WS_STALL_THRESHOLD_SECS));
        match tokio::time::timeout(wait, stream.next()).await {
            Ok(Some(header)) => {
                heads += 1;
                last_head = Instant::now();
                let number = header.number();
                // Missed tip numbers are measured separately; they are not
                // silent stalls (stalls = timeout with no head at all).
                if let Some(prev) = last_number {
                    if number > prev + 1 {
                        tip_number_gaps += 1;
                    }
                }
                last_number = Some(number);
            }
            Ok(None) => {
                disconnects += 1;
                // Reconnect behavior: one resubscribe attempt before giving up.
                reconnect_attempts += 1;
                match provider.subscribe_blocks().await {
                    Ok(s) => {
                        reconnect_successes += 1;
                        stream = s.into_stream();
                        last_head = Instant::now();
                    }
                    Err(e) => {
                        measured.insert(
                            "reconnect_error".into(),
                            json!(sanitize_error(&e.to_string())),
                        );
                        break;
                    }
                }
            }
            Err(_) => {
                if last_head.elapsed() >= Duration::from_secs(WS_STALL_THRESHOLD_SECS) {
                    stalls += 1;
                    last_head = Instant::now();
                }
            }
        }
    }

    measured.insert("heads_received".into(), json!(heads));
    measured.insert("disconnects".into(), json!(disconnects));
    measured.insert("stalls".into(), json!(stalls));
    measured.insert("tip_number_gaps".into(), json!(tip_number_gaps));
    measured.insert("reconnect_attempts".into(), json!(reconnect_attempts));
    measured.insert("reconnect_successes".into(), json!(reconnect_successes));
    measured.insert(
        "elapsed_secs".into(),
        json!(duration_secs.saturating_sub(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs()
        )),
    );

    let stability = if disconnects > MAX_WS_DISCONNECTS {
        CheckResult::fail(
            check_id::WS_STABILITY,
            failure_reason::WS_DISCONNECT,
            format!(
                "ws disconnects={disconnects} (max {MAX_WS_DISCONNECTS}); reconnect_attempts={reconnect_attempts} successes={reconnect_successes}"
            ),
            measured,
            th,
        )
    } else if stalls > MAX_WS_STALLS {
        CheckResult::fail(
            check_id::WS_STABILITY,
            failure_reason::WS_STALL,
            format!(
                "ws silent stalls={stalls} (threshold {}s, max {MAX_WS_STALLS})",
                WS_STALL_THRESHOLD_SECS
            ),
            measured,
            th,
        )
    } else if heads == 0 {
        CheckResult::fail(
            check_id::WS_STABILITY,
            failure_reason::WS_STALL,
            "ws subscription received zero new heads during the probe window".to_string(),
            measured,
            th,
        )
    } else {
        CheckResult::pass(check_id::WS_STABILITY, measured, th)
    };

    Ok(WsCheckOutput {
        historical_headers: historical,
        stability,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Wall-clock label for the report (`unix:<epoch_secs>`).
///
/// Stable and dependency-free; not RFC3339 (no chrono dep in this crate).
fn now_unix_label() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

fn resolve_git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Strip URL-like substrings and common credential markers so reports stay safe.
pub fn sanitize_error(err: &str) -> String {
    let mut out = redact_url_spans(err);
    // Redact common query/header credential shapes even without a full URL.
    for marker in [
        "api_key=",
        "apikey=",
        "api-key=",
        "x-api-key=",
        "token=",
        "bearer ",
        "authorization:",
    ] {
        if let Some(pos) = out.to_ascii_lowercase().find(marker) {
            let end = out[pos..]
                .find(|c: char| c.is_whitespace() || c == ',' || c == '"' || c == '\'')
                .map(|i| pos + i)
                .unwrap_or(out.len());
            out.replace_range(pos..end, "<redacted-secret>");
        }
    }
    if out.len() > 400 {
        out.truncate(400);
        out.push('…');
    }
    out
}

/// Redact `scheme://…` tokens without a regex dependency.
fn redact_url_spans(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 2 < bytes.len() && &input[i..i + 3] == "://" {
            let mut start = i;
            while start > 0 && !bytes[start - 1].is_ascii_whitespace() {
                start -= 1;
            }
            let already = i - start;
            for _ in 0..already {
                out.pop();
            }
            let mut end = i + 3;
            while end < bytes.len() && !bytes[end].is_ascii_whitespace() && bytes[end] != b',' {
                end += 1;
            }
            out.push_str("<redacted-url>");
            i = end;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc_probe::report::within_max;

    #[test]
    fn classify_429_and_413() {
        let a = classify_rpc_error("HTTP status 429 Too Many Requests");
        assert_eq!(a.count_429, 1);
        let b = classify_rpc_error("response too large / 413");
        assert_eq!(b.count_413, 1);
        let c = classify_rpc_error("blocked parameter: params.0.address.# code -32602");
        assert_eq!(c.status_key, "rpc_-32602");
    }

    #[test]
    fn sanitize_strips_urls() {
        let s = sanitize_error("connect failed for https://user:pass@host/path?api_key=1 boom");
        assert!(!s.contains("user:pass"));
        assert!(!s.contains("api_key=1"));
        assert!(s.contains("<redacted-url>"));
    }

    #[test]
    fn receipt_type_parses_hex() {
        let v = json!({"type": "0x7e"});
        assert_eq!(receipt_type_u64(&v), Some(0x7e));
        let v2 = json!({"type": "0x0"});
        assert_eq!(receipt_type_u64(&v2), Some(0));
    }

    #[test]
    fn threshold_comparison_matches_const_block() {
        assert!(within_max(0, MAX_HTTP_429));
        assert!(!within_max(1, MAX_HTTP_429));
    }
}
