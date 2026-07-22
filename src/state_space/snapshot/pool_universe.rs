//! Canonical identity of the executable pool universe (WHI-520).

use alloy::primitives::{keccak256, Address, B256};
use thiserror::Error;

pub const EFFECTIVE_MAX_HOPS: u8 = 3;
const DOMAIN: &[u8] = b"AMMS_POOL_UNIVERSE_V1";
const ROW_LEN: usize = 81;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PoolProtocol {
    UniswapV2 = 0,
    UniswapV3 = 1,
    Agni = 2,
    MoeLb = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolUniverseRow {
    pub protocol: PoolProtocol,
    pub factory: Address,
    pub pool: Address,
    /// Venue-canonical token0/tokenX.
    pub token0: Address,
    /// Venue-canonical token1/tokenY.
    pub token1: Address,
}

impl PoolUniverseRow {
    fn encode(self) -> [u8; ROW_LEN] {
        let mut encoded = [0u8; ROW_LEN];
        encoded[0] = self.protocol as u8;
        encoded[1..21].copy_from_slice(self.factory.as_slice());
        encoded[21..41].copy_from_slice(self.pool.as_slice());
        encoded[41..61].copy_from_slice(self.token0.as_slice());
        encoded[61..81].copy_from_slice(self.token1.as_slice());
        encoded
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PoolUniverseError {
    #[error("pool universe contains a duplicate canonical row")]
    DuplicateRow,
    #[error("pool universe row count exceeds u32")]
    TooManyRows,
}

pub fn pool_universe_fingerprint(
    chain_id: u64,
    settlement_asset: Address,
    rows: impl IntoIterator<Item = PoolUniverseRow>,
) -> Result<B256, PoolUniverseError> {
    let mut rows: Vec<[u8; ROW_LEN]> = rows.into_iter().map(PoolUniverseRow::encode).collect();
    rows.sort_unstable();
    if rows.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(PoolUniverseError::DuplicateRow);
    }
    let row_count = u32::try_from(rows.len()).map_err(|_| PoolUniverseError::TooManyRows)?;
    let mut preimage = Vec::with_capacity(DOMAIN.len() + 8 + 1 + 20 + 4 + rows.len() * ROW_LEN);
    preimage.extend_from_slice(DOMAIN);
    preimage.extend_from_slice(&chain_id.to_be_bytes());
    preimage.push(EFFECTIVE_MAX_HOPS);
    preimage.extend_from_slice(settlement_asset.as_slice());
    preimage.extend_from_slice(&row_count.to_be_bytes());
    for row in rows {
        preimage.extend_from_slice(&row);
    }
    Ok(keccak256(preimage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    fn fixture_rows() -> [PoolUniverseRow; 2] {
        [
            PoolUniverseRow {
                protocol: PoolProtocol::MoeLb,
                factory: address!("3000000000000000000000000000000000000001"),
                pool: address!("3000000000000000000000000000000000000002"),
                token0: address!("0000000000000000000000000000000000000002"),
                token1: address!("0000000000000000000000000000000000000001"),
            },
            PoolUniverseRow {
                protocol: PoolProtocol::UniswapV2,
                factory: address!("1000000000000000000000000000000000000001"),
                pool: address!("1000000000000000000000000000000000000002"),
                token0: address!("0000000000000000000000000000000000000001"),
                token1: address!("0000000000000000000000000000000000000002"),
            },
        ]
    }

    #[test]
    fn golden_two_row_pool_universe_fingerprint() {
        let actual = pool_universe_fingerprint(
            5000,
            address!("0000000000000000000000000000000000000001"),
            fixture_rows(),
        )
        .unwrap();
        assert_eq!(
            actual,
            b256!("d9c6997577a77904789b6fb7faaf0917a51fc51d68244edc34726d92764b8325")
        );
    }

    #[test]
    fn sort_order_does_not_change_identity_and_duplicates_fail() {
        let [first, second] = fixture_rows();
        let settlement = address!("0000000000000000000000000000000000000001");
        assert_eq!(
            pool_universe_fingerprint(5000, settlement, [first, second]).unwrap(),
            pool_universe_fingerprint(5000, settlement, [second, first]).unwrap()
        );
        assert_eq!(
            pool_universe_fingerprint(5000, settlement, [first, first]),
            Err(PoolUniverseError::DuplicateRow)
        );
    }
}
