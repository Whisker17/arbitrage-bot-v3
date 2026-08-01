//! Block continuity / gap detection (pure logic, offline-testable).

use alloy::primitives::B256;
use serde::{Deserialize, Serialize};

/// Minimal header identity used by continuity and header-completeness checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SampledHeader {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
}

/// A single continuity break between two adjacent samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuityGap {
    pub kind: ContinuityGapKind,
    pub previous_number: u64,
    pub current_number: u64,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuityGapKind {
    /// Block numbers are not strictly consecutive (`curr != prev + 1`).
    NumberGap,
    /// `curr.parent_hash` does not equal `prev.hash`.
    ParentHashMismatch,
    /// Duplicate block number observed.
    DuplicateNumber,
}

/// Result of scanning a sequence of headers in observation order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuityReport {
    pub samples: usize,
    pub gaps: Vec<ContinuityGap>,
}

impl ContinuityReport {
    pub fn gap_count(&self) -> u64 {
        self.gaps.len() as u64
    }

    pub fn is_contiguous(&self) -> bool {
        self.gaps.is_empty()
    }
}

/// Detect gaps in an ordered sample of headers (oldest → newest or tip stream).
///
/// Expects headers sorted by ascending `number`. Duplicate numbers and
/// non-consecutive numbers are both reported; parent linkage is checked only
/// when numbers are consecutive.
pub fn detect_continuity_gaps(headers: &[SampledHeader]) -> ContinuityReport {
    let mut gaps = Vec::new();
    for window in headers.windows(2) {
        let prev = &window[0];
        let curr = &window[1];
        if curr.number == prev.number {
            gaps.push(ContinuityGap {
                kind: ContinuityGapKind::DuplicateNumber,
                previous_number: prev.number,
                current_number: curr.number,
                detail: format!("duplicate block number {}", curr.number),
            });
            continue;
        }
        if curr.number != prev.number.saturating_add(1) {
            gaps.push(ContinuityGap {
                kind: ContinuityGapKind::NumberGap,
                previous_number: prev.number,
                current_number: curr.number,
                detail: format!(
                    "expected block {} after {}, got {}",
                    prev.number.saturating_add(1),
                    prev.number,
                    curr.number
                ),
            });
            // Still check parent when numbers skip? No — parent of a non-adjacent
            // block is not expected to match the previous sample hash.
            continue;
        }
        if curr.parent_hash != prev.hash {
            gaps.push(ContinuityGap {
                kind: ContinuityGapKind::ParentHashMismatch,
                previous_number: prev.number,
                current_number: curr.number,
                detail: format!(
                    "block {} parent_hash {:?} != previous hash {:?}",
                    curr.number, curr.parent_hash, prev.hash
                ),
            });
        }
    }
    ContinuityReport {
        samples: headers.len(),
        gaps,
    }
}

/// Header is complete when all identity fields are present and non-degenerate.
pub fn header_is_complete(h: &SampledHeader) -> bool {
    h.number > 0
        && h.hash != B256::ZERO
        && h.parent_hash != B256::ZERO
        && h.timestamp > 0
}

/// Cross-transport agreement at a shared height: same hash and parent_hash.
pub fn headers_agree(http: &SampledHeader, ws: &SampledHeader) -> bool {
    http.number == ws.number
        && http.hash == ws.hash
        && http.parent_hash == ws.parent_hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(number: u64, hash_byte: u8, parent_byte: u8, ts: u64) -> SampledHeader {
        SampledHeader {
            number,
            hash: B256::repeat_byte(hash_byte),
            parent_hash: B256::repeat_byte(parent_byte),
            timestamp: ts,
        }
    }

    #[test]
    fn contiguous_chain_has_no_gaps() {
        let headers = vec![
            h(10, 0x10, 0x0f, 1000),
            h(11, 0x11, 0x10, 1002),
            h(12, 0x12, 0x11, 1004),
        ];
        let report = detect_continuity_gaps(&headers);
        assert!(report.is_contiguous());
        assert_eq!(report.gap_count(), 0);
        assert_eq!(report.samples, 3);
    }

    #[test]
    fn detects_number_gap() {
        let headers = vec![h(10, 0x10, 0x0f, 1000), h(12, 0x12, 0x11, 1004)];
        let report = detect_continuity_gaps(&headers);
        assert_eq!(report.gap_count(), 1);
        assert_eq!(report.gaps[0].kind, ContinuityGapKind::NumberGap);
    }

    #[test]
    fn detects_parent_hash_mismatch() {
        let headers = vec![
            h(10, 0x10, 0x0f, 1000),
            h(11, 0x11, 0x99, 1002), // parent should be 0x10
        ];
        let report = detect_continuity_gaps(&headers);
        assert_eq!(report.gap_count(), 1);
        assert_eq!(report.gaps[0].kind, ContinuityGapKind::ParentHashMismatch);
    }

    #[test]
    fn detects_duplicate_number() {
        let headers = vec![h(10, 0x10, 0x0f, 1000), h(10, 0x1a, 0x0f, 1000)];
        let report = detect_continuity_gaps(&headers);
        assert_eq!(report.gaps[0].kind, ContinuityGapKind::DuplicateNumber);
    }

    #[test]
    fn incomplete_header_rejected() {
        assert!(!header_is_complete(&h(0, 0x1, 0x2, 1)));
        assert!(!header_is_complete(&SampledHeader {
            number: 1,
            hash: B256::ZERO,
            parent_hash: B256::repeat_byte(1),
            timestamp: 1,
        }));
        assert!(!header_is_complete(&h(1, 0x1, 0x2, 0)));
        assert!(header_is_complete(&h(1, 0x1, 0x2, 99)));
    }

    #[test]
    fn agreement_requires_matching_identity() {
        let a = h(5, 0xaa, 0xbb, 1);
        let b = h(5, 0xaa, 0xbb, 9);
        let c = h(5, 0xcc, 0xbb, 1);
        assert!(headers_agree(&a, &b)); // timestamp not part of agreement
        assert!(!headers_agree(&a, &c));
    }
}
