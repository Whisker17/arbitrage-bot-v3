//! RPC endpoint qualification probe (WHI-744).
//!
//! Pure logic (fingerprint, continuity, report, thresholds, universe load) is
//! unit-tested offline. Live Checks A–E live in [`runner`].

pub mod continuity;
pub mod fingerprint;
pub mod report;
pub mod runner;
pub mod thresholds;
pub mod universe;

pub use continuity::{
    detect_continuity_gaps, header_is_complete, headers_agree, ContinuityGap, ContinuityGapKind,
    ContinuityReport, SampledHeader,
};
pub use fingerprint::{endpoint_fingerprint, text_leaks_endpoint};
pub use report::{
    check_id, compute_qualified, failure_reason, format_summary, new_report, serialize_report,
    CheckResult, ProbeReport,
};
pub use runner::{run_probe, sanitize_error, ProbeConfig};
pub use thresholds::*;
pub use universe::{load_merged_pool_addresses, AddressSetSource};
