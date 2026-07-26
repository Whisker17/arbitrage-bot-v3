//! Domain-separated typed transaction digests for E2E bootstrap/trigger/cancel
//! actions (WHI-555).
//!
//! Mirrors [`crate::execution::final_request_digest`]'s scheme
//! (`keccak256(domain || 0x00 || from || 0x02 || rlp(unsigned type-2 tx
//! fields))`) with a distinct ASCII domain tag per action so that no two
//! actions — or the `arb` action, which uses
//! [`crate::execution::FinalRequestDigest`] directly and never this module —
//! can ever collide on digest bytes even given identical underlying
//! transaction fields. `arb` deliberately does not go through this file: its
//! digest must come only from a consumed `PreparedPipelineHead`, never be
//! recomputed here, per WHI-555's acceptance criteria.

use alloy::consensus::SignableTransaction;
use alloy::primitives::{keccak256, Address, B256};
use alloy::rpc::types::TransactionRequest;

use super::error::E2eCapabilityError;

const TRIGGER_DOMAIN_V1: &[u8] = b"whisker-arb/e2e-trigger-tx-digest/v1";
const CANCEL_DOMAIN_V1: &[u8] = b"whisker-arb/e2e-cancel-tx-digest/v1";
const BOOTSTRAP_DOMAIN_V1: &[u8] = b"whisker-arb/e2e-bootstrap-tx-digest/v1";

/// Domain-separated digest of an unsigned E2E "trigger" transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TriggerRequestDigest(pub B256);

/// Domain-separated digest of an unsigned E2E "cancel" transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CancelRequestDigest(pub B256);

/// Domain-separated digest of an unsigned E2E bootstrap transaction
/// (`deploy` | `config` | `initial-seed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BootstrapRequestDigest(pub B256);

/// The three bootstrap-only actions (WHI-555 step 3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BootstrapAction {
    Deploy,
    Config,
    InitialSeed,
}

impl BootstrapAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Deploy => "deploy",
            Self::Config => "config",
            Self::InitialSeed => "initial-seed",
        }
    }

    fn tag_byte(self) -> u8 {
        match self {
            Self::Deploy => 1,
            Self::Config => 2,
            Self::InitialSeed => 3,
        }
    }
}

/// The three post-manifest sign actions (WHI-555 step 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum E2eSignAction {
    Arb,
    Trigger,
    Cancel,
}

impl E2eSignAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arb => "arb",
            Self::Trigger => "trigger",
            Self::Cancel => "cancel",
        }
    }
}

fn domain_separated_tx_digest(
    domain: &'static [u8],
    tag_byte: Option<u8>,
    from: Address,
    tx: &TransactionRequest,
) -> Result<B256, E2eCapabilityError> {
    let built = tx
        .clone()
        .build_1559()
        .map_err(|e| E2eCapabilityError::IncompleteTransaction(e.to_string()))?;
    let mut preimage = Vec::new();
    preimage.extend_from_slice(domain);
    preimage.push(0x00);
    if let Some(tag) = tag_byte {
        preimage.push(tag);
    }
    preimage.extend_from_slice(from.as_slice());
    built.encode_for_signing(&mut preimage);
    Ok(keccak256(&preimage))
}

pub fn trigger_request_digest(
    tx: &TransactionRequest,
    from: Address,
) -> Result<TriggerRequestDigest, E2eCapabilityError> {
    domain_separated_tx_digest(TRIGGER_DOMAIN_V1, None, from, tx).map(TriggerRequestDigest)
}

pub fn cancel_request_digest(
    tx: &TransactionRequest,
    from: Address,
) -> Result<CancelRequestDigest, E2eCapabilityError> {
    domain_separated_tx_digest(CANCEL_DOMAIN_V1, None, from, tx).map(CancelRequestDigest)
}

pub fn bootstrap_request_digest(
    action: BootstrapAction,
    tx: &TransactionRequest,
    from: Address,
) -> Result<BootstrapRequestDigest, E2eCapabilityError> {
    domain_separated_tx_digest(BOOTSTRAP_DOMAIN_V1, Some(action.tag_byte()), from, tx)
        .map(BootstrapRequestDigest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::network::TransactionBuilder;
    use alloy::primitives::U256;

    fn fixture_tx() -> TransactionRequest {
        TransactionRequest::default()
            .with_chain_id(5003)
            .with_nonce(3)
            .with_max_priority_fee_per_gas(1)
            .with_max_fee_per_gas(2)
            .with_gas_limit(21_000)
            .with_to(Address::repeat_byte(0x22))
            .with_value(U256::ZERO)
            .with_input(vec![0x01, 0x02])
    }

    #[test]
    fn trigger_and_cancel_digests_differ_for_identical_tx_bytes() {
        let tx = fixture_tx();
        let from = Address::repeat_byte(0x11);
        let trigger = trigger_request_digest(&tx, from).unwrap();
        let cancel = cancel_request_digest(&tx, from).unwrap();
        assert_ne!(
            trigger.0, cancel.0,
            "distinct domain tags must prevent cross-action digest collisions"
        );
    }

    #[test]
    fn bootstrap_actions_have_distinct_digests_for_identical_tx_bytes() {
        let tx = fixture_tx();
        let from = Address::repeat_byte(0x11);
        let deploy = bootstrap_request_digest(BootstrapAction::Deploy, &tx, from).unwrap();
        let config = bootstrap_request_digest(BootstrapAction::Config, &tx, from).unwrap();
        let seed = bootstrap_request_digest(BootstrapAction::InitialSeed, &tx, from).unwrap();
        assert_ne!(deploy.0, config.0);
        assert_ne!(config.0, seed.0);
        assert_ne!(deploy.0, seed.0);
    }

    #[test]
    fn bootstrap_digest_never_collides_with_trigger_or_cancel_domains() {
        let tx = fixture_tx();
        let from = Address::repeat_byte(0x11);
        let deploy = bootstrap_request_digest(BootstrapAction::Deploy, &tx, from).unwrap();
        let trigger = trigger_request_digest(&tx, from).unwrap();
        let cancel = cancel_request_digest(&tx, from).unwrap();
        assert_ne!(deploy.0, trigger.0);
        assert_ne!(deploy.0, cancel.0);
    }

    #[test]
    fn digest_changes_if_from_changes() {
        let tx = fixture_tx();
        let a = trigger_request_digest(&tx, Address::repeat_byte(0x11)).unwrap();
        let b = trigger_request_digest(&tx, Address::repeat_byte(0x22)).unwrap();
        assert_ne!(a.0, b.0);
    }

    /// Golden-vector check, mirroring `final_request.rs`'s own preimage test:
    /// independently assembles `domain || 0x00 || from || rlp` (trusting only
    /// alloy's own `TxEip1559::encode_for_signing`, exactly as
    /// `final_request_digest` does and as its test already golden-checks —
    /// this test exists to prove *this file's* domain-tag/from wrapping is
    /// assembled in the documented order, not to re-verify RLP correctness).
    /// A preimage-order regression (e.g. swapping `tag_byte`/`from`, or
    /// dropping the `0x00` separator) would silently pass every other test in
    /// this file, since they only ever compare digests to each other.
    #[test]
    fn trigger_digest_matches_independently_assembled_preimage() {
        let tx = fixture_tx();
        let from = Address::repeat_byte(0x11);
        let digest = trigger_request_digest(&tx, from).unwrap();

        let built = tx.clone().build_1559().unwrap();
        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(b"whisker-arb/e2e-trigger-tx-digest/v1");
        expected_preimage.push(0x00);
        expected_preimage.extend_from_slice(from.as_slice());
        built.encode_for_signing(&mut expected_preimage);

        assert_eq!(digest.0, keccak256(&expected_preimage));
    }

    /// Same shape, but for a bootstrap action — proves the additional
    /// `tag_byte` slots in *after* the `0x00` separator and *before* `from`,
    /// per `domain_separated_tx_digest`'s documented preimage order.
    #[test]
    fn bootstrap_digest_matches_independently_assembled_preimage() {
        let tx = fixture_tx();
        let from = Address::repeat_byte(0x11);
        let digest = bootstrap_request_digest(BootstrapAction::Config, &tx, from).unwrap();

        let built = tx.clone().build_1559().unwrap();
        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(b"whisker-arb/e2e-bootstrap-tx-digest/v1");
        expected_preimage.push(0x00);
        expected_preimage.push(2); // BootstrapAction::Config's tag byte
        expected_preimage.extend_from_slice(from.as_slice());
        built.encode_for_signing(&mut expected_preimage);

        assert_eq!(digest.0, keccak256(&expected_preimage));
    }
}
