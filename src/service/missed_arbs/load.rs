//! Loading the external ground-truth arb extract (WHI-999).
//!
//! Split from the analysis because it changes for a different reason: the
//! extract's field names and shapes are an external contract, while the
//! analysis is our own. Nothing here classifies or ranks.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use alloy::primitives::U256;
use serde::Deserialize;

use crate::service::arb_coverage::{normalize_address, ArbCoverageError};

use super::MissedArbEvent;

#[derive(Debug, Deserialize)]
struct ArbJsonLine {
    #[serde(default)]
    block: Option<u64>,
    #[serde(default)]
    hash: Option<String>,
    #[serde(default)]
    path: Vec<String>,
    #[serde(default, rename = "nSwaps")]
    n_swaps: Option<u32>,
    /// Net-positive entity legs; the largest is the settlement asset.
    #[serde(default)]
    pos: Vec<PosLeg>,
}

/// `pos` legs are `[token, amount]` pairs in the external extract.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PosLeg {
    Pair(String, String),
    Object {
        token: String,
        #[serde(default)]
        amount: Option<String>,
    },
}

impl PosLeg {
    fn token(&self) -> &str {
        match self {
            Self::Pair(t, _) => t,
            Self::Object { token, .. } => token,
        }
    }

    fn amount(&self) -> Option<&str> {
        match self {
            Self::Pair(_, a) => Some(a),
            Self::Object { amount, .. } => amount.as_deref(),
        }
    }
}

/// Settlement asset = the `pos` leg with the largest amount.
///
/// Same rule as [`crate::service::ground_truth::settlement_asset_from_pos`],
/// re-derived here because this loader reads the raw external extract rather
/// than collector output. Amounts are compared as `U256` so a leg wider than
/// `u128` cannot silently sort as zero.
fn settlement_from_pos(pos: &[PosLeg]) -> Option<String> {
    let mut best: Option<(String, U256)> = None;
    for leg in pos {
        let token = normalize_address(leg.token());
        if token.is_empty() {
            continue;
        }
        let amount = leg
            .amount()
            .and_then(parse_amount_u256)
            .unwrap_or(U256::ZERO);
        match &best {
            Some((_, b)) if amount <= *b => {}
            _ => best = Some((token, amount)),
        }
    }
    best.map(|(t, _)| t)
}

fn parse_amount_u256(s: &str) -> Option<U256> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return U256::from_str_radix(hex, 16).ok();
    }
    s.parse::<U256>().ok()
}

/// Events plus what the loader had to drop, so a skip can never pass as a zero.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LoadedArbEvents {
    pub events: Vec<MissedArbEvent>,
    /// Non-blank JSONL rows read.
    pub rows_seen: usize,
    /// Rows whose `path` was empty or undecodable. They carry no universe
    /// information, so they cannot be classified — but they are counted and
    /// reported rather than silently vanishing (the attribution routes the same
    /// case to `unattributable`, so a silent drop would understate the residual).
    pub skipped_empty_path: usize,
}

/// Load ground-truth arbs from the external JSONL extract.
///
/// Recognized fields: `block`, `hash`, `path` (ordered pools), `nSwaps`, `pos`.
pub fn load_missed_arb_events(path: &Path) -> Result<LoadedArbEvents, ArbCoverageError> {
    let file = File::open(path).map_err(|source| ArbCoverageError::Io {
        path: path.display().to_string(),
        source,
    })?;
    let reader = BufReader::new(file);
    let mut loaded = LoadedArbEvents::default();
    for (i, line) in reader.lines().enumerate() {
        let line = line.map_err(|source| ArbCoverageError::Io {
            path: path.display().to_string(),
            source,
        })?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        loaded.rows_seen += 1;
        let parsed: ArbJsonLine =
            serde_json::from_str(line).map_err(|source| ArbCoverageError::Json {
                path: format!("{}:line {}", path.display(), i + 1),
                source,
            })?;
        let pools: Vec<String> = parsed
            .path
            .iter()
            .map(|p| normalize_address(p))
            .filter(|p| !p.is_empty())
            .collect();
        if pools.is_empty() {
            loaded.skipped_empty_path += 1;
            continue;
        }
        let hop_count = parsed.n_swaps.unwrap_or(pools.len() as u32);
        loaded.events.push(MissedArbEvent {
            block: parsed.block,
            tx_hash: parsed.hash.map(|h| h.to_ascii_lowercase()),
            pools,
            hop_count,
            settlement_asset: settlement_from_pos(&parsed.pos),
        });
    }
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_rows_without_a_decodable_path_instead_of_dropping_them() {
        // TempDir, like the neighbouring arb_coverage / unified_universe tests:
        // a fixed path leaks and collides with a concurrent run.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arbs.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"block\":1,\"path\":[\"0xAA\"],\"nSwaps\":2,\"pos\":[[\"0xbb\",\"5\"]]}\n",
                "\n",
                "{\"block\":2,\"path\":[],\"nSwaps\":3}\n",
                "{\"block\":3,\"nSwaps\":3}\n",
            ),
        )
        .unwrap();

        let loaded = load_missed_arb_events(&path).unwrap();
        assert_eq!(loaded.rows_seen, 3, "blank lines are not rows");
        assert_eq!(loaded.events.len(), 1);
        assert_eq!(loaded.skipped_empty_path, 2);
        // Addresses are normalized on load.
        assert_eq!(loaded.events[0].pools, vec!["0xaa".to_string()]);
        assert_eq!(loaded.events[0].settlement_asset.as_deref(), Some("0xbb"));
    }

    #[test]
    fn hop_count_falls_back_to_path_length_when_n_swaps_is_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arbs.jsonl");
        std::fs::write(&path, "{\"block\":1,\"path\":[\"0xa\",\"0xb\",\"0xc\"]}\n").unwrap();
        let loaded = load_missed_arb_events(&path).unwrap();
        assert_eq!(loaded.events[0].hop_count, 3);
    }

    #[test]
    fn a_malformed_row_fails_the_load_rather_than_being_skipped() {
        // A parse error is not a "row with no path" — silently skipping it would
        // shrink the denominator without saying so.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arbs.jsonl");
        std::fs::write(&path, "{\"block\":1,\"path\":\"not-an-array\"}\n").unwrap();
        let err = load_missed_arb_events(&path).unwrap_err();
        assert!(
            matches!(err, ArbCoverageError::Json { .. }),
            "expected a JSON error, got {err:?}"
        );
    }

    #[test]
    fn settlement_asset_is_the_largest_pos_leg_even_beyond_u128() {
        // A leg wider than u128 must not sort as zero.
        let pos = vec![
            PosLeg::Pair("0xAAA".into(), "1".into()),
            PosLeg::Pair(
                "0xBBB".into(),
                "340282366920938463463374607431768211456".into(),
            ),
        ];
        assert_eq!(settlement_from_pos(&pos).as_deref(), Some("0xbbb"));
    }

    #[test]
    fn settlement_asset_reads_both_pos_leg_shapes() {
        let pairs = vec![PosLeg::Pair("0xAA".into(), "9".into())];
        assert_eq!(settlement_from_pos(&pairs).as_deref(), Some("0xaa"));
        let objects = vec![
            PosLeg::Object {
                token: "0xCC".into(),
                amount: Some("2".into()),
            },
            PosLeg::Object {
                token: "0xDD".into(),
                amount: Some("7".into()),
            },
        ];
        assert_eq!(settlement_from_pos(&objects).as_deref(), Some("0xdd"));
        // No legs at all → unknown, which the scope rules treat as not-ruled-out.
        assert_eq!(settlement_from_pos(&[]), None);
    }

    #[test]
    fn hex_amounts_parse() {
        let pos = vec![
            PosLeg::Pair("0xAA".into(), "0x10".into()),
            PosLeg::Pair("0xBB".into(), "15".into()),
        ];
        assert_eq!(settlement_from_pos(&pos).as_deref(), Some("0xaa"));
    }
}
