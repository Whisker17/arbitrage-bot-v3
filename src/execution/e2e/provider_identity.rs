//! Process-local provider-identity binding for the E2E capability layer (WHI-555).
//!
//! Every validated provider instance gets a fresh, cryptographically random
//! 32-byte session nonce and a derived [`ProviderIdentityDigest`]. Neither is
//! cached globally or persisted: reconstructing the provider, substituting a
//! different one, or restarting the process all produce a different digest,
//! which invalidates every capability bound to the old one (permits carry the
//! digest they were minted under and are checked against the live digest at
//! sign time).

use alloy::primitives::{keccak256, B256};
use rand::RngCore;

use super::error::E2eCapabilityError;

/// The only Mantle Sepolia chain id the E2E send path accepts.
pub const MANTLE_SEPOLIA_CHAIN_ID: u64 = 5003;

/// Mantle mainnet chain id. Explicitly rejected regardless of any other check
/// passing, so a misconfigured RPC endpoint can never be treated as Sepolia.
pub const MANTLE_MAINNET_CHAIN_ID_REJECTED: u64 = 5000;

/// Genesis block hash of Mantle Sepolia (chain id 5003), pinned from a live
/// `eth_getBlockByNumber("0x0", false)` call against the public Sepolia RPC.
/// Guards against an RPC endpoint that reports chain id 5003 while actually
/// serving a different (e.g. forked or forged) chain.
pub const MANTLE_SEPOLIA_GENESIS_HASH: B256 = B256::new(hex_literal_genesis());

const fn hex_literal_genesis() -> [u8; 32] {
    // 0x5144ce54f2452b16a2ad4a5817e662f3a1599fdd790abde631e693114610d81f,
    // captured via `eth_getBlockByNumber("0x0", false)` against the public
    // Mantle Sepolia RPC (chain id confirmed 0x138b == 5003 in the same call).
    [
        0x51, 0x44, 0xce, 0x54, 0xf2, 0x45, 0x2b, 0x16, 0xa2, 0xad, 0x4a, 0x58, 0x17, 0xe6, 0x62,
        0xf3, 0xa1, 0x59, 0x9f, 0xdd, 0x79, 0x0a, 0xbd, 0xe6, 0x31, 0xe6, 0x93, 0x11, 0x46, 0x10,
        0xd8, 0x1f,
    ]
}

const PROVIDER_IDENTITY_DOMAIN_V1: &[u8] = b"whisker-arb/e2e-provider-session/v1";

/// Domain-separated digest binding one validated provider instance to a
/// fresh, process-local random session nonce, the validated chain id, and the
/// validated genesis hash.
///
/// `keccak256(ASCII("whisker-arb/e2e-provider-session/v1") || 0x00 ||
/// session_nonce || chain_id_u64_be || genesis_hash)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProviderIdentityDigest(pub B256);

/// A provider instance that has passed live chain-id and genesis-hash
/// validation, with its process-local session nonce and derived
/// [`ProviderIdentityDigest`]. Not persisted, not `Default`-constructible:
/// the only way to get one is [`validate_provider_identity`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidatedE2eProvider {
    chain_id: u64,
    genesis_hash: B256,
    session_nonce: [u8; 32],
    digest: ProviderIdentityDigest,
}

impl ValidatedE2eProvider {
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }

    pub fn genesis_hash(&self) -> B256 {
        self.genesis_hash
    }

    pub fn digest(&self) -> ProviderIdentityDigest {
        self.digest
    }
}

/// Chain-id-only half of [`validate_provider_identity`], split out so a
/// caller (namely [`super::capability::E2eBootstrapAuthority::establish`])
/// can fail fast on an already-known-wrong chain id without also needing a
/// genesis hash in hand yet — without duplicating the mainnet-reject/wrong-
/// chain policy itself.
///
/// Chain id 5000 (mainnet) is rejected explicitly and takes priority over the
/// generic mismatch error, so a caller can distinguish "this is mainnet" from
/// "this is some other, unrecognized chain."
pub(super) fn reject_invalid_chain_id(chain_id: u64) -> Result<(), E2eCapabilityError> {
    if chain_id == MANTLE_MAINNET_CHAIN_ID_REJECTED {
        return Err(E2eCapabilityError::MainnetChainIdRejected);
    }
    if chain_id != MANTLE_SEPOLIA_CHAIN_ID {
        return Err(E2eCapabilityError::WrongChainId {
            expected: MANTLE_SEPOLIA_CHAIN_ID,
            observed: chain_id,
        });
    }
    Ok(())
}

/// Validate a live `(chain_id, genesis_hash)` pair against the Mantle Sepolia
/// constants, then mint a fresh random session nonce and derive the digest.
pub fn validate_provider_identity(
    chain_id: u64,
    genesis_hash: B256,
) -> Result<ValidatedE2eProvider, E2eCapabilityError> {
    reject_invalid_chain_id(chain_id)?;
    if genesis_hash != MANTLE_SEPOLIA_GENESIS_HASH {
        return Err(E2eCapabilityError::WrongGenesisHash);
    }

    let mut session_nonce = [0u8; 32];
    rand::rng().fill_bytes(&mut session_nonce);
    let digest = provider_identity_digest(&session_nonce, chain_id, genesis_hash);

    Ok(ValidatedE2eProvider {
        chain_id,
        genesis_hash,
        session_nonce,
        digest,
    })
}

fn provider_identity_digest(
    session_nonce: &[u8; 32],
    chain_id: u64,
    genesis_hash: B256,
) -> ProviderIdentityDigest {
    let mut preimage = Vec::with_capacity(PROVIDER_IDENTITY_DOMAIN_V1.len() + 1 + 32 + 8 + 32);
    preimage.extend_from_slice(PROVIDER_IDENTITY_DOMAIN_V1);
    preimage.push(0x00);
    preimage.extend_from_slice(session_nonce);
    preimage.extend_from_slice(&chain_id.to_be_bytes());
    preimage.extend_from_slice(genesis_hash.as_slice());
    ProviderIdentityDigest(keccak256(&preimage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_sepolia_chain_id_and_committed_genesis_hash() {
        let validated =
            validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
                .expect("sepolia chain id + committed genesis hash must validate");
        assert_eq!(validated.chain_id(), MANTLE_SEPOLIA_CHAIN_ID);
        assert_eq!(validated.genesis_hash(), MANTLE_SEPOLIA_GENESIS_HASH);
    }

    #[test]
    fn rejects_mainnet_chain_id_explicitly() {
        let err = validate_provider_identity(MANTLE_MAINNET_CHAIN_ID_REJECTED, B256::ZERO)
            .expect_err("chain 5000 must never validate");
        assert_eq!(err, E2eCapabilityError::MainnetChainIdRejected);
    }

    #[test]
    fn rejects_unrecognized_chain_id() {
        let err = validate_provider_identity(1, MANTLE_SEPOLIA_GENESIS_HASH)
            .expect_err("chain 1 must never validate as sepolia");
        assert_eq!(
            err,
            E2eCapabilityError::WrongChainId {
                expected: MANTLE_SEPOLIA_CHAIN_ID,
                observed: 1,
            }
        );
    }

    #[test]
    fn rejects_wrong_genesis_hash_even_with_correct_chain_id() {
        let err = validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, B256::repeat_byte(0xAB))
            .expect_err("mismatched genesis hash must fail closed");
        assert_eq!(err, E2eCapabilityError::WrongGenesisHash);
    }

    #[test]
    fn two_validated_instances_have_different_session_nonces_and_digests() {
        let a = validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
            .unwrap();
        let b = validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
            .unwrap();
        assert_ne!(
            a.session_nonce, b.session_nonce,
            "session nonces must be freshly random per instance"
        );
        assert_ne!(
            a.digest(),
            b.digest(),
            "provider reconstruction must invalidate the previous identity digest"
        );
    }

    #[test]
    fn digest_matches_hand_computed_preimage() {
        let validated =
            validate_provider_identity(MANTLE_SEPOLIA_CHAIN_ID, MANTLE_SEPOLIA_GENESIS_HASH)
                .unwrap();

        let mut expected_preimage = Vec::new();
        expected_preimage.extend_from_slice(b"whisker-arb/e2e-provider-session/v1");
        expected_preimage.push(0x00);
        expected_preimage.extend_from_slice(&validated.session_nonce);
        expected_preimage.extend_from_slice(&MANTLE_SEPOLIA_CHAIN_ID.to_be_bytes());
        expected_preimage.extend_from_slice(MANTLE_SEPOLIA_GENESIS_HASH.as_slice());

        assert_eq!(validated.digest().0, keccak256(&expected_preimage));
    }
}
