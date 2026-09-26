//! UniswapV2-family venues on Mantle and their per-venue swap fee (WHI-1413).
//!
//! Every UniV2-family pair loads through the same constant-product math
//! ([`crate::amms::uniswap_v2::UniswapV2Pool`]); venues differ only in the swap
//! fee. The fee is a **venue property keyed by factory**, in the protocol-native
//! domain of parts per `100_000` (never basis-point labels). Before WHI-1413 the
//! bot quoted every V2 pool with one hard-coded `V2_FEE = 300`, which is why no
//! further V2 venue could be admitted (WHI-910, WHI-999
//! `unregistered_v2_family_factory`).
//!
//! A universe row on a factory that is **not** listed here fails to build
//! ([`crate::service::protocol::AgniV2Protocol::build_amm`]): a V2 pool is never
//! quoted under a guessed fee.
//!
//! Each fee below was measured on chain from the pair's own `Swap` + `Sync`
//! logs (`evidence/venues/whi-1413/fee_from_swaps.py`) and cross-checked by a
//! router `getAmountsOut` differential fixture (`tests/differential.rs`).
//!
//! Source of truth for *which pools* run live remains the unified universe CSV
//! (WHI-793); V2 rows keep the shared `agni-v2` protocol label (shared math),
//! and the row's `factory` column selects the venue — the same shape as
//! [`crate::service::v3_venues`].

use alloy::primitives::{address, Address};

use crate::service::config::INTERIM_V2_FACTORY;

/// Fee denominator for UniV2-family venues (parts per `100_000`).
pub const V2_FEE_DENOMINATOR: usize = 100_000;

/// One UniswapV2-family venue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct V2Venue {
    /// Stable operator label (e.g. `fusionx-v2`, `moe-v1`).
    pub label: &'static str,
    /// Factory address: discovery source and the key rows are resolved by.
    pub factory: Address,
    /// Swap fee in parts per [`V2_FEE_DENOMINATOR`].
    pub fee: usize,
}

/// FusionX V2 — the venue historically behind the interim `agni-v2` label.
/// Fee `200 / 100_000` (WHI-765 router inference; WHI-1413 swap logs: 196 exact
/// matches, K-check upper bound 200).
pub const FUSIONX_V2: V2Venue = V2Venue {
    label: "fusionx-v2",
    factory: INTERIM_V2_FACTORY,
    fee: 200,
};

/// Merchant Moe V1 **classic** — a UniV2 CPMM on its own factory. Not the
/// Merchant Moe Liquidity Book venue (`moe`), which is a different AMM.
/// Fee `300 / 100_000` (WHI-765 router inference; WHI-1413 swap logs: 54 exact
/// matches, K-check upper bound 300).
pub const MOE_V1: V2Venue = V2Venue {
    label: "moe-v1",
    factory: address!("5bEf015CA9424A7C07B68490616a4C1F094BEdEc"),
    fee: 300,
};

/// Admitted UniV2-family venues. MantleSwap V2 (`0x5c84…fd2f`, measured fee
/// 250) is deliberately absent: WHI-1413's decision record rejects it (+1 of
/// 44 arbs). Adding a venue here makes its rows quotable, so it needs a
/// measured fee, a differential fixture and gas qualification first.
pub const V2_VENUES: &[V2Venue] = &[FUSIONX_V2, MOE_V1];

/// The admitted V2 venue for `factory`, if any.
pub fn v2_venue_by_factory(factory: Address) -> Option<&'static V2Venue> {
    V2_VENUES.iter().find(|v| v.factory == factory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_fees_are_keyed_by_factory() {
        assert_eq!(
            v2_venue_by_factory(INTERIM_V2_FACTORY).map(|v| v.fee),
            Some(200)
        );
        assert_eq!(
            v2_venue_by_factory(MOE_V1.factory).map(|v| v.fee),
            Some(300)
        );
    }

    #[test]
    fn unadmitted_v2_factories_resolve_to_nothing() {
        // MantleSwap V2: measured (250) but rejected by the WHI-1413 decision record.
        assert!(
            v2_venue_by_factory(address!("5c84e5d27fc7575D002fe98c5A1791Ac3ce6fD2f")).is_none()
        );
        assert!(v2_venue_by_factory(Address::ZERO).is_none());
    }

    #[test]
    fn registry_has_unique_factories_and_fees_in_domain() {
        for (i, a) in V2_VENUES.iter().enumerate() {
            assert!(a.fee < V2_FEE_DENOMINATOR, "{} fee out of domain", a.label);
            for b in &V2_VENUES[i + 1..] {
                assert_ne!(a.factory, b.factory);
                assert_ne!(a.label, b.label);
            }
        }
    }
}
