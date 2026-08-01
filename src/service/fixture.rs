//! Offline multi-protocol fixture pools (WHI-728).
//!
//! Constructs a deliberately mispriced V2 + Agni pair on the same WMNT/TOKEN
//! market, plus an inert Moe pool on an unrelated pair. Only the **merged**
//! multi-protocol graph can form a profitable closed WMNT cycle; any single
//! protocol subset has at most one pool on the settlement pair and therefore
//! finds no cycle. This is the acceptance fixture for WHI-527 / WHI-728.

use crate::amms::agni::AgniPool;
use crate::amms::amm::AMM;
use crate::amms::moe::{
    BinReserve, MoeBinRange, MoeLbPair, MoeSnapshot, MoeSnapshotContext,
};
use crate::amms::uniswap_v2::UniswapV2Pool;
use crate::amms::Token;
use crate::service::config::DEFAULT_WMNT;
use crate::service::protocol::V2_FEE;
use alloy::primitives::{address, Address, B256, U256};

/// Settlement asset for the offline fixture (canonical Mantle WMNT).
pub fn fixture_settlement_asset() -> Address {
    DEFAULT_WMNT
}

/// Synthetic counter-asset paired with WMNT on the V2 and Agni venues.
pub fn fixture_token_a() -> Address {
    address!("00000000000000000000000000000000000000aa")
}

/// Unrelated token used only by the inert Moe pool.
pub fn fixture_token_b() -> Address {
    address!("00000000000000000000000000000000000000bb")
}

pub fn fixture_v2_pool_address() -> Address {
    address!("00000000000000000000000000000000000000a1")
}

pub fn fixture_agni_pool_address() -> Address {
    address!("00000000000000000000000000000000000000a2")
}

pub fn fixture_moe_pool_address() -> Address {
    address!("00000000000000000000000000000000000000a3")
}

/// V2 pool: WMNT/TOKEN with 1:2 reserves → TOKEN is cheap in WMNT terms.
///
/// Selling 1 WMNT buys ~2 TOKEN (minus fee).
pub fn fixture_v2_pool() -> AMM {
    let mut pool = UniswapV2Pool::new(fixture_v2_pool_address(), V2_FEE);
    pool.token_a = Token::new_with_decimals(fixture_settlement_asset(), 18);
    pool.token_b = Token::new_with_decimals(fixture_token_a(), 18);
    // Large, skewed reserves: price TOKEN/WMNT ≈ 2.
    pool.reserve_0 = 1_000_000_000_000_000_000_000; // 1000 WMNT
    pool.reserve_1 = 2_000_000_000_000_000_000_000; // 2000 TOKEN
    AMM::UniswapV2Pool(pool)
}

/// Agni V3 pool: same WMNT/TOKEN pair priced ~1:1 at tick 0.
///
/// Buying TOKEN on V2 (~2 per WMNT) and selling back on Agni (~1:1) is profitable.
pub fn fixture_agni_pool() -> AMM {
    let mut pool = AgniPool {
        address: fixture_agni_pool_address(),
        token_a: Token::new_with_decimals(fixture_settlement_asset(), 18),
        token_b: Token::new_with_decimals(fixture_token_a(), 18),
        // Deep liquidity so a small swap stays inside the current tick.
        liquidity: 10_000_000_000_000_000_000,
        sqrt_price: uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(0).expect("tick 0"),
        tick: 0,
        fee: 3_000,
        tick_spacing: 60,
        ..Default::default()
    };
    // Wide bitmap coverage so simulate_swap does not require live tick records.
    pool.tick_bitmap_coverage.extend(-200i16..=200i16);
    AMM::AgniPool(pool)
}

/// Inert Moe pool on an unrelated pair (TOKEN_B / dead token).
///
/// Present so `--protocols agni-v2,agni-v3,moe` loads three protocols, but it
/// cannot participate in a WMNT settlement cycle on its own.
pub fn fixture_moe_pool() -> AMM {
    let mut pair = MoeLbPair::new(fixture_moe_pool_address());
    pair.token_x = Token::new_with_decimals(fixture_token_b(), 18);
    pair.token_y = Token::new_with_decimals(address!("00000000000000000000000000000000000000cc"), 18);
    pair.bin_step = 20;
    pair.active_id = 8_388_608;
    pair.protocol_share_bps = 100;
    pair.max_volatility_acc = 250_000;
    pair.time_of_last_update = 1_700_000_000;
    let bins = [
        (
            pair.active_id - 1,
            BinReserve {
                reserve_x: 5_000_000_000_000_000_000,
                reserve_y: 5_000_000_000_000_000_000,
            },
        ),
        (
            pair.active_id,
            BinReserve {
                reserve_x: 10_000_000_000_000_000_000,
                reserve_y: 10_000_000_000_000_000_000,
            },
        ),
        (
            pair.active_id + 1,
            BinReserve {
                reserve_x: 5_000_000_000_000_000_000,
                reserve_y: 5_000_000_000_000_000_000,
            },
        ),
    ];
    for (id, bin) in bins {
        pair.reserve_x += bin.reserve_x;
        pair.reserve_y += bin.reserve_y;
        pair.bins.insert(id, bin);
    }
    let snapshot = MoeSnapshot::new(
        pair.snapshot_slot0(),
        pair.bins.clone(),
        vec![MoeBinRange::new(pair.active_id - 1, pair.active_id + 1)],
        MoeSnapshotContext::new(B256::repeat_byte(1), 1_700_000_000),
    )
    .expect("offline moe fixture snapshot");
    pair.install_snapshot(snapshot)
        .expect("install offline moe fixture snapshot");
    AMM::MoeLbPair(pair)
}

/// Full three-protocol offline fixture used by the bot and acceptance tests.
pub fn cross_protocol_fixture_pools() -> Vec<AMM> {
    vec![
        fixture_v2_pool(),
        fixture_agni_pool(),
        fixture_moe_pool(),
    ]
}

/// Smoke-check that a small V2 → Agni round-trip yields gross profit offline.
pub fn fixture_manual_roundtrip_profit(amount_in: U256) -> eyre::Result<U256> {
    use crate::arbitrage::pathfinder::{ArbitragePath, PathHop};
    use crate::service::discovery::simulate_mixed_path_with_route_key;

    let wmnt = fixture_settlement_asset();
    let token = fixture_token_a();
    let pools = vec![fixture_v2_pool(), fixture_agni_pool()];
    let path = ArbitragePath {
        hops: vec![
            PathHop {
                pool_address: fixture_v2_pool_address(),
                token_in: wmnt,
                token_out: token,
                fee_bps: 30,
            },
            PathHop {
                pool_address: fixture_agni_pool_address(),
                token_in: token,
                token_out: wmnt,
                fee_bps: 30,
            },
        ],
    };
    let (_outs, final_out, _rk) =
        simulate_mixed_path_with_route_key(&path, &pools, amount_in, 1_700_000_000)?;
    final_out
        .checked_sub(amount_in)
        .ok_or_else(|| eyre::eyre!("round-trip produced no gross profit"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    #[test]
    fn manual_roundtrip_is_profitable() {
        let profit = fixture_manual_roundtrip_profit(U256::from(10u128.pow(18))).unwrap();
        assert!(!profit.is_zero(), "expected positive gross profit");
    }

    #[test]
    fn fixture_has_three_protocol_variants() {
        let pools = cross_protocol_fixture_pools();
        assert_eq!(pools.len(), 3);
        assert!(matches!(pools[0], AMM::UniswapV2Pool(_)));
        assert!(matches!(pools[1], AMM::AgniPool(_)));
        assert!(matches!(pools[2], AMM::MoeLbPair(_)));
    }
}
