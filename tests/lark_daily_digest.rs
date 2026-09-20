//! Integration tests for the `lark_daily_digest` binary (WHI-1407).
//!
//! Exercises the compiled binary as a real subprocess (`env!("CARGO_BIN_EXE_...")`,
//! same convention as `tests/bot_cross_protocol.rs`) against a hand-rolled ledger
//! fixture and a minimal local mock HTTP server standing in for the Lark webhook —
//! no new test-server dependency.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lark_daily_digest")
}

fn tmp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("whi1407-{name}-{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// UTC day bounds, via the crate's own tested [`amms::notify::utc_date::UtcDay`]
/// (a bare integration test can reach `pub` library items directly, so there is no
/// need for a second, hand-rolled civil-date implementation here).
fn day_since_unix(y: i64, m: u32, d: u32) -> u64 {
    let day = amms::notify::utc_date::UtcDay::new(y, m, d).unwrap();
    day.bounds_unix().0
}

fn header_line(run_id: &str, started_at: u64, sequence: u64) -> String {
    serde_json::json!({
        "row_type": "run_header",
        "schema_version": "whisker-arb/shadow-ledger/v3",
        "run_id": run_id,
        "git_commit": "0".repeat(40),
        "chain_id": 5000,
        "service": "bot",
        "executor_contract": "0x0000000000000000000000000000000000000002",
        "wmnt_address": "0x0000000000000000000000000000000000000003",
        "config_digest": "0x0",
        "storage_layout_digest": "0x0",
        "wmnt_descriptor_digest": "0x0",
        "moe_allowlist_digest": "0x0",
        "identity_digest": "0x0",
        "approved_pools_digest": "0x0",
        "threshold_config_digest": "0x0",
        "profile_digest": "0x0",
        "override_digest": "0x0",
        "send_capability": "no_send",
        "start_identity": null,
        "started_at_unix": started_at,
        "sequence": sequence,
    })
    .to_string()
}

fn observation_line(block: u64, recorded_at: u64, sequence: u64) -> String {
    serde_json::json!({
        "row_type": "observation",
        "schema_version": "whisker-arb/shadow-ledger/v3",
        "snapshot_id": { "chain_id": 5000, "block_number": block, "block_hash": "0x01" },
        "header": { "parent_hash": "0x04", "block_timestamp": recorded_at },
        "recorded_at_unix": recorded_at,
        "sequence": sequence,
    })
    .to_string()
}

fn write_ledger(path: &Path, lines: &[String]) {
    let mut content = lines.join("\n");
    content.push('\n');
    fs::write(path, content).unwrap();
}

// ---------------------------------------------------------------------------
// Minimal mock Lark webhook (HTTP/1.1, one canned response per accepted
// connection, in order).
// ---------------------------------------------------------------------------

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

/// Spawns a background server that serves each of `responses` in order to
/// successive connections, then exits. Returns the bound webhook URL.
fn spawn_mock_webhook(responses: Vec<MockResponse>) -> String {
    spawn_mock_webhook_capturing(responses).0
}

/// Like [`spawn_mock_webhook`] but also returns the captured request bodies (in
/// arrival order) behind a shared `Mutex` — lets a test assert on what the binary
/// actually POSTed, not just its own log output.
fn spawn_mock_webhook_capturing(
    responses: Vec<MockResponse>,
) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let bodies_clone = bodies.clone();
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
            let body =
                String::from_utf8_lossy(&data[pos + 4..pos + 4 + content_length]).to_string();
            bodies_clone.lock().unwrap().push(body);
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
    (format!("http://{addr}/hook/test-token"), bodies)
}

fn ok_responses(n: usize) -> Vec<MockResponse> {
    (0..n)
        .map(|_| MockResponse {
            status: 200,
            body: r#"{"code":0,"msg":"success"}"#,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn dry_run_renders_without_network_or_state_mutation() {
    let dir = tmp_dir("dry-run");
    let ledger_path = dir.join("ledger.jsonl");
    let since = day_since_unix(2026, 6, 15);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", since, 0),
            observation_line(1, since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");

    let output = Command::new(bin())
        .args([
            "--dry-run",
            "--date",
            "2026-06-15",
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("\"mode\": \"dry_run\""));
    assert!(stdout.contains("2026-06-15"));
    assert!(!state_path.exists(), "dry-run must never mutate state");
}

#[test]
fn missing_ledger_flag_exits_nonzero_with_a_clear_message() {
    let output = Command::new(bin())
        .args(["--dry-run", "--date", "2026-06-15"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--ledger"));
}

#[test]
fn malformed_ledger_row_is_a_hard_failure_never_a_healthy_zero() {
    let dir = tmp_dir("malformed");
    let ledger_path = dir.join("ledger.jsonl");
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", 1_700_000_000, 0),
            "{not json".to_string(),
        ],
    );

    let output = Command::new(bin())
        .args([
            "--dry-run",
            "--date",
            "2026-06-15",
            "--ledger",
            ledger_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("\"mode\""),
        "a malformed ledger must never still print a rendered digest"
    );
}

#[test]
fn a_normal_invocation_sends_yesterday_and_persists_the_marker() {
    let dir = tmp_dir("normal-run");
    let ledger_path = dir.join("ledger.jsonl");
    let today_since = day_since_unix(2026, 6, 15);
    let yesterday_since = day_since_unix(2026, 6, 14);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", yesterday_since, 0),
            observation_line(1, yesterday_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    let webhook_url = spawn_mock_webhook(ok_responses(1));

    let output = Command::new(bin())
        .args([
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
            "--now-unix",
            &(today_since + 100).to_string(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let marker = fs::read_to_string(&state_path).unwrap();
    assert_eq!(marker.trim(), "2026-06-14");
}

#[test]
fn a_rerun_on_an_already_sent_day_does_not_resend() {
    let dir = tmp_dir("no-resend");
    let ledger_path = dir.join("ledger.jsonl");
    let today_since = day_since_unix(2026, 6, 15);
    let yesterday_since = day_since_unix(2026, 6, 14);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", yesterday_since, 0),
            observation_line(1, yesterday_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    fs::write(&state_path, "2026-06-14").unwrap();

    // No mock webhook is even started: if the binary tried to send, the POST
    // would fail to connect and the run would exit non-zero.
    let output = Command::new(bin())
        .args([
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            "http://127.0.0.1:1/unused",
            "--keyword",
            "ARB",
            "--now-unix",
            &(today_since + 100).to_string(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("no outstanding UTC day") || combined.contains("already up to date"));
    assert_eq!(
        fs::read_to_string(&state_path).unwrap().trim(),
        "2026-06-14"
    );
}

#[test]
fn overlapping_invocations_do_not_double_send() {
    let dir = tmp_dir("overlap");
    let ledger_path = dir.join("ledger.jsonl");
    let yesterday_since = day_since_unix(2026, 6, 14);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", yesterday_since, 0),
            observation_line(1, yesterday_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    let today_since = day_since_unix(2026, 6, 15);

    // Hold the exclusive lock ourselves (in-process, via the public library API)
    // to deterministically simulate a first invocation already running.
    let _held = amms::notify::state::StateHandle::open_exclusive(&state_path).unwrap();

    let output = Command::new(bin())
        .args([
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            "http://127.0.0.1:1/unused",
            "--keyword",
            "ARB",
            "--now-unix",
            &(today_since + 100).to_string(),
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a locked state file must refuse the second invocation"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.to_lowercase().contains("locked"));
}

#[test]
fn a_date_recovery_invocation_also_respects_the_single_flight_lock() {
    // The `--date` recovery path must acquire the lock *before* sending, not only
    // before persisting -- otherwise a concurrent scheduled run and a `--date`
    // run could race into a double-send.
    let dir = tmp_dir("overlap-date");
    let ledger_path = dir.join("ledger.jsonl");
    let day_since = day_since_unix(2026, 6, 14);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", day_since, 0),
            observation_line(1, day_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");

    let _held = amms::notify::state::StateHandle::open_exclusive(&state_path).unwrap();

    let output = Command::new(bin())
        .args([
            "--date",
            "2026-06-14",
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            "http://127.0.0.1:1/unused",
            "--keyword",
            "ARB",
        ])
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "a locked state file must refuse a concurrent --date recovery send too"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.to_lowercase().contains("locked"));
}

#[test]
fn a_failed_delivery_leaves_the_day_eligible_for_retry() {
    let dir = tmp_dir("failed-delivery");
    let ledger_path = dir.join("ledger.jsonl");
    let yesterday_since = day_since_unix(2026, 6, 14);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", yesterday_since, 0),
            observation_line(1, yesterday_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    let today_since = day_since_unix(2026, 6, 15);
    // Every attempt gets a 4xx (never retried, but still exhausts the digest's
    // one send attempt for the day and must not touch the marker).
    let webhook_url = spawn_mock_webhook(vec![MockResponse {
        status: 404,
        body: "nope",
    }]);

    let output = Command::new(bin())
        .args([
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
            "--now-unix",
            &(today_since + 100).to_string(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let marker_content = fs::read_to_string(&state_path).unwrap_or_default();
    assert!(
        marker_content.trim().is_empty(),
        "a failed delivery must never persist a day marker (found {marker_content:?})"
    );
}

#[test]
fn backlog_is_processed_in_order_and_bounded_with_an_explicit_remainder() {
    let dir = tmp_dir("backlog");
    let ledger_path = dir.join("ledger.jsonl");
    // Ledger spans several days; state marker is far behind "now".
    let day1 = day_since_unix(2026, 6, 1);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", day1, 0),
            observation_line(1, day1 + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    fs::write(&state_path, "2026-05-30").unwrap();
    let today_since = day_since_unix(2026, 6, 10);
    // 2026-05-31 .. 2026-06-09 inclusive = 10 outstanding days; bound to 3.
    let (webhook_url, sent_bodies) = spawn_mock_webhook_capturing(ok_responses(3));

    let output = Command::new(bin())
        .args([
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
            "--max-days",
            "3",
            "--now-unix",
            &(today_since + 100).to_string(),
        ])
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        fs::read_to_string(&state_path).unwrap().trim(),
        "2026-06-02"
    );
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("backlog exceeds"));

    // The backlog note must land in the *card actually sent*, not only the log --
    // on the last of the 3 cards this invocation sent, never the earlier ones.
    let bodies = sent_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 3);
    assert!(
        !bodies[0].contains("积压") && !bodies[1].contains("积压"),
        "only the last card of this invocation should carry the backlog note"
    );
    assert!(
        bodies[2].contains("积压"),
        "the last card of a bounded invocation with remaining backlog must carry the note: {}",
        bodies[2]
    );
}

#[test]
fn send_test_does_not_touch_the_daily_marker_or_require_a_ledger() {
    let dir = tmp_dir("send-test");
    let state_path = dir.join("state.marker");
    let webhook_url = spawn_mock_webhook(ok_responses(1));

    let output = Command::new(bin())
        .args([
            "--send-test",
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("sent=true"));
    assert!(!state_path.exists());
}

#[test]
fn date_recovery_for_an_older_day_never_moves_the_marker_backward() {
    let dir = tmp_dir("recovery-older");
    let ledger_path = dir.join("ledger.jsonl");
    let old_day_since = day_since_unix(2026, 6, 1);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", old_day_since, 0),
            observation_line(1, old_day_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    fs::write(&state_path, "2026-06-10").unwrap();
    let webhook_url = spawn_mock_webhook(ok_responses(1));

    let output = Command::new(bin())
        .args([
            "--date",
            "2026-06-01",
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        fs::read_to_string(&state_path).unwrap().trim(),
        "2026-06-10",
        "a recovery send for an older day must not move the marker backward"
    );
}

#[test]
fn date_recovery_for_a_newer_day_advances_the_marker() {
    let dir = tmp_dir("recovery-newer");
    let ledger_path = dir.join("ledger.jsonl");
    let day_since = day_since_unix(2026, 6, 20);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", day_since, 0),
            observation_line(1, day_since + 10, 1),
        ],
    );
    let state_path = dir.join("state.marker");
    fs::write(&state_path, "2026-06-10").unwrap();
    let webhook_url = spawn_mock_webhook(ok_responses(1));

    let output = Command::new(bin())
        .args([
            "--date",
            "2026-06-20",
            "--ledger",
            ledger_path.to_str().unwrap(),
            "--state",
            state_path.to_str().unwrap(),
            "--webhook-url",
            &webhook_url,
            "--keyword",
            "ARB",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert_eq!(
        fs::read_to_string(&state_path).unwrap().trim(),
        "2026-06-20"
    );
}

#[test]
fn unavailable_retention_is_surfaced_in_the_rendered_card() {
    let dir = tmp_dir("unavailable-retention");
    let ledger_path = dir.join("ledger.jsonl");
    // All retained data is from a much later day than the one requested.
    let later_day = day_since_unix(2026, 12, 1);
    write_ledger(
        &ledger_path,
        &[
            header_line("run-a", later_day, 0),
            observation_line(1, later_day + 10, 1),
        ],
    );

    let output = Command::new(bin())
        .args([
            "--dry-run",
            "--date",
            "2026-01-01",
            "--ledger",
            ledger_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("留存范围"));
}
