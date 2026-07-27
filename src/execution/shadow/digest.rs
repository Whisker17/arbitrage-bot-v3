//! Shared digest helper for shadow-mode config/manifest hashing.
//!
//! Reuses `runtime_identity`'s canonical-JSON keccak digest exactly, so every
//! shadow digest (manifest, WMNT descriptor, Moe allowlist) is computed the same
//! way as build-evidence digests — no second canonicalization scheme.

pub(crate) use crate::execution::runtime_identity::digest_of;
