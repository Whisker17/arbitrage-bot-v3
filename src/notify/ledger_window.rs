//! Reads the shadow ledger's active file + numeric rotated segments in
//! chronological order into plain, crate-visible rows (WHI-1407 item 1).
//!
//! The real ledger row types (`execution::shadow::ledger::Ledger*Row`) are declared in
//! a `mod ledger;` (no `pub`) inside `execution::shadow` — private to that module and
//! its descendants, unreachable from a sibling module (see
//! `execution::shadow_report`'s doc comment / `docs/DEFERRED_ISSUES.md` DI-31). This
//! module follows the same precedent `shadow_report.rs` set: local wire-mirror row
//! types that deserialize the same on-disk JSON shape, carrying only the fields this
//! digest needs. Unknown/future row types (e.g. `provenance`, `broadcast`) and unknown
//! fields are silently skipped/ignored rather than treated as a schema break — this
//! reader is intentionally forward-compatible with ledger rows it doesn't consume.
//!
//! Segment layout (see `ops::rotating_file` / `execution::shadow::ledger`'s module
//! doc): the active file is at the operator-facing path; rotated segments are named
//! `path.1` (most recently rotated), `path.2`, … (higher N is older). Chronological
//! order is therefore: oldest rotated segment first (highest N), down to `path.1`,
//! then the active file last.

use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::ops::{list_rotated_segments, RotationError, SegmentPaths};

/// Schema version this reader understands. Mirrors
/// `execution::shadow::ledger::LEDGER_SCHEMA_VERSION` — that constant is
/// `pub(crate)` to `execution::shadow` only, so it is re-declared here rather than
/// imported (same reason the row types are re-declared).
pub const LEDGER_SCHEMA_VERSION: &str = "whisker-arb/shadow-ledger/v3";

/// Bounded retries against a concurrently-appending/rotating writer before giving up
/// (WHI-1407: "obtain a stable read view or detect movement and bounded-reread").
const MAX_STABLE_READ_ATTEMPTS: u32 = 5;

#[derive(Debug, thiserror::Error)]
pub enum LedgerReadError {
    #[error("ledger io error reading {path}: {detail}")]
    Io { path: String, detail: String },
    #[error("neither an active ledger file nor any rotated segment exists at {path}")]
    LedgerNotFound { path: String },
    #[error("ledger segment {segment} line {line} is malformed JSON: {detail}")]
    MalformedRow {
        segment: String,
        line: usize,
        detail: String,
    },
    #[error(
        "ledger segment {segment} line {line} (row_type={row_type:?}) has a malformed shape: {detail}"
    )]
    MalformedShape {
        segment: String,
        line: usize,
        row_type: String,
        detail: String,
    },
    #[error(
        "ledger segment {segment} line {line} has unsupported schema_version {found:?} (expected {expected:?})"
    )]
    UnsupportedSchemaVersion {
        segment: String,
        line: usize,
        found: String,
        expected: String,
    },
    #[error(
        "ledger active file kept rotating/appending across {attempts} read attempts; \
         giving up rather than risk a duplicate or lost segment"
    )]
    UnstableReadView { attempts: u32 },
}

impl From<RotationError> for LedgerReadError {
    fn from(error: RotationError) -> Self {
        Self::Io {
            path: "<rotation metadata>".to_string(),
            detail: error.to_string(),
        }
    }
}

/// Identity of one `run_header` row. Every segment carries the *same* run's header
/// forward on rotation (same `run_id` / `started_at_unix` re-emitted) — this reader
/// returns every header row it encounters, in file order, **without deduplicating**;
/// deduplication into "observed run starts/transitions" is the pure aggregator's job
/// (see `digest.rs`), since collapsing here would throw away the ordering information
/// the aggregator needs to attribute other rows to the run active when they were
/// written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerRunIdentity {
    pub run_id: String,
    pub started_at_unix: u64,
    pub service: String,
    pub chain_id: u64,
    pub git_commit: String,
    pub executor_contract: String,
    pub wmnt_address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateOutcomeKind {
    Pass,
    Revert,
    RpcError,
    EnvUnsupported,
    SkippedApproved,
    SampledOut,
}

impl CandidateOutcomeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Revert => "revert",
            Self::RpcError => "rpc_error",
            Self::EnvUnsupported => "env_unsupported",
            Self::SkippedApproved => "skipped_approved",
            Self::SampledOut => "sampled_out",
        }
    }
}

/// WHI-957 discovery snapshot carried on an observation row, trimmed to what the
/// digest needs (dirty-pool *count*, not the addresses themselves — the card never
/// names pools).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRecord {
    pub skipped: bool,
    pub skip_reason: Option<String>,
    pub dirty_pools_count: usize,
    pub cycles_optimized: Option<u64>,
    pub cycles_total: Option<u64>,
}

/// One `observation` row — windowed by `recorded_at_unix` per the issue's Context note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationRecord {
    pub block_number: u64,
    pub block_timestamp: u64,
    pub recorded_at_unix: u64,
    pub discovery: Option<DiscoveryRecord>,
    /// `run_id` of the header active when this row was written (empty string if a
    /// row somehow precedes any header — defensive, never expected on a real ledger).
    pub run_id: String,
}

/// One `candidate` row — windowed by `recorded_at_unix` per the issue's Context note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateRecord {
    pub digest: String,
    pub outcome: CandidateOutcomeKind,
    pub recorded_at_unix: u64,
    pub run_id: String,
}

/// One `context` row — windowed by the *identity's* `block_timestamp`, **not** any
/// `recorded_at_unix` (context rows have none of their own) per the issue's Context
/// note on window semantics differing by row type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRecord {
    pub digest: String,
    pub opportunity_id: String,
    pub ordered_pools: Vec<String>,
    pub net_profit: String,
    pub block_timestamp: u64,
    pub run_id: String,
}

/// Everything read from the ledger, still unfiltered by any day window — the pure
/// aggregator ([`crate::notify::digest`]) applies the window.
#[derive(Debug, Clone, Default)]
pub struct LedgerWindowRead {
    pub run_headers: Vec<LedgerRunIdentity>,
    pub observations: Vec<ObservationRecord>,
    pub candidates: Vec<CandidateRecord>,
    pub contexts: Vec<ContextRecord>,
    /// `Some(diagnostic)` when the active file's final line had no trailing newline
    /// (a writer mid-append) — deferred, not an error, per the issue's spec.
    pub deferred_incomplete_tail: Option<String>,
    /// Segment labels read, oldest to newest (for footer / diagnostics).
    pub segments_read: Vec<String>,
}

/// Reads `active_path`'s ledger (active file + rotated segments) into a
/// [`LedgerWindowRead`], retrying a bounded number of times if a concurrent
/// rotation is detected mid-read.
pub fn read_ledger_window(active_path: &Path) -> Result<LedgerWindowRead, LedgerReadError> {
    let paths = SegmentPaths::new(active_path);
    let mut last_attempt = 0u32;
    for attempt in 1..=MAX_STABLE_READ_ATTEMPTS {
        last_attempt = attempt;
        let before = list_rotated_segments(&paths)?;
        let mut segment_bytes: Vec<(String, Vec<u8>)> = Vec::new();
        let mut rotation_moved = false;
        // Oldest (highest N) first, down to `.1`.
        for (n, path, _size) in before.iter().rev() {
            match fs::read(path) {
                Ok(bytes) => segment_bytes.push((format!("{}.{n}", active_path.display()), bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Retention reclaimed this segment mid-read; retry the whole pass.
                    rotation_moved = true;
                    break;
                }
                Err(error) => {
                    return Err(LedgerReadError::Io {
                        path: path.display().to_string(),
                        detail: error.to_string(),
                    })
                }
            }
        }
        if rotation_moved {
            continue;
        }
        let active_bytes = match fs::read(active_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(LedgerReadError::Io {
                    path: active_path.display().to_string(),
                    detail: error.to_string(),
                })
            }
        };
        let after = list_rotated_segments(&paths)?;
        if !segments_stable(&before, &after) {
            continue;
        }
        if active_bytes.is_none() && segment_bytes.is_empty() {
            return Err(LedgerReadError::LedgerNotFound {
                path: active_path.display().to_string(),
            });
        }
        if let Some(active_bytes) = active_bytes {
            segment_bytes.push((active_path.display().to_string(), active_bytes));
        }
        return parse_segments(segment_bytes);
    }
    Err(LedgerReadError::UnstableReadView {
        attempts: last_attempt,
    })
}

/// True when the two segment listings name the same set of rotated indices — a
/// mismatch means a rotation (or retention reclaim) happened between the two calls.
fn segments_stable(
    before: &[(u32, std::path::PathBuf, u64)],
    after: &[(u32, std::path::PathBuf, u64)],
) -> bool {
    let before_ns: Vec<u32> = before.iter().map(|(n, _, _)| *n).collect();
    let after_ns: Vec<u32> = after.iter().map(|(n, _, _)| *n).collect();
    before_ns == after_ns
}

fn parse_segments(segments: Vec<(String, Vec<u8>)>) -> Result<LedgerWindowRead, LedgerReadError> {
    let mut out = LedgerWindowRead::default();
    let last_index = segments.len().saturating_sub(1);
    let mut current_run_id = String::new();
    for (index, (label, bytes)) in segments.into_iter().enumerate() {
        out.segments_read.push(label.clone());
        let is_last_segment = index == last_index;
        let text = String::from_utf8_lossy(&bytes);
        let ends_with_newline = bytes.last() == Some(&b'\n');
        let lines: Vec<&str> = text.lines().collect();
        for (line_no, line) in lines.iter().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let is_final_line = line_no + 1 == lines.len();
            if is_last_segment && is_final_line && !ends_with_newline {
                // Incomplete trailing line on the *active* file only — a rotated
                // segment is closed and never appended to again, so its final line
                // is always complete.
                out.deferred_incomplete_tail = Some(format!(
                    "{label}: deferred {} trailing byte(s) with no terminating newline (writer mid-append)",
                    line.len()
                ));
                continue;
            }
            parse_one_line(&label, line_no + 1, line, &mut current_run_id, &mut out)?;
        }
    }
    Ok(out)
}

fn parse_one_line(
    segment: &str,
    line_no: usize,
    line: &str,
    current_run_id: &mut String,
    out: &mut LedgerWindowRead,
) -> Result<(), LedgerReadError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|error| LedgerReadError::MalformedRow {
            segment: segment.to_string(),
            line: line_no,
            detail: error.to_string(),
        })?;
    let row_type = value
        .get("row_type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    if !schema_version.is_empty() && schema_version != LEDGER_SCHEMA_VERSION {
        return Err(LedgerReadError::UnsupportedSchemaVersion {
            segment: segment.to_string(),
            line: line_no,
            found: schema_version,
            expected: LEDGER_SCHEMA_VERSION.to_string(),
        });
    }

    let shape_error = |detail: String| LedgerReadError::MalformedShape {
        segment: segment.to_string(),
        line: line_no,
        row_type: row_type.clone(),
        detail,
    };

    match row_type.as_str() {
        "run_header" => {
            let header: WireRunHeader =
                serde_json::from_value(value).map_err(|e| shape_error(e.to_string()))?;
            *current_run_id = header.run_id.clone();
            out.run_headers.push(LedgerRunIdentity {
                run_id: header.run_id,
                started_at_unix: header.started_at_unix,
                service: header.service,
                chain_id: header.chain_id,
                git_commit: header.git_commit,
                executor_contract: header.executor_contract,
                wmnt_address: header.wmnt_address,
            });
        }
        "observation" => {
            let row: WireObservationRow =
                serde_json::from_value(value).map_err(|e| shape_error(e.to_string()))?;
            out.observations.push(ObservationRecord {
                block_number: row.snapshot_id.block_number,
                block_timestamp: row.header.block_timestamp,
                recorded_at_unix: row.recorded_at_unix,
                discovery: row.discovery.map(|d| DiscoveryRecord {
                    skipped: d.skipped,
                    skip_reason: d.skip_reason,
                    dirty_pools_count: d.dirty_pools.len(),
                    cycles_optimized: d.cycles_optimized,
                    cycles_total: d.cycles_total,
                }),
                run_id: current_run_id.clone(),
            });
        }
        "candidate" => {
            let row: WireCandidateRow =
                serde_json::from_value(value).map_err(|e| shape_error(e.to_string()))?;
            let outcome = match row.outcome.kind.as_str() {
                "pass" => CandidateOutcomeKind::Pass,
                "revert" => CandidateOutcomeKind::Revert,
                "rpc_error" => CandidateOutcomeKind::RpcError,
                "env_unsupported" => CandidateOutcomeKind::EnvUnsupported,
                "skipped_approved" => CandidateOutcomeKind::SkippedApproved,
                "sampled_out" => CandidateOutcomeKind::SampledOut,
                other => {
                    return Err(shape_error(format!(
                        "unknown candidate outcome kind {other:?}"
                    )))
                }
            };
            out.candidates.push(CandidateRecord {
                digest: row.digest,
                outcome,
                recorded_at_unix: row.recorded_at_unix,
                run_id: current_run_id.clone(),
            });
        }
        "context" => {
            let row: WireContextRow =
                serde_json::from_value(value).map_err(|e| shape_error(e.to_string()))?;
            out.contexts.push(ContextRecord {
                digest: row.digest,
                opportunity_id: row.opportunity_id,
                ordered_pools: row.ordered_pools,
                net_profit: row.net_profit,
                block_timestamp: row.identity.header.block_timestamp,
                run_id: current_run_id.clone(),
            });
        }
        // Forward-compatible: `provenance` / `broadcast` / any future row type this
        // digest doesn't consume is intentionally ignored, not an error.
        _ => {}
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct WireRunHeader {
    run_id: String,
    started_at_unix: u64,
    service: String,
    chain_id: u64,
    git_commit: String,
    executor_contract: String,
    wmnt_address: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireSnapshotId {
    block_number: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireBlockHeaderContext {
    block_timestamp: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct WireDiscoveryView {
    #[serde(default)]
    skipped: bool,
    #[serde(default)]
    skip_reason: Option<String>,
    #[serde(default)]
    dirty_pools: Vec<String>,
    #[serde(default)]
    cycles_optimized: Option<u64>,
    #[serde(default)]
    cycles_total: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireObservationRow {
    snapshot_id: WireSnapshotId,
    header: WireBlockHeaderContext,
    recorded_at_unix: u64,
    #[serde(default)]
    discovery: Option<WireDiscoveryView>,
}

#[derive(Debug, Clone, Deserialize)]
struct WireOutcome {
    kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireCandidateRow {
    digest: String,
    outcome: WireOutcome,
    recorded_at_unix: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireContextIdentity {
    header: WireBlockHeaderContext,
}

#[derive(Debug, Clone, Deserialize)]
struct WireContextRow {
    digest: String,
    identity: WireContextIdentity,
    opportunity_id: String,
    ordered_pools: Vec<String>,
    net_profit: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::rotate_active_file;
    use std::path::PathBuf;

    fn header_line(run_id: &str, started_at: u64) -> String {
        serde_json::json!({
            "row_type": "run_header",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "run_id": run_id,
            "git_commit": "0".repeat(40),
            "chain_id": 5000,
            "service": "test-service",
            "executor_contract": "0x0000000000000000000000000000000000000002",
            "wmnt_address": "0x0000000000000000000000000000000000000003",
            "started_at_unix": started_at,
            "sequence": 0,
        })
        .to_string()
    }

    fn observation_line(block: u64, recorded_at: u64, sequence: u64) -> String {
        serde_json::json!({
            "row_type": "observation",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "snapshot_id": { "chain_id": 5000, "block_number": block, "block_hash": "0x01" },
            "header": { "parent_hash": "0x04", "block_timestamp": recorded_at },
            "recorded_at_unix": recorded_at,
            "sequence": sequence,
        })
        .to_string()
    }

    fn observation_line_with_discovery(
        block: u64,
        recorded_at: u64,
        sequence: u64,
        cycles_optimized: u64,
        cycles_total: u64,
        dirty_pools: usize,
    ) -> String {
        serde_json::json!({
            "row_type": "observation",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "snapshot_id": { "chain_id": 5000, "block_number": block, "block_hash": "0x01" },
            "header": { "parent_hash": "0x04", "block_timestamp": recorded_at },
            "recorded_at_unix": recorded_at,
            "discovery": {
                "skipped": false,
                "dirty_pools": (0..dirty_pools).map(|i| format!("0x{i:040x}")).collect::<Vec<_>>(),
                "cycles_optimized": cycles_optimized,
                "cycles_total": cycles_total,
                "scope": "touched",
            },
            "sequence": sequence,
        })
        .to_string()
    }

    fn candidate_line(digest: &str, kind: &str, recorded_at: u64, sequence: u64) -> String {
        serde_json::json!({
            "row_type": "candidate",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "digest": digest,
            "policy_key": "mandatory",
            "outcome": { "kind": kind },
            "recorded_at_unix": recorded_at,
            "sequence": sequence,
        })
        .to_string()
    }

    fn context_line(digest: &str, block_timestamp: u64, net_profit: &str, sequence: u64) -> String {
        serde_json::json!({
            "row_type": "context",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "digest": digest,
            "identity": {
                "snapshot_id": { "chain_id": 5000, "block_number": 10, "block_hash": "0x01" },
                "header": { "parent_hash": "0x04", "block_timestamp": block_timestamp },
                "pool_universe_fingerprint": "0x02",
                "route": { "protocols": ["v2"], "hop_count": 1 },
                "fee_context": { "block_number": 10, "block_hash": "0x01", "base_fee_per_gas": 1, "block_gas_limit": 2 },
                "gas_profile_identity": "profile"
            },
            "opportunity_id": "opportunity-1",
            "ordered_pools": ["0x03"],
            "amount_in": "1",
            "gross_profit": "5",
            "net_profit": net_profit,
            "profit_basis": "simulated",
            "sequence": sequence,
        })
        .to_string()
    }

    fn write_lines(path: &Path, lines: &[String]) {
        let mut content = lines.join("\n");
        content.push('\n');
        fs::write(path, content).unwrap();
    }

    fn tmp_path(name: &str) -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        // Leak the tempdir so the file survives for the test body -- acceptable in
        // a short-lived test process.
        std::mem::forget(dir);
        path
    }

    #[test]
    fn reads_a_single_active_file_in_order() {
        let path = tmp_path("ledger.jsonl");
        write_lines(
            &path,
            &[
                header_line("run-a", 1000),
                observation_line(1, 1000, 1),
                observation_line(2, 1001, 2),
                candidate_line("0xabc", "pass", 1002, 3),
                context_line("0xabc", 1000, "42", 4),
            ],
        );

        let read = read_ledger_window(&path).unwrap();
        assert_eq!(read.run_headers.len(), 1);
        assert_eq!(read.run_headers[0].run_id, "run-a");
        assert_eq!(read.observations.len(), 2);
        assert_eq!(read.observations[0].block_number, 1);
        assert_eq!(read.observations[1].block_number, 2);
        assert_eq!(read.candidates.len(), 1);
        assert_eq!(read.candidates[0].outcome, CandidateOutcomeKind::Pass);
        assert_eq!(read.candidates[0].run_id, "run-a");
        assert_eq!(read.contexts.len(), 1);
        assert_eq!(read.contexts[0].net_profit, "42");
        assert!(read.deferred_incomplete_tail.is_none());
    }

    #[test]
    fn missing_ledger_and_no_rotated_segments_is_an_explicit_error() {
        let path = tmp_path("missing.jsonl");
        let err = read_ledger_window(&path).unwrap_err();
        assert!(matches!(err, LedgerReadError::LedgerNotFound { .. }));
    }

    #[test]
    fn malformed_json_row_is_a_hard_error_not_a_healthy_zero() {
        let path = tmp_path("ledger.jsonl");
        write_lines(
            &path,
            &[header_line("run-a", 1000), "{not valid json".to_string()],
        );
        let err = read_ledger_window(&path).unwrap_err();
        assert!(matches!(err, LedgerReadError::MalformedRow { line: 2, .. }));
    }

    #[test]
    fn unsupported_schema_version_is_a_hard_error() {
        let path = tmp_path("ledger.jsonl");
        let mut bad_header: serde_json::Value =
            serde_json::from_str(&header_line("run-a", 1000)).unwrap();
        bad_header["schema_version"] = serde_json::json!("whisker-arb/shadow-ledger/v99");
        write_lines(&path, &[bad_header.to_string()]);
        let err = read_ledger_window(&path).unwrap_err();
        assert!(matches!(
            err,
            LedgerReadError::UnsupportedSchemaVersion { line: 1, .. }
        ));
    }

    #[test]
    fn incomplete_trailing_line_on_active_file_is_deferred_not_an_error() {
        let path = tmp_path("ledger.jsonl");
        let mut content = format!(
            "{}\n{}\n",
            header_line("run-a", 1000),
            observation_line(1, 1000, 1)
        );
        content.push_str(r#"{"row_type":"observation","sequence":2,"#); // truncated, no newline
        fs::write(&path, content).unwrap();

        let read = read_ledger_window(&path).unwrap();
        assert_eq!(read.observations.len(), 1);
        assert!(read.deferred_incomplete_tail.is_some());
        assert!(read
            .deferred_incomplete_tail
            .as_ref()
            .unwrap()
            .contains("deferred"));
    }

    #[test]
    fn discovery_fields_round_trip() {
        let path = tmp_path("ledger.jsonl");
        write_lines(
            &path,
            &[
                header_line("run-a", 1000),
                observation_line_with_discovery(1, 1000, 1, 3, 5, 2),
            ],
        );
        let read = read_ledger_window(&path).unwrap();
        let discovery = read.observations[0].discovery.as_ref().unwrap();
        assert_eq!(discovery.cycles_optimized, Some(3));
        assert_eq!(discovery.cycles_total, Some(5));
        assert_eq!(discovery.dirty_pools_count, 2);
        assert!(!discovery.skipped);
    }

    #[test]
    fn missing_discovery_field_is_none_not_zero() {
        let path = tmp_path("ledger.jsonl");
        write_lines(
            &path,
            &[header_line("run-a", 1000), observation_line(1, 1000, 1)],
        );
        let read = read_ledger_window(&path).unwrap();
        assert!(read.observations[0].discovery.is_none());
    }

    #[test]
    fn reads_rotated_segments_before_the_active_file_in_chronological_order() {
        let path = tmp_path("ledger.jsonl");
        // Oldest content, will become `.2` after two rotations.
        write_lines(
            &path,
            &[header_line("run-a", 1000), observation_line(1, 1000, 1)],
        );
        let paths = SegmentPaths::new(&path);
        rotate_active_file(&paths).unwrap(); // -> .1

        write_lines(
            &path,
            &[header_line("run-a", 1000), observation_line(2, 2000, 1)],
        );
        rotate_active_file(&paths).unwrap(); // old .1 -> .2, this -> .1

        write_lines(
            &path,
            &[header_line("run-a", 1000), observation_line(3, 3000, 1)],
        );
        // active file now has block 3.

        let read = read_ledger_window(&path).unwrap();
        assert_eq!(read.segments_read.len(), 3);
        assert!(read.segments_read[0].ends_with(".2"));
        assert!(read.segments_read[1].ends_with(".1"));
        assert!(
            !read.segments_read[2].contains('.') || read.segments_read[2].ends_with("ledger.jsonl")
        );
        let blocks: Vec<u64> = read.observations.iter().map(|o| o.block_number).collect();
        assert_eq!(blocks, vec![1, 2, 3]);
        // Repeated header re-emission is preserved as-is (3 header rows for 1 run) —
        // deduplication is the aggregator's job, not the reader's.
        assert_eq!(read.run_headers.len(), 3);
        assert!(read.run_headers.iter().all(|h| h.run_id == "run-a"));
    }

    #[test]
    fn a_utc_day_boundary_crossing_a_segment_rotation_reads_both_segments_contiguously() {
        let path = tmp_path("ledger.jsonl");
        // Day 1 (2026-01-01) ends at unix 1_767_312_000. Put one observation just
        // before midnight in the rotated segment and one just after in the active
        // file.
        let before_midnight = 1_767_311_999u64;
        let after_midnight = 1_767_312_000u64;
        write_lines(
            &path,
            &[
                header_line("run-a", 1_767_000_000),
                observation_line(100, before_midnight, 1),
            ],
        );
        let paths = SegmentPaths::new(&path);
        rotate_active_file(&paths).unwrap();
        write_lines(
            &path,
            &[
                header_line("run-a", 1_767_000_000),
                observation_line(101, after_midnight, 1),
            ],
        );

        let read = read_ledger_window(&path).unwrap();
        let timestamps: Vec<u64> = read
            .observations
            .iter()
            .map(|o| o.recorded_at_unix)
            .collect();
        assert_eq!(timestamps, vec![before_midnight, after_midnight]);
    }

    #[test]
    fn segments_stable_detects_a_new_rotation_between_two_listings() {
        let before: Vec<(u32, PathBuf, u64)> = vec![(1, PathBuf::from("x.1"), 10)];
        let after_same = before.clone();
        let after_rotated: Vec<(u32, PathBuf, u64)> =
            vec![(1, PathBuf::from("x.1"), 10), (2, PathBuf::from("x.2"), 10)];
        assert!(segments_stable(&before, &after_same));
        assert!(!segments_stable(&before, &after_rotated));
    }

    #[test]
    fn candidate_and_context_rows_carry_the_active_run_id() {
        let path = tmp_path("ledger.jsonl");
        write_lines(
            &path,
            &[
                header_line("run-a", 1000),
                candidate_line("0xabc", "pass", 1002, 1),
                context_line("0xabc", 1000, "42", 2),
            ],
        );
        let read = read_ledger_window(&path).unwrap();
        assert_eq!(read.candidates[0].run_id, "run-a");
        assert_eq!(read.contexts[0].run_id, "run-a");
    }

    #[test]
    fn unknown_row_types_are_ignored_not_errors() {
        let path = tmp_path("ledger.jsonl");
        let provenance = serde_json::json!({
            "row_type": "provenance",
            "schema_version": LEDGER_SCHEMA_VERSION,
            "digest": "0xabc",
            "outcome": "moe_allowlisted",
            "hop_outcomes": ["moe_allowlisted"],
            "sequence": 1,
        })
        .to_string();
        write_lines(&path, &[header_line("run-a", 1000), provenance]);
        let read = read_ledger_window(&path).unwrap();
        assert_eq!(read.observations.len(), 0);
        assert_eq!(read.candidates.len(), 0);
    }
}
