//! Offline pool-universe filters (WHI-793).
//!
//! Two pure stages applied after enumeration:
//! 1. **TVL floor** — pools whose WMNT-equivalent value is below the floor are
//!    dropped; pools with no valuation go to an explicit quarantine list (never
//!    silently included or silently dropped).
//! 2. **≤N-hop settlement cycles** — keep a pool only if it appears in at least
//!    one ordered cycle that starts and ends at the settlement asset with hop
//!    count in `{2, …, max_hops}`. Uses [`EFFECTIVE_MAX_HOPS`] as the default
//!    hop cap. Applied after TVL and iterated to a fixed point (dropping a pool
//!    can orphan others).

use crate::state_space::EFFECTIVE_MAX_HOPS;
use alloy::primitives::{Address, U256};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Default TVL floor in **WMNT wei** (1000 WMNT).
///
/// Policy note (recorded in the meta sidecar): there is no USD oracle on the
/// generator path, so the operator floor of "$1000" is expressed as a
/// WMNT-equivalent. Operators can override via `--min-tvl-wmnt-wei`.
pub const DEFAULT_MIN_TVL_WMNT_WEI: u128 = 1_000 * 10u128.pow(18);

/// Valuation / filter policy version stamped into the meta sidecar.
pub const FILTER_POLICY_VERSION: u32 = 1;

/// A pool candidate after enumeration (before or after filters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidatePool {
    pub protocol: String,
    pub factory: Address,
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_tier: Option<u32>,
    pub bin_step: Option<u16>,
    pub creation_block: Option<u64>,
}

/// Why a pool was quarantined (valuation could not be established).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineEntry {
    pub pool: Address,
    pub protocol: String,
    pub reason: String,
}

/// Stage-by-stage funnel counts (printed by the generator).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FunnelCounts {
    pub enumerated: usize,
    pub tvl_surviving: usize,
    pub tvl_rejected: usize,
    pub quarantined: usize,
    pub cycle_surviving: usize,
    pub cycle_rejected: usize,
    pub emitted: usize,
}

/// Result of applying TVL + cycle filters.
#[derive(Debug, Clone)]
pub struct FilterResult {
    pub kept: Vec<CandidatePool>,
    pub quarantine: Vec<QuarantineEntry>,
    pub funnel: FunnelCounts,
    /// Pools that cleared TVL but were dropped by the cycle fixed-point.
    pub cycle_rejected: Vec<CandidatePool>,
    /// Pools rejected solely for being under the TVL floor (valued, not quarantined).
    pub tvl_rejected: Vec<CandidatePool>,
}

/// Apply TVL floor then settlement-cycle fixed-point prune.
///
/// `valuations`: `Some(wmnt_wei)` = established value; `None` = quarantine.
/// Missing map entries are treated as `None` (quarantine with reason
/// `valuation_missing`).
pub fn apply_universe_filters(
    candidates: Vec<CandidatePool>,
    valuations: &HashMap<Address, Option<U256>>,
    min_tvl_wmnt_wei: U256,
    settlement: Address,
    max_hops: u8,
) -> FilterResult {
    let enumerated = candidates.len();
    let mut quarantine = Vec::new();
    let mut tvl_surviving = Vec::new();
    let mut tvl_rejected = Vec::new();

    for c in candidates {
        match valuations.get(&c.pool) {
            None => {
                quarantine.push(QuarantineEntry {
                    pool: c.pool,
                    protocol: c.protocol.clone(),
                    reason: "valuation_missing".into(),
                });
            }
            Some(None) => {
                quarantine.push(QuarantineEntry {
                    pool: c.pool,
                    protocol: c.protocol.clone(),
                    reason: "valuation_unavailable".into(),
                });
            }
            Some(Some(tvl)) if *tvl < min_tvl_wmnt_wei => {
                tvl_rejected.push(c);
            }
            Some(Some(_)) => tvl_surviving.push(c),
        }
    }

    let after_tvl = tvl_surviving.len();
    let (kept, cycle_rejected) =
        filter_settlement_cycles(tvl_surviving, settlement, max_hops);

    let funnel = FunnelCounts {
        enumerated,
        tvl_surviving: after_tvl,
        tvl_rejected: tvl_rejected.len(),
        quarantined: quarantine.len(),
        cycle_surviving: kept.len(),
        cycle_rejected: cycle_rejected.len(),
        emitted: kept.len(),
    };

    FilterResult {
        kept,
        quarantine,
        funnel,
        cycle_rejected,
        tvl_rejected,
    }
}

/// Keep only pools that appear in at least one ordered settlement cycle of
/// length `2..=max_hops`, iterating to a fixed point.
///
/// Returns `(kept, rejected)`.
pub fn filter_settlement_cycles(
    pools: Vec<CandidatePool>,
    settlement: Address,
    max_hops: u8,
) -> (Vec<CandidatePool>, Vec<CandidatePool>) {
    let max_hops = if max_hops == 0 {
        EFFECTIVE_MAX_HOPS
    } else {
        max_hops
    };

    let original = pools;
    let mut active: BTreeSet<Address> = original.iter().map(|p| p.pool).collect();
    loop {
        let subset: Vec<CandidatePool> = original
            .iter()
            .filter(|p| active.contains(&p.pool))
            .cloned()
            .collect();
        let on_cycle = pools_on_settlement_cycles(&subset, settlement, max_hops);
        if on_cycle == active {
            break;
        }
        active = on_cycle;
    }
    let (kept, rejected): (Vec<_>, Vec<_>) = original
        .into_iter()
        .partition(|p| active.contains(&p.pool));
    (kept, rejected)
}

/// Addresses of pools that participate in ≥1 ordered settlement cycle of
/// hop count in `2..=max_hops` over the given pool set.
pub fn pools_on_settlement_cycles(
    pools: &[CandidatePool],
    settlement: Address,
    max_hops: u8,
) -> BTreeSet<Address> {
    if max_hops < 2 || pools.is_empty() {
        return BTreeSet::new();
    }

    // token -> list of (other_token, pool_address)
    let mut adj: HashMap<Address, Vec<(Address, Address)>> = HashMap::new();
    for p in pools {
        if p.token0 == Address::ZERO || p.token1 == Address::ZERO || p.token0 == p.token1 {
            continue;
        }
        adj.entry(p.token0).or_default().push((p.token1, p.pool));
        adj.entry(p.token1).or_default().push((p.token0, p.pool));
    }

    let mut on_cycle: BTreeSet<Address> = BTreeSet::new();

    // DFS: path of tokens (start = settlement), set of used pools, hop count.
    fn dfs(
        current: Address,
        settlement: Address,
        hops: u8,
        max_hops: u8,
        path_tokens: &mut Vec<Address>,
        used_pools: &mut Vec<Address>,
        adj: &HashMap<Address, Vec<(Address, Address)>>,
        on_cycle: &mut BTreeSet<Address>,
    ) {
        let Some(edges) = adj.get(&current) else {
            return;
        };
        for &(next, pool) in edges {
            if used_pools.contains(&pool) {
                continue;
            }
            let next_hops = hops + 1;
            // Close a cycle back to settlement with hop count in 2..=max_hops.
            if next == settlement && next_hops >= 2 && next_hops <= max_hops {
                for p in used_pools.iter().copied() {
                    on_cycle.insert(p);
                }
                on_cycle.insert(pool);
                continue;
            }
            if next_hops >= max_hops {
                continue;
            }
            // Do not revisit intermediate tokens (simple cycles).
            if path_tokens.contains(&next) {
                continue;
            }
            path_tokens.push(next);
            used_pools.push(pool);
            dfs(
                next,
                settlement,
                next_hops,
                max_hops,
                path_tokens,
                used_pools,
                adj,
                on_cycle,
            );
            used_pools.pop();
            path_tokens.pop();
        }
    }

    let mut path_tokens = vec![settlement];
    let mut used_pools = Vec::new();
    dfs(
        settlement,
        settlement,
        0,
        max_hops,
        &mut path_tokens,
        &mut used_pools,
        &adj,
        &mut on_cycle,
    );
    on_cycle
}

/// Per-protocol counts for funnel reporting.
pub fn count_by_protocol(pools: &[CandidatePool]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for p in pools {
        *m.entry(p.protocol.clone()).or_insert(0) += 1;
    }
    m
}

/// Re-export for callers that want the strategy hop cap without importing snapshot.
pub fn default_max_hops() -> u8 {
    EFFECTIVE_MAX_HOPS
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use std::collections::HashSet;

    const WMNT: Address = address!("00000000000000000000000000000000000000aa");
    const T1: Address = address!("0000000000000000000000000000000000000001");
    const T2: Address = address!("0000000000000000000000000000000000000002");
    const T3: Address = address!("0000000000000000000000000000000000000003");
    const T4: Address = address!("0000000000000000000000000000000000000004");

    fn pool(id: u8, a: Address, b: Address) -> CandidatePool {
        CandidatePool {
            protocol: "test".into(),
            factory: address!("00000000000000000000000000000000000000ff"),
            pool: Address::with_last_byte(id),
            token0: a,
            token1: b,
            fee_tier: None,
            bin_step: None,
            creation_block: None,
        }
    }

    #[test]
    fn two_hop_wmnt_cycle_keeps_both_pools() {
        // Two distinct pools on WMNT-T1 form a 2-hop arb cycle.
        let p1 = pool(0x10, WMNT, T1);
        let p2 = pool(0x11, WMNT, T1);
        let (kept, rejected) =
            filter_settlement_cycles(vec![p1, p2], WMNT, EFFECTIVE_MAX_HOPS);
        assert_eq!(kept.len(), 2);
        assert!(rejected.is_empty());
    }

    #[test]
    fn three_hop_cycle_kept() {
        let pools = vec![
            pool(0x21, WMNT, T1),
            pool(0x22, T1, T2),
            pool(0x23, T2, WMNT),
        ];
        let (kept, rejected) = filter_settlement_cycles(pools, WMNT, EFFECTIVE_MAX_HOPS);
        assert_eq!(kept.len(), 3);
        assert!(rejected.is_empty());
    }

    #[test]
    fn four_hop_only_pool_excluded() {
        // Valid 2-hop pair + a 4-hop-only path WMNT-T2-T3-T4-WMNT.
        let two_a = pool(0x30, WMNT, T1);
        let two_b = pool(0x31, WMNT, T1);
        let h1 = pool(0x32, WMNT, T2);
        let h2 = pool(0x33, T2, T3);
        let h3 = pool(0x34, T3, T4);
        let h4 = pool(0x35, T4, WMNT);

        let pools = vec![two_a, two_b, h1, h2, h3, h4];
        let (kept, rejected) = filter_settlement_cycles(pools, WMNT, EFFECTIVE_MAX_HOPS);

        let kept_ids: HashSet<_> = kept.iter().map(|p| p.pool).collect();
        assert!(kept_ids.contains(&Address::with_last_byte(0x30)));
        assert!(kept_ids.contains(&Address::with_last_byte(0x31)));
        // Interior 4-hop-only edges must not survive with max_hops=3.
        assert!(!kept_ids.contains(&Address::with_last_byte(0x33)));
        assert!(!kept_ids.contains(&Address::with_last_byte(0x34)));
        assert!(!rejected.is_empty());
    }

    #[test]
    fn tvl_rejection_orphans_cycle_partner() {
        // Triangle WMNT-T1-T2-WMNT. Drop the T1-T2 leg via TVL → remaining legs
        // cannot form a cycle and must be cycle-rejected (fixed point).
        let a = pool(0x41, WMNT, T1);
        let b = pool(0x42, T1, T2);
        let c = pool(0x43, T2, WMNT);

        let mut vals = HashMap::new();
        let high = U256::from(DEFAULT_MIN_TVL_WMNT_WEI);
        vals.insert(a.pool, Some(high));
        vals.insert(b.pool, Some(U256::from(1u64))); // under floor
        vals.insert(c.pool, Some(high));

        let result = apply_universe_filters(
            vec![a, b, c],
            &vals,
            U256::from(DEFAULT_MIN_TVL_WMNT_WEI),
            WMNT,
            EFFECTIVE_MAX_HOPS,
        );
        assert_eq!(result.funnel.tvl_rejected, 1);
        assert_eq!(result.funnel.tvl_surviving, 2);
        assert!(result.kept.is_empty(), "orphaned triangle legs must drop");
        assert_eq!(result.cycle_rejected.len(), 2);
    }

    #[test]
    fn missing_valuation_goes_to_quarantine_not_kept() {
        let a = pool(0x50, WMNT, T1);
        let b = pool(0x51, WMNT, T1);
        let mut vals = HashMap::new();
        vals.insert(a.pool, Some(U256::from(DEFAULT_MIN_TVL_WMNT_WEI)));
        // b missing entirely
        let result = apply_universe_filters(
            vec![a, b.clone()],
            &vals,
            U256::from(DEFAULT_MIN_TVL_WMNT_WEI),
            WMNT,
            EFFECTIVE_MAX_HOPS,
        );
        assert_eq!(result.quarantine.len(), 1);
        assert_eq!(result.quarantine[0].pool, b.pool);
        assert_eq!(result.quarantine[0].reason, "valuation_missing");
        // a alone cannot form a 2-hop cycle
        assert!(result.kept.is_empty());
    }

    #[test]
    fn explicit_none_valuation_quarantines_with_unavailable_reason() {
        let a = pool(0x60, WMNT, T1);
        let mut vals = HashMap::new();
        vals.insert(a.pool, None);
        let result = apply_universe_filters(
            vec![a],
            &vals,
            U256::from(DEFAULT_MIN_TVL_WMNT_WEI),
            WMNT,
            EFFECTIVE_MAX_HOPS,
        );
        assert_eq!(result.quarantine.len(), 1);
        assert_eq!(result.quarantine[0].reason, "valuation_unavailable");
        assert!(result.kept.is_empty());
    }

    #[test]
    fn default_max_hops_matches_effective() {
        assert_eq!(default_max_hops(), EFFECTIVE_MAX_HOPS);
        assert_eq!(EFFECTIVE_MAX_HOPS, 3);
    }
}
