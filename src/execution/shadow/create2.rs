//! Pure, I/O-free reproduction of `ArbitrageExecutor.sol`'s `_pairForV2`/`_pairForV3`
//! CREATE2 pair-address derivation (`contracts/executor/ArbitrageExecutor.sol:423-451`),
//! used to verify a candidate pool's address against its claimed factory/venue instead
//! of trusting the pool's own self-reported `factory()`.
//!
//! Moe Liquidity Book pairs are explicitly *not* CREATE2-derivable this way (contract
//! comment at `ArbitrageExecutor.sol:201`: "Moe LB pair addresses are not
//! CREATE2(tokenX,tokenY)-simple; allowlist only") — see `moe_allowlist.rs` for that
//! pool type instead.

use alloy::primitives::{keccak256, Address, B256};

use crate::execution::provenance::contract_pool_type;
use crate::state_space::PoolProtocol;

fn sort_tokens(token_a: Address, token_b: Address) -> (Address, Address) {
    if token_a < token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

fn create2_address(factory: Address, salt: B256, init_code_hash: B256) -> Address {
    let mut preimage = [0u8; 85];
    preimage[0] = 0xff;
    preimage[1..21].copy_from_slice(factory.as_slice());
    preimage[21..53].copy_from_slice(salt.as_slice());
    preimage[53..85].copy_from_slice(init_code_hash.as_slice());
    let hash = keccak256(preimage);
    Address::from_slice(&hash[12..])
}

/// `_pairForV2`'s salt: `keccak256(abi.encodePacked(token0, token1))` (40-byte packed
/// concatenation, no padding).
fn salt_for_v2(token_a: Address, token_b: Address) -> B256 {
    let (token0, token1) = sort_tokens(token_a, token_b);
    let mut salt_preimage = [0u8; 40];
    salt_preimage[..20].copy_from_slice(token0.as_slice());
    salt_preimage[20..].copy_from_slice(token1.as_slice());
    keccak256(salt_preimage)
}

/// `_pairForV3`'s salt: `keccak256(abi.encode(token0, token1, fee))` — a 96-byte
/// *non-packed* ABI encoding (each argument left-padded to its own 32-byte word),
/// unlike V2's packed 40-byte salt.
fn salt_for_v3(token_a: Address, token_b: Address, fee: u32) -> B256 {
    let (token0, token1) = sort_tokens(token_a, token_b);
    let mut salt_preimage = [0u8; 96];
    salt_preimage[12..32].copy_from_slice(token0.as_slice());
    salt_preimage[44..64].copy_from_slice(token1.as_slice());
    // uint24 `fee`, abi.encode-padded to a full word; only the low 3 bytes are used.
    let fee_bytes = fee.to_be_bytes();
    salt_preimage[93..96].copy_from_slice(&fee_bytes[1..4]);
    keccak256(salt_preimage)
}

/// Reproduces `_pairForV2`.
pub(crate) fn pair_for_v2(
    factory: Address,
    token_a: Address,
    token_b: Address,
    init_code_hash: B256,
) -> Address {
    create2_address(factory, salt_for_v2(token_a, token_b), init_code_hash)
}

/// Reproduces `_pairForV3`.
pub(crate) fn pair_for_v3(
    factory: Address,
    token_a: Address,
    token_b: Address,
    fee: u32,
    init_code_hash: B256,
) -> Address {
    create2_address(factory, salt_for_v3(token_a, token_b, fee), init_code_hash)
}

/// Dispatches on the contract's 3-way `poolType` (via [`contract_pool_type`], the
/// same collapsing map `provenance::verify_pool_provenance` already uses) and
/// returns the expected CREATE2 pool address, or `None` for `PoolProtocol::MoeLb`
/// (not CREATE2-derivable — check against the committed allowlist instead).
pub fn expected_pool_address(
    protocol: PoolProtocol,
    factory: Address,
    token_a: Address,
    token_b: Address,
    fee: u32,
    init_code_hash: B256,
) -> Option<Address> {
    match contract_pool_type(protocol) {
        0 => Some(pair_for_v2(factory, token_a, token_b, init_code_hash)),
        1 => Some(pair_for_v3(factory, token_a, token_b, fee, init_code_hash)),
        _ => None,
    }
}

/// Returns the CREATE2 salt `expected_pool_address` derived internally to reach its
/// address, for callers that need to record the salt as CREATE2 proof detail (see
/// `manifest::Create2Proof`) rather than just the resulting address. `None` for
/// `PoolProtocol::MoeLb`, same dispatch as [`expected_pool_address`].
pub(crate) fn expected_salt(protocol: PoolProtocol, token_a: Address, token_b: Address, fee: u32) -> Option<B256> {
    match contract_pool_type(protocol) {
        0 => Some(salt_for_v2(token_a, token_b)),
        1 => Some(salt_for_v3(token_a, token_b, fee)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    // Golden values independently derived via Foundry `cast` (not by running this
    // code), so a regression in the salt/preimage arithmetic is actually caught:
    //
    //   TOKEN0=1111111111111111111111111111111111111111
    //   TOKEN1=2222222222222222222222222222222222222222
    //   FACTORY=0x3333333333333333333333333333333333333333
    //   INIT_CODE_HASH=0x4444444444444444444444444444444444444444444444444444444444444444
    //
    //   # V2: salt = keccak256(packed token0 ++ token1)
    //   cast keccak 0x${TOKEN0}${TOKEN1}
    //   # -> 0xa284ddd69adb56d959922d24c73d2cd9e6b24d5e789a4106eca975c86ec900e1
    //   cast create2 --deployer $FACTORY --salt <above> --init-code-hash $INIT_CODE_HASH
    //   # -> 0xB484E12d146271DE6Eb53EfF40b4dbc1950D4Be7
    //
    //   # V3: salt = keccak256(abi.encode(token0, token1, uint24(500)))
    //   cast keccak 0x000000000000000000000000${TOKEN0}000000000000000000000000${TOKEN1}00000000000000000000000000000000000000000000000000000000000001f4
    //   # -> 0xd84af969fdf6567f10d7393fdcda82951ec41a3d9aaf18ac96c687a4d4057019
    //   cast create2 --deployer $FACTORY --salt <above> --init-code-hash $INIT_CODE_HASH
    //   # -> 0x779471b632F42C161E49cf6908541E00Ab26b419
    const FACTORY: Address = address!("3333333333333333333333333333333333333333");
    const TOKEN_A: Address = address!("1111111111111111111111111111111111111111");
    const TOKEN_B: Address = address!("2222222222222222222222222222222222222222");
    const INIT_CODE_HASH: B256 = B256::new([0x44; 32]);

    #[test]
    fn pair_for_v2_matches_cast_derived_golden_value() {
        let expected = address!("B484E12d146271DE6Eb53EfF40b4dbc1950D4Be7");
        assert_eq!(
            pair_for_v2(FACTORY, TOKEN_A, TOKEN_B, INIT_CODE_HASH),
            expected
        );
    }

    #[test]
    fn pair_for_v2_is_independent_of_argument_order() {
        assert_eq!(
            pair_for_v2(FACTORY, TOKEN_A, TOKEN_B, INIT_CODE_HASH),
            pair_for_v2(FACTORY, TOKEN_B, TOKEN_A, INIT_CODE_HASH)
        );
    }

    #[test]
    fn pair_for_v3_matches_cast_derived_golden_value() {
        let expected = address!("779471b632F42C161E49cf6908541E00Ab26b419");
        assert_eq!(
            pair_for_v3(FACTORY, TOKEN_A, TOKEN_B, 500, INIT_CODE_HASH),
            expected
        );
    }

    #[test]
    fn pair_for_v3_is_independent_of_argument_order() {
        assert_eq!(
            pair_for_v3(FACTORY, TOKEN_A, TOKEN_B, 500, INIT_CODE_HASH),
            pair_for_v3(FACTORY, TOKEN_B, TOKEN_A, 500, INIT_CODE_HASH)
        );
    }

    #[test]
    fn expected_pool_address_dispatches_v2_v3_and_rejects_moe() {
        assert_eq!(
            expected_pool_address(
                PoolProtocol::UniswapV2,
                FACTORY,
                TOKEN_A,
                TOKEN_B,
                0,
                INIT_CODE_HASH
            ),
            Some(pair_for_v2(FACTORY, TOKEN_A, TOKEN_B, INIT_CODE_HASH))
        );
        assert_eq!(
            expected_pool_address(
                PoolProtocol::UniswapV3,
                FACTORY,
                TOKEN_A,
                TOKEN_B,
                500,
                INIT_CODE_HASH
            ),
            Some(pair_for_v3(FACTORY, TOKEN_A, TOKEN_B, 500, INIT_CODE_HASH))
        );
        assert_eq!(
            expected_pool_address(
                PoolProtocol::Agni,
                FACTORY,
                TOKEN_A,
                TOKEN_B,
                500,
                INIT_CODE_HASH
            ),
            Some(pair_for_v3(FACTORY, TOKEN_A, TOKEN_B, 500, INIT_CODE_HASH))
        );
        assert_eq!(
            expected_pool_address(
                PoolProtocol::MoeLb,
                FACTORY,
                TOKEN_A,
                TOKEN_B,
                0,
                INIT_CODE_HASH
            ),
            None
        );
    }

    #[test]
    fn expected_salt_matches_the_salt_baked_into_expected_pool_address() {
        let salt = expected_salt(PoolProtocol::UniswapV2, TOKEN_A, TOKEN_B, 0).unwrap();
        assert_eq!(
            create2_address(FACTORY, salt, INIT_CODE_HASH),
            pair_for_v2(FACTORY, TOKEN_A, TOKEN_B, INIT_CODE_HASH)
        );

        let salt_v3 = expected_salt(PoolProtocol::UniswapV3, TOKEN_A, TOKEN_B, 500).unwrap();
        assert_eq!(
            create2_address(FACTORY, salt_v3, INIT_CODE_HASH),
            pair_for_v3(FACTORY, TOKEN_A, TOKEN_B, 500, INIT_CODE_HASH)
        );

        assert_eq!(
            expected_salt(PoolProtocol::MoeLb, TOKEN_A, TOKEN_B, 0),
            None
        );
    }
}
