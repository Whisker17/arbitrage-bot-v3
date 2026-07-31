//! Unified candidate / opportunity log row schema (WHI-727).
//!
//! Covers the CSV ledgers written by the three monitor services. Distinct from
//! the shadow JSONL schema (`whisker-arb/shadow-ledger/v3`) used by
//! [`crate::execution::shadow`]. Unifying JSONL ledger fields is WHI-527.4.

use serde::{Deserialize, Serialize};

/// Headers shared by v3/moe positive-path and best-path CSV logs.
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

/// Alias — v3/moe best-path logs use the same columns as positive-path logs.
pub const BEST_PATH_LOG_HEADERS: &[&str] = POSITIVE_PATH_LOG_HEADERS;

/// Headers for the v2 opportunity CSV (`logs/opportunities.csv`-style).
pub const V2_OPPORTUNITY_LOG_HEADERS: &[&str] = &[
    "timestamp",
    "block_number",
    "signature",
    "hops",
    "input_amount",
    "gross_profit",
    "net_profit",
    "path_description",
];

/// Canonical positive-path / best-path ledger row (v3/moe shape).
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_and_best_headers_match() {
        assert_eq!(POSITIVE_PATH_LOG_HEADERS, BEST_PATH_LOG_HEADERS);
        assert_eq!(POSITIVE_PATH_LOG_HEADERS.len(), 9);
    }

    #[test]
    fn csv_record_order_matches_headers() {
        let row = CandidateLedgerRow {
            block_number: 42,
            path_signature: "sig".into(),
            hops: 2,
            input_amount: "1".into(),
            output_amount: "2".into(),
            profit: "3".into(),
            net_profit: "4".into(),
            roi_percent: "5.0".into(),
            path: "a->b".into(),
        };
        let record = row.to_csv_record();
        assert_eq!(record[0], "42");
        assert_eq!(record[1], "sig");
        assert_eq!(record[8], "a->b");
    }
}
