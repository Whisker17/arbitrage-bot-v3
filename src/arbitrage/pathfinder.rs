use std::collections::{HashSet, VecDeque};

use alloy::primitives::Address;
use itertools::Itertools;
use petgraph::visit::EdgeRef;

use super::data::PoolEdge;
use super::graph::PoolGraph;

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
        Self {
            max_length: 3,
            allow_self_cycle: false,
            required_start_token: None,
            required_end_token: None,
        }
    }
}

pub struct PathFinder<'a> {
    graph: &'a PoolGraph,
    constraints: PathConstraints,
}

impl<'a> PathFinder<'a> {
    pub fn new(graph: &'a PoolGraph, constraints: PathConstraints) -> Self {
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

                    if !self.constraints.allow_self_cycle
                        && tokens_seen.contains(&target_token)
                        && target != start
                    {
                        continue;
                    }

                    let mut new_path = path.clone();
                    let pool_edge = edge.weight().clone();
                    new_path.push(pool_edge);

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

        cycles
            .into_iter()
            .unique_by(|path| {
                path.hops
                    .iter()
                    .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    pub fn find_two_pool_misprices(&self) -> Vec<ArbitragePath> {
        let mut opportunities = Vec::new();

        for node in self.graph.graph.node_indices() {
            let incoming: Vec<_> = self
                .graph
                .graph
                .edges_directed(node, petgraph::Direction::Incoming)
                .collect();
            let outgoing: Vec<_> = self
                .graph
                .graph
                .edges_directed(node, petgraph::Direction::Outgoing)
                .collect();

            for in_edge in incoming {
                for out_edge in &outgoing {
                    if in_edge.weight().pool_address == out_edge.weight().pool_address {
                        continue;
                    }

                    let hops = vec![in_edge.weight().clone(), out_edge.weight().clone()];
                    if let Some(path) = convert_edges(&hops) {
                        if path_matches_constraints(&path, &self.constraints) {
                            opportunities.push(path);
                        }
                    }
                }
            }
        }

        opportunities
            .into_iter()
            .unique_by(|path| {
                path.hops
                    .iter()
                    .map(|hop| (hop.pool_address, hop.token_in, hop.token_out))
                    .collect::<Vec<_>>()
            })
            .collect()
    }
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

    #[test]
    fn find_cycles_returns_three_hop_cycle() {
        let graph = simple_graph();
        let finder = PathFinder::new(&graph, PathConstraints::default());
        let cycles = finder.find_cycles();
        assert_eq!(cycles.len(), 3);
        assert_eq!(cycles[0].hops.len(), 3);
    }

    #[test]
    fn find_two_pool_misprices_detects_pairs() {
        let graph = simple_graph();
        let finder = PathFinder::new(&graph, PathConstraints::default());
        let misprices = finder.find_two_pool_misprices();
        assert!(!misprices.is_empty());
    }

    #[test]
    fn find_two_pool_misprices_supports_parallel_pools() {
        let mut graph = DiGraph::new();
        let token_a = graph.add_node(addr(1));
        let token_b = graph.add_node(addr(2));

        graph.add_edge(token_a, token_b, edge(10, 1, 2));
        graph.add_edge(token_b, token_a, edge(10, 2, 1));
        graph.add_edge(token_a, token_b, edge(11, 1, 2));
        graph.add_edge(token_b, token_a, edge(11, 2, 1));

        let pool_graph = PoolGraph {
            node_tokens: vec![(token_a, addr(1)), (token_b, addr(2))]
                .into_iter()
                .collect(),
            node_decimals: vec![(token_a, 18), (token_b, 18)].into_iter().collect(),
            graph,
        };
        let finder = PathFinder::new(
            &pool_graph,
            PathConstraints {
                max_length: 2,
                required_start_token: Some(addr(1)),
                required_end_token: Some(addr(1)),
                ..PathConstraints::default()
            },
        );

        let misprices = finder.find_two_pool_misprices();

        assert!(misprices.iter().any(|path| {
            path.hops.len() == 2
                && path.hops[0].pool_address == addr(10)
                && path.hops[1].pool_address == addr(11)
        }));
    }
}
