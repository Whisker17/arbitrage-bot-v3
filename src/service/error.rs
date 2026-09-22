//! Typed errors for the service scaffolding module (WHI-727).
//!
//! Follows the repo convention that each library module owns a `thiserror`
//! error surface (see Agents.md).

use crate::state_space::PoolUniverseError;
use alloy::primitives::Address;
use thiserror::Error;

/// Errors raised by [`super::protocol::Protocol`] methods and service startup checks.
#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("protocol simulation: {0}")]
    Simulation(String),
    /// Transiently-incomplete AMM state (WHI-1409): the underlying
    /// [`crate::amms::error::AMMError::IncompleteState`] /
    /// `AMMError::MoeError(MoeError::IncompleteState)` classification,
    /// preserved through [`super::protocol::Protocol::simulate_path_with_route_key`]
    /// so callers can soft-skip (this candidate is unquotable right now) instead
    /// of treating it the same as a genuine simulation bug ([`Self::Simulation`]).
    #[error("protocol simulation: incomplete AMM state: {0}")]
    IncompleteState(String),
    #[error("protocol build: {0}")]
    Build(String),
    #[error("protocol tip refresh: {0}")]
    TipRefresh(String),
    #[error("protocol execution: {0}")]
    Execution(String),
    #[error("route key: {0}")]
    RouteKey(String),
    /// Settlement asset is not equal to the wrapped native gas asset (config-only).
    #[error(
        "settlement asset {configured} != gas asset {gas_asset}. On Mantle, settlement must \
         equal WMNT so profit and gas share units. Non-WMNT settlement requires a generalized \
         ArbitrageExecutor and a native-gas→settlement conversion (neither exists); fail closed \
         (WHI-529)"
    )]
    SettlementAssetGasMismatch {
        configured: Address,
        gas_asset: Address,
    },
    /// Settlement asset is not equal to the gas asset and/or executor WMNT.
    ///
    /// A non-WMNT settlement asset requires a generalized executor **and** a
    /// reliable native-gas → settlement conversion; neither exists on this deployment.
    #[error(
        "settlement asset mismatch: configured={configured}, executor_wmnt={executor_wmnt}, \
         gas_asset={gas_asset}. Non-WMNT settlement requires a generalized ArbitrageExecutor \
         and a native-gas→settlement conversion (neither exists); fail closed (WHI-529)"
    )]
    SettlementAssetMismatch {
        configured: Address,
        executor_wmnt: Address,
        gas_asset: Address,
    },
    #[error("settlement asset must not be the zero address")]
    SettlementAssetZero,
    #[error("settlement asset validation RPC failed: {0}")]
    SettlementAssetRpc(String),
}

/// Errors from a [`super::pool_universe::PoolUniverseSource`].
#[derive(Debug, Error)]
pub enum PoolUniverseSourceError {
    #[error("pool universe I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("pool universe CSV: {0}")]
    Csv(#[from] csv::Error),
    #[error("pool universe fingerprint: {0}")]
    Fingerprint(#[from] PoolUniverseError),
    /// Selected protocol's pool-list file is missing. Fail closed — never discover.
    #[error(
        "pool universe missing for {protocol} at {path}. \
         The live binary never discovers pools; regenerate offline with: {regenerate}"
    )]
    Missing {
        protocol: String,
        path: String,
        regenerate: String,
    },
    /// Companion `.meta.json` required for provenance / staleness checks is missing.
    #[error(
        "pool universe metadata missing for {protocol} at {path}. \
         The live binary never discovers pools; regenerate offline with: {regenerate}"
    )]
    MetaMissing {
        protocol: String,
        path: String,
        regenerate: String,
    },
    /// Pool list loaded zero rows for a selected protocol.
    #[error(
        "pool universe empty for {protocol} at {path}. \
         The live binary never discovers pools; regenerate offline with: {regenerate}"
    )]
    Empty {
        protocol: String,
        path: String,
        regenerate: String,
    },
    /// `meta.json` `snapshot_block` is too far behind the chain tip.
    #[error(
        "pool universe for {protocol} is stale: snapshot_block={snapshot_block}, \
         tip={tip_block}, age={age_blocks} blocks exceeds max_age={max_age_blocks}. \
         The live binary never discovers pools; regenerate offline with: {regenerate}"
    )]
    Stale {
        protocol: String,
        snapshot_block: u64,
        tip_block: u64,
        age_blocks: u64,
        max_age_blocks: u64,
        regenerate: String,
    },
    #[error("pool universe: {0}")]
    Other(String),
}
