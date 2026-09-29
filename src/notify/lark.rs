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

use crate::notify::digest::{
    CandidateJoinStatus, DigestAggregate, Freshness, OperationalActivity, RetentionStatus,
    ZeroCompletionCause, LIMITED_EVALUATION_COVERAGE_PERCENT,
};

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
            "⚠ 部分留存缺失：当日窗口早段缺少可验证的活动数据（可能已超出留存范围或服务未运行），最早可用数据从 {} 开始",
            fmt_unix(earliest_retained_unix)
        )),
        RetentionStatus::TailUnverified {
            latest_retained_unix,
        } => Some(format!(
            "⚠ 留存覆盖未知：当日窗口末段缺少可验证的活动数据，最晚可用数据为 {}",
            fmt_unix(latest_retained_unix)
        )),
        RetentionStatus::NoActivityRetained => {
            Some("⚠ 留存覆盖未知：账本仅保留运行头记录，没有可验证的活动数据".to_string())
        }
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

/// WHI-1424 completed-search coverage line (`paths_quoted`, labelled completed
/// Ok/NoOptimum searches per WHI-1544): the ratio, the reject breakdown and the
/// limited-coverage warning, or an explicit N/A when the evidence is partial.
fn evaluation_coverage_label(activity: &OperationalActivity) -> String {
    let Some(c) = activity.evaluation_coverage else {
        return if activity.pipeline_liveness_unknown {
            "N/A（部分评估记录缺少 paths_quoted，不下覆盖率结论）".to_string()
        } else {
            "N/A".to_string()
        };
    };
    let mut label = format!(
        "Completed Ok/NoOptimum searches: {} / {} paths ({:.3}%)",
        c.paths_quoted,
        c.paths_evaluated,
        c.paths_quoted as f64 * 100.0 / c.paths_evaluated as f64
    );
    match c.rejects {
        Some(r) => label.push_str(&format!(
            "\n未成候选原因（路径数）: unknown_route {} · unapproved_route {} · pool_lookup {} · no_optimum {} · zero_profit {} · other {}",
            r.unknown_route, r.unapproved_route, r.pool_lookup, r.no_optimum, r.zero_profit, r.other
        )),
        None => label.push_str("\n未成候选原因: N/A（部分记录早于拒绝原因遥测）"),
    }
    if let Some(n) = c.fee_resolution_failures {
        label.push_str(&format!("\n费用无法定价的样本: {n}（样本数，非路径数）"));
    }
    if activity.limited_evaluation_coverage {
        label.push_str(&format!(
            "\n⚠ limited evaluation coverage：完整 Full 轮次仅 {} / {} 路径完成 Ok/NoOptimum 搜索，低于暂定 {}% 阈值（运营取值，非已证明的健康边界）",
            c.full_pass_paths_quoted, c.full_pass_paths_evaluated, LIMITED_EVALUATION_COVERAGE_PERCENT
        ));
    } else if c.full_pass_paths_evaluated == 0 {
        label.push_str(&format!(
            "\n（本窗口无完整 Full 轮次，未评估 {}% 覆盖阈值）",
            LIMITED_EVALUATION_COVERAGE_PERCENT
        ));
    }
    label
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
/// its own. `backlog_note`, when `Some`, is appended to the footer — the issue's
/// "explicitly surface any backlog" requirement is per-*invocation* (how many more
/// outstanding days remain after this one), not a property of any single day's
/// aggregate, so it is threaded in by the caller rather than computed here.
pub fn render_card(
    aggregate: &DigestAggregate,
    keyword: &str,
    backlog_note: Option<&str>,
) -> Value {
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
    let optimizer_paths_label = if activity.is_pipeline_dead {
        format!(
            "{} (⚠ 异常: 0 次完成的 Ok/NoOptimum 搜索)",
            activity.paths_quoted_sum
        )
    } else if activity.pipeline_liveness_unknown {
        if activity.any_paths_quoted_recorded {
            // WHI-1411 round-3: a partial-coverage window (some observations recorded
            // paths_quoted, some didn't) still has a real, if incomplete, sum from the
            // rows that did record it. Show it rather than discarding it as a blanket
            // N/A -- an unrecorded row elsewhere could itself have been fully dead, so
            // the window as a whole still cannot be called healthy.
            // WHI-1424: a zero here is "zero among recorded rows", never proof of a
            // dead pipeline.
            let zero_note = if activity.paths_quoted_sum == 0 {
                "已记录行中为零 / zero among recorded rows；"
            } else {
                ""
            };
            format!(
                "{}（⚠ 部分未知：{zero_note}另有评估周期未记录 paths_quoted，本窗口整体是否存活无法确认）",
                activity.paths_quoted_sum
            )
        } else {
            "N/A（本窗口 ledger 记录缺少 paths_quoted 字段，无法判断管道是否存活）".to_string()
        }
    } else if activity.any_paths_quoted_recorded {
        activity.paths_quoted_sum.to_string()
    } else {
        "N/A".to_string()
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
        ("完成的 Ok/NoOptimum 搜索", optimizer_paths_label),
        ("优化器覆盖", evaluation_coverage_label(activity)),
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
        if activity.is_pipeline_dead {
            // WHI-1544: the cause is read off the aggregate, never assumed — Error
            // searches reach the optimizer yet complete nothing.
            let cause = match activity.zero_completion_cause {
                Some(ZeroCompletionCause::PreSimulationOnly) => {
                    "拒绝原因仅含仿真前类别 unknown_route/unapproved_route/pool_lookup，全部在仿真前被拒绝".to_string()
                }
                Some(ZeroCompletionCause::OptimizerWorkPresent { other }) => format!(
                    "不能认定为仿真前拒绝：存在 other 类拒绝 {other} 条（含优化器 Error 搜索：此类搜索已进入优化器但未完成）"
                ),
                Some(ZeroCompletionCause::Undetermined) | None => {
                    "拒绝原因明细缺失，无法判定是否在仿真前被拒绝".to_string()
                }
            };
            arb_lines.push(format!(
                "⚠ 发现管道异常：统计周期内完成的 Ok/NoOptimum 搜索为 0（{cause}；发现管道失效，非单纯市场安静）；已记录候选 0；无成交"
            ));
        } else if activity.pipeline_liveness_unknown {
            arb_lines.push(
                "⚠ 无法确认发现管道是否存活（本窗口部分或全部 discovery 记录缺少 paths_quoted 字段，无法区分“定价均未盈利”与“无法定价”）；已记录候选 0；无成交"
                    .to_string(),
            );
        } else if activity.limited_evaluation_coverage {
            arb_lines.push(
                "⚠ limited evaluation coverage：绝大多数路径未完成 Ok/NoOptimum 搜索，“无候选”不代表市场安静；已记录候选 0；无成交"
                    .to_string(),
            );
        } else {
            arb_lines.push("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）".to_string());
        }
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
        // Empty day: the issue's literal "N/A — 无候选" label.
        None if arb.candidate_count == 0 => {
            arb_lines.push("最佳建模净利润: N/A — 无候选".to_string())
        }
        // Candidates exist, but none produced a usable modeled-profit value (all
        // missing/boundary-mismatched context, or an unrecognized profit basis) —
        // a distinct label so this is never confused with the empty-day case above.
        None => arb_lines.push("最佳建模净利润: N/A — 候选存在但无可用净利润数据".to_string()),
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
    if arb.malformed_net_profit_count > 0 {
        arb_lines.push(format!(
            "⚠ {} 条上下文记录的 net_profit 无法解析（数据质量问题，已排除于最佳利润之外）",
            arb.malformed_net_profit_count
        ));
    }
    if arb.unmodeled_profit_basis_count > 0 {
        arb_lines.push(format!(
            "⚠ {} 条上下文记录的 profit_basis 不是 simulated（未展示为建模利润）",
            arb.unmodeled_profit_basis_count
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
                let route = c
                    .route_pool_count
                    .map(|n| format!("{n} hop"))
                    .unwrap_or_else(|| "N/A".to_string());
                let boundary_tag = match c.join_status {
                    CandidateJoinStatus::BoundaryMismatch => " [跨日界]",
                    CandidateJoinStatus::MissingContext => " [缺失上下文]",
                    _ => "",
                };
                format!(
                    "· {} | {} | 路线 {route} | 建模利润 {profit} | 结果 {}{boundary_tag}",
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
            "服务: {} | chain_id: {} | executor: {} | commit: {}",
            aggregate.run_identity.service.as_deref().unwrap_or("N/A"),
            aggregate
                .run_identity
                .chain_id
                .map(|c| c.to_string())
                .unwrap_or_else(|| "N/A".to_string()),
            aggregate
                .run_identity
                .executor_contract
                .as_deref()
                .unwrap_or("N/A"),
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
        format!("读取日志段数: {}", aggregate.segments_read_count),
    ];
    if !aggregate.run_identity.from_window {
        footer_lines.push("⚠ 上述服务身份来自该窗口之外最近一次已知的运行记录".to_string());
    }
    for note_line in &aggregate.data_quality_notes {
        footer_lines.push(format!("⚠ 数据质量: {note_line}"));
    }
    if let Some(backlog) = backlog_note {
        footer_lines.push(format!("⚠ 积压: {backlog}"));
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
        let transient = match client.post(webhook_url).json(&payload).send() {
            Ok(response) => {
                let status = response.status().as_u16();
                last_status = Some(status);
                // Status takes precedence: a non-2xx never depends on its body, so a
                // body-read failure there is irrelevant. On a 2xx, a transport
                // failure while reading the body goes through the same sanitized,
                // bounded transport-retry path as a `send()` failure.
                let body = match response.text() {
                    Ok(body) => Ok(body),
                    Err(error) if (200..300).contains(&status) => Err(error),
                    Err(_) => Ok(String::new()),
                };
                match body.map(|body| validate_response(status, &body)) {
                    Ok(Ok(())) => {
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
                    Ok(Err(failure)) => {
                        tracing::warn!(
                            target: "notify.lark",
                            webhook = %redacted,
                            attempt,
                            status,
                            error = %failure.reason,
                            "lark digest delivery attempt failed"
                        );
                        last_error = Some(failure.reason);
                        failure.retryable
                    }
                    Err(error) => {
                        let (transient, reason) = transport_failure(error);
                        tracing::warn!(
                            target: "notify.lark",
                            webhook = %redacted,
                            attempt,
                            status,
                            transient,
                            error = %reason,
                            "lark digest delivery transport error reading the response body"
                        );
                        last_error = Some(reason);
                        transient
                    }
                }
            }
            Err(error) => {
                let (transient, reason) = transport_failure(error);
                tracing::warn!(
                    target: "notify.lark",
                    webhook = %redacted,
                    attempt,
                    transient,
                    error = %reason,
                    "lark digest delivery transport error"
                );
                last_error = Some(reason);
                transient
            }
        };
        if !transient || attempt == config.max_attempts {
            break;
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

/// Classifies and describes a reqwest transport failure. The URL is stripped
/// first: reqwest's `Display`/`Debug` include the full request URL, which carries
/// the webhook token. The description appends the (URL-free) source chain so the
/// operator can see the actual cause (refused, timed out, certificate, …).
fn transport_failure(error: reqwest::Error) -> (bool, String) {
    let transient = is_transient_transport_error(&error);
    let error = error.without_url();
    let mut reason = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        reason.push_str(": ");
        reason.push_str(&cause.to_string());
        source = cause.source();
    }
    (transient, reason)
}

/// Only transient transport failures are retried (the daily-digest spec:
/// "bounded retries on transient transport failures and 5xx only").
///
/// Transient means one of:
/// - a timeout;
/// - a recognized recoverable connection/I/O failure: refused, reset, aborted,
///   broken pipe, unexpected EOF, or unreachable;
/// - the peer closing the connection before a complete response (hyper's
///   `IncompleteMessage`, which carries no io cause);
/// - a *temporary* resolver failure (`EAI_AGAIN`-style "temporary failure" /
///   "try again" / "at this time").
///
/// A permanent resolver answer (unknown host, non-recoverable failure) is a
/// configuration error and fails fast.
///
/// Everything else also fails fast:
/// - builder errors (bad URL/scheme, serialization);
/// - redirect and decode errors;
/// - malformed HTTP replies;
/// - TLS certificate/protocol validation failures (reported as `InvalidData`),
///   which repeating the same request cannot fix.
///
/// The cause-message checks match the pinned hyper 1.x / hyper-util 0.1 wording.
/// The regression tests pin that wording, so a change fails loudly.
fn is_transient_transport_error(error: &reqwest::Error) -> bool {
    use std::io::ErrorKind;
    const PERMANENT: [ErrorKind; 4] = [
        ErrorKind::InvalidData,
        ErrorKind::InvalidInput,
        ErrorKind::Unsupported,
        ErrorKind::PermissionDenied,
    ];
    const RECOVERABLE: [ErrorKind; 11] = [
        ErrorKind::ConnectionRefused,
        ErrorKind::ConnectionReset,
        ErrorKind::ConnectionAborted,
        ErrorKind::NotConnected,
        ErrorKind::BrokenPipe,
        ErrorKind::UnexpectedEof,
        ErrorKind::Interrupted,
        ErrorKind::TimedOut,
        ErrorKind::HostUnreachable,
        ErrorKind::NetworkUnreachable,
        ErrorKind::NetworkDown,
    ];
    const TEMPORARY_RESOLVER: [&str; 3] = ["temporary failure", "try again", "at this time"];

    if error.is_builder() || error.is_redirect() {
        return false;
    }
    if error.is_timeout() {
        return true;
    }
    // Collect every `io::Error` kind and every cause message in the chain.
    // `io::Error::source()` skips the error it wraps, so the walk descends
    // through `get_ref()` explicitly: rustls failures arrive as
    // `Other(InvalidData(<rustls error>))`.
    let mut kinds = Vec::new();
    let mut messages = Vec::new();
    let mut next = std::error::Error::source(error);
    while let Some(cause) = next {
        messages.push(cause.to_string().to_lowercase());
        next = match cause.downcast_ref::<std::io::Error>() {
            Some(io) => {
                kinds.push(io.kind());
                match io.get_ref() {
                    Some(inner) => Some(inner as &(dyn std::error::Error + 'static)),
                    None => cause.source(),
                }
            }
            None => cause.source(),
        };
    }
    if kinds.iter().any(|kind| PERMANENT.contains(kind)) {
        return false;
    }
    let early_close = messages
        .iter()
        .any(|m| m.contains("connection closed before message completed"));
    let temporary_dns = error.is_connect()
        && messages.iter().any(|m| m.starts_with("dns error"))
        && messages
            .iter()
            .any(|m| TEMPORARY_RESOLVER.iter().any(|t| m.contains(t)));
    early_close || temporary_dns || kinds.iter().any(|kind| RECOVERABLE.contains(kind))
}

/// Bundles a built [`reqwest::blocking::Client`], webhook URL, and
/// [`LarkClientConfig`] so callers that send more than one card (the normal
/// multi-day backlog loop, `--date` recovery, `--send-test`) don't each
/// reconstruct the client and re-spell `send_card(&client, &webhook_url, card,
/// &config)` at every call site. One [`build_client`] failure surfaces once at
/// construction, not per-call.
pub struct LarkSender {
    client: reqwest::blocking::Client,
    webhook_url: String,
    config: LarkClientConfig,
}

impl LarkSender {
    pub fn new(webhook_url: String, config: LarkClientConfig) -> Result<Self, String> {
        let client = build_client(&config)?;
        Ok(Self {
            client,
            webhook_url,
            config,
        })
    }

    pub fn send(&self, card: &Value) -> DeliveryOutcome {
        send_card(&self.client, &self.webhook_url, card, &self.config)
    }

    /// Redacted webhook target for logging — never the full URL/token.
    pub fn redacted_webhook(&self) -> String {
        redact_webhook_url(&self.webhook_url)
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
        let card = render_card(&aggregate, "ARB", None);
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
        let card = render_card(&aggregate, "ARB", None);
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
        let card = render_card(&aggregate, "ARB", None);
        let text = card.to_string();
        assert!(text.contains("无套利候选"));
        assert!(text.contains("dry-run"));
        assert!(text.contains("N/A — 无候选"));
    }

    /// WHI-1411 acceptance: a digest rendered from a zero-optimizer-path window is visibly
    /// distinct from one rendered from a genuinely quiet market; both fixtures are tested.
    #[test]
    fn render_card_distinguishes_zero_optimizer_path_from_quiet_market() {
        use crate::notify::ledger_window::{DiscoveryRecord, LedgerWindowRead, ObservationRecord};

        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;

        // 1. Quiet market fixture: cycles evaluated, paths reached optimizer (paths_quoted > 0),
        // but no candidates found (all NoOptimum / unprofitable).
        let quiet_read = LedgerWindowRead {
            observations: vec![ObservationRecord {
                block_number: 100,
                block_timestamp: since + 10,
                recorded_at_unix: since + 10,
                discovery: Some(DiscoveryRecord {
                    skipped: false,
                    skip_reason: None,
                    dirty_pools_count: 2,
                    cycles_optimized: Some(50),
                    cycles_total: Some(100),
                    paths_quoted: Some(50),
                    ..Default::default()
                }),
                run_id: "run-quiet".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let quiet_aggregate = crate::notify::digest::aggregate_digest(&quiet_read, window, since + 20);
        let quiet_card = render_card(&quiet_aggregate, "ARB", None);
        let quiet_text = quiet_card.to_string();

        // 2. Dead pipeline fixture: cycles evaluated, but ZERO paths reached optimizer
        // (paths_quoted == 0, e.g. 100% pre-simulation rejection).
        let dead_read = LedgerWindowRead {
            observations: vec![ObservationRecord {
                block_number: 100,
                block_timestamp: since + 10,
                recorded_at_unix: since + 10,
                discovery: Some(DiscoveryRecord {
                    skipped: false,
                    skip_reason: None,
                    dirty_pools_count: 2,
                    cycles_optimized: Some(50),
                    cycles_total: Some(100),
                    paths_quoted: Some(0),
                    ..Default::default()
                }),
                run_id: "run-dead".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let dead_aggregate = crate::notify::digest::aggregate_digest(&dead_read, window, since + 20);
        let dead_card = render_card(&dead_aggregate, "ARB", None);
        let dead_text = dead_card.to_string();

        // Visibly distinct:
        assert_ne!(quiet_text, dead_text, "quiet and dead pipeline digests must be distinct");

        // Quiet market: healthy 0 candidates, no pipeline alarm
        assert!(quiet_text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"));
        assert!(quiet_text.contains("50"));
        assert!(!quiet_text.contains("发现管道异常"));

        // Dead pipeline: explicitly warns about zero completed Ok/NoOptimum searches
        assert!(dead_text.contains("发现管道异常"));
        assert!(dead_text.contains("完成的 Ok/NoOptimum 搜索为 0"));
        assert!(!dead_text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"));
        assert!(dead_text.contains("0 (⚠ 异常: 0 次完成的 Ok/NoOptimum 搜索)"));
        // WHI-1544: this fixture records no reject breakdown, so the card must not
        // assert a pre-simulation cause it has no evidence for.
        assert!(dead_text.contains("拒绝原因明细缺失"), "{dead_text}");
        assert!(!dead_text.contains("全部在仿真前被拒绝"), "{dead_text}");
    }

    /// WHI-1411 fail-closed acceptance: when cycles were evaluated in-window but **no**
    /// observation carries `paths_quoted` at all (e.g. an older ledger schema), the card
    /// must say liveness is undeterminable — never silently render the healthy clean zero.
    #[test]
    fn render_card_says_unknown_not_healthy_when_paths_quoted_is_never_recorded() {
        use crate::notify::ledger_window::{DiscoveryRecord, LedgerWindowRead, ObservationRecord};

        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;

        let unknown_read = LedgerWindowRead {
            observations: vec![ObservationRecord {
                block_number: 100,
                block_timestamp: since + 10,
                recorded_at_unix: since + 10,
                discovery: Some(DiscoveryRecord {
                    skipped: false,
                    skip_reason: None,
                    dirty_pools_count: 2,
                    cycles_optimized: Some(50),
                    cycles_total: Some(100),
                    paths_quoted: None,
                    ..Default::default()
                }),
                run_id: "run-unknown".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&unknown_read, window, since + 20);
        assert!(aggregate.operational_activity.pipeline_liveness_unknown);
        assert!(!aggregate.operational_activity.is_pipeline_dead);

        let card = render_card(&aggregate, "ARB", None);
        let text = card.to_string();

        // Must not silently claim healthy:
        assert!(
            !text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"),
            "must not render the healthy clean-zero text when liveness is undeterminable: {text}"
        );
        // Must not claim the pipeline is confirmed dead either (we cannot tell):
        assert!(!text.contains("发现管道异常"));
        // Must explicitly say liveness is unknown:
        assert!(text.contains("无法确认发现管道是否存活"));
    }

    /// WHI-1411 round-3: a partial-coverage window (some observations record
    /// `paths_quoted`, others don't) must still render as undeterminable, and the label
    /// must surface the real partial `paths_quoted_sum` rather than discarding it as a
    /// blanket N/A -- an unrecorded row elsewhere could itself have been fully dead.
    #[test]
    fn render_card_surfaces_partial_paths_quoted_sum_while_still_flagging_unknown() {
        use crate::notify::ledger_window::{DiscoveryRecord, LedgerWindowRead, ObservationRecord};

        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;

        let partial_read = LedgerWindowRead {
            observations: vec![
                // Tiny healthy-looking recorded pass.
                ObservationRecord {
                    block_number: 100,
                    block_timestamp: since + 10,
                    recorded_at_unix: since + 10,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10),
                        cycles_total: Some(10),
                        paths_quoted: Some(7),
                        ..Default::default()
                    }),
                    run_id: "run-partial".to_string(),
                },
                // Much larger unrecorded pass -- liveness for this pass is genuinely
                // unknown and must not be masked by the recorded row above.
                ObservationRecord {
                    block_number: 101,
                    block_timestamp: since + 20,
                    recorded_at_unix: since + 20,
                    discovery: Some(DiscoveryRecord {
                        skipped: false,
                        skip_reason: None,
                        dirty_pools_count: 1,
                        cycles_optimized: Some(10_000),
                        cycles_total: Some(10_000),
                        paths_quoted: None,
                        ..Default::default()
                    }),
                    run_id: "run-partial".to_string(),
                },
            ],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&partial_read, window, since + 30);
        assert!(aggregate.operational_activity.pipeline_liveness_unknown);
        assert!(!aggregate.operational_activity.is_pipeline_dead);
        assert_eq!(aggregate.operational_activity.paths_quoted_sum, 7);

        let card = render_card(&aggregate, "ARB", None);
        let text = card.to_string();

        // Must not silently claim healthy:
        assert!(!text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"));
        // Must explicitly say liveness is unknown, same as the never-recorded case:
        assert!(text.contains("无法确认发现管道是否存活"));
        // Must surface the real recorded partial sum (7), not discard it as a blanket N/A:
        assert!(
            text.contains("7（⚠ 部分未知"),
            "expected the partial paths_quoted_sum surfaced with a caveat, got: {text}"
        );
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
                has_block_tag: true,
                run_id: "run-a".to_string(),
            }],
            contexts: vec![ContextRecord {
                digest: "0xabc".to_string(),
                opportunity_id: "opp-1".to_string(),
                ordered_pools: vec!["0x01".to_string()],
                net_profit: "500".to_string(),
                profit_basis: "simulated".to_string(),
                block_timestamp: since + 5,
                run_id: "run-a".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&read, window, since + 10);
        let card = render_card(&aggregate, "ARB", None);
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
    fn render_card_distinguishes_no_candidates_from_candidates_with_no_usable_profit() {
        use crate::notify::ledger_window::{
            CandidateOutcomeKind, CandidateRecord, LedgerWindowRead,
        };
        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;
        // A candidate exists but has no matching context at all -> MissingContext,
        // no usable net profit -- must NOT render the empty-day "无候选" label.
        let read = LedgerWindowRead {
            candidates: vec![CandidateRecord {
                digest: "0xabc".to_string(),
                outcome: CandidateOutcomeKind::Pass,
                recorded_at_unix: since + 5,
                has_block_tag: true,
                run_id: "run-a".to_string(),
            }],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&read, window, since + 10);
        let card = render_card(&aggregate, "ARB", None);
        let text = card.to_string();
        assert!(
            !text.contains("N/A — 无候选"),
            "1 candidate exists; the empty-day label must not appear"
        );
        assert!(text.contains("候选存在但无可用净利润数据"));
    }

    #[test]
    fn render_card_shows_route_hop_count_and_executor_identity_and_segment_count() {
        use crate::notify::ledger_window::{
            CandidateOutcomeKind, CandidateRecord, ContextRecord, LedgerRunIdentity,
            LedgerWindowRead,
        };
        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let since = window.since_unix;
        let read = LedgerWindowRead {
            run_headers: vec![LedgerRunIdentity {
                run_id: "run-a".to_string(),
                started_at_unix: since,
                service: "bot".to_string(),
                chain_id: 5000,
                git_commit: "deadbeef".to_string(),
                executor_contract: "0xExecutor".to_string(),
                wmnt_address: "0xWmnt".to_string(),
            }],
            candidates: vec![CandidateRecord {
                digest: "0xabc".to_string(),
                outcome: CandidateOutcomeKind::Pass,
                recorded_at_unix: since + 5,
                has_block_tag: true,
                run_id: "run-a".to_string(),
            }],
            contexts: vec![ContextRecord {
                digest: "0xabc".to_string(),
                opportunity_id: "opp-1".to_string(),
                ordered_pools: vec!["0x01".to_string(), "0x02".to_string(), "0x03".to_string()],
                net_profit: "777".to_string(),
                profit_basis: "simulated".to_string(),
                block_timestamp: since + 5,
                run_id: "run-a".to_string(),
            }],
            segments_read: vec!["ledger.jsonl".to_string(), "ledger.jsonl.1".to_string()],
            ..LedgerWindowRead::default()
        };
        let aggregate = crate::notify::digest::aggregate_digest(&read, window, since + 10);
        let card = render_card(&aggregate, "ARB", None);
        let text = card.to_string();
        assert!(
            text.contains("3 hop"),
            "route hop count must be rendered: {text}"
        );
        assert!(
            text.contains("0xExecutor"),
            "executor identity must be in the footer: {text}"
        );
        assert!(
            text.contains("读取日志段数: 2"),
            "segment count must be in the footer: {text}"
        );
    }

    #[test]
    fn render_card_surfaces_an_explicit_backlog_note_when_given_one() {
        let aggregate = crate::notify::digest::aggregate_digest(
            &crate::notify::ledger_window::LedgerWindowRead::default(),
            crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            ),
            1_770_000_000,
        );
        let card = render_card(&aggregate, "ARB", Some("5 个待发送日期仍在排队"));
        let text = card.to_string();
        assert!(text.contains("积压"));
        assert!(text.contains("5 个待发送日期仍在排队"));

        let card_without = render_card(&aggregate, "ARB", None);
        assert!(!card_without.to_string().contains("积压"));
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
        let card = render_card(&aggregate, "ARB", None);
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

    // -- WHI-1541: transport errors must never carry the webhook URL/token -------

    const FAKE_TOKEN: &str = "fake-secret-token-0123456789abcdef";

    /// A local port with nothing listening (bound, then released).
    fn closed_local_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Runs `send_card` under a capturing tracing subscriber; returns the outcome
    /// and every emitted log line.
    fn send_capturing_logs(url: &str, max_attempts: u32) -> (DeliveryOutcome, String) {
        send_capturing_logs_with(url, &fast_config(max_attempts))
    }

    fn send_capturing_logs_with(url: &str, config: &LarkClientConfig) -> (DeliveryOutcome, String) {
        send_capturing_logs_via(&build_client(config).unwrap(), url, config)
    }

    fn send_capturing_logs_via(
        client: &reqwest::blocking::Client,
        url: &str,
        config: &LarkClientConfig,
    ) -> (DeliveryOutcome, String) {
        use std::io;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct Buf(Arc<Mutex<Vec<u8>>>);
        impl io::Write for Buf {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().write(bytes)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
            type Writer = Buf;
            fn make_writer(&'a self) -> Self::Writer {
                self.clone()
            }
        }

        let buf = Buf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let outcome = tracing::subscriber::with_default(subscriber, || {
            send_card(client, url, &json!({}), config)
        });
        let logs = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        (outcome, logs)
    }

    fn assert_no_secret(label: &str, text: &str, url: &str) {
        assert!(
            !text.contains(FAKE_TOKEN),
            "{label} leaks the token: {text}"
        );
        assert!(!text.contains(url), "{label} leaks the full URL: {text}");
        assert!(
            !text.contains("/hook/"),
            "{label} leaks the URL path: {text}"
        );
    }

    #[test]
    fn a_transport_error_never_leaks_the_webhook_url_or_token() {
        let url = format!("http://127.0.0.1:{}/hook/{FAKE_TOKEN}", closed_local_port());
        let (outcome, logs) = send_capturing_logs(&url, 2);
        assert!(!outcome.sent);
        // Connection refused is transient: it is retried up to the bound.
        assert_eq!(outcome.attempts, 2);
        let error = outcome.error.expect("a transport failure reports an error");
        assert!(
            logs.contains("lark digest delivery transport error"),
            "{logs}"
        );
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    #[test]
    fn a_permanent_request_error_is_not_retried_and_does_not_leak_the_url() {
        // An unsupported scheme is a reqwest builder error: permanent, never
        // transient, so it must fail fast on the first attempt.
        let url = format!("ftp://127.0.0.1:{}/hook/{FAKE_TOKEN}", closed_local_port());
        let (outcome, logs) = send_capturing_logs(&url, 3);
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1, "a permanent error must not be retried");
        let error = outcome.error.expect("a builder failure reports an error");
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    /// One scripted loopback connection: after the client's first bytes arrive,
    /// write `reply`, then hold the socket open for `hold` before closing it.
    struct RawReply {
        reply: Vec<u8>,
        hold: Duration,
    }

    /// Serves each of `replies` to successive connections, each on its own thread
    /// (so a held connection never delays the next accept).
    fn spawn_raw_server(replies: Vec<RawReply>) -> u16 {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for RawReply { reply, hold } in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                std::thread::spawn(move || {
                    // Read the whole HTTP request (so closing never resets unread
                    // bytes); a non-HTTP client (TLS ClientHello) ends on the
                    // read timeout instead.
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
                    let mut data = Vec::new();
                    let mut buf = [0u8; 16 * 1024];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => data.extend_from_slice(&buf[..n]),
                        }
                        if let Some(pos) = find_double_crlf(&data) {
                            let headers = String::from_utf8_lossy(&data[..pos]).to_string();
                            if data.len() >= pos + 4 + parse_content_length(&headers) {
                                break;
                            }
                        }
                    }
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    std::thread::sleep(hold);
                });
            }
        });
        port
    }

    fn handshake(msg_type: u8, body: &[u8]) -> Vec<u8> {
        let len = body.len() as u32;
        let mut out = vec![msg_type];
        out.extend_from_slice(&len.to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    /// A canned TLS 1.2 server flight (ServerHello, Certificate, ServerKeyExchange,
    /// ServerHelloDone) presenting `tests/fixtures/notify/untrusted-self-signed-localhost.der`,
    /// a self-signed certificate whose private key was discarded at generation.
    /// The client validates the certificate chain before it would check the
    /// (dummy) key-exchange signature, so this deterministically produces a real
    /// rustls certificate-validation failure with TLS verification left on.
    fn untrusted_certificate_flight() -> Vec<u8> {
        let cert: &[u8] =
            include_bytes!("../../tests/fixtures/notify/untrusted-self-signed-localhost.der");

        let mut server_hello = vec![0x03, 0x03];
        server_hello.extend_from_slice(&[0x42; 32]); // server random
        server_hello.push(0); // empty session id
        server_hello.extend_from_slice(&[0xc0, 0x2f]); // ECDHE_RSA_WITH_AES_128_GCM_SHA256
        server_hello.push(0); // null compression
        server_hello.extend_from_slice(&[0x00, 0x04, 0x00, 0x17, 0x00, 0x00]); // extended_master_secret

        let cert_len = (cert.len() as u32).to_be_bytes();
        let list_len = (cert.len() as u32 + 3).to_be_bytes();
        let mut certificate = list_len[1..].to_vec();
        certificate.extend_from_slice(&cert_len[1..]);
        certificate.extend_from_slice(cert);

        let mut key_exchange = vec![0x03, 0x00, 0x1d, 0x20]; // named curve x25519, 32-byte key
        key_exchange.extend_from_slice(&[0x09; 32]);
        key_exchange.extend_from_slice(&[0x08, 0x04, 0x01, 0x00]); // rsa_pss_rsae_sha256, 256 bytes
        key_exchange.extend_from_slice(&[0x00; 256]);

        let mut messages = handshake(2, &server_hello);
        messages.extend(handshake(11, &certificate));
        messages.extend(handshake(12, &key_exchange));
        messages.extend(handshake(14, &[]));

        let mut record = vec![0x16, 0x03, 0x03];
        record.extend_from_slice(&(messages.len() as u16).to_be_bytes());
        record.extend(messages);
        record
    }

    #[test]
    fn a_tls_certificate_failure_is_permanent_and_not_retried() {
        let replies = (0..3)
            .map(|_| RawReply {
                reply: untrusted_certificate_flight(),
                hold: Duration::from_millis(200),
            })
            .collect();
        let url = format!(
            "https://127.0.0.1:{}/hook/{FAKE_TOKEN}",
            spawn_raw_server(replies)
        );
        let (outcome, logs) = send_capturing_logs(&url, 3);
        assert!(!outcome.sent);
        assert_eq!(
            outcome.attempts, 1,
            "an invalid certificate must not be retried"
        );
        assert!(
            logs.to_lowercase().contains("certificate"),
            "expected a certificate-validation failure: {logs}"
        );
        let error = outcome.error.expect("a TLS failure reports an error");
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    #[test]
    fn a_non_http_protocol_reply_is_permanent_and_not_retried() {
        let replies = (0..3)
            .map(|_| RawReply {
                reply: b"NOT-HTTP\r\n\r\n".to_vec(),
                hold: Duration::ZERO,
            })
            .collect();
        let url = format!(
            "http://127.0.0.1:{}/hook/{FAKE_TOKEN}",
            spawn_raw_server(replies)
        );
        let (outcome, logs) = send_capturing_logs(&url, 3);
        assert!(!outcome.sent);
        assert_eq!(
            outcome.attempts, 1,
            "a protocol violation must not be retried"
        );
        let error = outcome.error.expect("a protocol failure reports an error");
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    #[test]
    fn a_body_read_timeout_after_200_headers_is_retried_as_transient() {
        let stall = RawReply {
            reply: b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n".to_vec(),
            hold: Duration::from_millis(1500),
        };
        let ok = RawReply {
            reply: b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"code\":0}".to_vec(),
            hold: Duration::ZERO,
        };
        let config = LarkClientConfig {
            timeout: Duration::from_millis(300),
            max_attempts: 3,
            retry_backoff: Duration::from_millis(5),
        };

        // Stall, then a good reply: the timed-out body read is retried and the
        // second attempt delivers.
        let port = spawn_raw_server(vec![stall, ok]);
        let url = format!("http://127.0.0.1:{port}/hook/{FAKE_TOKEN}");
        let (outcome, logs) = send_capturing_logs_with(&url, &config);
        assert!(outcome.sent, "{outcome:?}\n{logs}");
        assert_eq!(outcome.attempts, 2);
        assert!(!logs.contains("non-JSON"), "{logs}");
        assert_no_secret("tracing output", &logs, &url);

        // Stalls on every attempt: bounded, reported as a transport failure (not a
        // malformed body), and sanitized.
        let stalls = (0..3)
            .map(|_| RawReply {
                reply: b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n"
                    .to_vec(),
                hold: Duration::from_millis(1500),
            })
            .collect();
        let url = format!(
            "http://127.0.0.1:{}/hook/{FAKE_TOKEN}",
            spawn_raw_server(stalls)
        );
        let (outcome, logs) = send_capturing_logs_with(&url, &config);
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 3);
        assert_eq!(outcome.http_status, Some(200));
        let error = outcome.error.expect("a body timeout reports an error");
        assert!(!error.contains("non-JSON"), "{error}");
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    #[test]
    fn a_complete_malformed_200_body_is_still_not_retried() {
        let replies = (0..3)
            .map(|_| RawReply {
                reply: b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nnot-json"
                    .to_vec(),
                hold: Duration::ZERO,
            })
            .collect();
        let url = format!(
            "http://127.0.0.1:{}/hook/{FAKE_TOKEN}",
            spawn_raw_server(replies)
        );
        let (outcome, _logs) = send_capturing_logs(&url, 3);
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1);
        assert!(outcome.error.unwrap().contains("non-JSON"));
    }

    const OK_REPLY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{\"code\":0}";

    #[test]
    fn an_early_close_before_complete_headers_is_retried_as_transient() {
        // The peer closes gracefully after the request, before a complete
        // response head (hyper `IncompleteMessage`, no io cause): partial
        // headers, and no bytes at all. Both recover on the next attempt.
        for early in [b"HTTP/1.1 200 OK\r\nContent-Len".to_vec(), Vec::new()] {
            let port = spawn_raw_server(vec![
                RawReply {
                    reply: early,
                    hold: Duration::ZERO,
                },
                RawReply {
                    reply: OK_REPLY.to_vec(),
                    hold: Duration::ZERO,
                },
            ]);
            let url = format!("http://127.0.0.1:{port}/hook/{FAKE_TOKEN}");
            let (outcome, logs) = send_capturing_logs(&url, 3);
            assert_eq!(outcome.attempts, 2, "{outcome:?}\n{logs}");
            assert!(outcome.sent, "{outcome:?}\n{logs}");
            assert!(
                logs.contains("connection closed before message completed"),
                "{logs}"
            );
            assert_no_secret("tracing output", &logs, &url);
        }
    }

    /// Test resolver for `localhost`: fails the first `failures` lookups with
    /// `message`, then resolves to the loopback `addr`. No real DNS query.
    struct ScriptedResolver {
        failures: usize,
        message: &'static str,
        addr: std::net::SocketAddr,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl reqwest::dns::Resolve for ScriptedResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result: Result<reqwest::dns::Addrs, Box<dyn std::error::Error + Send + Sync>> =
                if call < self.failures {
                    Err(Box::new(std::io::Error::other(self.message)))
                } else {
                    Ok(Box::new(std::iter::once(self.addr)))
                };
            Box::pin(async move { result })
        }
    }

    fn send_via_scripted_resolver(
        failures: usize,
        message: &'static str,
    ) -> (DeliveryOutcome, String, String) {
        let port = spawn_raw_server(vec![RawReply {
            reply: OK_REPLY.to_vec(),
            hold: Duration::ZERO,
        }]);
        let resolver = std::sync::Arc::new(ScriptedResolver {
            failures,
            message,
            addr: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let config = fast_config(3);
        let client = reqwest::blocking::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .dns_resolver(resolver)
            .build()
            .unwrap();
        let url = format!("http://localhost:{port}/hook/{FAKE_TOKEN}");
        let (outcome, logs) = send_capturing_logs_via(&client, &url, &config);
        (outcome, logs, url)
    }

    #[test]
    fn a_temporary_resolver_failure_is_retried_as_transient() {
        // glibc/macOS `EAI_AGAIN` wording.
        let (outcome, logs, url) =
            send_via_scripted_resolver(1, "Temporary failure in name resolution");
        assert_eq!(outcome.attempts, 2, "{outcome:?}\n{logs}");
        assert!(outcome.sent, "{outcome:?}\n{logs}");
        assert!(logs.contains("dns error"), "{logs}");
        assert_no_secret("tracing output", &logs, &url);
    }

    #[test]
    fn a_permanent_resolver_failure_is_not_retried() {
        // `EAI_NONAME`: the host does not exist -- a configuration error that
        // repeating the lookup cannot fix.
        let (outcome, logs, url) = send_via_scripted_resolver(3, "Name or service not known");
        assert!(!outcome.sent);
        assert_eq!(outcome.attempts, 1, "{outcome:?}\n{logs}");
        let error = outcome.error.expect("a resolver failure reports an error");
        assert!(error.contains("dns error"), "{error}");
        assert_no_secret("tracing output", &logs, &url);
        assert_no_secret("DeliveryOutcome.error", &error, &url);
    }

    // -- WHI-1424: evaluation-coverage visibility --------------------------------

    fn coverage_row(
        block: u64,
        at: u64,
        evaluated: u64,
        quoted: Option<u64>,
        rejects: Option<crate::notify::ledger_window::DiscoveryRejects>,
    ) -> crate::notify::ledger_window::ObservationRecord {
        crate::notify::ledger_window::ObservationRecord {
            block_number: block,
            block_timestamp: at,
            recorded_at_unix: at,
            discovery: Some(crate::notify::ledger_window::DiscoveryRecord {
                cycles_optimized: Some(evaluated),
                cycles_total: Some(evaluated),
                paths_quoted: quoted,
                scope: Some("full".to_string()),
                fee_resolution_failures: rejects.map(|_| 0),
                rejects,
                ..Default::default()
            }),
            run_id: "run-coverage".to_string(),
        }
    }

    fn render_rows(
        rows: Vec<crate::notify::ledger_window::ObservationRecord>,
    ) -> (DigestAggregate, String) {
        let window = crate::notify::digest::DigestWindow::for_day(
            crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
        );
        let read = crate::notify::ledger_window::LedgerWindowRead {
            observations: rows,
            ..Default::default()
        };
        let aggregate =
            crate::notify::digest::aggregate_digest(&read, window, window.since_unix + 100);
        let text = render_card(&aggregate, "ARB", None).to_string();
        (aggregate, text)
    }

    /// WHI-1424 AC: the WHI-1411 live window — three Full re-baselines of 6,962
    /// cycles, 8 reaching the optimizer each, i.e. 24 / 20,886
    /// (`evidence/shadow/whi-1411-rejection-liveness/STATUS.md`) — shows the
    /// coverage line and the limited-coverage warning instead of the healthy
    /// "no candidates" card. A quiet window above the threshold does not warn.
    #[test]
    fn render_card_flags_limited_evaluation_coverage_on_the_whi_1411_live_window() {
        use crate::notify::ledger_window::DiscoveryRejects;

        let live_rejects = DiscoveryRejects {
            unknown_route: 6794,
            unapproved_route: 160,
            no_optimum: 8,
            ..Default::default()
        };
        let since = crate::notify::utc_date::UtcDay::parse("2026-06-15")
            .unwrap()
            .bounds_unix()
            .0;
        let (live, live_text) = render_rows(
            (0..3)
                .map(|i| coverage_row(100 + i, since + 10 + i, 6962, Some(8), Some(live_rejects)))
                .collect(),
        );
        assert!(live.operational_activity.limited_evaluation_coverage);
        assert!(!live.operational_activity.is_pipeline_dead);
        assert!(
            live_text.contains("Completed Ok/NoOptimum searches: 24 / 20886 paths (0.115%)"),
            "coverage line missing: {live_text}"
        );
        assert!(live_text.contains("unknown_route 20382 · unapproved_route 480"));
        assert!(live_text.contains("limited evaluation coverage"));
        assert!(!live_text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"));

        // Quiet market above the threshold: 300 / 20,886 ≈ 1.44%.
        let quiet_rejects = DiscoveryRejects {
            unknown_route: 6662,
            no_optimum: 100,
            unapproved_route: 200,
            ..Default::default()
        };
        let (quiet, quiet_text) = render_rows(
            (0..3)
                .map(|i| {
                    coverage_row(
                        100 + i,
                        since + 10 + i,
                        6962,
                        Some(100),
                        Some(quiet_rejects),
                    )
                })
                .collect(),
        );
        assert!(!quiet.operational_activity.limited_evaluation_coverage);
        assert!(quiet_text.contains("Completed Ok/NoOptimum searches: 300 / 20886 paths (1.436%)"));
        assert!(!quiet_text.contains("limited evaluation coverage"));
        assert!(quiet_text.contains("已记录候选 0；无套利候选；无成交（dry-run 不发送交易）"));
    }

    /// WHI-1544 AC: an Error-only zero-completion window (the release review's
    /// reproduction: a Full pass of four evaluated paths, `paths_quoted = 0`,
    /// rejects `other = 4` — the shape of four optimizer Error searches) and the
    /// committed live trigger window (2,950 unapproved + 18 optimizer errors) are
    /// both dead, but must never claim pre-simulation rejection; a truly
    /// pre-rejected control keeps that explanation. Same unhealthy verdict,
    /// different explanations.
    #[test]
    fn render_card_never_claims_pre_simulation_rejection_when_optimizer_errored() {
        use crate::notify::digest::ZeroCompletionCause;
        use crate::notify::ledger_window::DiscoveryRejects;

        let since = crate::notify::utc_date::UtcDay::parse("2026-06-15")
            .unwrap()
            .bounds_unix()
            .0;
        const PRE_SIM_CLAIM: &str = "全部在仿真前被拒绝";

        let error_only = DiscoveryRejects {
            other: 4,
            ..Default::default()
        };
        let (agg, error_text) = render_rows(vec![coverage_row(
            100,
            since + 10,
            4,
            Some(0),
            Some(error_only),
        )]);
        assert!(
            agg.operational_activity.is_pipeline_dead,
            "error-only stays unhealthy"
        );
        assert_eq!(
            agg.operational_activity.zero_completion_cause,
            Some(ZeroCompletionCause::OptimizerWorkPresent { other: 4 })
        );
        assert!(error_text.contains("发现管道异常"), "{error_text}");
        assert!(!error_text.contains(PRE_SIM_CLAIM), "{error_text}");
        assert!(
            error_text.contains("不能认定为仿真前拒绝")
                && error_text.contains("other 类拒绝 4 条（含优化器 Error 搜索"),
            "{error_text}"
        );
        assert!(
            error_text.contains("Completed Ok/NoOptimum searches: 0 / 4 paths"),
            "{error_text}"
        );

        // Live mixed trigger window (touched passes; scope does not matter here).
        let (live, live_text) = render_rows(vec![
            coverage_row(
                101,
                since + 20,
                48,
                Some(0),
                Some(DiscoveryRejects {
                    unapproved_route: 44,
                    other: 4,
                    ..Default::default()
                }),
            ),
            coverage_row(
                102,
                since + 30,
                2_920,
                Some(0),
                Some(DiscoveryRejects {
                    unapproved_route: 2_906,
                    other: 14,
                    ..Default::default()
                }),
            ),
        ]);
        assert!(live.operational_activity.is_pipeline_dead);
        assert_eq!(
            live.operational_activity.zero_completion_cause,
            Some(ZeroCompletionCause::OptimizerWorkPresent { other: 18 })
        );
        assert!(!live_text.contains(PRE_SIM_CLAIM), "{live_text}");
        assert!(live_text.contains("other 类拒绝 18 条"), "{live_text}");

        // Truly pre-rejected control: only pre-simulation buckets.
        let pre_rejected = DiscoveryRejects {
            unknown_route: 3,
            unapproved_route: 44,
            pool_lookup: 1,
            ..Default::default()
        };
        let (control, control_text) = render_rows(vec![coverage_row(
            103,
            since + 40,
            48,
            Some(0),
            Some(pre_rejected),
        )]);
        assert!(
            control.operational_activity.is_pipeline_dead,
            "control stays unhealthy"
        );
        assert_eq!(
            control.operational_activity.zero_completion_cause,
            Some(ZeroCompletionCause::PreSimulationOnly)
        );
        assert!(control_text.contains(PRE_SIM_CLAIM), "{control_text}");
        assert!(
            !control_text.contains("不能认定为仿真前拒绝"),
            "{control_text}"
        );
        assert_ne!(error_text, control_text);
    }

    /// WHI-1424 AC: zero paths among the recorded rows plus rows with no
    /// `paths_quoted` telemetry (a deployment-day mixed window) renders as
    /// unknown/partial — "zero among recorded rows" — never as a dead pipeline.
    #[test]
    fn render_card_says_zero_among_recorded_rows_not_dead_for_a_mixed_telemetry_window() {
        let since = crate::notify::utc_date::UtcDay::parse("2026-06-15")
            .unwrap()
            .bounds_unix()
            .0;
        let (agg, text) = render_rows(vec![
            coverage_row(100, since + 10, 50, Some(0), None),
            coverage_row(101, since + 20, 10_000, None, None),
        ]);
        assert!(!agg.operational_activity.is_pipeline_dead);
        assert!(agg.operational_activity.pipeline_liveness_unknown);
        assert_eq!(agg.operational_activity.evaluation_coverage, None);
        assert!(
            !text.contains("发现管道异常"),
            "must not claim dead: {text}"
        );
        assert!(text.contains("zero among recorded rows"));
        assert!(text.contains("无法确认发现管道是否存活"));
        assert!(text.contains("不下覆盖率结论"));
    }

    /// WHI-1424 (PR107-F1): an observation row with **no `discovery` object at
    /// all** (pre-WHI-957 rows; the live startup row written before its one-shot
    /// pass) is missing telemetry exactly like `discovery: {}` — it must not let
    /// a zero among the recorded rows read as dead, nor produce a coverage ratio.
    /// Built from real JSON bytes through the ledger reader.
    #[test]
    fn render_card_treats_an_omitted_discovery_object_as_missing_telemetry_not_dead() {
        let since = crate::notify::utc_date::UtcDay::parse("2026-06-15")
            .unwrap()
            .bounds_unix()
            .0;
        let recorded = format!(
            r#"{{"row_type":"observation","schema_version":"whisker-arb/shadow-ledger/v3","sequence":1,"snapshot_id":{{"chain_id":5000,"block_number":1,"block_hash":"0x01"}},"header":{{"parent_hash":"0x00","block_timestamp":{t}}},"recorded_at_unix":{t},"discovery":{{"skipped":false,"cycles_optimized":1000,"cycles_total":1000,"paths_quoted":0,"scope":"full"}}}}"#,
            t = since + 1
        );
        let second = |discovery: &str| {
            format!(
                r#"{{"row_type":"observation","schema_version":"whisker-arb/shadow-ledger/v3","sequence":2,"snapshot_id":{{"chain_id":5000,"block_number":2,"block_hash":"0x01"}},"header":{{"parent_hash":"0x00","block_timestamp":{t}}},"recorded_at_unix":{t}{discovery}}}"#,
                t = since + 2
            )
        };
        let dir = tempfile::tempdir().unwrap();
        for (label, tail) in [
            ("omitted discovery", String::new()),
            ("empty discovery control", r#","discovery":{}"#.to_string()),
        ] {
            let path = dir
                .path()
                .join(format!("{}.jsonl", label.replace(' ', "-")));
            std::fs::write(&path, format!("{recorded}\n{}\n", second(&tail))).unwrap();
            let read = crate::notify::ledger_window::read_ledger_window(&path).unwrap();
            assert_eq!(read.observations.len(), 2, "{label}");
            if tail.is_empty() {
                assert_eq!(
                    read.observations[1].discovery, None,
                    "{label}: bytes omit it"
                );
            }
            let window = crate::notify::digest::DigestWindow::for_day(
                crate::notify::utc_date::UtcDay::parse("2026-06-15").unwrap(),
            );
            let agg =
                crate::notify::digest::aggregate_digest(&read, window, window.since_unix + 100);
            let text = render_card(&agg, "ARB", None).to_string();
            let activity = &agg.operational_activity;
            assert!(!activity.is_pipeline_dead, "{label}: must not claim dead");
            assert!(
                activity.pipeline_liveness_unknown,
                "{label}: must be unknown"
            );
            assert_eq!(activity.evaluation_coverage, None, "{label}: no ratio");
            assert!(
                !activity.limited_evaluation_coverage,
                "{label}: no threshold"
            );
            assert!(!text.contains("发现管道异常"), "{label}: {text}");
            assert!(text.contains("无法确认发现管道是否存活"), "{label}: {text}");
            assert!(text.contains("zero among recorded rows"), "{label}: {text}");
            assert!(
                !text.contains("Completed Ok/NoOptimum searches:"),
                "{label}: {text}"
            );
            assert!(
                !text.contains("limited evaluation coverage"),
                "{label}: {text}"
            );
        }
    }
}
