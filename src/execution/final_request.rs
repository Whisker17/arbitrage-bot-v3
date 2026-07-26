//! Wallet-free final Execute request.

use alloy::consensus::SignableTransaction;
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::rpc::types::TransactionRequest;

use super::fee_context::FeePlan;
use super::identity::ExecutionIdentity;
use super::intent::{CandidateRef, PreparedPayload};
use super::types::ExecutionParams;
use crate::state_space::SnapshotId;

/// Domain-separated, sender-bound digest of a [`FinalRequest`]'s unsigned type-2
/// (EIP-1559) transaction payload.
///
/// Consumed by WHI-521, WHI-549, and E2E permit integration; must not be
/// reimplemented — call [`final_request_digest`] instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FinalRequestDigest(pub B256);

/// `keccak256(ASCII_DOMAIN_V1 || 0x00 || from_20_bytes || 0x02 || rlp([chain_id, nonce,
/// max_priority_fee_per_gas, max_fee_per_gas, gas_limit, to, value, data,
/// access_list]))`.
///
/// Reuses `alloy_consensus::TxEip1559::encode_for_signing`, which already produces
/// exactly `0x02 || rlp([...])` in the required field order, with an absent access list
/// defaulting to the canonical empty RLP list via
/// [`TransactionRequest::build_1559`](alloy::rpc::types::TransactionRequest::build_1559).
///
/// Returns an error rather than panicking if the request is not a fully-populated type-2
/// transaction — no construction path should produce one, but a digest helper on the
/// send path must not be able to abort the process.
pub fn final_request_digest(request: &FinalRequest) -> eyre::Result<FinalRequestDigest> {
    const ASCII_DOMAIN_V1: &[u8] = b"whisker-arb/final-request-digest/v1";

    let tx = request
        .transaction
        .clone()
        .build_1559()
        .map_err(|e| eyre::eyre!("FinalRequest is not a complete type-2 transaction: {e}"))?;

    let mut preimage = Vec::new();
    preimage.extend_from_slice(ASCII_DOMAIN_V1);
    preimage.push(0x00);
    preimage.extend_from_slice(request.from.as_slice());
    tx.encode_for_signing(&mut preimage);

    Ok(FinalRequestDigest(keccak256(&preimage)))
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::fee_context::BlockFeeContext;
    use crate::execution::gas_profile::{ProtocolKind, RouteKey};
    use alloy::network::TransactionBuilder;
    use alloy::primitives::keccak256;
    use crate::state_space::BlockHeaderContext;

    fn fixture_request() -> FinalRequest {
        let from = Address::repeat_byte(0x11);
        let to = Address::repeat_byte(0xAB);

        let transaction = TransactionRequest::default()
            .with_chain_id(5000)
            .with_nonce(7)
            .with_max_priority_fee_per_gas(1_500_000_000)
            .with_max_fee_per_gas(3_000_000_000)
            .with_gas_limit(500_000)
            .with_to(to)
            .with_value(U256::ZERO)
            .with_input(vec![0xDE, 0xAD, 0xBE, 0xEF]);

        let block_fee_context = BlockFeeContext {
            block_number: 1,
            block_hash: B256::ZERO,
            base_fee_per_gas: 1_000_000_000,
            block_gas_limit: 30_000_000,
        };
        let fee_plan = FeePlan {
            block_fee_context: block_fee_context.clone(),
            gas_limit: 500_000,
            expected_gas_used: 400_000,
            expected_gas_cost: U256::from(1_500_000_000_000_000u128),
            max_fee_per_gas: 3_000_000_000,
            max_priority_fee_per_gas: 1_500_000_000,
            profile_identity: "test-profile".to_string(),
        };
        let route_key = RouteKey::new(vec![ProtocolKind::V2]).expect("valid route key");
        let submitted_at = SnapshotId::new(5000, 1, B256::ZERO);
        let identity = ExecutionIdentity {
            snapshot_id: submitted_at,
            header: BlockHeaderContext::new(B256::ZERO, 0),
            pool_universe_fingerprint: B256::ZERO,
            route: route_key,
            fee_context: block_fee_context,
            gas_profile_identity: "test-profile".to_string(),
        };

        FinalRequest::new(
            transaction,
            fee_plan,
            PreparedPayload::Cancel {
                to: Address::ZERO,
                gas_limit: 21_000,
            },
            B256::ZERO,
            7,
            submitted_at,
            from,
            identity,
            U256::ZERO,
            U256::ZERO,
        )
    }

    #[test]
    fn final_request_digest_matches_hand_computed_golden_vector() {
        let request = fixture_request();

        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(b"whisker-arb/final-request-digest/v1");
        expected_preimage.push(0x00);
        expected_preimage.extend_from_slice(Address::repeat_byte(0x11).as_slice());

        // 0x02 || rlp([chain_id, nonce, max_priority_fee_per_gas, max_fee_per_gas,
        // gas_limit, to, value, input, access_list]), hand-encoded field by field.
        expected_preimage.push(0x02);
        expected_preimage.push(0xee); // list header: 0xc0 + 46 bytes of field payload
        expected_preimage.extend_from_slice(&[0x82, 0x13, 0x88]); // chain_id = 5000
        expected_preimage.push(0x07); // nonce = 7
        expected_preimage.extend_from_slice(&[0x84, 0x59, 0x68, 0x2F, 0x00]); // max_priority_fee_per_gas = 1_500_000_000
        expected_preimage.extend_from_slice(&[0x84, 0xB2, 0xD0, 0x5E, 0x00]); // max_fee_per_gas = 3_000_000_000
        expected_preimage.extend_from_slice(&[0x83, 0x07, 0xA1, 0x20]); // gas_limit = 500_000
        expected_preimage.push(0x94); // to: 20-byte string header
        expected_preimage.extend_from_slice(&[0xAB; 20]);
        expected_preimage.push(0x80); // value = 0
        expected_preimage.extend_from_slice(&[0x84, 0xDE, 0xAD, 0xBE, 0xEF]); // input
        expected_preimage.push(0xc0); // access_list = empty list

        let expected_digest = FinalRequestDigest(keccak256(&expected_preimage));

        assert_eq!(
            final_request_digest(&request).expect("fixture request is a complete type-2 tx"),
            expected_digest
        );
    }
}
