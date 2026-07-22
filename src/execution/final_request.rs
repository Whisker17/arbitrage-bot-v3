//! Wallet-free final Execute request.

use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::TransactionRequest;

use super::fee_context::FeePlan;
use super::identity::ExecutionIdentity;
use super::intent::{CandidateRef, PreparedPayload};
use super::types::ExecutionParams;
use crate::state_space::SnapshotId;

#[derive(Clone, Debug)]
pub struct FinalRequestParams {
    pub params: ExecutionParams,
    pub candidate: CandidateRef,
    pub fee_plan: FeePlan,
    pub deadline: U256,
}

/// Opaque, single-owner request. Its wire fields and authorization cannot be
/// externally constructed or duplicated.
#[derive(Debug)]
pub struct FinalRequest {
    pub(crate) transaction: TransactionRequest,
    pub(crate) fee_plan: FeePlan,
    pub(crate) payload: PreparedPayload,
    pub(crate) calldata_digest: B256,
    pub(crate) nonce: u64,
    pub(crate) submitted_at: SnapshotId,
    pub(crate) from: Address,
    identity: ExecutionIdentity,
    min_profit: U256,
    deadline: U256,
}

impl FinalRequest {
    pub fn identity(&self) -> &ExecutionIdentity {
        &self.identity
    }

    pub fn from(&self) -> Address {
        self.from
    }

    pub fn min_profit(&self) -> U256 {
        self.min_profit
    }

    pub fn deadline(&self) -> U256 {
        self.deadline
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        transaction: TransactionRequest,
        fee_plan: FeePlan,
        payload: PreparedPayload,
        calldata_digest: B256,
        nonce: u64,
        submitted_at: SnapshotId,
        from: Address,
        identity: ExecutionIdentity,
        min_profit: U256,
        deadline: U256,
    ) -> Self {
        Self {
            transaction,
            fee_plan,
            payload,
            calldata_digest,
            nonce,
            submitted_at,
            from,
            identity,
            min_profit,
            deadline,
        }
    }
}
