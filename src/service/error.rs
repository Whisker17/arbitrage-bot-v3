//! Typed errors for the service scaffolding module (WHI-727).
//!
//! Follows the repo convention that each library module owns a `thiserror`
//! error surface (see Agents.md).

use crate::state_space::PoolUniverseError;
use thiserror::Error;

/// Errors raised by [`super::protocol::Protocol`] methods.
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
