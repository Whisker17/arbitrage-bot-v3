use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum MoeLbtMathError {
    #[error("zero value not allowed")]
    ZeroValue,
    #[error("division by zero")]
    DivisionByZero,
    #[error("multiplication results overflowed")]
    Overflow,
    #[error("result underflowed")]
    Underflow,
    #[error("logarithm underflow")]
    LogUnderflow,
    #[error("power underflow")]
    PowUnderflow,
    #[error("value exceeds {0} bits")]
    ValueExceedsBits(u32),
    #[error("multiplier too large")]
    MultiplierTooLarge,
    #[error("fee too large")]
    FeeTooLarge,
    #[error("protocol share too large")]
    ProtocolShareTooLarge,
    #[error("max liquidity per bin exceeded")]
    MaxLiquidityPerBinExceeded,
    #[error("liquidity overflow")]
    LiquidityOverflow,
    #[error("invalid configuration")]
    InvalidConfig,
    #[error("invalid parameter")]
    InvalidParameter,
}

