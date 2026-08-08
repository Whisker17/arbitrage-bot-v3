//! Drop-in UniswapV3-family venues on Mantle (WHI-910 / WHI-765 / WHI-938).
//!
//! These factories share the Agni/UniV3 math surface (`slot0`, per-pool
//! `fee()` uint24, uint32-width `feeProtocol`) and load through
//! [`crate::service::protocol::AgniV3Protocol`] / `AgniPool`. They do **not**
//! share CREATE2 deployers — each venue keeps its own identity (WHI-765:
//! "do not merge CREATE2 into Agni").
//!
//! Source of truth for *which pools* run live remains the unified universe CSV
//! (WHI-793). This registry is the generator + validation catalogue of
//! factories that may appear on `agni-v3` rows.
//!
//! **WHI-938:** Cleopatra CL matches `slot0` and even direct `ticks()` eth_calls,
//! but the Agni **tick-data batch CREATE** path reverts (WHI-929 live). It is
//! **not** a loadable drop-in; it lives in [`QUARANTINED_V3_VENUES`] so seed rows
//! are recorded with reason rather than silently emitted into the frozen universe.

use alloy::primitives::{address, Address};

/// One UniV3-family venue (loadable drop-in or quarantined).
///
/// `create2_deployer` is recorded so operators can see that Agni and FusionX
/// (etc.) are distinct CREATE2 domains even while sharing math. Production
/// reverse-lookup uses [`DropInV3Venue::factory`], not the deployer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropInV3Venue {
    /// Stable operator label (e.g. `agni-v3`, `fusionx-v3`).
    pub label: &'static str,
    /// Factory address used for discovery + reverse-lookup provenance.
    pub factory: Address,
    /// Optional CREATE2 pool deployer when distinct from the factory.
    /// Not used for discovery or live quoting; identity documentation only.
    pub create2_deployer: Option<Address>,
    /// Earliest block to scan for `PoolCreated` when discovering. `0` means
    /// "unknown — full-history scan" and is deliberately slow.
    pub creation_block: u64,
    /// Legacy `data/poolLists.csv` Protocol column values that map to this
    /// factory (case-insensitive). Empty when the venue is not present in the
    /// legacy seed.
    pub seed_protocol_tags: &'static [&'static str],
}

/// A venue that shares UniV3 surface partially but must not enter the live
/// universe until a dedicated adapter/batch ABI exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantinedV3Venue {
    pub venue: DropInV3Venue,
    /// Stable reason stamped into `pool_universe.quarantine.json`.
    pub reason: &'static str,
}

/// Agni V3 — already first-class.
pub const AGNI_V3: DropInV3Venue = DropInV3Venue {
    label: "agni-v3",
    factory: address!("25780dc8Fc3cfBD75F33bFDAB65e969b603b2035"),
    create2_deployer: Some(address!("e9827B4EBeB9AE41FC57efDdDd79EDddC2EA4d03")),
    creation_block: 110_692,
    seed_protocol_tags: &["agni"],
};

/// Butter V3 CL.
pub const BUTTER: DropInV3Venue = DropInV3Venue {
    label: "butter",
    factory: address!("EECa0a86431A7B42ca2Ee5F479832c3D4a4c2644"),
    create2_deployer: None,
    creation_block: 0,
    // WHI-906/910: allow census-expanded seed rows (tag is operator-side only).
    seed_protocol_tags: &["butter"],
};

/// FusionX V3 — highest missing arb coverage; holds top uncovered pool.
pub const FUSIONX_V3: DropInV3Venue = DropInV3Venue {
    label: "fusionx-v3",
    factory: address!("530d2766D1988CC1c000C8b7d00334c14B69AD71"),
    create2_deployer: Some(address!("8790c2C3BA67223D83C8FCF2a5E3C650059987b4")),
    creation_block: 0,
    seed_protocol_tags: &["fusionx"],
};

/// Cleopatra CL — **not loadable** (WHI-938): Agni tick-data batch CREATE reverts.
pub const CLEOPATRA_CL: DropInV3Venue = DropInV3Venue {
    label: "cleopatra-cl",
    factory: address!("AAA32926fcE6bE95ea2c51cB4Fcb60836D320C42"),
    create2_deployer: None,
    creation_block: 0,
    seed_protocol_tags: &["cleopatra", "cleopatra-cl"],
};

/// Fluxion V3.
pub const FLUXION_V3: DropInV3Venue = DropInV3Venue {
    label: "fluxion-v3",
    factory: address!("F883162Ed9c7E8EF604214c964c678E40c9B737C"),
    create2_deployer: None,
    creation_block: 0,
    seed_protocol_tags: &["fluxion", "fluxion-v3"],
};

/// Unnamed UniV3 fork at `0x636ea2…`.
pub const V3FORK_636EA2: DropInV3Venue = DropInV3Venue {
    label: "v3fork-636ea2",
    factory: address!("636eA278699A300d3A849aB2cE36c891C4eE3Da0"),
    create2_deployer: None,
    creation_block: 0,
    seed_protocol_tags: &["v3fork", "v3fork-636ea2"],
};

/// Uniswap V3 on Mantle.
pub const UNISWAP_V3_MANTLE: DropInV3Venue = DropInV3Venue {
    label: "uniswap-v3",
    factory: address!("0d922Fb1Bc191F64970ac40376643808b4B74Df9"),
    create2_deployer: None,
    creation_block: 0,
    seed_protocol_tags: &["uniswap", "uniswap-v3"],
};

/// Loadable drop-in UniV3-family factories (WHI-765 / WHI-910 / WHI-938).
///
/// Cleopatra CL is intentionally **absent** — see [`QUARANTINED_V3_VENUES`].
/// Order is stable for funnel reporting: Agni first, then by descending
/// expected arb weight from the WHI-906 census where known.
pub const DROP_IN_V3_VENUES: &[DropInV3Venue] = &[
    AGNI_V3,
    FUSIONX_V3,
    BUTTER,
    FLUXION_V3,
    V3FORK_636EA2,
    UNISWAP_V3_MANTLE,
];

/// Venues that must not enter the frozen universe until an adapter exists.
///
/// WHI-938: Cleopatra CL `slot0` matches UniV3, but `ticks()` reverts under the
/// Agni tick-data batch contract (all 7 live pools). Quarantine rather than
/// carry empty tick data that inflates coverage and never quotes.
pub const QUARANTINED_V3_VENUES: &[QuarantinedV3Venue] = &[QuarantinedV3Venue {
    venue: CLEOPATRA_CL,
    // Direct ticks()/tickBitmap() may still succeed; the Agni *batch CREATE*
    // tick-data path reverts (WHI-929 live). Do not misread as "ticks() missing".
    reason: "tick_data_batch_abi_incompatible: Agni tick-data batch CREATE reverts (WHI-938)",
}];

/// Universe CSV protocol label for every drop-in V3 venue.
///
/// Math is identical across venues; per-venue identity lives in the row
/// `factory` column, not in `SelectedProtocol` variants.
pub const V3_UNIVERSE_PROTOCOL_LABEL: &str = "agni-v3";

/// Factories of every **loadable** drop-in V3 venue (excludes quarantined).
pub fn drop_in_v3_factories() -> Vec<Address> {
    DROP_IN_V3_VENUES.iter().map(|v| v.factory).collect()
}

/// True when `factory` is a quarantined V3 venue (must not emit to universe).
pub fn is_quarantined_v3_factory(factory: Address) -> bool {
    quarantined_v3_by_factory(factory).is_some()
}

/// Look up a quarantined venue by factory.
pub fn quarantined_v3_by_factory(factory: Address) -> Option<&'static QuarantinedV3Venue> {
    QUARANTINED_V3_VENUES
        .iter()
        .find(|q| q.venue.factory == factory)
}

/// Stable quarantine reason for a factory, if registered.
pub fn quarantine_reason_for_factory(factory: Address) -> Option<&'static str> {
    quarantined_v3_by_factory(factory).map(|q| q.reason)
}

/// Look up a venue by factory address (loadable **or** quarantined).
pub fn venue_by_factory(factory: Address) -> Option<&'static DropInV3Venue> {
    DROP_IN_V3_VENUES
        .iter()
        .find(|v| v.factory == factory)
        .or_else(|| {
            QUARANTINED_V3_VENUES
                .iter()
                .find(|q| q.venue.factory == factory)
                .map(|q| &q.venue)
        })
}

/// Resolve a legacy seed `Protocol` column value to a factory (loadable or
/// quarantined). Matching is case-insensitive exact against each venue's
/// [`DropInV3Venue::seed_protocol_tags`].
pub fn factory_for_seed_protocol_tag(tag: &str) -> Option<Address> {
    let needle = tag.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    for venue in DROP_IN_V3_VENUES {
        for t in venue.seed_protocol_tags {
            if t.eq_ignore_ascii_case(&needle) {
                return Some(venue.factory);
            }
        }
    }
    for q in QUARANTINED_V3_VENUES {
        for t in q.venue.seed_protocol_tags {
            if t.eq_ignore_ascii_case(&needle) {
                return Some(q.venue.factory);
            }
        }
    }
    None
}

/// Per-venue counts for loadable drop-in factories (including zeros).
///
/// A zero is intentional signal — silent zero is the WHI-863 failure mode.
pub fn drop_in_v3_funnel_counts(
    pools: &[crate::service::universe_filter::CandidatePool],
) -> Vec<(&'static str, Address, usize)> {
    DROP_IN_V3_VENUES
        .iter()
        .map(|v| {
            let n = pools.iter().filter(|p| p.factory == v.factory).count();
            (v.label, v.factory, n)
        })
        .collect()
}

/// Format a loud per-factory report for loadable drop-ins; zeros called out.
pub fn format_v3_factory_funnel(
    stage: &str,
    pools: &[crate::service::universe_filter::CandidatePool],
) -> String {
    let n = DROP_IN_V3_VENUES.len();
    let mut out = format!("  {stage} per V3 factory ({n} loadable drop-in):\n");
    let mut any_zero = false;
    for (label, factory, count) in drop_in_v3_funnel_counts(pools) {
        if count == 0 {
            any_zero = true;
            out.push_str(&format!(
                "    {label} ({factory:?}): 0  ← ZERO (loud; not silent)\n"
            ));
        } else {
            out.push_str(&format!("    {label} ({factory:?}): {count}\n"));
        }
    }
    if any_zero {
        out.push_str(
            "  note: factories with zero pools are visible by design (WHI-863 / WHI-910); \
             expand seeds or run --discover once creation_block is known.\n",
        );
    }
    // Loud note when quarantined factories still appear in the candidate set.
    for q in QUARANTINED_V3_VENUES {
        let n = pools.iter().filter(|p| p.factory == q.venue.factory).count();
        if n > 0 {
            out.push_str(&format!(
                "  QUARANTINED {} ({:?}): {n} pool(s) — {}\n",
                q.venue.label, q.venue.factory, q.reason
            ));
        }
    }
    out
}

/// Split candidates whose factory is quarantined out of the loadable set.
///
/// Returns `(loadable, quarantine_entries)`. Only factories in
/// [`QUARANTINED_V3_VENUES`] are moved; unknown factories stay loadable.
/// Quarantine reason is the registry string for that venue.
pub fn split_quarantined_v3_candidates(
    candidates: Vec<crate::service::universe_filter::CandidatePool>,
) -> (
    Vec<crate::service::universe_filter::CandidatePool>,
    Vec<crate::service::universe_filter::QuarantineEntry>,
) {
    let mut loadable = Vec::with_capacity(candidates.len());
    let mut quarantine = Vec::new();
    for c in candidates {
        if let Some(q) = quarantined_v3_by_factory(c.factory) {
            quarantine.push(crate::service::universe_filter::QuarantineEntry {
                pool: c.pool,
                protocol: c.protocol,
                reason: q.reason.into(),
            });
        } else {
            loadable.push(c);
        }
    }
    (loadable, quarantine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::universe_filter::CandidatePool;
    use std::collections::HashSet;

    #[test]
    fn six_distinct_loadable_drop_in_factories() {
        let factories: HashSet<Address> = DROP_IN_V3_VENUES.iter().map(|v| v.factory).collect();
        assert_eq!(DROP_IN_V3_VENUES.len(), 6);
        assert_eq!(factories.len(), 6);
        assert!(!factories.contains(&CLEOPATRA_CL.factory));
    }

    #[test]
    fn cleopatra_is_quarantined_not_loadable() {
        assert!(is_quarantined_v3_factory(CLEOPATRA_CL.factory));
        assert_eq!(
            quarantine_reason_for_factory(CLEOPATRA_CL.factory),
            Some(QUARANTINED_V3_VENUES[0].reason)
        );
        assert!(DROP_IN_V3_VENUES
            .iter()
            .all(|v| v.factory != CLEOPATRA_CL.factory));
        // Still resolvable for operator diagnostics.
        assert_eq!(venue_by_factory(CLEOPATRA_CL.factory), Some(&CLEOPATRA_CL));
    }

    #[test]
    fn seed_tags_resolve_loadable_and_quarantined() {
        // Legacy tags (case-insensitive).
        assert_eq!(factory_for_seed_protocol_tag("Agni"), Some(AGNI_V3.factory));
        assert_eq!(
            factory_for_seed_protocol_tag("fusionx"),
            Some(FUSIONX_V3.factory)
        );
        assert_eq!(factory_for_seed_protocol_tag("Butter"), Some(BUTTER.factory));
        assert_eq!(
            factory_for_seed_protocol_tag("fluxion-v3"),
            Some(FLUXION_V3.factory)
        );
        assert_eq!(
            factory_for_seed_protocol_tag("v3fork"),
            Some(V3FORK_636EA2.factory)
        );
        assert_eq!(
            factory_for_seed_protocol_tag("uniswap"),
            Some(UNISWAP_V3_MANTLE.factory)
        );
        // Quarantined venue still resolves so seed rows can be recorded.
        assert_eq!(
            factory_for_seed_protocol_tag("cleopatra"),
            Some(CLEOPATRA_CL.factory)
        );
        assert_eq!(
            factory_for_seed_protocol_tag("cleopatra-cl"),
            Some(CLEOPATRA_CL.factory)
        );
        assert_eq!(factory_for_seed_protocol_tag(""), None);
        assert_eq!(factory_for_seed_protocol_tag("unknown-dex"), None);
    }

    #[test]
    fn split_quarantine_moves_cleopatra_out_of_loadable() {
        let pools = vec![
            CandidatePool {
                protocol: V3_UNIVERSE_PROTOCOL_LABEL.into(),
                factory: AGNI_V3.factory,
                pool: Address::with_last_byte(0x01),
                token0: Address::with_last_byte(0x02),
                token1: Address::with_last_byte(0x03),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: None,
            },
            CandidatePool {
                protocol: V3_UNIVERSE_PROTOCOL_LABEL.into(),
                factory: CLEOPATRA_CL.factory,
                pool: Address::with_last_byte(0xAA),
                token0: Address::with_last_byte(0x02),
                token1: Address::with_last_byte(0x03),
                fee_tier: Some(500),
                bin_step: None,
                creation_block: None,
            },
        ];
        let (loadable, q) = split_quarantined_v3_candidates(pools);
        assert_eq!(loadable.len(), 1);
        assert_eq!(loadable[0].factory, AGNI_V3.factory);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].pool, Address::with_last_byte(0xAA));
        assert!(q[0].reason.contains("WHI-938"));
    }

    #[test]
    fn funnel_reports_zeros_loudly() {
        let pools = vec![CandidatePool {
            protocol: V3_UNIVERSE_PROTOCOL_LABEL.into(),
            factory: AGNI_V3.factory,
            pool: Address::with_last_byte(0x01),
            token0: Address::with_last_byte(0x02),
            token1: Address::with_last_byte(0x03),
            fee_tier: Some(500),
            bin_step: None,
            creation_block: None,
        }];
        let report = format_v3_factory_funnel("enumerated", &pools);
        assert!(report.contains("agni-v3"));
        assert!(report.contains("ZERO"));
        assert!(report.contains("fusionx-v3"));
        assert!(report.contains("loadable drop-in"));
        let counts = drop_in_v3_funnel_counts(&pools);
        assert_eq!(counts.iter().find(|c| c.0 == "agni-v3").unwrap().2, 1);
        assert_eq!(counts.iter().find(|c| c.0 == "butter").unwrap().2, 0);
        assert!(counts.iter().all(|c| c.0 != "cleopatra-cl"));
    }

    #[test]
    fn create2_domains_are_not_shared_across_venues() {
        // Documented WHI-765 constraint: Agni and FusionX have distinct deployers.
        assert_ne!(AGNI_V3.create2_deployer, FUSIONX_V3.create2_deployer);
        assert_ne!(AGNI_V3.factory, FUSIONX_V3.factory);
    }
}
