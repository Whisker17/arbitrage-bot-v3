//! Shared canonical artifact-signing module.
//!
//! Provides RFC 8785 (JCS) canonicalization, OpenSSH detached-signature
//! sign/verify, domain-scoped principal authorization, and revocation
//! checking for operator-signed artifacts. Deliberately schema-agnostic: it
//! knows nothing about any consumer's concrete payload shape.
//!
//! # Payload conventions (producer responsibility)
//!
//! Because this module is schema-agnostic it cannot itself validate
//! consumer-specific fields. Producers of a signed envelope (e.g. WHI-521,
//! WHI-554) are responsible for following these conventions; only the
//! numeric-value rule is mechanically enforced here (via
//! [`canonical::assert_no_numbers`], run on both the sign and verify paths):
//!
//! - Numeric values must be decimal strings, never JSON numbers.
//! - Hashes and addresses must be lowercase `0x`-prefixed hex strings.
//! - No signature field is embedded in the payload itself — the signature
//!   travels alongside the payload bytes, never inside them.
//!
//! # Offline operator provisioning
//!
//! 1. Generate a keypair: `ssh-keygen -t ed25519 -f <key-path> -N ""`.
//! 2. Add a line to the committed `config/signers/allowed_signers` (see that
//!    file's header for the exact format), restricting the principal to the
//!    domain(s) it may sign for via `namespaces="..."`. Have the change
//!    reviewed and committed like any other repository change.
//! 3. Sign artifacts offline with [`ssh::sign`], using the private key that
//!    never leaves the operator's control.
//! 4. To revoke a key, append its public key line to the committed
//!    `config/signers/revoked_keys` file — no rebuild required, it is
//!    checked on every verification.

pub mod artifact;
pub mod canonical;
pub mod config;
pub mod error;
pub mod scope;
pub mod ssh;

pub use artifact::VerifiedArtifact;
pub use error::SigningError;
pub use scope::ExpectedScope;

use std::path::Path;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use canonical::{assert_no_numbers, canonicalize_value};

#[derive(Deserialize)]
struct EnvelopeFields {
    schema_version: String,
    domain: String,
    scope: Value,
}

/// Verifies a signed artifact and returns its opaque [`VerifiedArtifact<T>`].
///
/// This is the only entrypoint production code should call: it always uses
/// the committed, code-constant `allowed_signers`/`revoked_keys` paths from
/// [`config`].
pub fn verify<T: DeserializeOwned>(
    payload_bytes: &[u8],
    signature: &[u8],
    domain: &'static str,
    principal: &str,
    expected_scope: &ExpectedScope,
) -> Result<VerifiedArtifact<T>, SigningError> {
    verify_with_paths(
        payload_bytes,
        signature,
        domain,
        principal,
        expected_scope,
        &config::allowed_signers_path(),
        &config::revoked_keys_path(),
    )
}

/// Test/tooling-only: verifies a signed artifact against explicit
/// `allowed_signers`/`revoked_keys` paths instead of the production
/// code-constant ones.
///
/// Production code must call [`verify`] instead. This function is `pub`
/// (not `pub(crate)`) only because `tests/signing.rs` is a separate
/// integration-test crate that can inject temp-fixture paths but can only
/// see this crate's public API surface.
#[doc(hidden)]
pub fn verify_with_paths<T: DeserializeOwned>(
    payload_bytes: &[u8],
    signature: &[u8],
    domain: &'static str,
    principal: &str,
    expected_scope: &ExpectedScope,
    allowed_signers_path: &Path,
    revoked_keys_path: &Path,
) -> Result<VerifiedArtifact<T>, SigningError> {
    ssh::verify_detached(
        allowed_signers_path,
        revoked_keys_path,
        principal,
        domain,
        payload_bytes,
        signature,
    )?;

    let value: Value = serde_json::from_slice(payload_bytes)?;
    assert_no_numbers(&value, "$")?;

    let fields: EnvelopeFields = serde_json::from_value(value.clone())?;

    if fields.domain != domain {
        return Err(SigningError::DomainMismatch {
            expected: domain.to_string(),
            found: fields.domain,
        });
    }

    if !fields.scope.is_object() {
        return Err(SigningError::ScopeNotObject);
    }
    expected_scope.matches(&fields.scope)?;

    let canonical_bytes = canonicalize_value(&value)?;
    if canonical_bytes != payload_bytes {
        return Err(SigningError::CanonicalFormMismatch);
    }

    let payload: T = serde_json::from_value(value)?;

    Ok(VerifiedArtifact::new(
        fields.schema_version,
        fields.domain,
        payload,
    ))
}
