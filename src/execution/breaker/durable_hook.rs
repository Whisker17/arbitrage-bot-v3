//! DurableSubmissionHook backed by DurableIntentCoordinator (WHI-524).

use std::sync::Arc;

use alloy::primitives::{keccak256, U256};
use eyre::{eyre, Result};

use super::coordinator::DurableIntentCoordinator;
use crate::execution::intent::SignedSubmission;
use crate::execution::pipeline::DurableSubmissionHook;
use crate::execution::pause::AttemptKind;

/// Appends `SubmissionPrepared` before SM `record_submission` / RPC.
pub struct WalDurableHook {
    coordinator: Arc<DurableIntentCoordinator>,
}

impl WalDurableHook {
    pub fn new(coordinator: Arc<DurableIntentCoordinator>) -> Self {
        Self { coordinator }
    }
}

impl DurableSubmissionHook for WalDurableHook {
    fn on_signed(&self, signed: &SignedSubmission, min_profit: U256, deadline: U256) -> Result<()> {
        let kind = if matches!(signed.payload, crate::execution::intent::PreparedPayload::Cancel { .. })
        {
            AttemptKind::Cancel as u8
        } else {
            AttemptKind::Execute as u8
        };
        let identity = keccak256(
            [
                signed.calldata_digest.as_slice(),
                &signed.nonce.to_be_bytes(),
            ]
            .concat(),
        );
        self.coordinator
            .append_submission_prepared(
                signed.nonce,
                signed.tx_hash,
                signed.raw.to_vec(),
                kind,
                signed.fee_plan.max_fee_per_gas,
                signed.fee_plan.max_priority_fee_per_gas,
                signed.fee_plan.gas_limit,
                deadline,
                min_profit,
                signed.calldata_digest,
                identity,
            )
            .map_err(|e| eyre!("submission journal append failed: {e}"))
    }
}
