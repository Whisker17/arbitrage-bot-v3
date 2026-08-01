//! Protocol multi-select parsing for the merged bot CLI (WHI-728 / WHI-527.3).

use crate::amms::amm::AMM;
use crate::execution::ProtocolKind;
use crate::service::protocol::{AgniV2Protocol, AgniV3Protocol, MoeProtocol, Protocol};
use crate::state_space::PoolProtocol;
use eyre::{eyre, Result};
use std::fmt;
use std::str::FromStr;

/// One selectable protocol for `--protocols`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectedProtocol {
    AgniV2,
    AgniV3,
    Moe,
}

impl SelectedProtocol {
    /// Default multi-select: all three protocols concurrent.
    pub fn all() -> Vec<Self> {
        vec![Self::AgniV2, Self::AgniV3, Self::Moe]
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgniV2 => AgniV2Protocol::NAME,
            Self::AgniV3 => AgniV3Protocol::NAME,
            Self::Moe => MoeProtocol::NAME,
        }
    }

    pub fn pool_protocol(self) -> PoolProtocol {
        match self {
            Self::AgniV2 => AgniV2Protocol::pool_universe_protocol(),
            Self::AgniV3 => AgniV3Protocol::pool_universe_protocol(),
            Self::Moe => MoeProtocol::pool_universe_protocol(),
        }
    }

    pub fn protocol_kind(self) -> ProtocolKind {
        match self {
            Self::AgniV2 => AgniV2Protocol::protocol_kind(),
            Self::AgniV3 => AgniV3Protocol::protocol_kind(),
            Self::Moe => MoeProtocol::protocol_kind(),
        }
    }

    /// Whether an in-memory [`AMM`] belongs to this selected protocol.
    pub fn matches_amm(self, amm: &AMM) -> bool {
        match (self, amm) {
            (Self::AgniV2, AMM::UniswapV2Pool(_)) => true,
            (Self::AgniV3, AMM::AgniPool(_)) => true,
            (Self::Moe, AMM::MoeLbPair(_)) => true,
            _ => false,
        }
    }
}

impl fmt::Display for SelectedProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SelectedProtocol {
    type Err = eyre::Report;

    fn from_str(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agni-v2" | "v2" | "uniswap-v2" => Ok(Self::AgniV2),
            "agni-v3" | "v3" | "agni" => Ok(Self::AgniV3),
            "moe" | "moe-lb" | "moelb" => Ok(Self::Moe),
            other => Err(eyre!(
                "unknown protocol `{other}` (expected agni-v2, agni-v3, moe)"
            )),
        }
    }
}

/// Parse a comma-separated `--protocols` value.
///
/// Empty / whitespace-only input yields the default all-three set.
pub fn parse_protocols_flag(raw: &str) -> Result<Vec<SelectedProtocol>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(SelectedProtocol::all());
    }
    let mut out = Vec::new();
    for part in trimmed.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let proto = SelectedProtocol::from_str(part)?;
        if !out.contains(&proto) {
            out.push(proto);
        }
    }
    if out.is_empty() {
        return Err(eyre!("--protocols must select at least one protocol"));
    }
    Ok(out)
}

/// Map an AMM variant to its gas-profile [`ProtocolKind`].
pub fn protocol_kind_of_amm(amm: &AMM) -> ProtocolKind {
    match amm {
        AMM::UniswapV2Pool(_) => ProtocolKind::V2,
        AMM::AgniPool(_) | AMM::UniswapV3Pool(_) => ProtocolKind::V3,
        AMM::MoeLbPair(_) => ProtocolKind::Moe,
    }
}

/// Keep only pools belonging to any of the selected protocols.
pub fn filter_pools_by_protocols(pools: &[AMM], selected: &[SelectedProtocol]) -> Vec<AMM> {
    pools
        .iter()
        .filter(|amm| selected.iter().any(|s| s.matches_amm(amm)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_default_all_three() {
        let p = parse_protocols_flag("").unwrap();
        assert_eq!(p, SelectedProtocol::all());
        assert_eq!(p.len(), 3);
    }

    #[test]
    fn parse_comma_list_dedups() {
        let p = parse_protocols_flag("agni-v2, moe, agni-v2").unwrap();
        assert_eq!(p, vec![SelectedProtocol::AgniV2, SelectedProtocol::Moe]);
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(
            SelectedProtocol::from_str("v2").unwrap(),
            SelectedProtocol::AgniV2
        );
        assert_eq!(
            SelectedProtocol::from_str("agni").unwrap(),
            SelectedProtocol::AgniV3
        );
        assert_eq!(
            SelectedProtocol::from_str("moe-lb").unwrap(),
            SelectedProtocol::Moe
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        assert!(parse_protocols_flag("foo").is_err());
    }
}
