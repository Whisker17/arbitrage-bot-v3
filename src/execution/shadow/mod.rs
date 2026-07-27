//! WHI-549: signerless shadow runtime — a real `eth_call` + `StateOverride`
//! semantic preflight (reusing WHI-521's `preflight::SemanticCallExecutor` seam)
//! that records every candidate to an append-only ledger, without ever
//! constructing a wallet or signing/broadcasting a transaction.
//!
//! Built entirely on top of the existing wallet-free `Executor` /
//! `ExecutionContext` (see `executor.rs`, `types.rs`) and the closed-send
//! pipeline head (`pipeline::run_pipeline_head_closed`) — this module supplies
//! only the pieces those seams were deliberately left open for: storage-layout-
//! driven state overrides, CREATE2/Moe-allowlist pool provenance, and the ledger.

mod approved_pools;
mod call_executor;
mod context;
mod create2;
mod digest;
mod env_guard;
mod identity_source;
mod invariant;
mod ledger;
mod manifest;
mod moe_allowlist;
mod overrides;
mod slots;
mod thresholds;
mod wmnt_descriptor;

pub use approved_pools::{
    approved_entry_for, load_approved_pools, ApprovedPoolEntry, ApprovedPoolProtocol,
    ApprovedPoolsConfig, ApprovedPoolsError,
};
pub use call_executor::ShadowSemanticCallExecutor;
pub use context::{NoSend, ShadowConfigPaths, ShadowContextError, ShadowExecutionContext};
pub use create2::expected_pool_address;
pub use env_guard::{guard_shadow_env, ShadowEnvGuardError, ENV_SHADOW_MODE};
pub use identity_source::ShadowIdentitySource;
pub use invariant::ShadowInvariantSink;
pub use ledger::{LedgerError, ProfitBasis, ShadowLedgerWriter};
pub use manifest::{Create2Proof, ManifestError, PoolProvenanceOutcome, ShadowOverrideManifest};
pub use moe_allowlist::{
    is_allowlisted, load_moe_allowlist, MoeAllowlist, MoeAllowlistEntry, MoeAllowlistError,
};
pub use overrides::{ShadowOverrideInputs, ShadowPoolOverrideInputs};
pub use thresholds::{load_threshold_bytes, ThresholdError};
pub use wmnt_descriptor::{
    load_wmnt_descriptor, WmntDescriptor, WmntDescriptorError, WmntStorageShape,
};
