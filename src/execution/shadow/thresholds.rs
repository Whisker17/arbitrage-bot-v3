//! Shadow-mode risk/sampling threshold config — pinned as **opaque bytes**.
//!
//! Unlike every other shadow config file (`moe_allowlist.rs`, `wmnt_descriptor.rs`,
//! `approved_pools.rs`), this file's schema is intentionally never parsed or
//! validated here: shadow mode only needs to prove the file a run started with is
//! the exact file still on disk at each batch boundary, not to understand its
//! contents. Treating it as opaque bytes means a schema change on the threshold
//! side never requires a change to this loader.

use std::fs;
use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ThresholdError {
    #[error("io: {0}")]
    Io(String),
}

/// Reads the shadow-threshold config file as raw bytes. Fails if the file is
/// missing — shadow mode must never silently run without a pinned threshold
/// config. The contents are never deserialized; see the module-level doc comment.
pub fn load_threshold_bytes(path: &Path) -> Result<Vec<u8>, ThresholdError> {
    fs::read(path).map_err(|error| ThresholdError::Io(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn loads_the_exact_bytes_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow_thresholds.json");
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"{ not even valid json on purpose }")
            .unwrap();

        let bytes = load_threshold_bytes(&path).unwrap();
        assert_eq!(bytes, b"{ not even valid json on purpose }".to_vec());
    }

    #[test]
    fn errors_when_the_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        let error = load_threshold_bytes(&path).unwrap_err();
        assert!(matches!(error, ThresholdError::Io(_)));
    }
}
