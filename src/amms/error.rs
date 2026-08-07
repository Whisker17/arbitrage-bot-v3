use super::{
    agni::AgniError, moe::MoeError, uniswap_v2::UniswapV2Error, uniswap_v3::UniswapV3Error,
};
use alloy::{primitives::FixedBytes, transports::TransportErrorKind};
use rug::float::ParseFloatError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AMMError {
    #[error(transparent)]
    TransportError(#[from] alloy::transports::RpcError<TransportErrorKind>),
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    #[error(transparent)]
    ABIError(#[from] alloy::dyn_abi::Error),
    #[error(transparent)]
    SolTypesError(#[from] alloy::sol_types::Error),
    #[error(transparent)]
    UniswapV2Error(#[from] UniswapV2Error),
    #[error(transparent)]
    UniswapV3Error(#[from] UniswapV3Error),
    #[error(transparent)]
    AgniError(#[from] AgniError),
    #[error(transparent)]
    MoeError(#[from] MoeError),
    #[error("Incomplete AMM state")]
    IncompleteState,
    #[error(transparent)]
    BatchContractError(#[from] BatchContractError),
    #[error("Unrecognized Event Signature {0}")]
    UnrecognizedEventSignature(FixedBytes<32>),
    #[error(transparent)]
    JoinError(#[from] tokio::task::JoinError),
    #[error(transparent)]
    ParseFloatError(#[from] ParseFloatError),
}

#[derive(Error, Debug)]
pub enum BatchContractError {
    #[error(transparent)]
    ContractError(#[from] alloy::contract::Error),
    #[error(transparent)]
    DynABIError(#[from] alloy::dyn_abi::Error),
    /// A single-pool batch CREATE still hit the EIP-170 code-size limit
    /// (WHI-925). Names the pool so the operator can quarantine it rather
    /// than seeing a bare EVM error.
    #[error(
        "CREATE size limit on single pool path={path} pool={pool:?}: {message}"
    )]
    CreateSizeSinglePool {
        path: &'static str,
        pool: Option<alloy::primitives::Address>,
        message: String,
    },
}
