//! Operator-facing utilities that do not belong to the trading hot path.
//!
//! WHI-952 / G-5: size-bounded append files with segment rotation and a hard
//! total-bytes retention cap. Used by the shadow ledger (Rust) and mirrored by
//! `scripts/golive/rotating_tee.sh` for tracing logs.

pub mod file_lock;
pub mod rotating_file;

pub use file_lock::{is_lock_contended, FileExtLock};
pub use rotating_file::{
    apply_retention, list_rotated_segments, rotate_active_file, total_bytes_for_path,
    RotationError, RotationPolicy, SegmentPaths, DEFAULT_LOG_MAX_SEGMENT_BYTES,
    DEFAULT_LOG_MAX_TOTAL_BYTES, DEFAULT_SHADOW_LEDGER_MAX_SEGMENT_BYTES,
    DEFAULT_SHADOW_LEDGER_MAX_TOTAL_BYTES, ENV_LOG_MAX_SEGMENT_BYTES, ENV_LOG_MAX_TOTAL_BYTES,
    ENV_SHADOW_LEDGER_MAX_SEGMENT_BYTES, ENV_SHADOW_LEDGER_MAX_TOTAL_BYTES,
};
