//! Unified candidate / opportunity log row schema (WHI-727 / WHI-729).
//!
//! Covers the in-memory gross/positive candidate shapes and the CSV ledgers
//! written by the monitor services. Distinct from the shadow JSONL schema
//! (`whisker-arb/shadow-ledger/v3`) used by [`crate::execution::shadow`].
//!
//! ## Canonical field counts (WHI-729)
//!
//! * [`GrossCandidate`] — 14 fields (v3/`_1559` two-tier quote-cache shape;
//!   no `net_profit`).
//! * [`Candidate`] / [`PositiveCandidate`] — 15 fields (`GrossCandidate` +
//!   `net_profit`). v2 gains `roi`; moe gains `amounts_out` / `expected_states`.
//! * CSV positive/best-path logs — 9 columns ([`POSITIVE_PATH_LOG_HEADERS`]).
//!   The old v2-only 8-field `OpportunityCsvLogger` header set is retired from
//!   this module (legacy examples may still own local copies until WHI-534).

use crate::amms::amm::AMM;
use crate::arbitrage::ArbitragePath;
use crate::service::gas::{default_gas_safety_margin, GasConfig};
use crate::state_space::SnapshotId;
use alloy::primitives::{Address, I256, U256};
use serde::{Deserialize, Serialize};

/// Unified opportunity candidate (canonical 15-field positive-path shape).
///
/// After WHI-729 schema unification:
/// * v2 gains `roi`
/// * moe gains `amounts_out` / `expected_states`
/// * all three protocols share `profit`, `log_hops`, and the rest of the v3 set
///
/// Distinct from the 14-field [`GrossCandidate`] (no `net_profit`) used by the
/// two-tier quote cache.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub snapshot_id: SnapshotId,
    pub signature: String,
    pub hops: usize,
    pub input: U256,
    pub output: U256,
    pub profit: I256,
    pub net_profit: U256,
    pub pool_addresses: Vec<Address>,
    pub token_path: Vec<Address>,
    pub amounts_out: Vec<U256>,
    pub expected_states: Vec<U256>,
    pub path: ArbitragePath,
    pub pools: Vec<AMM>,
    pub log_hops: String,
    pub roi: String,
}

impl Candidate {
    /// Field count for schema documentation / round-trip tests (WHI-729).
    pub const FIELD_COUNT: usize = 15;
}

/// Legacy example name for [`Candidate`] (v3/moe `PositiveCandidate`).
pub type PositiveCandidate = Candidate;

/// Headers shared by positive-path and best-path CSV logs (canonical 9-col set).
pub const POSITIVE_PATH_LOG_HEADERS: &[&str] = &[
    "block_number",
    "path_signature",
    "hops",
    "input_amount",
    "output_amount",
    "profit",
    "net_profit",
    "roi_percent",
    "path",
];

/// Alias — best-path logs use the same columns as positive-path logs.
pub const BEST_PATH_LOG_HEADERS: &[&str] = POSITIVE_PATH_LOG_HEADERS;

/// Gross-quote cache row (two-tier cache; no `net_profit`).
///
/// Matches the v3/`_1559` `GrossCandidate` shape. Adopting this cache for Moe
/// is deferred to M4-2 (see WHI-729 / DI-11); the type is canonical here so
/// all protocols share one schema when that lands.
#[derive(Clone, Debug)]
pub struct GrossCandidate {
    pub snapshot_id: SnapshotId,
    pub signature: String,
    pub hops: usize,
    pub input: U256,
    pub output: U256,
    pub profit: I256,
    pub pool_addresses: Vec<Address>,
    pub token_path: Vec<Address>,
    pub amounts_out: Vec<U256>,
    pub expected_states: Vec<U256>,
    pub path: ArbitragePath,
    pub pools: Vec<AMM>,
    pub log_hops: String,
    pub roi: String,
}

impl GrossCandidate {
    /// Field count for schema documentation / round-trip tests.
    pub const FIELD_COUNT: usize = 14;

    /// Promote a gross quote to a positive candidate after gas + min-net gates.
    ///
    /// Uses [`default_gas_safety_margin`] (not a hardcoded `1.2` literal) for
    /// the gross-quote profitability screen — intentional fix from WHI-729.
    pub fn promote(
        &self,
        gas_config: &GasConfig,
        min_net_profit: U256,
        snapshot_id: SnapshotId,
    ) -> Option<Candidate> {
        if self.snapshot_id != snapshot_id {
            return None;
        }

        let profit_u256 = U256::from_limbs(*self.profit.as_limbs());
        let net_profit = gas_config.net_profit(profit_u256, self.hops)?;
        if net_profit < min_net_profit
            || !gas_config.is_profitable_after_gas(
                profit_u256,
                self.hops,
                default_gas_safety_margin(),
            )
        {
            return None;
        }

        Some(Candidate {
            snapshot_id,
            signature: self.signature.clone(),
            hops: self.hops,
            input: self.input,
            output: self.output,
            profit: self.profit,
            net_profit,
            pool_addresses: self.pool_addresses.clone(),
            token_path: self.token_path.clone(),
            amounts_out: self.amounts_out.clone(),
            expected_states: self.expected_states.clone(),
            path: self.path.clone(),
            pools: self.pools.clone(),
            log_hops: self.log_hops.clone(),
            roi: self.roi.clone(),
        })
    }
}

/// Canonical positive-path / best-path CSV ledger row (9 columns).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateLedgerRow {
    pub block_number: u64,
    pub path_signature: String,
    pub hops: usize,
    pub input_amount: String,
    pub output_amount: String,
    pub profit: String,
    pub net_profit: String,
    pub roi_percent: String,
    pub path: String,
}

impl CandidateLedgerRow {
    pub const FIELD_COUNT: usize = 9;

    /// Build a CSV row from a unified in-memory [`Candidate`].
    pub fn from_candidate(block_number: u64, candidate: &Candidate) -> Self {
        Self {
            block_number,
            path_signature: candidate.signature.clone(),
            hops: candidate.hops,
            input_amount: candidate.input.to_string(),
            output_amount: candidate.output.to_string(),
            profit: candidate.profit.to_string(),
            net_profit: candidate.net_profit.to_string(),
            roi_percent: candidate.roi.clone(),
            path: candidate.log_hops.clone(),
        }
    }

    /// Serialize as a CSV record ordered like [`POSITIVE_PATH_LOG_HEADERS`].
    pub fn to_csv_record(&self) -> [String; 9] {
        [
            self.block_number.to_string(),
            self.path_signature.clone(),
            self.hops.to_string(),
            self.input_amount.clone(),
            self.output_amount.clone(),
            self.profit.clone(),
            self.net_profit.clone(),
            self.roi_percent.clone(),
            self.path.clone(),
        ]
    }

    /// Parse a CSV record written by [`Self::to_csv_record`] (header-less body).
    pub fn from_csv_record(record: &[String]) -> Result<Self, String> {
        if record.len() != Self::FIELD_COUNT {
            return Err(format!(
                "expected {} fields, got {}",
                Self::FIELD_COUNT,
                record.len()
            ));
        }
        let hops: usize = record[2]
            .parse()
            .map_err(|e| format!("hops parse: {e}"))?;
        let block_number: u64 = record[0]
            .parse()
            .map_err(|e| format!("block_number parse: {e}"))?;
        Ok(Self {
            block_number,
            path_signature: record[1].clone(),
            hops,
            input_amount: record[3].clone(),
            output_amount: record[4].clone(),
            profit: record[5].clone(),
            net_profit: record[6].clone(),
            roi_percent: record[7].clone(),
            path: record[8].clone(),
        })
    }
}

/// Format ROI percent for ledger / candidate rows (v3-style four decimal places).
pub fn format_roi_percent(profit: I256, input: U256) -> Option<String> {
    if input.is_zero() {
        return None;
    }
    let profit_f64 = profit.to_string().parse::<f64>().ok()?;
    let input_f64 = input.to_string().parse::<f64>().ok()?;
    if input_f64.abs() < f64::EPSILON {
        return None;
    }
    let ratio = (profit_f64 / input_f64) * 100.0;
    Some(format!("{ratio:.4}"))
}

/// Human-readable hop description for `log_hops` / CSV `path` columns.
pub fn hops_description(path: &ArbitragePath) -> String {
    path.hops
        .iter()
        .map(|hop| {
            format!(
                "{:#x}->{:#x}@{:#x}(fee_bps={})",
                hop.token_in, hop.token_out, hop.pool_address, hop.fee_bps
            )
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Collect protocol-specific expected state words for every hop pool.
///
/// * V2: `(reserve0, reserve1)` per pool
/// * Agni V3: `(sqrt_price, liquidity)` per pool
/// * Moe: `(active_id, bin_step)` per pool
///
/// Mixed paths append each hop's pair in path order. Moe gaining this field
/// is an intentional WHI-729 schema unification change.
pub fn collect_expected_states(pools: &[AMM]) -> Result<Vec<U256>, String> {
    let mut states = Vec::with_capacity(pools.len() * 2);
    for amm in pools {
        match amm {
            AMM::UniswapV2Pool(pool) => {
                states.push(U256::from(pool.reserve_0));
                states.push(U256::from(pool.reserve_1));
            }
            AMM::AgniPool(pool) => {
                states.push(U256::from(pool.sqrt_price));
                states.push(U256::from(pool.liquidity));
            }
            AMM::MoeLbPair(pool) => {
                states.push(U256::from(pool.active_id));
                states.push(U256::from(pool.bin_step));
            }
            other => {
                return Err(format!(
                    "unsupported AMM variant for expected_states: {:?}",
                    std::mem::discriminant(other)
                ));
            }
        }
    }
    Ok(states)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::Token;
    use crate::amms::uniswap_v2::UniswapV2Pool;
    use crate::arbitrage::pathfinder::PathHop;
    use alloy::primitives::{address, B256};

    fn sample_path() -> ArbitragePath {
        ArbitragePath {
            hops: vec![PathHop {
                pool_address: address!("00000000000000000000000000000000000000a1"),
                token_in: address!("0000000000000000000000000000000000000001"),
                token_out: address!("0000000000000000000000000000000000000002"),
                fee_bps: 30,
            }],
        }
    }

    fn sample_v2_pool() -> AMM {
        let mut pool =
            UniswapV2Pool::new(address!("00000000000000000000000000000000000000a1"), 300);
        pool.token_a =
            Token::new_with_decimals(address!("0000000000000000000000000000000000000001"), 18);
        pool.token_b =
            Token::new_with_decimals(address!("0000000000000000000000000000000000000002"), 18);
        pool.reserve_0 = 1_000;
        pool.reserve_1 = 2_000;
        AMM::UniswapV2Pool(pool)
    }

    fn sample_candidate() -> Candidate {
        let path = sample_path();
        let pools = vec![sample_v2_pool()];
        let expected_states = collect_expected_states(&pools).unwrap();
        Candidate {
            snapshot_id: SnapshotId::new(42, 1, B256::ZERO),
            signature: "v2:t0->t1".into(),
            hops: 1,
            input: U256::from(100u64),
            output: U256::from(110u64),
            profit: I256::try_from(10i64).unwrap(),
            net_profit: U256::from(5u64),
            pool_addresses: vec![address!("00000000000000000000000000000000000000a1")],
            token_path: vec![
                address!("0000000000000000000000000000000000000001"),
                address!("0000000000000000000000000000000000000002"),
            ],
            amounts_out: vec![U256::from(110u64)],
            expected_states,
            path: path.clone(),
            pools,
            log_hops: hops_description(&path),
            roi: format_roi_percent(I256::try_from(10i64).unwrap(), U256::from(100u64))
                .unwrap_or_else(|| "-".into()),
        }
    }

    #[test]
    fn positive_and_best_headers_match_nine_columns() {
        assert_eq!(POSITIVE_PATH_LOG_HEADERS, BEST_PATH_LOG_HEADERS);
        assert_eq!(POSITIVE_PATH_LOG_HEADERS.len(), CandidateLedgerRow::FIELD_COUNT);
        assert_eq!(POSITIVE_PATH_LOG_HEADERS.len(), 9);
        // v2 8-field OpportunityCsvLogger headers are retired (WHI-729).
        assert!(POSITIVE_PATH_LOG_HEADERS.contains(&"roi_percent"));
    }

    #[test]
    fn gross_candidate_field_count_is_fourteen() {
        // Compile-time documentation of the schema; runtime counter for
        // round-trip tests that list every field explicitly.
        assert_eq!(GrossCandidate::FIELD_COUNT, 14);
        assert_eq!(Candidate::FIELD_COUNT, 15);
    }

    #[test]
    fn ledger_round_trip_preserves_all_csv_fields() {
        let candidate = sample_candidate();
        // Unified candidate carries fields that old per-protocol shapes split
        // across: v2 lacked roi; moe lacked amounts_out/expected_states.
        assert!(!candidate.roi.is_empty());
        assert!(!candidate.amounts_out.is_empty());
        assert_eq!(candidate.expected_states.len(), 2); // V2: reserve0, reserve1
        assert!(!candidate.log_hops.is_empty());

        let row = CandidateLedgerRow::from_candidate(42, &candidate);
        let record = row.to_csv_record();
        assert_eq!(record.len(), 9);
        assert_eq!(record[0], "42");
        assert_eq!(record[1], "v2:t0->t1");
        assert_eq!(record[7], candidate.roi);
        assert_eq!(record[8], candidate.log_hops);

        let parsed = CandidateLedgerRow::from_csv_record(&record.to_vec()).unwrap();
        assert_eq!(parsed, row);
        assert_eq!(parsed.roi_percent, candidate.roi);
        assert_eq!(parsed.profit, candidate.profit.to_string());
        assert_eq!(parsed.net_profit, candidate.net_profit.to_string());
    }

    #[test]
    fn gross_promote_uses_default_safety_margin_not_literal() {
        let path = sample_path();
        let pools = vec![sample_v2_pool()];
        let gross = GrossCandidate {
            snapshot_id: SnapshotId::new(1, 1, B256::ZERO),
            signature: "sig".into(),
            hops: 2,
            input: U256::from(1_000u64),
            output: U256::from(2_000u64),
            profit: I256::try_from(1_000i64).unwrap(),
            pool_addresses: vec![address!("00000000000000000000000000000000000000a1")],
            token_path: vec![
                address!("0000000000000000000000000000000000000001"),
                address!("0000000000000000000000000000000000000002"),
            ],
            amounts_out: vec![U256::from(2_000u64)],
            expected_states: collect_expected_states(&pools).unwrap(),
            path,
            pools,
            log_hops: "h".into(),
            roi: "10.0".into(),
        };

        // Gas price 1 wei, hops=2 → cost = 900_000_000. Gross 1000 << 1.2*cost.
        let gas = GasConfig { gas_price_wei: 1 };
        assert!(gross
            .promote(&gas, U256::ZERO, SnapshotId::new(1, 1, B256::ZERO))
            .is_none());

        // Zero gas → margin gate passes; min_net still applies.
        let free = GasConfig { gas_price_wei: 0 };
        let positive = gross
            .promote(&free, U256::ZERO, SnapshotId::new(1, 1, B256::ZERO))
            .expect("should promote when gas is free");
        assert_eq!(positive.net_profit, U256::from(1_000u64));
        assert_eq!(positive.roi, "10.0");
        assert_eq!(positive.amounts_out, vec![U256::from(2_000u64)]);
        assert_eq!(positive.expected_states.len(), 2);
        assert_eq!(Candidate::FIELD_COUNT, 15);
    }

    #[test]
    fn format_roi_and_hops_helpers() {
        let roi = format_roi_percent(I256::try_from(25i64).unwrap(), U256::from(100u64)).unwrap();
        assert_eq!(roi, "25.0000");
        let desc = hops_description(&sample_path());
        assert!(desc.contains("fee_bps=30"));
    }

    #[test]
    fn collect_expected_states_v2_pair() {
        let states = collect_expected_states(&[sample_v2_pool()]).unwrap();
        assert_eq!(states, vec![U256::from(1_000u64), U256::from(2_000u64)]);
    }
}
