//! Error type for the E2E-only typed transaction capability layer (WHI-555).

use crate::execution::intent::IntentError;
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

    #[error("bootstrap/sign/trigger/cancel transaction is missing a nonce")]
    MissingTxNonce,

    #[error("permit tx nonce mismatch: bound nonce {expected}, tx carries {actual:?}")]
    PermitNonceMismatch { expected: u64, actual: Option<u64> },

    /// Deliberately has no embedded reason: the underlying failure comes from
    /// the caller-supplied wallet/signer, and an alloy signer error's
    /// `Display` is not something this module can prove never embeds
    /// sensitive state.
    #[error("local signing failed")]
    LocalSignFailed,

    /// Deliberately has no embedded reason: the underlying failure comes from
    /// the caller-supplied provider's transport, whose `Display` can
    /// legitimately include endpoint detail (redirect targets, connection
    /// diagnostics) this module must never format.
    #[error("broadcast (eth_sendRawTransaction) failed")]
    BroadcastFailed,

    /// Live `eth_chainId`/`eth_getBlockByNumber` read failed while
    /// establishing an authority. Same redaction rationale as
    /// [`Self::BroadcastFailed`]: no embedded transport error text.
    #[error("live provider identity read (chain id / genesis block) failed")]
    ProviderReadFailed,

    #[error("provider returned no genesis block (number 0) for chain id {0}")]
    GenesisBlockUnavailable(u64),

    /// `alloy`'s type-2 transaction builder error — reports which structural
    /// field (nonce, gas, etc.) is missing, never a credential.
    #[error("transaction is not a complete type-2 transaction: {0}")]
    IncompleteTransaction(String),

    /// `IntentError`'s `Display` never embeds env values or credentials, and
    /// it already derives `Clone + PartialEq + Eq` (unlike third-party
    /// builder errors elsewhere in this enum), so it's wrapped typed rather
    /// than stringified.
    #[error("recording the signed arb submission into the intent state machine failed: {0}")]
    SubmissionRecordingFailed(#[from] IntentError),

    /// The caller-supplied `DurableSubmissionHook`'s error is an opaque
    /// `eyre::Report` (no `Clone`/`PartialEq`, so it can't be wrapped typed
    /// like `IntentError` above); its `Display` is caller-controlled and not
    /// something this module can prove never embeds sensitive state.
    #[error("durable submission hook failed: {0}")]
    DurableHookFailed(String),
}
