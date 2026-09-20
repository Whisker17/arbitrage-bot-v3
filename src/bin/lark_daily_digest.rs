//! `lark_daily_digest` — one-shot binary that renders (and, unless `--dry-run`,
//! sends) the daily Lark digest card for the signerless Mantle shadow-mode dry run
//! (WHI-1407).
//!
//! **Deliberately not embedded in `bot.rs --watch`** — this is a separate,
//! short-lived process meant to be invoked once a day by a systemd oneshot
//! service+timer (see `scripts/systemd/lark-daily-digest.{service,timer}`). It never
//! writes to the shadow ledger, never changes its schema, and never touches the live
//! watch loop.
//!
//! ## Modes
//!
//! * **Normal scheduled invocation** (no `--date`/`--dry-run`/`--send-test`): opens
//!   the exclusive single-flight state lock, computes the outstanding UTC day(s)
//!   since the last confirmed send (bounded to
//!   [`amms::notify::state::MAX_DAYS_PER_INVOCATION`] per run — see
//!   `notify::state::plan_backlog`), and sends each in order. The state marker is
//!   persisted **only** after a confirmed provider success for that day; a failure
//!   stops the loop immediately so no day is skipped.
//! * **`--date <YYYY-MM-DD>`**: explicit recovery send for exactly one UTC day,
//!   bypassing the normal backlog plan. On confirmed success this **only** advances
//!   the state marker if `date` is strictly newer than the currently recorded day —
//!   it never moves the marker backward, so an out-of-order backfill cannot corrupt
//!   the forward backlog plan for subsequent normal runs.
//! * **`--dry-run`**: renders the exact card for the target day(s) with **no**
//!   network call and **no** state mutation; prints the rendered card JSON to
//!   stdout.
//! * **`--send-test`**: sends a visibly labeled test card to the real webhook
//!   **without** touching the daily marker or reading the ledger at all — run this
//!   once against production before the first scheduled timer fire.
//!
//! Any ledger read failure (malformed row, unsupported schema version, unreadable
//! path) aborts the run with a non-zero exit and **no** send for that invocation —
//! this binary never fabricates a "healthy zero" digest when it cannot actually read
//! the ledger.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use amms::notify::digest::{aggregate_digest, DigestAggregate, DigestWindow};
use amms::notify::lark::{render_card, LarkClientConfig, LarkSender};
use amms::notify::ledger_window::{read_ledger_window, LedgerWindowRead};
use amms::notify::state::{plan_backlog, StateHandle, MAX_DAYS_PER_INVOCATION};
use amms::notify::utc_date::UtcDay;
use clap::Parser;
use eyre::{bail, Context, Result};
use serde_json::json;
use tracing::{error, info, warn};

#[derive(Debug, Parser)]
#[command(
    name = "lark_daily_digest",
    about = "One-shot daily Lark digest for the signerless Mantle shadow-mode dry run (WHI-1407)"
)]
struct Args {
    /// Shadow ledger active-file path (rotated segments alongside it are read too).
    #[arg(long, env = "SHADOW_LEDGER_PATH")]
    ledger: Option<PathBuf>,

    /// Durable idempotency state file (day-keyed `last_sent_day` marker).
    #[arg(long, env = "LARK_DIGEST_STATE_PATH")]
    state: Option<PathBuf>,

    /// Lark custom-bot webhook URL. Never logged in full — see
    /// `notify::lark::redact_webhook_url`.
    #[arg(long, env = "LARK_WEBHOOK_URL")]
    webhook_url: Option<String>,

    /// Keyword the Lark custom bot requires in the card title.
    #[arg(long, env = "LARK_KEYWORD")]
    keyword: Option<String>,

    /// Explicit single-UTC-day recovery send/render (`YYYY-MM-DD`). Bypasses the
    /// normal backlog plan; see this binary's module doc for state-mutation rules.
    #[arg(long)]
    date: Option<String>,

    /// Render only: no network call, no state mutation.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// Send a visibly labeled test card to the real webhook; does not touch the
    /// daily marker and does not read the ledger.
    #[arg(long, default_value_t = false)]
    send_test: bool,

    /// Bound on how many outstanding days one invocation processes.
    #[arg(long, default_value_t = MAX_DAYS_PER_INVOCATION)]
    max_days: usize,

    /// Test-only clock override (unix seconds); hidden from `--help`.
    #[arg(long, hide = true)]
    now_unix: Option<u64>,
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

fn now_unix(args: &Args) -> u64 {
    args.now_unix.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

fn main() -> ExitCode {
    init_tracing();
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            error!(target: "lark_daily_digest", error = %format!("{error:#}"), "lark_daily_digest failed");
            eprintln!("lark_daily_digest error: {error:#}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool> {
    let args = Args::parse();

    if args.send_test {
        return run_send_test(&args);
    }

    let ledger_path = args.ledger.clone().ok_or_else(|| {
        eyre::eyre!("--ledger (or SHADOW_LEDGER_PATH) is required outside --send-test")
    })?;
    let read = read_ledger_window(&ledger_path)
        .with_context(|| format!("reading shadow ledger at {}", ledger_path.display()))?;
    if let Some(tail) = &read.deferred_incomplete_tail {
        info!(target: "lark_daily_digest", %tail, "deferred an incomplete trailing ledger line");
    }

    let generated_at = now_unix(&args);

    if let Some(date_str) = &args.date {
        let day = UtcDay::parse(date_str)
            .map_err(|e| eyre::eyre!("--date {date_str:?} is invalid: {e}"))?;
        return run_single_day(&args, &read, day, generated_at);
    }

    if args.dry_run {
        // No explicit --date in dry-run mode: default to the previous completed
        // UTC day, matching the normal first-run contract.
        let today = UtcDay::from_unix(generated_at);
        let day = today.previous();
        let aggregate = aggregate_digest(&read, DigestWindow::for_day(day), generated_at);
        print_dry_run(&args, &aggregate)?;
        return Ok(true);
    }

    run_normal_invocation(&args, &read, generated_at)
}

fn print_dry_run(args: &Args, aggregate: &DigestAggregate) -> Result<()> {
    let keyword = args.keyword.clone().unwrap_or_else(|| "ARB".to_string());
    let card = render_card(aggregate, &keyword, None);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode": "dry_run",
            "window_day": aggregate.window.day.to_string(),
            "card": card,
        }))?
    );
    Ok(())
}

fn run_single_day(
    args: &Args,
    read: &LedgerWindowRead,
    day: UtcDay,
    generated_at: u64,
) -> Result<bool> {
    if args.dry_run {
        let aggregate = aggregate_digest(read, DigestWindow::for_day(day), generated_at);
        print_dry_run(args, &aggregate)?;
        return Ok(true);
    }

    let webhook_url = require_webhook_url(args)?;
    let keyword = require_keyword(args)?;

    // Single-flight lock spans read → POST → write for the recovery path too —
    // acquired *before* the send, exactly like `run_normal_invocation`, so a
    // concurrent scheduled invocation and a `--date` recovery run can never race
    // each other into a double-send.
    let mut state = match &args.state {
        Some(state_path) => Some(StateHandle::open_exclusive(state_path).with_context(|| {
            format!(
                "opening digest state at {} (a Locked error here means another invocation is already running)",
                state_path.display()
            )
        })?),
        None => None,
    };

    let aggregate = aggregate_digest(read, DigestWindow::for_day(day), generated_at);
    let card = render_card(&aggregate, &keyword, None);
    let sender = LarkSender::new(webhook_url, LarkClientConfig::default())
        .map_err(|e| eyre::eyre!("building lark http client: {e}"))?;
    let outcome = sender.send(&card);
    if !outcome.sent {
        bail!(
            "--date {day} delivery failed after {} attempt(s): {}",
            outcome.attempts,
            outcome.error.unwrap_or_else(|| "unknown".to_string())
        );
    }
    info!(target: "lark_daily_digest", %day, webhook = %sender.redacted_webhook(), "recovery digest sent");

    if let Some(state) = &mut state {
        let current = state
            .last_sent_day()
            .with_context(|| "reading last_sent_day")?;
        if current.is_none_or(|existing| day > existing) {
            state
                .record_sent_day(day)
                .with_context(|| "persisting last_sent_day")?;
        } else {
            info!(
                target: "lark_daily_digest",
                %day,
                "recovery send for a day at/before the current marker; marker left unchanged"
            );
        }
    }
    Ok(true)
}

fn run_normal_invocation(args: &Args, read: &LedgerWindowRead, generated_at: u64) -> Result<bool> {
    let state_path = args.state.clone().ok_or_else(|| {
        eyre::eyre!("--state (or LARK_DIGEST_STATE_PATH) is required for a scheduled run")
    })?;
    let webhook_url = require_webhook_url(args)?;
    let keyword = require_keyword(args)?;

    let mut state = StateHandle::open_exclusive(&state_path).with_context(|| {
        format!(
            "opening digest state at {} (a Locked error here means another invocation is already running)",
            state_path.display()
        )
    })?;
    let last_sent_day = state
        .last_sent_day()
        .with_context(|| "reading last_sent_day")?;
    let today = UtcDay::from_unix(generated_at);
    let plan = plan_backlog(last_sent_day, today, args.max_days);

    if plan.days.is_empty() {
        info!(target: "lark_daily_digest", "no outstanding UTC day to send; already up to date");
        return Ok(true);
    }
    if plan.backlog_remains {
        warn!(
            target: "lark_daily_digest",
            days_this_invocation = plan.days.len(),
            "backlog exceeds this invocation's bound; remaining days will be picked up on a later run"
        );
    }

    let sender = LarkSender::new(webhook_url, LarkClientConfig::default())
        .map_err(|e| eyre::eyre!("building lark http client: {e}"))?;

    let day_count = plan.days.len();
    for (index, day) in plan.days.into_iter().enumerate() {
        // Surface the remaining backlog on the *last* card this invocation sends
        // (issue item 6: "explicitly surface any backlog") — backlog is a property
        // of the invocation, not of any single day, so it rides along on the
        // final card rather than being invented as a per-day aggregate field.
        let backlog_note = if plan.backlog_remains && index + 1 == day_count {
            Some("本次运行仍未处理完所有待发送日期，余下的将在下一次调度继续处理".to_string())
        } else {
            None
        };
        let aggregate = aggregate_digest(read, DigestWindow::for_day(day), generated_at);
        let card = render_card(&aggregate, &keyword, backlog_note.as_deref());
        let outcome = sender.send(&card);
        if !outcome.sent {
            bail!(
                "digest delivery for {day} failed after {} attempt(s): {} — day remains eligible for retry on the next invocation",
                outcome.attempts,
                outcome.error.unwrap_or_else(|| "unknown".to_string())
            );
        }
        state
            .record_sent_day(day)
            .with_context(|| format!("persisting last_sent_day={day}"))?;
        info!(target: "lark_daily_digest", %day, webhook = %sender.redacted_webhook(), "daily digest sent");
    }
    Ok(true)
}

fn run_send_test(args: &Args) -> Result<bool> {
    let webhook_url = require_webhook_url(args)?;
    let keyword = require_keyword(args)?;
    let card = amms::notify::lark::card_shell(
        &format!("{keyword} · lark_daily_digest 测试卡片 / TEST"),
        "grey",
        vec![json!({
            "tag": "div",
            "text": {
                "tag": "lark_md",
                "content": "这是一张测试卡片，用于验证 webhook 连通性；不消耗每日发送标记 / this is a connectivity test card and does not consume the daily marker."
            }
        })],
    );
    let sender = LarkSender::new(webhook_url, LarkClientConfig::default())
        .map_err(|e| eyre::eyre!("building lark http client: {e}"))?;
    let outcome = sender.send(&card);
    println!(
        "send-test webhook={} sent={} attempts={} status={:?} error={:?}",
        sender.redacted_webhook(),
        outcome.sent,
        outcome.attempts,
        outcome.http_status,
        outcome.error
    );
    Ok(outcome.sent)
}

fn require_webhook_url(args: &Args) -> Result<String> {
    args.webhook_url
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("--webhook-url (or LARK_WEBHOOK_URL) is required to send"))
}

fn require_keyword(args: &Args) -> Result<String> {
    args.keyword
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| eyre::eyre!("--keyword (or LARK_KEYWORD) is required to send"))
}
