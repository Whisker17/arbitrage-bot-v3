//! Lark/Feishu custom-bot card rendering + delivery (WHI-1407 items 3-4).
//!
//! `render_card` is pure presentation over an already-computed
//! [`crate::notify::digest::DigestAggregate`] — **zero aggregation logic** here, per
//! the issue's explicit requirement. `send_card` is the `reqwest::blocking` delivery
//! client: Lark returns HTTP 200 with a rejection body (`code != 0`) for a keyword
//! mismatch / revoked bot / sign failure, so a validator that only checks the HTTP
//! status cannot see that — [`validate_response`] reads the body.
//!
//! `reqwest::blocking::Client` (not the async `Client`) is deliberate for this
//! short-lived one-shot binary — see `Cargo.toml`'s WHI-1407 comment: `blocking` only
//! adds an additional API surface on top of the crate's existing async `Client`, it
//! does not disable it.

use std::time::Duration;

use serde_json::{json, Value};

use crate::notify::digest::{CandidateJoinStatus, DigestAggregate, Freshness, RetentionStatus};

/// Keyword-secured Lark custom bots require this word in the card *title* or the
/// message is silently discarded — same convention as the Python reference
/// (`arb.alerts.lark_elements.KEYWORD`).
pub fn card_shell(title: &str, template: &str, elements: Vec<Value>) -> Value {
    json!({
        "config": { "wide_screen_mode": true },
        "header": {
            "title": { "tag": "plain_text", "content": title },
            "template": template,
        },
        "elements": elements,
    })
}

fn div_md(content: impl Into<String>) -> Value {
    json!({ "tag": "div", "text": { "tag": "lark_md", "content": content.into() } })
}

fn hr() -> Value {
    json!({ "tag": "hr" })
}

fn note(text: impl Into<String>) -> Value {
    json!({ "tag": "note", "elements": [{ "tag": "plain_text", "content": text.into() }] })
}

fn div_fields(pairs: &[(&str, String)]) -> Value {
    json!({
        "tag": "div",
        "fields": pairs.iter().map(|(label, value)| json!({
            "is_short": true,
            "text": { "tag": "lark_md", "content": format!("**{label}**\n{value}") }
        })).collect::<Vec<_>>(),
    })
}

fn fmt_unix(unix_secs: u64) -> String {
    // Dependency-free (`YYYY-MM-DD HH:MM:SS UTC`) — reuses `notify::utc_date`'s civil
    // conversion rather than a new time-formatting dependency.
    let day = crate::notify::utc_date::UtcDay::from_unix(unix_secs);
    let secs_into_day = unix_secs % 86_400;
    let (h, m, s) = (
        secs_into_day / 3600,
        (secs_into_day % 3600) / 60,
        secs_into_day % 60,
    );
    format!("{day} {h:02}:{m:02}:{s:02} UTC")
}

fn retention_label(retention: RetentionStatus) -> Option<String> {
    match retention {
        RetentionStatus::Covered => None,
        RetentionStatus::EmptyLedger => None,
        RetentionStatus::PartiallyRetained {
            earliest_retained_unix,
        } => Some(format!(
            "⚠ 部分留存缺失：当日窗口早段数据已超出留存范围，最早可用数据从 {} 开始",
            fmt_unix(earliest_retained_unix)
        )),
        RetentionStatus::OutsideRetention {
            earliest_retained_unix,
            latest_retained_unix,
        } => Some(format!(
            "⚠ 该日期数据不在当前留存范围内（当前留存 {} → {}），512MiB 留存上限不代表覆盖此窗口",
            fmt_unix(earliest_retained_unix),
            fmt_unix(latest_retained_unix)
        )),
    }
}

fn freshness_label(freshness: Freshness) -> Option<String> {
    match freshness {
        Freshness::NotApplicableHistorical => None,
        Freshness::NoObservations => None,
        Freshness::Fresh { gap_secs } => {
            Some(format!("数据新鲜（最近观测距生成时刻 {gap_secs}s）"))
        }
        Freshness::Stale { gap_secs } => Some(format!(
            "⚠ 数据可能过期（最近观测距生成时刻 {gap_secs}s，超出新鲜度阈值）"
        )),
    }
}

/// Renders the 7-section digest card. Pure function — every value is read straight
/// off `aggregate`; this function performs no aggregation, joining, or windowing of
/// its own.
pub fn render_card(aggregate: &DigestAggregate, keyword: &str) -> Value {
    let title = format!("{keyword} · Mantle dry-run 日报 {}", aggregate.window.day);
    let service_name = aggregate
        .run_identity
        .service
        .clone()
        .unwrap_or_else(|| "未知服务".to_string());
    let banner = div_md(format!(
        "**Signerless / SHADOW_MODE=1 / no sends** — 监控对象: {service_name}"
    ));

    // 2. Window + data health.
    let health = &aggregate.data_health;
    let mut health_lines = vec![format!(
        "窗口 {} → {} (UTC)",
        fmt_unix(aggregate.window.since_unix),
        fmt_unix(aggregate.window.until_unix)
    )];
    if health.observation_count == 0 {
        health_lines.push("无观测数据".to_string());
    } else {
        health_lines.push(format!(
            "观测 {} 次，{} 个不同区块高度",
            health.observation_count, health.distinct_block_heights
        ));
        if let Some(first) = health.first_observed {
            health_lines.push(format!(
                "首次观测: {} (高度 {})",
                fmt_unix(first.recorded_at_unix),
                first.block_number
            ));
        }
        if let Some(last) = health.last_observed {
            health_lines.push(format!(
                "末次观测: {} (高度 {})",
                fmt_unix(last.recorded_at_unix),
                last.block_number
            ));
        }
    }
    if let Some(label) = freshness_label(health.freshness) {
        health_lines.push(label);
    }
    if let Some(label) = retention_label(health.retention) {
        health_lines.push(label);
    }
    let health_block = div_md(health_lines.join("\n"));

    // 3. Operational activity.
    let activity = &aggregate.operational_activity;
    let coverage_label = match activity.cycle_evaluation_coverage {
        Some(c) => format!("{}/{}", c.cycles_optimized_sum, c.cycles_total_sum),
        None => "N/A".to_string(),
    };
    let activity_block = div_fields(&[
        (
            "脏池区块 D/K",
            format!(
                "{}/{}",
                activity.dirty_pool_blocks, activity.discovery_present_count
            ),
        ),
        (
            "缺失 discovery",
            activity.missing_discovery_count.to_string(),
        ),
        ("周期评估覆盖率", coverage_label),
    ]);

    // 4. Continuity.
    let continuity = &aggregate.continuity;
    let gap_label = match continuity.gap_proxy {
        Some(gap) => format!(
            "{}/{} (观测跨度缺口代理指标，非实际跳过率)",
            gap.unobserved_heights, gap.span
        ),
        None => "N/A（观测高度不足以计算跨度）".to_string(),
    };
    let continuity_block = div_md(format!(
        "跨度缺口代理: {gap_label}\n实际跳过率: N/A — 当前 ledger 未记录 skipped heads\n\
         观测到的运行启动/切换: {} 次（⚠ 存在不确定性：受留存窗口/时钟影响，可能与真实重启次数不完全一致）",
        continuity.run_starts_in_window
    ));

    // 5 + 6. Arbitrage summary + per-candidate detail.
    let arb = &aggregate.arbitrage;
    let mut arb_lines = Vec::new();
    if arb.candidate_count == 0 {
        arb_lines.push("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）".to_string());
    } else {
        arb_lines.push(format!(
            "已记录候选 {} 次（preflight 尝试次数，非成交次数）",
            arb.candidate_count
        ));
        for (label, count) in &arb.outcome_counts {
            arb_lines.push(format!("  · {label}: {count}"));
        }
    }
    match &arb.best_net_profit {
        Some(best) => arb_lines.push(format!(
            "最佳建模净利润: {} wei WMNT（digest {}）",
            best.net_profit_wei,
            short_hex(&best.digest)
        )),
        None => arb_lines.push("最佳建模净利润: N/A — 无候选".to_string()),
    }
    if arb.boundary_mismatch_count > 0 {
        arb_lines.push(format!(
            "⚠ {} 条候选/上下文记录跨越日界边界（已标注，未静默合并）",
            arb.boundary_mismatch_count
        ));
    }
    if arb.missing_context_count > 0 {
        arb_lines.push(format!(
            "⚠ {} 条候选缺失对应上下文记录（真实数据缺口）",
            arb.missing_context_count
        ));
    }
    if arb.orphan_context_count > 0 {
        arb_lines.push(format!(
            "⚠ {} 条上下文记录未找到匹配候选（可能是进程中断遗留）",
            arb.orphan_context_count
        ));
    }
    let arb_block = div_md(arb_lines.join("\n"));

    let mut elements = vec![
        banner,
        hr(),
        health_block,
        hr(),
        activity_block,
        hr(),
        continuity_block,
        hr(),
        arb_block,
    ];

    if !arb.candidates_detail.is_empty() {
        let mut lines: Vec<String> = arb
            .candidates_detail
            .iter()
            .map(|c| {
                let profit = c
                    .net_profit_wei
                    .as_deref()
                    .map(|p| format!("{p} wei"))
                    .unwrap_or_else(|| "N/A".to_string());
                let opp = c.opportunity_id.as_deref().unwrap_or("(无上下文)");
                let boundary_tag = match c.join_status {
                    CandidateJoinStatus::BoundaryMismatch => " [跨日界]",
                    CandidateJoinStatus::MissingContext => " [缺失上下文]",
                    _ => "",
                };
                format!(
                    "· {} | {} | 建模利润 {profit} | 结果 {}{boundary_tag}",
                    short_hex(&c.digest),
                    opp,
                    c.outcome_label
                )
            })
            .collect();
        if arb.candidates_detail_remainder > 0 {
            lines.push(format!("+{} more", arb.candidates_detail_remainder));
        }
        elements.push(hr());
        elements.push(div_md(lines.join("\n")));
    }

    // 7. Footer.
    let mut footer_lines = vec![
        format!(
            "服务: {} | chain_id: {} | commit: {}",
            aggregate.run_identity.service.as_deref().unwrap_or("N/A"),
            aggregate
                .run_identity
                .chain_id
                .map(|c| c.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            aggregate
                .run_identity
                .git_commit
                .as_deref()
                .unwrap_or("N/A"),
        ),
        format!("生成时刻: {}", fmt_unix(aggregate.generated_at_unix)),
        format!(
            "窗口: {} → {} (UTC)",
            fmt_unix(aggregate.window.since_unix),
            fmt_unix(aggregate.window.until_unix)
        ),
    ];
    if !aggregate.run_identity.from_window {
        footer_lines.push("⚠ 上述服务身份来自该窗口之外最近一次已知的运行记录".to_string());
    }
    for note_line in &aggregate.data_quality_notes {
        footer_lines.push(format!("⚠ 数据质量: {note_line}"));
    }
    elements.push(hr());
    elements.push(note(footer_lines.join(" | ")));

    card_shell(&title, "blue", elements)
}

fn short_hex(digest: &str) -> String {
    if digest.len() <= 10 {
        digest.to_string()
    } else {
        format!("{}…{}", &digest[..6], &digest[digest.len() - 4..])
    }
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LarkClientConfig {
    pub timeout: Duration,
    /// Total attempts including the first (bounded retries — never unbounded).
    pub max_attempts: u32,
    pub retry_backoff: Duration,
}

impl Default for LarkClientConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_attempts: 3,
            retry_backoff: Duration::from_millis(500),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryOutcome {
    pub sent: bool,
    pub attempts: u32,
    pub http_status: Option<u16>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DeliveryFailure {
    reason: String,
    retryable: bool,
}

/// Redacts a webhook URL to `host …<tail>` — never the full URL/token. Used in
/// **every** log line this module emits, including error paths.
pub fn redact_webhook_url(url_str: &str) -> String {
    let Ok(parsed) = url::Url::parse(url_str) else {
        return "(unparseable-webhook-url)".to_string();
    };
    let host = parsed.host_str().unwrap_or("(no-host)").to_string();
    let path = parsed.path().trim_end_matches('/');
    if path.is_empty() {
        return host;
    }
    let last: Vec<char> = path.rsplit('/').next().unwrap_or("").chars().collect();
    const TAIL_CHARS: usize = 8;
    let tail: String = if last.len() <= TAIL_CHARS {
        let keep = (last.len() / 2).max(1).min(last.len());
        last[last.len() - keep..].iter().collect()
    } else {
        last[last.len() - TAIL_CHARS..].iter().collect()
    };
    format!("{host} …{tail}")
}

/// Validates an HTTP response for the Lark custom-bot flavour: HTTP 2xx **and** a
/// parseable JSON body with numeric `code == 0`. Everything else is a failure —
/// non-2xx, malformed JSON, a missing/non-numeric `code`, and a provider-body
/// rejection (`code != 0`) are all failures, never manufactured successes.
///
/// Only a 5xx status is `retryable`; 4xx (including 429) and any 2xx-but-rejected
/// body are not — matching the issue's explicit retry policy.
fn validate_response(status: u16, body: &str) -> Result<(), DeliveryFailure> {
    if !(200..300).contains(&status) {
        return Err(DeliveryFailure {
            reason: format!("webhook HTTP {status}"),
            retryable: (500..600).contains(&status),
        });
    }
    let parsed: Value = match serde_json::from_str(body) {
        Ok(value) => value,
        Err(error) => {
            return Err(DeliveryFailure {
                reason: format!("HTTP 200 with a non-JSON body: {error}"),
                retryable: false,
            })
        }
    };
    let code = match parsed.get("code") {
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    };
    match code {
        Some(0) => Ok(()),
        Some(other) => {
            let msg = parsed.get("msg").and_then(Value::as_str).unwrap_or("");
            Err(DeliveryFailure {
                reason: format!("lark rejected the card: code={other} msg={msg:?}"),
                retryable: false,
            })
        }
        None => Err(DeliveryFailure {
            reason: "HTTP 200 with a missing/non-numeric `code` field".to_string(),
            retryable: false,
        }),
    }
}

/// Builds the `reqwest::blocking::Client` this module's delivery uses: bounded
/// timeout, redirects disabled ("reject/disable unexpected redirects" — the issue's
/// explicit requirement; a webhook that starts 302-redirecting should surface as a
/// failure, not be silently followed).
pub fn build_client(config: &LarkClientConfig) -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .timeout(config.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| error.to_string())
}

/// POSTs `card` to `webhook_url` as `{"msg_type":"interactive","card":card}`, with
/// bounded retries on transient transport failures and 5xx only. `webhook_url` is
/// never logged in full — only [`redact_webhook_url`]'s output.
pub fn send_card(
    client: &reqwest::blocking::Client,
    webhook_url: &str,
    card: &Value,
    config: &LarkClientConfig,
) -> DeliveryOutcome {
    let redacted = redact_webhook_url(webhook_url);
    let payload = json!({ "msg_type": "interactive", "card": card });
    let mut last_status = None;
    let mut last_error = None;
    let mut attempts_used = 0u32;

    for attempt in 1..=config.max_attempts.max(1) {
        attempts_used = attempt;
        match client.post(webhook_url).json(&payload).send() {
            Ok(response) => {
                let status = response.status().as_u16();
                last_status = Some(status);
                let body = response.text().unwrap_or_default();
                match validate_response(status, &body) {
                    Ok(()) => {
                        tracing::info!(
                            target: "notify.lark",
                            webhook = %redacted,
                            attempts = attempt,
                            "lark digest card delivered"
                        );
                        return DeliveryOutcome {
                            sent: true,
                            attempts: attempt,
                            http_status: last_status,
                            error: None,
                        };
                    }
                    Err(failure) => {
                        tracing::warn!(
                            target: "notify.lark",
                            webhook = %redacted,
                            attempt,
                            status,
                            error = %failure.reason,
                            "lark digest delivery attempt failed"
                        );
                        last_error = Some(failure.reason);
                        if !failure.retryable || attempt == config.max_attempts {
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    target: "notify.lark",
                    webhook = %redacted,
                    attempt,
                    error = %error,
                    "lark digest delivery transport error"
                );
                last_error = Some(error.to_string());
                if attempt == config.max_attempts {
                    break;
                }
            }
        }
        std::thread::sleep(config.retry_backoff);
    }

    tracing::error!(
        target: "notify.lark",
        webhook = %redacted,
        error = last_error.as_deref().unwrap_or("unknown"),
        "lark digest delivery failed after bounded retries"
    );
    DeliveryOutcome {
        sent: false,
        attempts: attempts_used,
        http_status: last_status,
        error: last_error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_the_webhook_url_to_host_and_a_short_tail() {
        let redacted = redact_webhook_url(
            "https://open.larksuite.com/open-apis/bot/v2/hook/1234567890abcdef1234",
        );
        assert!(redacted.starts_with("open.larksuite.com"));
        assert!(!redacted.contains("1234567890abcdef1234"));
        assert!(redacted.contains("…"));
    }

    #[test]
    fn redact_never_reveals_a_short_token_in_full() {
        let redacted = redact_webhook_url("https://example.com/hook/ab");
        // "ab" is 2 chars; half-reveal keeps at most 1.
        assert!(!redacted.contains("/ab"));
    }

    #[test]
    fn redact_handles_an_unparseable_url_without_panicking() {
        let redacted = redact_webhook_url("not a url");
        assert_eq!(redacted, "(unparseable-webhook-url)");
    }

    #[test]
    fn validate_response_accepts_only_numeric_code_zero() {
        assert!(validate_response(200, r#"{"code":0,"msg":"success"}"#).is_ok());
    }

    #[test]
    fn validate_response_rejects_non_2xx_status() {
        let err = validate_response(500, "").unwrap_err();
        assert!(err.retryable);
        let err = validate_response(404, "").unwrap_err();
        assert!(!err.retryable);
        let err = validate_response(429, "").unwrap_err();
        assert!(!err.retryable, "429 must never be retried");
    }

    #[test]
    fn validate_response_rejects_malformed_json_on_200() {
        let err = validate_response(200, "not json").unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn validate_response_rejects_missing_code_field() {
        let err = validate_response(200, r#"{"msg":"ok"}"#).unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn validate_response_rejects_non_numeric_code_field() {
        let err = validate_response(200, r#"{"code":"0"}"#).unwrap_err();
        assert!(!err.retryable);
    }

    #[test]
    fn validate_response_rejects_a_provider_body_rejection_on_200() {
        let err = validate_response(200, r#"{"code":19021,"msg":"sign match fail"}"#).unwrap_err();
        assert!(!err.retryable, "provider rejection must never be retried");
        assert!(err.reason.contains("19021"));
    }

    #[test]
    fn render_card_includes_the_keyword_in_the_title() {
        let aggregate = crate::notify::digest::aggregate_digest(
            &crate::notify::ledger_window::LedgerWindowRead::default(),
            crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            ),
            1_770_000_000,
        );
        let card = render_card(&aggregate, "ARB");
        let title = card["header"]["title"]["content"].as_str().unwrap();
        assert!(title.contains("ARB"));
        assert!(title.contains("2026-06-15"));
    }

    #[test]
    fn render_card_zero_observations_says_no_observation_data_not_healthy_zero() {
        let aggregate = crate::notify::digest::aggregate_digest(
            &crate::notify::ledger_window::LedgerWindowRead::default(),
            crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            ),
            1_770_000_000,
        );
        let card = render_card(&aggregate, "ARB");
        let text = card.to_string();
        assert!(text.contains("无观测数据"));
        assert!(!text.contains("healthy"));
    }

    #[test]
    fn render_card_zero_candidates_names_dry_run_and_never_shows_a_fabricated_zero_profit() {
        let aggregate = crate::notify::digest::aggregate_digest(
            &crate::notify::ledger_window::LedgerWindowRead::default(),
            crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            ),
            1_770_000_000,
        );
        let card = render_card(&aggregate, "ARB");
        let text = card.to_string();
        assert!(text.contains("无套利候选"));
        assert!(text.contains("dry-run"));
        assert!(text.contains("N/A — 无候选"));
    }

    #[test]
    fn render_card_never_labels_a_pass_outcome_as_a_completed_trade() {
        use crate::notify::ledger_window::{
            CandidateOutcomeKind, CandidateRecord, ContextRecord, LedgerWindowRead,
        };
        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;
        let read = LedgerWindowRead {
            candidates: vec![CandidateRecord {
                digest: "0xabc".to_string(),
                outcome: CandidateOutcomeKind::Pass,
                recorded_at_unix: since + 5,
                run_id: "run-a".to_string(),
            }],
            contexts: vec![ContextRecord {
                digest: "0xabc".to_string(),
                opportunity_id: "opp-1".to_string(),
                ordered_pools: vec!["0x01".to_string()],
                net_profit: "500".to_string(),
                block_timestamp: since + 5,
                run_id: "run-a".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&read, window, since + 10);
        let card = render_card(&aggregate, "ARB");
        let text = card.to_string();
        assert!(
            !text.contains("已成交") && !text.contains("完成交易"),
            "a shadow Pass must never read as a completed trade"
        );
        assert!(text.contains("preflight"));
        assert!(
            text.contains("非成交次数"),
            "the disclaimer must explicitly say this is not a trade count"
        );
    }

    #[test]
    fn render_card_skip_rate_is_always_explicit_n_a() {
        let aggregate = crate::notify::digest::aggregate_digest(
            &crate::notify::ledger_window::LedgerWindowRead::default(),
            crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            ),
            1_770_000_000,
        );
        let card = render_card(&aggregate, "ARB");
        let text = card.to_string();
        assert!(text.contains("实际跳过率: N/A"));
        assert!(text.contains("当前 ledger 未记录 skipped heads"));
    }

    // -----------------------------------------------------------------------
    // Delivery integration tests against a minimal hand-rolled HTTP/1.1 mock
    // server (no new test-server dependency: read the request, ignore its
    // exact bytes beyond Content-Length, write back a canned status/body).
    // -----------------------------------------------------------------------

    struct MockResponse {
        status: u16,
        body: &'static str,
    }

    fn find_double_crlf(data: &[u8]) -> Option<usize> {
        data.windows(4).position(|w| w == b"\r\n\r\n")
    }

    fn parse_content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-length") {
                    value.trim().parse::<usize>().ok()
                } else {
                    None
                }
            })
            .unwrap_or(0)
    }

    /// Serves each of `responses` in order to successive connections, then exits.
    /// Returns the bound URL immediately; the server thread is detached (test
    /// assertions rely on the client-observed `DeliveryOutcome`, not on join).
    fn spawn_mock_server(responses: Vec<MockResponse>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut data = Vec::new();
                let mut buf = [0u8; 4096];
                let header_end = loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    data.extend_from_slice(&buf[..n]);
                    if let Some(pos) = find_double_crlf(&data) {
                        break Some(pos);
                    }
                };
                let Some(pos) = header_end else { continue };
                let headers = String::from_utf8_lossy(&data[..pos]).to_string();
                let content_length = parse_content_length(&headers);
                while data.len() < pos + 4 + content_length {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    data.extend_from_slice(&buf[..n]);
                }
                let resp = format!(
                    "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.body.len(),
                    response.body
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/hook/test-token-should-not-appear-in-logs")
    }

    fn fast_config(max_attempts: u32) -> LarkClientConfig {
        LarkClientConfig {
            timeout: Duration::from_secs(2),
            max_attempts,
            retry_backoff: Duration::from_millis(5),
        }
    }

    #[test]
    fn send_card_succeeds_on_the_first_attempt() {
        let url = spawn_mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"code":0,"msg":"success"}"#,
        }]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({"header": {}}), &fast_config(3));
        assert!(outcome.sent);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.http_status, Some(200));
    }

    #[test]
    fn send_card_retries_a_5xx_then_succeeds() {
        let url = spawn_mock_server(vec![
            MockResponse {
                status: 500,
                body: "internal error",
            },
            MockResponse {
                status: 200,
                body: r#"{"code":0}"#,
            },
        ]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({}), &fast_config(3));
        assert!(outcome.sent);
        assert_eq!(outcome.attempts, 2);
    }

    #[test]
    fn send_card_never_retries_a_4xx() {
        let url = spawn_mock_server(vec![MockResponse {
            status: 404,
            body: "not found",
        }]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({}), &fast_config(3));
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1, "a 4xx must never be retried");
        assert_eq!(outcome.http_status, Some(404));
    }

    #[test]
    fn send_card_never_retries_a_provider_body_rejection_on_200() {
        let url = spawn_mock_server(vec![MockResponse {
            status: 200,
            body: r#"{"code":19021,"msg":"keyword not in title"}"#,
        }]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({}), &fast_config(3));
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1);
        assert!(outcome.error.unwrap().contains("19021"));
    }

    #[test]
    fn send_card_does_not_follow_a_redirect() {
        let url = spawn_mock_server(vec![MockResponse {
            status: 302,
            body: "",
        }]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({}), &fast_config(3));
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.http_status, Some(302));
    }

    #[test]
    fn send_card_gives_up_after_bounded_retries_on_repeated_5xx() {
        let url = spawn_mock_server(vec![
            MockResponse {
                status: 500,
                body: "",
            },
            MockResponse {
                status: 500,
                body: "",
            },
            MockResponse {
                status: 500,
                body: "",
            },
        ]);
        let client = build_client(&fast_config(3)).unwrap();
        let outcome = send_card(&client, &url, &json!({}), &fast_config(3));
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 3);
    }

    #[test]
    fn send_card_reports_a_transport_failure_without_panicking() {
        // Nothing listening on this port -> instant connection refused.
        let client = build_client(&fast_config(2)).unwrap();
        let outcome = send_card(&client, "http://127.0.0.1:1", &json!({}), &fast_config(2));
        assert!(!outcome.sent);
        assert!(outcome.error.is_some());
    }
}
