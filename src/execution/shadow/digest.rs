//! Shared digest helper for shadow-mode config/manifest hashing.
//!
//! Reuses `runtime_identity`'s canonical-JSON keccak digest exactly, so every
//! shadow digest (manifest, WMNT descriptor, Moe allowlist) is computed the same
//! way as build-evidence digests — no second canonicalization scheme.

use alloy::primitives::{keccak256, B256};

pub(crate) use crate::execution::runtime_identity::digest_of;

/// Plain `keccak256` over raw bytes, with no JSON canonicalization — used for the
/// shadow-threshold config, which is pinned as opaque bytes and must never be
/// parsed/deserialized (see `thresholds.rs`).
pub(crate) fn digest_of_bytes(bytes: &[u8]) -> B256 {
    keccak256(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_of_bytes_is_deterministic_and_changes_with_content() {
        let a = b"hello".as_slice();
        let b = b"hellp".as_slice();
        assert_eq!(digest_of_bytes(a), digest_of_bytes(a));
        assert_ne!(digest_of_bytes(a), digest_of_bytes(b));
        assert_eq!(digest_of_bytes(a), keccak256(a));
    }
}
