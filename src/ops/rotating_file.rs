//! Size-based segment rotation with a hard total-bytes retention cap (WHI-952).
//!
//! Active file stays at the operator-facing path (e.g. `ledger.jsonl` /
//! `live.log`). On rotation the current contents move to `path.1`, previous
//! `.1` becomes `.2`, and so on — classic logrotate numbering. Retention walks
//! from oldest (highest N) and deletes until total size of active + rotated
//! segments is `≤ max_total_bytes`.
//!
//! This module is pure filesystem logic. Callers that hold an open write handle
//! (e.g. [`crate::execution::shadow::ShadowLedgerWriter`]) must close/reopen
//! around [`rotate_active_file`].

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Env: max size of one shadow-ledger segment before rotation (bytes).
pub const ENV_SHADOW_LEDGER_MAX_SEGMENT_BYTES: &str = "SHADOW_LEDGER_MAX_SEGMENT_BYTES";
/// Env: hard cap across active + rotated shadow-ledger segments (bytes).
pub const ENV_SHADOW_LEDGER_MAX_TOTAL_BYTES: &str = "SHADOW_LEDGER_MAX_TOTAL_BYTES";
/// Env: max size of one tracing-log segment before rotation (bytes).
pub const ENV_LOG_MAX_SEGMENT_BYTES: &str = "LOG_MAX_SEGMENT_BYTES";
/// Env: hard cap across active + rotated tracing-log segments (bytes).
pub const ENV_LOG_MAX_TOTAL_BYTES: &str = "LOG_MAX_TOTAL_BYTES";

/// Default segment cap for the shadow ledger (64 MiB).
pub const DEFAULT_SHADOW_LEDGER_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Default total cap for the shadow ledger (512 MiB).
pub const DEFAULT_SHADOW_LEDGER_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Default segment cap for tracing logs (64 MiB).
pub const DEFAULT_LOG_MAX_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
/// Default total cap for tracing logs (512 MiB).
pub const DEFAULT_LOG_MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RotationError {
    #[error("rotation policy invalid: max_segment_bytes ({max_segment}) must be > 0 and ≤ max_total_bytes ({max_total})")]
    InvalidPolicy { max_segment: u64, max_total: u64 },
    #[error("rotation io: {0}")]
    Io(String),
    #[error("rotation env: {0}")]
    Env(String),
}

impl From<io::Error> for RotationError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// Size thresholds that force rotation and reclaim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationPolicy {
    /// Soft cap for the active segment; a write that would exceed it triggers
    /// rotation first (or after the write, for callers that check post-write).
    pub max_segment_bytes: u64,
    /// Hard upper bound on active + all rotated segments combined.
    pub max_total_bytes: u64,
}

impl RotationPolicy {
    /// Effectively unbounded — kept for unit tests that exercise non-rotating paths.
    pub const UNBOUNDED: Self = Self {
        max_segment_bytes: u64::MAX / 4,
        max_total_bytes: u64::MAX / 2,
    };

    pub fn new(max_segment_bytes: u64, max_total_bytes: u64) -> Result<Self, RotationError> {
        let policy = Self {
            max_segment_bytes,
            max_total_bytes,
        };
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(self) -> Result<(), RotationError> {
        if self.max_segment_bytes == 0 || self.max_segment_bytes > self.max_total_bytes {
            return Err(RotationError::InvalidPolicy {
                max_segment: self.max_segment_bytes,
                max_total: self.max_total_bytes,
            });
        }
        Ok(())
    }

    pub fn should_rotate_segment(self, segment_bytes: u64) -> bool {
        segment_bytes >= self.max_segment_bytes
    }

    /// Shadow-ledger policy from env, falling back to production defaults.
    pub fn shadow_ledger_from_env() -> Result<Self, RotationError> {
        Self::from_env_keys(
            ENV_SHADOW_LEDGER_MAX_SEGMENT_BYTES,
            ENV_SHADOW_LEDGER_MAX_TOTAL_BYTES,
            DEFAULT_SHADOW_LEDGER_MAX_SEGMENT_BYTES,
            DEFAULT_SHADOW_LEDGER_MAX_TOTAL_BYTES,
        )
    }

    /// Tracing-log policy from env, falling back to production defaults.
    pub fn tracing_log_from_env() -> Result<Self, RotationError> {
        Self::from_env_keys(
            ENV_LOG_MAX_SEGMENT_BYTES,
            ENV_LOG_MAX_TOTAL_BYTES,
            DEFAULT_LOG_MAX_SEGMENT_BYTES,
            DEFAULT_LOG_MAX_TOTAL_BYTES,
        )
    }

    fn from_env_keys(
        segment_key: &str,
        total_key: &str,
        default_segment: u64,
        default_total: u64,
    ) -> Result<Self, RotationError> {
        let max_segment_bytes = read_u64_env(segment_key, default_segment)?;
        let max_total_bytes = read_u64_env(total_key, default_total)?;
        Self::new(max_segment_bytes, max_total_bytes)
    }
}

fn read_u64_env(key: &str, default: u64) -> Result<u64, RotationError> {
    match std::env::var(key) {
        Ok(raw) => raw.trim().parse::<u64>().map_err(|e| {
            RotationError::Env(format!("{key}={raw:?} is not a u64: {e}"))
        }),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(e) => Err(RotationError::Env(format!("{key}: {e}"))),
    }
}

/// Paths derived from an active file path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentPaths {
    pub active: PathBuf,
}

impl SegmentPaths {
    pub fn new(active: impl Into<PathBuf>) -> Self {
        Self {
            active: active.into(),
        }
    }

    /// Rotated segment path for index `n` (`n ≥ 1`). Higher `n` is older.
    pub fn rotated(&self, n: u32) -> PathBuf {
        let active = self.active.as_os_str().to_string_lossy();
        PathBuf::from(format!("{active}.{n}"))
    }
}

/// List existing rotated segments as `(n, path, size_bytes)`, sorted by `n` ascending
/// (newest rotated first).
pub fn list_rotated_segments(paths: &SegmentPaths) -> Result<Vec<(u32, PathBuf, u64)>, RotationError> {
    let parent = paths
        .active
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = paths
        .active
        .file_name()
        .ok_or_else(|| RotationError::Io("active path has no file name".into()))?
        .to_string_lossy();
    let prefix = format!("{file_name}.");

    let mut out = Vec::new();
    let entries = match fs::read_dir(parent) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(suffix) = name.strip_prefix(&prefix) {
            if let Ok(n) = suffix.parse::<u32>() {
                if n >= 1 {
                    let meta = entry.metadata()?;
                    if meta.is_file() {
                        out.push((n, entry.path(), meta.len()));
                    }
                }
            }
        }
    }
    out.sort_by_key(|(n, _, _)| *n);
    Ok(out)
}

/// Bytes of the active file (0 if missing) plus every rotated segment.
pub fn total_bytes_for_path(paths: &SegmentPaths) -> Result<u64, RotationError> {
    let mut total = 0u64;
    if let Ok(meta) = fs::metadata(&paths.active) {
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        }
    }
    for (_, _, size) in list_rotated_segments(paths)? {
        total = total.saturating_add(size);
    }
    Ok(total)
}

/// Rotate `paths.active` → `.1`, shifting existing rotated segments up by one.
///
/// The active path must **not** be held open by the caller — rename replaces the
/// name; an open fd would keep writing into the old inode under the new name.
///
/// Returns the path of the newly created rotated segment (`.1`).
pub fn rotate_active_file(paths: &SegmentPaths) -> Result<PathBuf, RotationError> {
    if !paths.active.exists() {
        return Err(RotationError::Io(format!(
            "cannot rotate missing active file {}",
            paths.active.display()
        )));
    }

    let existing = list_rotated_segments(paths)?;
    // Shift high → higher first so we do not clobber.
    for (n, src, _) in existing.into_iter().rev() {
        let dest = paths.rotated(n.saturating_add(1));
        fs::rename(&src, &dest)?;
    }
    let dest = paths.rotated(1);
    fs::rename(&paths.active, &dest)?;
    Ok(dest)
}

/// Delete oldest rotated segments until total size ≤ `policy.max_total_bytes`.
///
/// Never deletes the active file. Returns the number of segments removed.
pub fn apply_retention(
    paths: &SegmentPaths,
    policy: RotationPolicy,
) -> Result<usize, RotationError> {
    policy.validate()?;
    let mut removed = 0usize;
    loop {
        let total = total_bytes_for_path(paths)?;
        if total <= policy.max_total_bytes {
            break;
        }
        let rotated = list_rotated_segments(paths)?;
        let Some((n, path, _)) = rotated.into_iter().next_back() else {
            // Only the active file remains and it alone exceeds the cap —
            // cannot reclaim further without destroying live data. Callers that
            // need a hard guarantee should set max_segment ≤ max_total and
            // rotate before the active file grows past the total.
            break;
        };
        fs::remove_file(&path).map_err(|e| {
            RotationError::Io(format!(
                "failed to reclaim rotated segment {}.{}: {e}",
                paths.active.display(),
                n
            ))
        })?;
        removed += 1;
    }
    Ok(removed)
}

/// Append bytes to a path, rotating first when the active segment is already at
/// the soft cap. After the write, rotate again if over the soft cap, then apply
/// retention. Returns `(bytes_written, rotations_this_call)`.
///
/// Intended for the tracing-log tee and unit tests. The shadow ledger uses the
/// lower-level helpers so it can re-emit a run header after rotation.
pub fn append_with_rotation(
    paths: &SegmentPaths,
    policy: RotationPolicy,
    bytes: &[u8],
) -> Result<(u64, u32), RotationError> {
    policy.validate()?;
    if let Some(parent) = paths.active.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut rotations = 0u32;
    let current_len = fs::metadata(&paths.active).map(|m| m.len()).unwrap_or(0);
    if policy.should_rotate_segment(current_len) && current_len > 0 {
        rotate_active_file(paths)?;
        rotations += 1;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.active)?;
    file.write_all(bytes)?;
    file.flush()?;
    let new_len = fs::metadata(&paths.active)?.len();
    if policy.should_rotate_segment(new_len) {
        // Close before rename.
        drop(file);
        rotate_active_file(paths)?;
        // Touch a fresh empty active file so the operator path always exists.
        File::create(&paths.active)?;
        rotations += 1;
    }
    apply_retention(paths, policy)?;
    Ok((bytes.len() as u64, rotations))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    fn read_file(path: &Path) -> Vec<u8> {
        let mut buf = Vec::new();
        File::open(path).unwrap().read_to_end(&mut buf).unwrap();
        buf
    }

    #[test]
    fn policy_rejects_zero_or_segment_above_total() {
        assert!(RotationPolicy::new(0, 100).is_err());
        assert!(RotationPolicy::new(200, 100).is_err());
        assert!(RotationPolicy::new(100, 100).is_ok());
    }

    #[test]
    fn force_rotation_with_small_threshold_and_retention_bounds_total() {
        // Acceptance: force at least one rotation with a small threshold and
        // verify reclaimed segments keep total size under the hard cap.
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("live.log");
        let paths = SegmentPaths::new(&active);
        let policy = RotationPolicy::new(32, 80).unwrap();

        let mut total_rotations = 0u32;
        for i in 0..20 {
            let line = format!("line-{i:02}xxxxxxxxxx\n"); // 18 bytes each
            let (_n, rotations) = append_with_rotation(&paths, policy, line.as_bytes()).unwrap();
            total_rotations += rotations;
            let total = total_bytes_for_path(&paths).unwrap();
            assert!(
                total <= policy.max_total_bytes,
                "total {total} exceeded hard cap {} after write {i}",
                policy.max_total_bytes
            );
        }

        assert!(
            total_rotations >= 1,
            "small threshold must force at least one rotation; got {total_rotations}"
        );
        let rotated = list_rotated_segments(&paths).unwrap();
        assert!(
            !rotated.is_empty(),
            "expected at least one rotated segment on disk"
        );
        // Active file may be empty (just rotated) or partial — either is fine.
        assert!(paths.active.exists());
    }

    #[test]
    fn rotate_shifts_segment_numbers_and_preserves_content() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("ledger.jsonl");
        let paths = SegmentPaths::new(&active);
        write_file(&active, b"seg-active\n");
        write_file(&paths.rotated(1), b"seg-one\n");

        let dest = rotate_active_file(&paths).unwrap();
        assert_eq!(dest, paths.rotated(1));
        assert!(!active.exists());
        assert_eq!(read_file(&paths.rotated(1)), b"seg-active\n");
        assert_eq!(read_file(&paths.rotated(2)), b"seg-one\n");
    }

    #[test]
    fn retention_deletes_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let active = dir.path().join("x.log");
        let paths = SegmentPaths::new(&active);
        write_file(&active, b"aaaa"); // 4
        write_file(&paths.rotated(1), b"bbbb"); // 4
        write_file(&paths.rotated(2), b"cccc"); // 4
        write_file(&paths.rotated(3), b"dddd"); // 4
        // total 16; cap 10 → must drop .3 then .2
        let removed = apply_retention(&paths, RotationPolicy::new(4, 10).unwrap()).unwrap();
        assert!(removed >= 2);
        assert!(paths.rotated(3).exists() == false || total_bytes_for_path(&paths).unwrap() <= 10);
        assert!(total_bytes_for_path(&paths).unwrap() <= 10);
        assert!(active.exists());
    }
}
