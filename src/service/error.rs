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
    #[error("protocol build: {0}")]
    Build(String),
    #[error("protocol tip refresh: {0}")]
    TipRefresh(String),
    #[error("protocol execution: {0}")]
    Execution(String),
    #[error("route key: {0}")]
    RouteKey(String),
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
    #[error("pool universe: {0}")]
    Other(String),
}
