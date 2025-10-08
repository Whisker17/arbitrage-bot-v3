use alloy::primitives::Address;

use crate::amms::{
    agni::AgniPool,
    amm::AMM,
    uniswap_v2::UniswapV2Pool,
    uniswap_v3::UniswapV3Pool,
    Token,
};

use super::error::ArbitrageError;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenState {
    pub address: Address,
    pub decimals: u8,
}

impl TokenState {
    pub fn new(address: Address, decimals: u8) -> Self {
        Self { address, decimals }
    }

    pub fn from_token(token: &Token) -> Result<Self, ArbitrageError> {
        if token.decimals == 0 {
            return Err(ArbitrageError::MissingTokenDecimals(token.address));
        }

        Ok(Self {
            address: token.address,
            decimals: token.decimals,
        })
    }
}

#[derive(Debug, Clone)]
pub struct PoolEdge {
    pub pool_address: Address,
    pub token_in: TokenState,
    pub token_out: TokenState,
    pub fee_bps: u32,
}

impl PoolEdge {
    pub fn reversed(&self) -> Self {
        Self {
            pool_address: self.pool_address,
            token_in: self.token_out.clone(),
            token_out: self.token_in.clone(),
            fee_bps: self.fee_bps,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PoolExtraction {
    pub token_a: TokenState,
    pub token_b: TokenState,
    pub fee_bps: u32,
}

impl PoolExtraction {
    pub fn into_edges(self, pool_address: Address) -> (PoolEdge, PoolEdge) {
        let forward = PoolEdge {
            pool_address,
            token_in: self.token_a.clone(),
            token_out: self.token_b.clone(),
            fee_bps: self.fee_bps,
        };

        let reverse = forward.reversed();

        (forward, reverse)
    }
}

pub fn extract_pool(pool: &AMM) -> Result<PoolExtraction, ArbitrageError> {
    match pool {
        AMM::UniswapV3Pool(inner) => extract_uniswap_v3(inner),
        AMM::AgniPool(inner) => extract_agni(inner),
        AMM::UniswapV2Pool(inner) => extract_uniswap_v2(inner),
        other => Err(ArbitrageError::Graph(format!(
            "Unsupported AMM variant for arbitrage graph: {:?}",
            other.variant()
        ))),
    }
}

fn extract_uniswap_v3(pool: &UniswapV3Pool) -> Result<PoolExtraction, ArbitrageError> {
    let token_a = TokenState::from_token(&pool.token_a)?;
    let token_b = TokenState::from_token(&pool.token_b)?;

    Ok(PoolExtraction {
        token_a,
        token_b,
        fee_bps: pool.fee,
    })
}

fn extract_agni(pool: &AgniPool) -> Result<PoolExtraction, ArbitrageError> {
    let token_a = TokenState::from_token(&pool.token_a)?;
    let token_b = TokenState::from_token(&pool.token_b)?;

    Ok(PoolExtraction {
        token_a,
        token_b,
        fee_bps: pool.fee,
    })
}

fn extract_uniswap_v2(pool: &UniswapV2Pool) -> Result<PoolExtraction, ArbitrageError> {
    let token_a = TokenState::from_token(&pool.token_a)?;
    let token_b = TokenState::from_token(&pool.token_b)?;

    // Uniswap V2 fee is stored in 1e5 scale (e.g. 300 => 0.3%). Convert to bps for display.
    let fee_bps = (pool.fee as u32) / 10u32;

    Ok(PoolExtraction {
        token_a,
        token_b,
        fee_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;

    fn sample_token(addr_byte: u8, decimals: u8) -> Token {
        let mut raw = [0u8; 20];
        raw[19] = addr_byte;
        Token::new_with_decimals(Address::from(raw), decimals)
    }

    #[test]
    fn extract_uniswap_pool_success() {
        let mut pool = UniswapV3Pool::default();
        pool.address = sample_token(1, 18).address;
        pool.token_a = sample_token(2, 6);
        pool.token_b = sample_token(3, 18);
        pool.fee = 500;

        let pool_address = pool.address;
        let extraction = extract_pool(&pool.clone().into()).expect("pool extraction");
        assert_eq!(extraction.fee_bps, 500);
        assert_eq!(extraction.token_a.decimals, 6);
        assert_eq!(extraction.token_b.decimals, 18);

        let (forward, reverse) = extraction.into_edges(pool_address);
        assert_eq!(forward.token_in.address, pool.token_a.address);
        assert_eq!(forward.token_out.address, pool.token_b.address);
        assert_eq!(reverse.token_in.address, pool.token_b.address);
    }

    #[test]
    fn extract_pool_missing_decimals_is_error() {
        let mut pool = UniswapV3Pool::default();
        pool.address = sample_token(11, 18).address;
        pool.token_a = Token::from(Address::from([0u8; 20]));
        pool.token_b = sample_token(4, 18);

        let err = extract_pool(&pool.clone().into()).expect_err("expected decimals error");
        match err {
            ArbitrageError::MissingTokenDecimals(addr) => {
                assert_eq!(addr, pool.token_a.address)
            }
            other => panic!("unexpected error: unexpected variant {other:?}"),
        }
    }
}
