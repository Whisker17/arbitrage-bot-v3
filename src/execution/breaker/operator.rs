//! Signed operator commands for init / unpause / recovery (WHI-524).
//!
//! The runtime never holds the operator private key. `pause_control` signs
//! offline and delivers commands through a local channel; `actor` is derived
//! from the recovered address.

use alloy::primitives::{keccak256, Address, B256, Signature};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum ControlKind {
    Init = 1,
    Unpause = 2,
    Recovery = 3,
}

impl ControlKind {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Init),
            2 => Some(Self::Unpause),
            3 => Some(Self::Recovery),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlCommand {
    pub control_seq: u64,
    pub kind: ControlKind,
    pub chain_id: u64,
    pub executor: Address,
    pub signer: Address,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedOperatorCommand {
    pub command: ControlCommand,
    /// Compact 65-byte secp256k1 signature (r||s||y_parity).
    pub signature: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OperatorError {
    #[error("invalid signature")]
    BadSignature,
    #[error("unauthorized operator {0}")]
    Unauthorized(Address),
    #[error("control_seq regression")]
    SeqRegression,
    #[error("init rejected: scope already initialized")]
    InitNotVirgin,
    #[error("unpause rejected: not initialized")]
    UnpauseBeforeInit,
    #[error("recovery required for initialized missing state")]
    RecoveryRequired,
}

pub trait OperatorVerifier: Send + Sync {
    fn verify(&self, signed: &SignedOperatorCommand) -> Result<Address, OperatorError>;
}

#[derive(Debug, Clone)]
pub struct StaticOperatorVerifier {
    pub allowed: Address,
}

impl OperatorVerifier for StaticOperatorVerifier {
    fn verify(&self, signed: &SignedOperatorCommand) -> Result<Address, OperatorError> {
        let digest = command_digest(&signed.command);
        let sig = parse_signature(&signed.signature)?;
        let actor = sig
            .recover_address_from_prehash(&digest)
            .map_err(|_| OperatorError::BadSignature)?;
        if actor != self.allowed {
            return Err(OperatorError::Unauthorized(actor));
        }
        Ok(actor)
    }
}

pub fn command_digest(cmd: &ControlCommand) -> B256 {
    let mut buf = Vec::new();
    buf.extend_from_slice(b"AMMS_BREAKER_CTRL_V1");
    buf.extend_from_slice(&cmd.control_seq.to_be_bytes());
    buf.push(cmd.kind as u8);
    buf.extend_from_slice(&cmd.chain_id.to_be_bytes());
    buf.extend_from_slice(cmd.executor.as_slice());
    buf.extend_from_slice(cmd.signer.as_slice());
    buf.extend_from_slice(cmd.detail.as_bytes());
    keccak256(buf)
}

pub fn sign_command(
    signer: &PrivateKeySigner,
    command: ControlCommand,
) -> Result<SignedOperatorCommand, OperatorError> {
    let digest = command_digest(&command);
    let sig = signer
        .sign_hash_sync(&digest)
        .map_err(|_| OperatorError::BadSignature)?;
    Ok(SignedOperatorCommand {
        command,
        signature: sig.as_bytes().to_vec(),
    })
}

fn parse_signature(bytes: &[u8]) -> Result<Signature, OperatorError> {
    Signature::try_from(bytes).map_err(|_| OperatorError::BadSignature)
}

/// Alias kept for call-sites that speak of OperatorCommand.
pub type OperatorCommand = SignedOperatorCommand;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::signers::local::PrivateKeySigner;

    #[test]
    fn signed_command_recovers_actor() {
        let sk = PrivateKeySigner::random();
        let cmd = ControlCommand {
            control_seq: 1,
            kind: ControlKind::Init,
            chain_id: 5003,
            executor: Address::repeat_byte(1),
            signer: Address::repeat_byte(2),
            detail: "virgin".into(),
        };
        let signed = sign_command(&sk, cmd).unwrap();
        let verifier = StaticOperatorVerifier {
            allowed: sk.address(),
        };
        let actor = verifier.verify(&signed).unwrap();
        assert_eq!(actor, sk.address());
    }

    #[test]
    fn unauthorized_operator_rejected() {
        let sk = PrivateKeySigner::random();
        let other = PrivateKeySigner::random();
        let cmd = ControlCommand {
            control_seq: 1,
            kind: ControlKind::Unpause,
            chain_id: 1,
            executor: Address::ZERO,
            signer: Address::ZERO,
            detail: String::new(),
        };
        let signed = sign_command(&sk, cmd).unwrap();
        let verifier = StaticOperatorVerifier {
            allowed: other.address(),
        };
        assert!(matches!(
            verifier.verify(&signed),
            Err(OperatorError::Unauthorized(_))
        ));
    }
}
