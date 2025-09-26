use alloy::primitives::Address;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{HashMap, HashSet};

use crate::amms::amm::AutomatedMarketMaker;
use crate::state_space::StateSpace;

use super::data::{extract_pool, PoolEdge};
use super::error::ArbitrageError;

#[derive(Debug, Clone)]
pub struct PoolGraph {
    pub graph: DiGraph<Address, PoolEdge>,
    pub node_tokens: HashMap<NodeIndex, Address>,
    pub node_decimals: HashMap<NodeIndex, u8>,
}

impl PoolGraph {
    pub fn edges_from(&self, node: NodeIndex) -> impl Iterator<Item = PoolEdge> + '_ {
        self.graph.edges(node).map(|edge| edge.weight().clone())
    }

    pub fn neighbors(&self, node: NodeIndex) -> impl Iterator<Item = NodeIndex> + '_ {
        self.graph.neighbors(node)
    }

    pub fn token_of(&self, node: NodeIndex) -> Option<&Address> {
        self.node_tokens.get(&node)
    }

    pub fn decimals_of(&self, node: NodeIndex) -> Option<&u8> {
        self.node_decimals.get(&node)
    }

    pub fn node_for_token(&self, token: Address) -> Option<NodeIndex> {
        self.node_tokens
            .iter()
            .find_map(|(node, addr)| if *addr == token { Some(*node) } else { None })
    }
}

pub fn build_graph(state_space: &StateSpace) -> Result<PoolGraph, ArbitrageError> {
    let mut graph: DiGraph<Address, PoolEdge> = DiGraph::new();
    let mut node_tokens: HashMap<NodeIndex, Address> = HashMap::new();
    let mut node_decimals: HashMap<NodeIndex, u8> = HashMap::new();
    let mut token_nodes: HashMap<Address, NodeIndex> = HashMap::new();
    let mut edge_seen: HashSet<(Address, Address, Address)> = HashSet::new();

    for pool in state_space.state.values() {
        let extraction = match extract_pool(pool) {
            Ok(ext) => ext,
            Err(e) => {
                tracing::debug!("Skipping pool {:?}: {e:?}", pool.address());
                continue;
            }
        };

        let (forward_edge, reverse_edge) = extraction.into_edges(pool.address());

        let token_a_idx = *token_nodes
            .entry(forward_edge.token_in.address)
            .or_insert_with(|| {
                let idx = graph.add_node(forward_edge.token_in.address);
                node_tokens.insert(idx, forward_edge.token_in.address);
                node_decimals.insert(idx, forward_edge.token_in.decimals);
                idx
            });

        let token_b_idx = *token_nodes
            .entry(forward_edge.token_out.address)
            .or_insert_with(|| {
                let idx = graph.add_node(forward_edge.token_out.address);
                node_tokens.insert(idx, forward_edge.token_out.address);
                node_decimals.insert(idx, forward_edge.token_out.decimals);
                idx
            });

        let key = (
            forward_edge.pool_address,
            forward_edge.token_in.address,
            forward_edge.token_out.address,
        );
        if edge_seen.insert(key) {
            graph.add_edge(token_a_idx, token_b_idx, forward_edge);
        }

        let reverse_key = (
            reverse_edge.pool_address,
            reverse_edge.token_in.address,
            reverse_edge.token_out.address,
        );

        if edge_seen.insert(reverse_key) {
            graph.add_edge(token_b_idx, token_a_idx, reverse_edge);
        }
    }

    Ok(PoolGraph {
        graph,
        node_tokens,
        node_decimals,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::{amm::AMM, uniswap_v3::UniswapV3Pool, Token};
    use crate::arbitrage::data::TokenState;
    use alloy::primitives::Address;

    fn addr(b: u8) -> Address {
        let mut raw = [0u8; 20];
        raw[19] = b;
        Address::from(raw)
    }

    fn mock_pool(id: u8, token_a: TokenState, token_b: TokenState, fee: u32) -> AMM {
        let mut pool = UniswapV3Pool::default();
        pool.address = addr(150 + id);
        pool.token_a = Token::new_with_decimals(token_a.address, token_a.decimals);
        pool.token_b = Token::new_with_decimals(token_b.address, token_b.decimals);
        pool.fee = fee;
        AMM::from(pool)
    }

    #[test]
    fn build_graph_creates_nodes_and_edges() {
        let mut state = StateSpace::default();
        let usdc = TokenState::new(addr(1), 6);
        let wmnt = TokenState::new(addr(2), 18);
        let weth = TokenState::new(addr(3), 18);

        let pool_ab = mock_pool(1, usdc.clone(), wmnt.clone(), 500);
        let pool_bc = mock_pool(2, wmnt.clone(), weth.clone(), 500);

        state.state.insert(pool_ab.address(), pool_ab.clone());
        state.state.insert(pool_bc.address(), pool_bc.clone());

        let graph = build_graph(&state).expect("graph build");
        assert_eq!(graph.graph.node_count(), 3);
        assert_eq!(graph.graph.edge_count(), 4); // bidirectional edges
    }

    #[test]
    fn build_graph_skips_unsupported_pool() {
        let mut state = StateSpace::default();
        let pool = AMM::UniswapV2Pool(Default::default());
        state.state.insert(pool.address(), pool.clone());
        let graph = build_graph(&state).expect("graph build");
        assert_eq!(graph.graph.edge_count(), 0);
    }
}
