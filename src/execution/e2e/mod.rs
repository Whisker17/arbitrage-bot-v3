//! Typed E2E transaction capabilities for Mantle Sepolia (WHI-555).
//!
//! A credential-isolated, one-shot-capability-gated send path used only by
//! the Mantle Sepolia E2E harness (WHI-525). It cannot enable or weaken
//! production sending: [`crate::execution::pipeline`]'s guarded Execute tail
//! and [`super::intent::IntentStateMachine`]'s `production_send_allowed`
//! remain completely untouched by this module, and every constructor here is
//! private to this module tree — external callers can only mint and consume
//! permits, never fabricate an authority, a permit, or reach the signer.
//!
//! Lifecycle:
//! 1. [`env_guard::validate_e2e_startup`] — namespace-scoped env read,
//!    forbidden-legacy-var presence check, denylist check.
//! 2. [`provider_identity::validate_provider_identity`] — live chain id +
//!    genesis hash, process-local random session nonce ->
//!    [`ProviderIdentityDigest`].
//! 3. [`E2eBootstrapAuthority::establish`] — fetches live chain id + genesis
//!    hash from the given provider itself, tying the identity to that exact
//!    instance — -> mint/send [`BootstrapActionPermit`]s
//!    (`deploy | config | initial-seed`) -> [`E2eBootstrapAuthority::finalize`]
//!    -> [`VerifiedE2eManifest`].
//! 4. [`VerifiedE2eManifest`] mints [`E2eSignPermit`]s (`arb | trigger |
//!    cancel`), signs them into a [`BroadcastableE2eSubmission`], and
//!    broadcasts.
//!
//! The only `tracing::*` calls in this module tree are diagnostic-only
//! `Drop` impls on [`E2eSignPermit`] and
//! [`crate::execution::pipeline::PreparedPipelineHead`] (leaked-permit /
//! leaked-head warnings): they log a nonce and typed `IntentError` debug
//! output, never env values. The private key and RPC URL themselves are
//! validated for shape/parseability and then immediately dropped (see
//! [`env_guard::validate_e2e_startup`]'s doc comment), so no instrumentation
//! point anywhere in this module tree can ever format them into a trace.

mod capability;
mod digest;
mod env_guard;
mod error;
mod provider_identity;

pub use capability::{
    BootstrapActionPermit, BroadcastableE2eSubmission, E2eBootstrapAuthority, E2eSignPermit,
    ExecuteSubmissionMetaView, SignedSubmissionView, SubmissionAction, VerifiedE2eManifest,
};
pub use digest::{
    BootstrapAction, BootstrapRequestDigest, CancelRequestDigest, E2eSignAction,
    TriggerRequestDigest,
};
pub use env_guard::validate_e2e_startup;
#[cfg(feature = "e2e-test-util")]
pub use env_guard::{validate_e2e_startup_with_denylist, MapEnvSource};
pub use env_guard::{
    EnvSource, ProcessEnvSource, ValidatedE2eStartup, ENV_E2E_EXECUTOR_ADDRESS,
    ENV_E2E_PRIVATE_KEY, ENV_E2E_RPC_URL, FORBIDDEN_ENV_VAR_NAMES, PRODUCTION_SIGNER_DENYLIST,
};
pub use error::E2eCapabilityError;
pub use provider_identity::{
    validate_provider_identity, ProviderIdentityDigest, ValidatedE2eProvider,
    MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH,
};
