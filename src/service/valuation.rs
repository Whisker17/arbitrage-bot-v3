//! WMNT-equivalent pool valuation (WHI-793 generator path, shared since WHI-999).
//!
//! Extracted verbatim from `src/bin/universe_gen.rs` so the offline research
//! binaries can value candidate pools with the **same** heuristic the frozen
//! universe was filtered by. Splitting it in two also makes the arithmetic
//! unit-testable without RPC:
//!
//! 1. [`fetch_valuation_inputs`] — the only RPC stage (`decimals()` per token,
//!    `balanceOf(pool)` per pool side), pinned to one block.
//! 2. [`value_pools_from_inputs`] — pure integer arithmetic over those reads.
//!
//! Policy (stamped into `pool_universe.meta.json` as
//! `wmnt_reserve_balance_heuristic`): there is no USD oracle on this path, so
//! the operator's "$1000 floor" is expressed as a WMNT-equivalent.
//!
//! * Pool holds WMNT → `tvl = 2 × wmnt_balance` (50/50 value assumption).
//! * Otherwise price each side from a direct WMNT pair's balance ratio when one
//!   exists among the candidates; one priced side is doubled.
//! * A failed `balanceOf` / `decimals` read yields `None` (quarantine), never a
//!   silent zero.

use std::collections::{HashMap, HashSet};

use alloy::eips::BlockId;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::sol;
use eyre::Result;
use tracing::warn;

use crate::service::universe_filter::CandidatePool;

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
        function decimals() external view returns (uint8);
    }
}

/// 1e18, the fixed-point scale every price and normalized balance uses.
const WAD: u64 = 1_000_000_000_000_000_000;

/// Stable label for the valuation method (mirrors the meta sidecar note).
pub const VALUATION_METHOD: &str = "wmnt_reserve_balance_heuristic";

/// Chain reads that [`value_pools_from_inputs`] consumes, pinned to one block.
///
/// Both maps distinguish "read failed" from "value is zero": a `None` balance
/// means the `balanceOf` call errored and every pool using it must be
/// quarantined rather than valued at zero.
#[derive(Debug, Clone, Default)]
pub struct ValuationInputs {
    /// `(token, pool)` → `balanceOf(pool)`; `None` when the call failed.
    pub balances: HashMap<(Address, Address), Option<U256>>,
    /// `token` → `decimals()`. Absent entries fall back to 18.
    pub decimals: HashMap<Address, u8>,
    /// Tokens whose `decimals()` failed; any pool using one is quarantined.
    pub bad_decimals: HashSet<Address>,
}

/// Scale a raw token amount to 18-decimal fixed point (truncate if >18).
pub fn normalize_to_18(amount: U256, decimals: u8) -> U256 {
    if decimals == 18 {
        amount
    } else if decimals < 18 {
        amount.saturating_mul(U256::from(10u64).pow(U256::from((18 - decimals) as u64)))
    } else {
        amount / U256::from(10u64).pow(U256::from((decimals - 18) as u64))
    }
}

/// Read `decimals()` per distinct token and `balanceOf(pool)` per pool side.
///
/// One request per token and per `(token, pool)` pair; the caller's provider
/// stack supplies throttling and retry (`ThrottleLayer`, WHI-862).
pub async fn fetch_valuation_inputs(
    provider: &impl Provider,
    candidates: &[CandidatePool],
    wmnt: Address,
    block: u64,
) -> Result<ValuationInputs> {
    let block_id = BlockId::Number(block.into());
    let mut inputs = ValuationInputs::default();
    inputs.decimals.insert(wmnt, 18);

    let mut tokens = std::collections::BTreeSet::new();
    for c in candidates {
        tokens.insert(c.token0);
        tokens.insert(c.token1);
    }

    for &token in &tokens {
        if token == Address::ZERO || inputs.decimals.contains_key(&token) {
            continue;
        }
        let erc = IERC20::new(token, provider);
        match erc.decimals().call().block(block_id).await {
            Ok(d) => {
                inputs.decimals.insert(token, d);
            }
            Err(e) => {
                warn!(token = %token, error = %e, "decimals() failed → quarantine pools using token");
                inputs.bad_decimals.insert(token);
            }
        }
    }

    for c in candidates {
        for token in [c.token0, c.token1] {
            if token == Address::ZERO {
                continue;
            }
            let key = (token, c.pool);
            if inputs.balances.contains_key(&key) {
                continue;
            }
            let erc = IERC20::new(token, provider);
            let bal = match erc.balanceOf(c.pool).call().block(block_id).await {
                Ok(b) => Some(b),
                Err(e) => {
                    warn!(pool = %c.pool, token = %token, error = %e, "balanceOf failed → quarantine");
                    None
                }
            };
            inputs.balances.insert(key, bal);
        }
    }

    Ok(inputs)
}

/// WMNT-equivalent TVL per pool from pre-fetched reads (integer `U256` only).
///
/// `None` for a pool means "no valuation could be established" — the caller
/// quarantines it (see [`crate::service::universe_filter::apply_universe_filters`]).
pub fn value_pools_from_inputs(
    candidates: &[CandidatePool],
    inputs: &ValuationInputs,
    wmnt: Address,
) -> HashMap<Address, Option<U256>> {
    let ValuationInputs {
        balances,
        decimals,
        bad_decimals,
    } = inputs;

    // price_x18[token] = WMNT-wei value of 1e18 normalized token units, taken
    // from direct WMNT pairs. Integer ratio only.
    let mut price_x18: HashMap<Address, U256> = HashMap::new();
    price_x18.insert(wmnt, U256::from(WAD));

    for c in candidates {
        let other = if c.token0 == wmnt {
            c.token1
        } else if c.token1 == wmnt {
            c.token0
        } else {
            continue;
        };
        let Some(Some(bal_w)) = balances.get(&(wmnt, c.pool)) else {
            continue;
        };
        let Some(Some(bal_o)) = balances.get(&(other, c.pool)) else {
            continue;
        };
        if bal_w.is_zero() || bal_o.is_zero() {
            continue;
        }
        let d_w = *decimals.get(&wmnt).unwrap_or(&18);
        let d_o = *decimals.get(&other).unwrap_or(&18);
        let w_n = normalize_to_18(*bal_w, d_w);
        let o_n = normalize_to_18(*bal_o, d_o);
        if o_n.is_zero() {
            continue;
        }
        // WMNT-wei per 1e18 units of other: w_n * 1e18 / o_n
        let px = w_n
            .checked_mul(U256::from(WAD))
            .and_then(|v| v.checked_div(o_n));
        if let Some(px) = px {
            // Prefer the deeper WMNT side when multiple pairs exist.
            price_x18
                .entry(other)
                .and_modify(|p| {
                    if px > *p {
                        *p = px;
                    }
                })
                .or_insert(px);
        }
    }

    let mut out = HashMap::new();
    for c in candidates {
        if bad_decimals.contains(&c.token0) || bad_decimals.contains(&c.token1) {
            out.insert(c.pool, None);
            continue;
        }
        let b0 = balances.get(&(c.token0, c.pool));
        let b1 = balances.get(&(c.token1, c.pool));
        // Any failed balanceOf for this pool → quarantine
        match (b0, b1) {
            (Some(None), _) | (_, Some(None)) => {
                out.insert(c.pool, None);
                continue;
            }
            _ => {}
        }
        let b0 = b0.and_then(|o| *o).unwrap_or(U256::ZERO);
        let b1 = b1.and_then(|o| *o).unwrap_or(U256::ZERO);

        if c.token0 == wmnt || c.token1 == wmnt {
            let wmnt_bal = if c.token0 == wmnt { b0 } else { b1 };
            out.insert(c.pool, Some(wmnt_bal.saturating_mul(U256::from(2u64))));
            continue;
        }

        let d0 = *decimals.get(&c.token0).unwrap_or(&18);
        let d1 = *decimals.get(&c.token1).unwrap_or(&18);
        let v0 = price_x18.get(&c.token0).and_then(|px| {
            let n = normalize_to_18(b0, d0);
            n.checked_mul(*px)
                .and_then(|v| v.checked_div(U256::from(WAD)))
        });
        let v1 = price_x18.get(&c.token1).and_then(|px| {
            let n = normalize_to_18(b1, d1);
            n.checked_mul(*px)
                .and_then(|v| v.checked_div(U256::from(WAD)))
        });
        match (v0, v1) {
            (Some(a), Some(b)) => out.insert(c.pool, Some(a.saturating_add(b))),
            (Some(a), None) => out.insert(c.pool, Some(a.saturating_mul(U256::from(2u64)))),
            (None, Some(b)) => out.insert(c.pool, Some(b.saturating_mul(U256::from(2u64)))),
            (None, None) => out.insert(c.pool, None),
        };
    }
    out
}

/// WMNT-equivalent TVL via ERC20 balances at the pool address.
///
/// Convenience wrapper: [`fetch_valuation_inputs`] then
/// [`value_pools_from_inputs`].
pub async fn value_pools_wmnt(
    provider: &impl Provider,
    candidates: &[CandidatePool],
    wmnt: Address,
    block: u64,
) -> Result<HashMap<Address, Option<U256>>> {
    let inputs = fetch_valuation_inputs(provider, candidates, wmnt, block).await?;
    Ok(value_pools_from_inputs(candidates, &inputs, wmnt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const WMNT: Address = address!("00000000000000000000000000000000000000aa");
    const USDC: Address = address!("0000000000000000000000000000000000000001");
    const FOO: Address = address!("0000000000000000000000000000000000000002");
    const P_WMNT_USDC: Address = address!("00000000000000000000000000000000000000b1");
    const P_USDC_FOO: Address = address!("00000000000000000000000000000000000000b2");

    fn pool(p: Address, t0: Address, t1: Address) -> CandidatePool {
        CandidatePool {
            protocol: "agni-v2".into(),
            factory: Address::ZERO,
            pool: p,
            token0: t0,
            token1: t1,
            fee_tier: None,
            bin_step: None,
            creation_block: None,
        }
    }

    fn wad(n: u64) -> U256 {
        U256::from(n) * U256::from(WAD)
    }

    #[test]
    fn normalize_scales_both_directions() {
        // 6-decimal 1.0 USDC → 1e18
        assert_eq!(normalize_to_18(U256::from(1_000_000u64), 6), wad(1));
        // 18-decimal passthrough
        assert_eq!(normalize_to_18(wad(3), 18), wad(3));
        // 21-decimal truncates by 1e3
        assert_eq!(
            normalize_to_18(U256::from(5_000u64), 21),
            U256::from(5u64)
        );
    }

    #[test]
    fn wmnt_side_is_doubled() {
        let pools = vec![pool(P_WMNT_USDC, WMNT, USDC)];
        let mut inputs = ValuationInputs::default();
        inputs.decimals.insert(WMNT, 18);
        inputs.decimals.insert(USDC, 6);
        inputs
            .balances
            .insert((WMNT, P_WMNT_USDC), Some(wad(500)));
        inputs
            .balances
            .insert((USDC, P_WMNT_USDC), Some(U256::from(250_000_000u64)));

        let out = value_pools_from_inputs(&pools, &inputs, WMNT);
        assert_eq!(out.get(&P_WMNT_USDC), Some(&Some(wad(1_000))));
    }

    #[test]
    fn non_wmnt_pool_priced_through_a_direct_wmnt_pair() {
        // WMNT/USDC holds 1000 WMNT vs 500 USDC → 1 USDC = 2 WMNT.
        // USDC/FOO holds 100 USDC and an unpriced FOO side → 200 WMNT, doubled.
        let pools = vec![
            pool(P_WMNT_USDC, WMNT, USDC),
            pool(P_USDC_FOO, USDC, FOO),
        ];
        let mut inputs = ValuationInputs::default();
        inputs.decimals.insert(WMNT, 18);
        inputs.decimals.insert(USDC, 6);
        inputs.decimals.insert(FOO, 18);
        inputs
            .balances
            .insert((WMNT, P_WMNT_USDC), Some(wad(1_000)));
        inputs
            .balances
            .insert((USDC, P_WMNT_USDC), Some(U256::from(500_000_000u64)));
        inputs
            .balances
            .insert((USDC, P_USDC_FOO), Some(U256::from(100_000_000u64)));
        inputs.balances.insert((FOO, P_USDC_FOO), Some(wad(7)));

        let out = value_pools_from_inputs(&pools, &inputs, WMNT);
        assert_eq!(out.get(&P_USDC_FOO), Some(&Some(wad(400))));
    }

    #[test]
    fn failed_balance_read_quarantines_rather_than_valuing_zero() {
        let pools = vec![pool(P_WMNT_USDC, WMNT, USDC)];
        let mut inputs = ValuationInputs::default();
        inputs.decimals.insert(WMNT, 18);
        inputs.decimals.insert(USDC, 6);
        inputs.balances.insert((WMNT, P_WMNT_USDC), None);
        inputs
            .balances
            .insert((USDC, P_WMNT_USDC), Some(U256::from(1u64)));

        let out = value_pools_from_inputs(&pools, &inputs, WMNT);
        assert_eq!(out.get(&P_WMNT_USDC), Some(&None));
    }

    #[test]
    fn bad_decimals_quarantines_every_pool_using_the_token() {
        let pools = vec![pool(P_WMNT_USDC, WMNT, USDC)];
        let mut inputs = ValuationInputs::default();
        inputs.decimals.insert(WMNT, 18);
        inputs.bad_decimals.insert(USDC);
        inputs
            .balances
            .insert((WMNT, P_WMNT_USDC), Some(wad(500)));
        inputs
            .balances
            .insert((USDC, P_WMNT_USDC), Some(U256::from(1u64)));

        let out = value_pools_from_inputs(&pools, &inputs, WMNT);
        assert_eq!(out.get(&P_WMNT_USDC), Some(&None));
    }

    #[test]
    fn unpriceable_pool_yields_none() {
        // Neither side connects to WMNT anywhere in the candidate set.
        let pools = vec![pool(P_USDC_FOO, USDC, FOO)];
        let mut inputs = ValuationInputs::default();
        inputs.decimals.insert(USDC, 6);
        inputs.decimals.insert(FOO, 18);
        inputs
            .balances
            .insert((USDC, P_USDC_FOO), Some(U256::from(1_000_000u64)));
        inputs.balances.insert((FOO, P_USDC_FOO), Some(wad(1)));

        let out = value_pools_from_inputs(&pools, &inputs, WMNT);
        assert_eq!(out.get(&P_USDC_FOO), Some(&None));
    }
}
