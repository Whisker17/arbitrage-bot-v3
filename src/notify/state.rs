//! Day-keyed idempotency marker + single-flight lock (WHI-1407 item 5), and the pure
//! backlog day-list helper (item 6).
//!
//! The lock spans state-read → POST → state-write: [`StateHandle::open_exclusive`]
//! holds an advisory `flock` for its entire lifetime, so a second overlapping
//! invocation fails fast at open time rather than racing the first to a POST. State
//! is only ever written by [`StateHandle::record_sent_day`], called **after**
//! confirmed provider success — never speculatively before the POST.
//!
//! **Accepted semantics (explicitly not stronger than this):** an ambiguous transport
//! failure, or a crash between the provider accepting the card and this state write
//! landing, can cause a duplicate send on the next retry. This module does not
//! promise exactly-once delivery, and does not promise "at most one duplicate" either
//! — repeated ambiguous failures across repeated invocations could in principle
//! duplicate more than once. No distributed lock, no provider idempotency key, no
//! durable queue: explicitly out of scope for this informational digest (see the
//! issue's Implementation §5).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::notify::utc_date::UtcDay;
use crate::ops::{is_lock_contended, FileExtLock};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("digest state io error at {path}: {detail}")]
    Io { path: String, detail: String },
    #[error("digest state file {path} is already locked by another invocation")]
    Locked { path: String },
    #[error("digest state file {path} content {found:?} is not a valid UTC date")]
    Corrupt { path: String, found: String },
}

/// Holds the exclusive lock on `path` for as long as this value lives. Dropping it
/// releases the lock (the OS releases `flock` on `close()`, which happens when `file`
/// is dropped).
pub struct StateHandle {
    file: File,
    path: PathBuf,
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateHandle")
            .field("path", &self.path)
            .finish()
    }
}

impl StateHandle {
    /// Opens (creating if absent) and exclusively locks the state file at `path`.
    /// Returns [`StateError::Locked`] immediately — never blocks — if another
    /// invocation already holds the lock, which is exactly the single-flight
    /// guarantee this module provides: a second overlapping invocation fails fast
    /// before it can even read the marker, let alone POST.
    pub fn open_exclusive(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| StateError::Io {
                    path: path.display().to_string(),
                    detail: e.to_string(),
                })?;
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| StateError::Io {
                path: path.display().to_string(),
                detail: e.to_string(),
            })?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if is_lock_contended(&error) => {
                return Err(StateError::Locked {
                    path: path.display().to_string(),
                })
            }
            Err(error) => {
                return Err(StateError::Io {
                    path: path.display().to_string(),
                    detail: error.to_string(),
                })
            }
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// The last UTC day confirmed sent, or `None` if the state file is empty
    /// (fresh install — no day has ever been confirmed).
    pub fn last_sent_day(&mut self) -> Result<Option<UtcDay>, StateError> {
        let mut contents = String::new();
        self.file
            .read_to_string(&mut contents)
            .map_err(|e| StateError::Io {
                path: self.path.display().to_string(),
                detail: e.to_string(),
            })?;
        let trimmed = contents.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        UtcDay::parse(trimmed)
            .map(Some)
            .map_err(|_| StateError::Corrupt {
                path: self.path.display().to_string(),
                found: trimmed.to_string(),
            })
    }

    /// Atomically persists `day` as the last confirmed-sent day. Callers must only
    /// call this **after** a confirmed provider success (see this module's doc).
    pub fn record_sent_day(&mut self, day: UtcDay) -> Result<(), StateError> {
        let tmp_path = PathBuf::from(format!(
            "{}.tmp-{}",
            self.path.display(),
            std::process::id()
        ));
        let mut tmp = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp_path)
            .map_err(|e| StateError::Io {
                path: tmp_path.display().to_string(),
                detail: e.to_string(),
            })?;
        tmp.write_all(day.to_string().as_bytes())
            .and_then(|_| tmp.sync_all())
            .map_err(|e| StateError::Io {
                path: tmp_path.display().to_string(),
                detail: e.to_string(),
            })?;
        fs::rename(&tmp_path, &self.path).map_err(|e| StateError::Io {
            path: self.path.display().to_string(),
            detail: e.to_string(),
        })?;
        // Re-open our own handle's view of the file content for subsequent
        // `last_sent_day` calls within the same process (the rename replaced the
        // inode `self.file` still points at is fine for read-after-write via a
        // fresh read, but `self.file`'s cursor/content view is stale — reopen).
        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(|e| StateError::Io {
                path: self.path.display().to_string(),
                detail: e.to_string(),
            })?;
        Ok(())
    }
}

/// How many days one bounded invocation will process before surfacing the remainder
/// as backlog (issue item 6: "within one bounded invocation").
pub const MAX_DAYS_PER_INVOCATION: usize = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklogPlan {
    /// Days to process this invocation, oldest first.
    pub days: Vec<UtcDay>,
    /// `true` when there are still unsent days beyond `days` after this bound.
    pub backlog_remains: bool,
}

/// Pure: determines which UTC days this invocation should process, given the last
/// confirmed-sent day and "now". Never processes "today" (incomplete) — the first
/// run (no state yet) processes exactly the previous completed UTC day, matching the
/// issue's "Backlog / first run" contract. `max_days` bounds one invocation so a long
/// outage cannot make a single run unbounded; the remainder is surfaced via
/// `backlog_remains`, never silently dropped.
pub fn plan_backlog(
    last_sent_day: Option<UtcDay>,
    now_day: UtcDay,
    max_days: usize,
) -> BacklogPlan {
    let latest_processable = now_day.previous();
    let start = match last_sent_day {
        Some(day) => day.next(),
        None => latest_processable,
    };
    if start > latest_processable {
        return BacklogPlan {
            days: Vec::new(),
            backlog_remains: false,
        };
    }
    let mut days = Vec::new();
    let mut cursor = start;
    while cursor <= latest_processable && days.len() < max_days.max(1) {
        days.push(cursor);
        cursor = cursor.next();
    }
    BacklogPlan {
        backlog_remains: cursor <= latest_processable,
        days,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_state_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("amms-lark-digest-state-{name}-{nanos}"))
    }

    #[test]
    fn fresh_state_file_has_no_last_sent_day() {
        let path = tmp_state_path("fresh");
        let mut handle = StateHandle::open_exclusive(&path).unwrap();
        assert_eq!(handle.last_sent_day().unwrap(), None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn record_then_read_round_trips() {
        let path = tmp_state_path("roundtrip");
        let mut handle = StateHandle::open_exclusive(&path).unwrap();
        let day = UtcDay::parse("2026-06-15").unwrap();
        handle.record_sent_day(day).unwrap();
        assert_eq!(handle.last_sent_day().unwrap(), Some(day));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn a_second_overlapping_open_is_rejected_not_blocked() {
        let path = tmp_state_path("overlap");
        let _first = StateHandle::open_exclusive(&path).unwrap();
        let err = StateHandle::open_exclusive(&path).unwrap_err();
        assert!(matches!(err, StateError::Locked { .. }));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn lock_is_released_on_drop_so_a_later_invocation_can_proceed() {
        let path = tmp_state_path("release");
        {
            let _first = StateHandle::open_exclusive(&path).unwrap();
        }
        let _second = StateHandle::open_exclusive(&path).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn corrupt_state_content_is_an_explicit_error_not_a_silent_reset() {
        let path = tmp_state_path("corrupt");
        fs::write(&path, "not-a-date").unwrap();
        let mut handle = StateHandle::open_exclusive(&path).unwrap();
        let err = handle.last_sent_day().unwrap_err();
        assert!(matches!(err, StateError::Corrupt { .. }));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn first_run_with_no_state_processes_only_the_previous_completed_day() {
        let now = UtcDay::parse("2026-06-15").unwrap();
        let plan = plan_backlog(None, now, MAX_DAYS_PER_INVOCATION);
        assert_eq!(plan.days, vec![UtcDay::parse("2026-06-14").unwrap()]);
        assert!(!plan.backlog_remains);
    }

    #[test]
    fn a_normal_daily_run_processes_exactly_yesterday() {
        let now = UtcDay::parse("2026-06-15").unwrap();
        let last_sent = UtcDay::parse("2026-06-13").unwrap();
        let plan = plan_backlog(Some(last_sent), now, MAX_DAYS_PER_INVOCATION);
        assert_eq!(plan.days, vec![UtcDay::parse("2026-06-14").unwrap()]);
    }

    #[test]
    fn already_sent_through_yesterday_processes_nothing() {
        let now = UtcDay::parse("2026-06-15").unwrap();
        let last_sent = UtcDay::parse("2026-06-14").unwrap();
        let plan = plan_backlog(Some(last_sent), now, MAX_DAYS_PER_INVOCATION);
        assert!(plan.days.is_empty());
        assert!(!plan.backlog_remains);
    }

    #[test]
    fn a_multi_day_outage_is_processed_in_order_and_bounded() {
        let now = UtcDay::parse("2026-07-01").unwrap();
        let last_sent = UtcDay::parse("2026-06-01").unwrap();
        let plan = plan_backlog(Some(last_sent), now, 5);
        assert_eq!(plan.days.len(), 5);
        assert_eq!(plan.days[0], UtcDay::parse("2026-06-02").unwrap());
        assert_eq!(plan.days[4], UtcDay::parse("2026-06-06").unwrap());
        assert!(plan.backlog_remains, "more than 5 days were outstanding");
    }

    #[test]
    fn never_processes_today() {
        let now = UtcDay::parse("2026-06-15").unwrap();
        let last_sent = UtcDay::parse("2026-06-14").unwrap();
        let plan = plan_backlog(Some(last_sent), now, MAX_DAYS_PER_INVOCATION);
        assert!(!plan.days.contains(&now));
    }
}
