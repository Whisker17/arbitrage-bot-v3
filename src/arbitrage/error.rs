use alloy::transports::TransportErrorKind;
use thiserror::Error;

use crate::amms::error::AMMError;
use crate::state_space::error::StateSpaceError;

#[derive(Debug, Error)]
pub enum ArbitrageError {
    #[error(transparent)]
    StateSpace(#[from] StateSpaceError),
    #[error(transparent)]
    Amm(#[from] AMMError),
    #[error(transparent)]
    Rpc(#[from] alloy::transports::RpcError<TransportErrorKind>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Csv(#[from] csv::Error),
    #[error("Token decimals missing for {0:?}")]
    MissingTokenDecimals(alloy::primitives::Address),
    #[error("Graph construction failed: {0}")]
    Graph(String),
    #[error("Path simulation failed: {0}")]
    Simulation(String),
    #[error("Optimization failed: {0}")]
    Optimization(String),
}
