//! Error type for the E2E-only typed transaction capability layer (WHI-555).

use thiserror::Error;

/// Failure reasons for the E2E capability layer.
///
/// Every variant is deliberately silent about *values* that could leak
/// secrets (private keys, credentialed RPC URLs, userinfo, query strings): the
/// `Display` impl never formats a raw env value, only variable *names*,
/// addresses, chain ids, and digests, all of which are safe to trace.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum E2eCapabilityError {
    #[error("required env var {0} is not set")]
    MissingEnvVar(&'static str),

    #[error("env var {0} could not be parsed: {1}")]
    InvalidEnvVar(&'static str, String),

    #[error(
        "forbidden production/legacy credential variable {0} is present in the process \
         environment; unset it before running the E2E harness"
    )]
    ForbiddenEnvVarPresent(&'static str),

    #[error("live chain id {observed} does not match required Mantle Sepolia chain id {expected}")]
    WrongChainId { expected: u64, observed: u64 },

    #[error("chain id 5000 (Mantle mainnet) is never permitted for the E2E send path")]
    MainnetChainIdRejected,

    #[error("live genesis hash does not match the committed Mantle Sepolia genesis hash")]
    WrongGenesisHash,

    #[error("derived signer address is on the committed production denylist")]
    DenylistedSigner,

    #[error(
        "bootstrap authority already finalized into a manifest; no further permits can be minted"
    )]
    BootstrapAlreadyFinalized,

    #[error("permit chain id mismatch: manifest is {expected}, permit is bound to {actual}")]
    ChainIdMismatch { expected: u64, actual: u64 },

    #[error("permit provider-identity mismatch: this signer/provider session does not match the one that minted the permit")]
    ProviderIdentityMismatch,

    #[error("permit manifest-identity mismatch: this manifest does not match the one that minted the permit")]
    ManifestIdentityMismatch,

    #[error("permit signer mismatch: expected {expected}, permit is bound to {actual}")]
    SignerMismatch {
        expected: alloy::primitives::Address,
        actual: alloy::primitives::Address,
    },

    #[error("permit executor mismatch: expected {expected}, permit is bound to {actual}")]
    ExecutorMismatch {
        expected: alloy::primitives::Address,
        actual: alloy::primitives::Address,
    },

    #[error("permit digest does not match a fresh digest of its bound transaction")]
    DigestMismatch,

    #[error(
        "submission integrity check failed: keccak256(raw) does not match the recorded tx hash"
    )]
    SubmissionIntegrityFailed,

    #[error("{0}")]
    Other(String),
}
