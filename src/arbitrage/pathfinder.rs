use std::collections::{HashMap, HashSet, VecDeque};

use alloy::primitives::Address;
use itertools::Itertools;
use petgraph::visit::EdgeRef;

use super::data::PoolEdge;
use super::graph::PoolGraph;

/// Strategy-level hop cap (ARB_PATHS_MANTLE.md §4: 93.5% of arb is 2–3 pools).
///
/// Single source of truth for pathfinder defaults, discovery knobs, and bot CLI.
/// Do not introduce a second literal `3` hop cap elsewhere in the tree.
pub const DEFAULT_MAX_HOPS: usize = 3;

#[derive(Debug, Clone, Copy)]
pub struct PathHop {
    pub pool_address: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub fee_bps: u32,
}

#[derive(Debug, Clone)]
pub struct ArbitragePath {
    pub hops: Vec<PathHop>,
}

#[derive(Debug, Clone, Copy)]
pub struct PathConstraints {
    pub max_length: usize,
    pub allow_self_cycle: bool,
    pub required_start_token: Option<Address>,
    pub required_end_token: Option<Address>,
}

impl Default for PathConstraints {
    fn default() -> Self {
        // Unconstrained endpoints remain representable for unit fixtures only.
        // Production / strategy callers must use [`PathConstraints::settlement_cycle`].
        Self {
            max_length: DEFAULT_MAX_HOPS,
            allow_self_cycle: false,
            required_start_token: None,
            required_end_token: None,
        }
    }
}

impl PathConstraints {
    /// Closed settlement cycle: start and end must equal `settlement_asset`.
    ///
    /// This is the only opportunity primitive the engine may emit or execute
    /// (WHI-529). Open `A -> B` paths are unit-invalid under profit arithmetic.
    pub fn settlement_cycle(settlement_asset: Address, max_hops: usize) -> Self {
        Self {
            max_length: max_hops,
            allow_self_cycle: false,
            required_start_token: Some(settlement_asset),
            required_end_token: Some(settlement_asset),
        }
    }
}

pub struct PathFinder<'a> {
    graph: &'a PoolGraph,
    constraints: PathConstraints,
}

impl<'a> PathFinder<'a> {
    pub fn new(graph: &'a PoolGraph, constraints: PathConstraints) -> Self {
        debug_assert!(
            match (
                constraints.required_start_token,
                constraints.required_end_token
            ) {
                (None, None) => true,
                (Some(s), Some(e)) => s == e,
                _ => false,
            },
            "PathConstraints: if either endpoint is set, both must be Some and equal (settlement cycle)"
        );
        Self { graph, constraints }
    }

    pub fn find_cycles(&self) -> Vec<ArbitragePath> {
        let mut cycles = Vec::new();

        for start in self.graph.graph.node_indices() {
            let mut queue = VecDeque::new();

            let start_token = match self.graph.token_of(start) {
                Some(token) => *token,
                None => continue,
            };

            if let Some(required_start) = self.constraints.required_start_token {
                if start_token != required_start {
                    continue;
                }
            }

            let mut seen_tokens = HashSet::new();
            seen_tokens.insert(start_token);

            queue.push_back((start, vec![], seen_tokens));

            while let Some((node, path, tokens_seen)) = queue.pop_front() {
                if path.len() >= self.constraints.max_length {
                    continue;
                }

                for edge in self.graph.graph.edges(node) {
                    let target = edge.target();
                    let target_token = match self.graph.token_of(target) {
                        Some(token) => *token,
                        None => continue,
                    };

                    // Reject immediate same-pool reversals (WHI-529): preserve the
                    // adjacent pool-address uniqueness property of the deleted
                    // open two-pool misprice finder.
                    let pool_edge = edge.weight();
                    if path
                        .last()
                        .is_some_and(|prev: &PoolEdge| prev.pool_address == pool_edge.pool_address)
                    {
                        continue;
                    }

                    if !self.constraints.allow_self_cycle
                        && tokens_seen.contains(&target_token)
                        && target != start
                    {
                        continue;
                    }

                    let mut new_path = path.clone();
                    new_path.push(pool_edge.clone());

                    if target == start {
                        if self.constraints.allow_self_cycle || !new_path.is_empty() {
                            if let Some(arbitrage_path) = convert_edges(&new_path) {
                                if path_matches_constraints(&arbitrage_path, &self.constraints) {
                                    cycles.push(arbitrage_path);
                                }
                            }
                        }
                        continue;
                    }

                    let mut next_tokens = tokens_seen.clone();
                    next_tokens.insert(target_token);
                    queue.push_back((target, new_path, next_tokens));
                }
            }
        }

        let raw_cycles = cycles.len();
        let mut signature_counts: HashMap<Vec<(Address, Address, Address)>, usize> =
            HashMap::new();
        for path in &cycles {
            let key: Vec<_> = path
                .hops
                .iter()
                .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
                .collect();
            *signature_counts.entry(key).or_default() += 1;
        }
        let mut top_repeated: Vec<_> = signature_counts
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(sig, count)| (sig.clone(), *count))
            .collect();
        top_repeated.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_repeated.truncate(5);

        // Dedup on rotation-canonical ordered hop sequence (WHI-529).
        // Opposite-direction cycles remain distinct because (token_in, token_out)
        // orientations differ per hop.
        let deduped: Vec<ArbitragePath> = cycles
            .into_iter()
            .unique_by(canonical_cycle_key)
            .collect();
        let after_dedup = deduped.len();

        tracing::debug!(
            target: "arb.pathfinder",
            raw_cycles,
            after_dedup,
            top_repeated = ?top_repeated
                .iter()
                .map(|(sig, count)| format!("count={count} hops={}", sig.len()))
                .collect::<Vec<_>>(),
            "find_cycles dedup summary"
        );

        deduped
    }
}

/// Lexicographically smallest rotation of the ordered `(pool, token_in, token_out)` hop sequence.
///
/// Rotation only — opposite directions (`WMNT→A→B→WMNT` vs `WMNT→B→A→WMNT`) stay distinct.
pub fn canonical_cycle_key(path: &ArbitragePath) -> Vec<(Address, Address, Address)> {
    let hops: Vec<(Address, Address, Address)> = path
        .hops
        .iter()
        .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
        .collect();
    if hops.is_empty() {
        return hops;
    }
    let n = hops.len();
    (0..n)
        .map(|i| {
            let mut rotated = Vec::with_capacity(n);
            rotated.extend_from_slice(&hops[i..]);
            rotated.extend_from_slice(&hops[..i]);
            rotated
        })
        .min()
        .unwrap_or(hops)
}

fn convert_edges(edges: &[PoolEdge]) -> Option<ArbitragePath> {
    if edges.is_empty() {
        return None;
    }

    let hops = edges
        .iter()
        .map(|edge| PathHop {
            pool_address: edge.pool_address,
            token_in: edge.token_in.address,
            token_out: edge.token_out.address,
            fee_bps: edge.fee_bps,
        })
        .collect();

    Some(ArbitragePath { hops })
}

fn path_matches_constraints(path: &ArbitragePath, constraints: &PathConstraints) -> bool {
    if let Some(required_start) = constraints.required_start_token {
        if let Some(first_hop) = path.hops.first() {
            if first_hop.token_in != required_start {
                return false;
            }
        } else {
            return false;
        }
    }

    if let Some(required_end) = constraints.required_end_token {
        if let Some(last_hop) = path.hops.last() {
            if last_hop.token_out != required_end {
                return false;
            }
        } else {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::{
        data::{PoolEdge, TokenState},
        graph::PoolGraph,
    };
    use alloy::primitives::Address;
    use petgraph::graph::DiGraph;

    fn addr(b: u8) -> Address {
        let mut raw = [0u8; 20];
        raw[19] = b;
        Address::from(raw)
    }

    fn edge(pool: u8, from: u8, to: u8) -> PoolEdge {
        PoolEdge {
            pool_address: addr(pool),
            token_in: TokenState::new(addr(from), 18),
            token_out: TokenState::new(addr(to), 18),
            fee_bps: 3000,
        }
    }

    fn edge_with_decimals(pool: u8, from: u8, from_dec: u8, to: u8, to_dec: u8) -> PoolEdge {
        PoolEdge {
            pool_address: addr(pool),
            token_in: TokenState::new(addr(from), from_dec),
            token_out: TokenState::new(addr(to), to_dec),
            fee_bps: 3000,
        }
    }

    fn simple_graph() -> PoolGraph {
        let mut graph = DiGraph::new();
        let a = graph.add_node(addr(1));
        let b = graph.add_node(addr(2));
        let c = graph.add_node(addr(3));

        graph.add_edge(a, b, edge(10, 1, 2));
        graph.add_edge(b, c, edge(11, 2, 3));
        graph.add_edge(c, a, edge(12, 3, 1));

        PoolGraph {
            node_tokens: vec![(a, addr(1)), (b, addr(2)), (c, addr(3))]
                .into_iter()
                .collect(),
            node_decimals: vec![(a, 18), (b, 18), (c, 18)].into_iter().collect(),
            graph,
        }
    }

    fn parallel_pool_graph() -> PoolGraph {
        let mut graph = DiGraph::new();
        let token_a = graph.add_node(addr(1));
        let token_b = graph.add_node(addr(2));

        graph.add_edge(token_a, token_b, edge(10, 1, 2));
        graph.add_edge(token_b, token_a, edge(10, 2, 1));
        graph.add_edge(token_a, token_b, edge(11, 1, 2));
        graph.add_edge(token_b, token_a, edge(11, 2, 1));

        PoolGraph {
            node_tokens: vec![(token_a, addr(1)), (token_b, addr(2))]
                .into_iter()
                .collect(),
            node_decimals: vec![(token_a, 18), (token_b, 18)].into_iter().collect(),
            graph,
        }
    }

    #[test]
    fn find_cycles_returns_three_hop_cycle() {
        // WHI-529: rotation-canonical dedup collapses the three start-token
        // rotations of one triangle into a single ordered cycle key.
        let graph = simple_graph();
        let finder = PathFinder::new(&graph, PathConstraints::default());
        let cycles = finder.find_cycles();
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0].hops.len(), 3);
    }

    #[test]
    fn find_cycles_finds_parallel_pool_two_cycle() {
        // WHI-550 regression: two parallel pools over the same pair must yield
        // a 2-hop closed cycle via find_cycles.
        let pool_graph = parallel_pool_graph();
        let finder = PathFinder::new(
            &pool_graph,
            PathConstraints::settlement_cycle(addr(1), 2),
        );

        let cycles = finder.find_cycles();

        assert!(
            cycles.iter().any(|path| {
                path.hops.len() == 2
                    && path.hops[0].pool_address == addr(10)
                    && path.hops[1].pool_address == addr(11)
            }),
            "expected 2-hop cycle via pools 10 then 11; got {:?}",
            cycles
                .iter()
                .map(|p| p
                    .hops
                    .iter()
                    .map(|h| h.pool_address)
                    .collect::<Vec<_>>())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn find_cycles_rejects_immediate_same_pool_reversal() {
        let pool_graph = parallel_pool_graph();
        let finder = PathFinder::new(
            &pool_graph,
            PathConstraints::settlement_cycle(addr(1), 2),
        );
        let cycles = finder.find_cycles();
        for path in &cycles {
            for window in path.hops.windows(2) {
                assert_ne!(
                    window[0].pool_address, window[1].pool_address,
                    "adjacent same-pool reversal must be rejected"
                );
            }
        }
    }

    #[test]
    fn find_cycles_under_settlement_never_emits_open_paths() {
        // Open A->B two-hop with mismatched decimals (6 vs 18) must not reach
        // profit arithmetic: settlement-cycle constraints force closed cycles.
        let mut graph = DiGraph::new();
        let usdc = graph.add_node(addr(1)); // 6 decimals
        let wmnt = graph.add_node(addr(2)); // 18 decimals
        let other = graph.add_node(addr(3));

        graph.add_edge(usdc, wmnt, edge_with_decimals(10, 1, 6, 2, 18));
        graph.add_edge(wmnt, other, edge_with_decimals(11, 2, 18, 3, 18));
        // No edge back to usdc — no closed cycle through usdc.

        let pool_graph = PoolGraph {
            node_tokens: vec![(usdc, addr(1)), (wmnt, addr(2)), (other, addr(3))]
                .into_iter()
                .collect(),
            node_decimals: vec![(usdc, 6), (wmnt, 18), (other, 18)]
                .into_iter()
                .collect(),
            graph,
        };

        let finder = PathFinder::new(
            &pool_graph,
            PathConstraints::settlement_cycle(addr(1), DEFAULT_MAX_HOPS),
        );
        let cycles = finder.find_cycles();
        for path in &cycles {
            let first_in = path.hops.first().map(|h| h.token_in);
            let last_out = path.hops.last().map(|h| h.token_out);
            assert_eq!(
                first_in, last_out,
                "settlement-cycle paths must be closed: first.token_in == last.token_out"
            );
            assert_eq!(first_in, Some(addr(1)));
        }
        // Graph has no closed cycle starting at usdc, so result is empty.
        assert!(cycles.is_empty());
    }

    #[test]
    fn canonical_cycle_key_collapses_rotations_keeps_opposite_directions() {
        let a = addr(1);
        let b = addr(2);
        let c = addr(3);
        let p0 = addr(10);
        let p1 = addr(11);
        let p2 = addr(12);

        // Forward: A->B->C->A
        let forward = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: p0,
                    token_in: a,
                    token_out: b,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p1,
                    token_in: b,
                    token_out: c,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p2,
                    token_in: c,
                    token_out: a,
                    fee_bps: 0,
                },
            ],
        };
        // Rotation starting at B
        let rotated = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: p1,
                    token_in: b,
                    token_out: c,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p2,
                    token_in: c,
                    token_out: a,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p0,
                    token_in: a,
                    token_out: b,
                    fee_bps: 0,
                },
            ],
        };
        // Opposite: A->C->B->A
        let reverse = ArbitragePath {
            hops: vec![
                PathHop {
                    pool_address: p2,
                    token_in: a,
                    token_out: c,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p1,
                    token_in: c,
                    token_out: b,
                    fee_bps: 0,
                },
                PathHop {
                    pool_address: p0,
                    token_in: b,
                    token_out: a,
                    fee_bps: 0,
                },
            ],
        };

        assert_eq!(
            canonical_cycle_key(&forward),
            canonical_cycle_key(&rotated),
            "rotations of one cycle must share a key"
        );
        assert_ne!(
            canonical_cycle_key(&forward),
            canonical_cycle_key(&reverse),
            "opposite-direction cycles must stay distinct"
        );
    }

    #[test]
    fn default_max_hops_is_three() {
        assert_eq!(DEFAULT_MAX_HOPS, 3);
        assert_eq!(PathConstraints::default().max_length, DEFAULT_MAX_HOPS);
    }
}
