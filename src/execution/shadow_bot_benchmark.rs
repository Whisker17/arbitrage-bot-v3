//! Offline known-bot benchmark comparator for continuous shadow ledgers (WHI-715).
//!
//! Cross-references a shadow-mode ledger against a list of known Mantle arbitrage
//! bot transactions and classifies each ground-truth event into one of three
//! buckets:
//!
//! 1. **Missed detection** — we produced no candidate at that block/route.
//! 2. **Unprofitable / revert** — we produced a candidate but preflight was not a
//!    profitable `Pass`.
//! 3. **Would-have-been profitable** — we produced a candidate whose preflight
//!    `Pass`ed with positive simulated net profit.
//!
//! Pure offline I/O: no RPC, no signing. Unit-testable with synthetic ledgers.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

/// Schema version for the comparator report artifact.
pub const BENCHMARK_REPORT_SCHEMA_VERSION: &str = "whisker-arb/shadow-bot-benchmark/v1";

/// Expected ledger schema (matches `shadow/ledger.rs`).
pub const LEDGER_SCHEMA_VERSION: &str = "whisker-arb/shadow-ledger/v3";

pub const NO_SEND_CAPABILITY: &str = "no_send";

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("malformed JSON in {path} at line {line}: {source}")]
    MalformedJson {
        path: String,
        line: usize,
        source: serde_json::Error,
    },
    #[error("malformed JSON in {path}: {source}")]
    MalformedFile {
        path: String,
        source: serde_json::Error,
    },
    #[error("ledger {path} is empty")]
    EmptyLedger { path: String },
    #[error("ledger {path} does not start with a run_header")]
    MissingRunHeader { path: String },
    #[error(
        "ledger {path} schema_version {found:?} unsupported (expected {expected:?})"
    )]
    UnsupportedSchema {
        path: String,
        expected: String,
        found: String,
    },
    #[error(
        "ledger {path} for service {service:?} has send_capability {found:?}, expected {expected:?}"
    )]
    SendCapabilityViolation {
        path: String,
        service: String,
        expected: String,
        found: String,
    },
    #[error("known-bot event list is empty")]
    EmptyEvents,
    #[error("known-bot event {index} has block_number 0 (must be a real mainnet block)")]
    InvalidBlockNumber { index: usize },
    #[error("known-bot event {index} is missing tx_hash")]
    MissingTxHash { index: usize },
}

/// Classification bucket for one known-bot ground-truth event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    /// No candidate at the block/route → detection/coverage bug.
    MissedDetection,
    /// Candidate exists but preflight is Revert / unprofitable / non-Pass.
    UnprofitableOrRevert,
    /// Candidate exists and preflight Pass with net_profit > 0.
    WouldHaveBeenProfitable,
}

impl Bucket {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissedDetection => "missed_detection",
            Self::UnprofitableOrRevert => "unprofitable_or_revert",
            Self::WouldHaveBeenProfitable => "would_have_been_profitable",
        }
    }
}

/// One ground-truth event: a known bot executed a profitable arb in this block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownBotEvent {
    /// Bot EOAs / contract that sent the arb tx.
    pub bot_address: String,
    /// Transaction hash of the arb.
    pub tx_hash: String,
    /// Block number where the bot's arb landed.
    pub block_number: u64,
    /// Optional ordered pool addresses for route matching. When empty/absent,
    /// any candidate at `block_number` counts as a match for the route.
    #[serde(default)]
    pub ordered_pools: Vec<String>,
    /// Optional human route descriptor for bucket-1 investigation (e.g.
    /// `"h2:v2+v2"` or `"WMNT→USDC→WMNT"`). Not used for matching — only
    /// reported when we miss detection.
    #[serde(default)]
    pub route: Option<String>,
    /// Optional free-form label (e.g. "bot-a cycle").
    #[serde(default)]
    pub label: Option<String>,
}

/// Wire shape for a list file: either a bare array or `{ "events": [...] }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum KnownBotEventFile {
    Array(Vec<KnownBotEvent>),
    Wrapped { events: Vec<KnownBotEvent> },
}

impl KnownBotEventFile {
    fn into_events(self) -> Vec<KnownBotEvent> {
        match self {
            Self::Array(events) | Self::Wrapped { events } => events,
        }
    }
}

/// One shadow candidate opportunity joined from context + candidate rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowOpportunity {
    pub service: String,
    pub block_number: u64,
    pub digest: String,
    pub opportunity_id: String,
    pub ordered_pools: Vec<String>,
    pub net_profit: String,
    pub gross_profit: String,
    pub amount_in: String,
    /// Preflight outcome `kind` string (`pass`, `revert`, …).
    pub outcome_kind: String,
    /// Optional revert reason when `outcome_kind == "revert"`.
    pub outcome_reason: Option<String>,
}

impl ShadowOpportunity {
    pub fn is_profitable_pass(&self) -> bool {
        if self.outcome_kind != "pass" {
            return false;
        }
        parse_u256_positive(&self.net_profit)
    }
}

/// Per-event classification result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassifiedEvent {
    pub bucket: Bucket,
    pub bot_address: String,
    pub tx_hash: String,
    pub block_number: u64,
    pub ordered_pools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Digests of matching shadow opportunities (empty for bucket 1).
    pub matching_digests: Vec<String>,
    /// Services that produced a match.
    pub matching_services: Vec<String>,
    /// First profitable-Pass match's outcome when any; otherwise the first
    /// match's outcome (ledger order). Not a ranked optimum.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_outcome_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_net_profit: Option<String>,
    /// Human-readable note for follow-up investigation.
    pub detail: String,
}

/// Aggregate report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub schema_version: String,
    pub event_count: usize,
    pub bucket_counts: BucketCounts,
    pub no_send_enforced: bool,
    pub ledger_services: Vec<String>,
    pub events: Vec<ClassifiedEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BucketCounts {
    pub missed_detection: usize,
    pub unprofitable_or_revert: usize,
    pub would_have_been_profitable: usize,
}

impl BucketCounts {
    fn record(&mut self, bucket: Bucket) {
        match bucket {
            Bucket::MissedDetection => self.missed_detection += 1,
            Bucket::UnprofitableOrRevert => self.unprofitable_or_revert += 1,
            Bucket::WouldHaveBeenProfitable => self.would_have_been_profitable += 1,
        }
    }
}

// --- Ledger wire mirrors (intentionally narrow; serde ignores unknown fields) ---

#[derive(Debug, Clone, Deserialize)]
struct WireSnapshotId {
    block_number: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct WireExecutionIdentity {
    snapshot_id: WireSnapshotId,
}

#[derive(Debug, Clone, Deserialize)]
struct WireRunHeader {
    schema_version: String,
    service: String,
    send_capability: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireContextRow {
    digest: String,
    identity: WireExecutionIdentity,
    opportunity_id: String,
    ordered_pools: Vec<String>,
    amount_in: String,
    gross_profit: String,
    net_profit: String,
}

#[derive(Debug, Clone, Deserialize)]
struct WireCandidateRow {
    digest: String,
    outcome: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "row_type", rename_all = "snake_case")]
enum WireLedgerRow {
    RunHeader(WireRunHeader),
    Context(WireContextRow),
    Candidate(WireCandidateRow),
    #[serde(other)]
    Other,
}

/// Index of opportunities parsed from one or more ledgers.
#[derive(Debug, Default)]
pub struct ShadowLedgerIndex {
    pub opportunities: Vec<ShadowOpportunity>,
    pub services: BTreeSet<String>,
    /// True when every run_header declared `send_capability = no_send`.
    pub no_send_enforced: bool,
}

impl ShadowLedgerIndex {
    pub fn from_ledgers(ledgers: &[LedgerBytes]) -> Result<Self, BenchmarkError> {
        let mut index = Self {
            no_send_enforced: true,
            ..Self::default()
        };
        if ledgers.is_empty() {
            return Ok(index);
        }
        for ledger in ledgers {
            parse_ledger_into(ledger, &mut index)?;
        }
        Ok(index)
    }

    /// Opportunities at `block_number`, optionally filtered by route pools.
    pub fn matches(
        &self,
        block_number: u64,
        ordered_pools: &[String],
    ) -> Vec<&ShadowOpportunity> {
        self.opportunities
            .iter()
            .filter(|opp| opp.block_number == block_number)
            .filter(|opp| pools_match(&opp.ordered_pools, ordered_pools))
            .collect()
    }
}

/// One ledger file's bytes + label (usually its path).
pub struct LedgerBytes {
    pub label: String,
    pub bytes: Vec<u8>,
}

impl LedgerBytes {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, BenchmarkError> {
        let path = path.as_ref();
        let label = path.display().to_string();
        let bytes = std::fs::read(path).map_err(|source| BenchmarkError::Io {
            path: label.clone(),
            source,
        })?;
        Ok(Self { label, bytes })
    }
}

fn parse_ledger_into(
    ledger: &LedgerBytes,
    index: &mut ShadowLedgerIndex,
) -> Result<(), BenchmarkError> {
    if ledger.bytes.is_empty() {
        return Err(BenchmarkError::EmptyLedger {
            path: ledger.label.clone(),
        });
    }

    let mut service = String::new();
    let mut saw_header = false;
    // digest -> partial context (candidate may arrive later or earlier).
    let mut contexts: BTreeMap<String, WireContextRow> = BTreeMap::new();
    let mut candidates: BTreeMap<String, WireCandidateRow> = BTreeMap::new();

    for (line_idx, line) in ledger.bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let row: WireLedgerRow = serde_json::from_slice(line).map_err(|source| {
            BenchmarkError::MalformedJson {
                path: ledger.label.clone(),
                line: line_idx + 1,
                source,
            }
        })?;

        match row {
            WireLedgerRow::RunHeader(header) => {
                if header.schema_version != LEDGER_SCHEMA_VERSION {
                    return Err(BenchmarkError::UnsupportedSchema {
                        path: ledger.label.clone(),
                        expected: LEDGER_SCHEMA_VERSION.to_string(),
                        found: header.schema_version,
                    });
                }
                if header.send_capability != NO_SEND_CAPABILITY {
                    index.no_send_enforced = false;
                    return Err(BenchmarkError::SendCapabilityViolation {
                        path: ledger.label.clone(),
                        service: header.service,
                        expected: NO_SEND_CAPABILITY.to_string(),
                        found: header.send_capability,
                    });
                }
                service = header.service.clone();
                index.services.insert(header.service);
                saw_header = true;
            }
            WireLedgerRow::Context(ctx) => {
                if !saw_header {
                    return Err(BenchmarkError::MissingRunHeader {
                        path: ledger.label.clone(),
                    });
                }
                contexts.insert(ctx.digest.clone(), ctx);
            }
            WireLedgerRow::Candidate(cand) => {
                if !saw_header {
                    return Err(BenchmarkError::MissingRunHeader {
                        path: ledger.label.clone(),
                    });
                }
                candidates.insert(cand.digest.clone(), cand);
            }
            WireLedgerRow::Other => {}
        }
    }

    if !saw_header {
        return Err(BenchmarkError::MissingRunHeader {
            path: ledger.label.clone(),
        });
    }

    // Join context ∩ candidate by digest. A context without a candidate is not
    // a completed preflight sample — it cannot satisfy bucket 2/3.
    for (digest, ctx) in contexts {
        let Some(cand) = candidates.get(&digest) else {
            continue;
        };
        let (outcome_kind, outcome_reason) = outcome_kind_and_reason(&cand.outcome);
        index.opportunities.push(ShadowOpportunity {
            service: service.clone(),
            block_number: ctx.identity.snapshot_id.block_number,
            digest,
            opportunity_id: ctx.opportunity_id,
            ordered_pools: ctx.ordered_pools,
            net_profit: ctx.net_profit,
            gross_profit: ctx.gross_profit,
            amount_in: ctx.amount_in,
            outcome_kind,
            outcome_reason,
        });
    }

    Ok(())
}

fn outcome_kind_and_reason(outcome: &serde_json::Value) -> (String, Option<String>) {
    if let Some(kind) = outcome.get("kind").and_then(|v| v.as_str()) {
        let reason = outcome
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        return (kind.to_string(), reason);
    }
    // Flat string fallback (defensive).
    if let Some(s) = outcome.as_str() {
        return (s.to_string(), None);
    }
    ("unknown".to_string(), None)
}

/// When `wanted` is empty, any opportunity matches (block-only).
/// Otherwise require the same multiset of normalized addresses, order-sensitive
/// (route direction matters for arb paths).
fn pools_match(actual: &[String], wanted: &[String]) -> bool {
    if wanted.is_empty() {
        return true;
    }
    if actual.len() != wanted.len() {
        return false;
    }
    actual
        .iter()
        .zip(wanted.iter())
        .all(|(a, b)| normalize_addr(a) == normalize_addr(b))
}

fn normalize_addr(addr: &str) -> String {
    let trimmed = addr.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("0x") {
        lower
    } else {
        format!("0x{lower}")
    }
}

/// Positive net profit on the wire is digits-only decimal (same convention as
/// `shadow_report::parse_net_profit`). Hex / malformed values are treated as
/// non-positive so bucket 3 stays fail-closed.
fn parse_u256_positive(raw: &str) -> bool {
    let s = raw.trim();
    if s.is_empty() || s.starts_with('-') {
        return false;
    }
    if !s.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match U256::from_str(s) {
        Ok(v) => !v.is_zero(),
        Err(_) => false,
    }
}

fn classified(
    event: &KnownBotEvent,
    bucket: Bucket,
    matches: &[&ShadowOpportunity],
    representative: Option<&ShadowOpportunity>,
    detail: String,
) -> ClassifiedEvent {
    ClassifiedEvent {
        bucket,
        bot_address: event.bot_address.clone(),
        tx_hash: event.tx_hash.clone(),
        block_number: event.block_number,
        ordered_pools: event.ordered_pools.clone(),
        route: event.route.clone(),
        label: event.label.clone(),
        matching_digests: matches.iter().map(|o| o.digest.clone()).collect(),
        matching_services: unique_services(matches),
        best_outcome_kind: representative.map(|o| o.outcome_kind.clone()),
        best_net_profit: representative.map(|o| o.net_profit.clone()),
        detail,
    }
}

/// Classify a single known-bot event against the ledger index.
pub fn classify_event(
    event: &KnownBotEvent,
    index: &ShadowLedgerIndex,
) -> ClassifiedEvent {
    let matches = index.matches(event.block_number, &event.ordered_pools);

    if matches.is_empty() {
        let mut detail = format!("no shadow candidate at block {}", event.block_number);
        if !event.ordered_pools.is_empty() {
            detail.push_str(&format!(" for pools {:?}", event.ordered_pools));
        }
        if let Some(route) = &event.route {
            detail.push_str(&format!(" route={route}"));
        }
        return classified(event, Bucket::MissedDetection, &[], None, detail);
    }

    let profitable: Vec<&ShadowOpportunity> = matches
        .iter()
        .copied()
        .filter(|o| o.is_profitable_pass())
        .collect();

    if let Some(best) = profitable.first().copied() {
        return classified(
            event,
            Bucket::WouldHaveBeenProfitable,
            &matches,
            Some(best),
            format!(
                "profitable Pass at block {} via service(s) {:?} (digest {})",
                event.block_number,
                unique_services(&matches),
                best.digest
            ),
        );
    }

    let best = matches[0];
    classified(
        event,
        Bucket::UnprofitableOrRevert,
        &matches,
        Some(best),
        format!(
            "candidate(s) at block {} but not profitable Pass (best outcome={}, net_profit={}, reason={:?})",
            event.block_number, best.outcome_kind, best.net_profit, best.outcome_reason
        ),
    )
}

fn unique_services(matches: &[&ShadowOpportunity]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for m in matches {
        set.insert(m.service.clone());
    }
    set.into_iter().collect()
}

/// Load known-bot events from a JSON file.
pub fn load_known_bot_events(path: impl AsRef<Path>) -> Result<Vec<KnownBotEvent>, BenchmarkError> {
    let path = path.as_ref();
    let label = path.display().to_string();
    let bytes = std::fs::read(path).map_err(|source| BenchmarkError::Io {
        path: label.clone(),
        source,
    })?;
    let file: KnownBotEventFile =
        serde_json::from_slice(&bytes).map_err(|source| BenchmarkError::MalformedFile {
            path: label,
            source,
        })?;
    let events = file.into_events();
    if events.is_empty() {
        return Err(BenchmarkError::EmptyEvents);
    }
    for (index, event) in events.iter().enumerate() {
        if event.block_number == 0 {
            return Err(BenchmarkError::InvalidBlockNumber { index });
        }
        if event.tx_hash.trim().is_empty() {
            return Err(BenchmarkError::MissingTxHash { index });
        }
    }
    Ok(events)
}

/// Full compare: load ledgers + events, classify, produce report.
pub fn compare(
    ledgers: &[LedgerBytes],
    events: &[KnownBotEvent],
) -> Result<BenchmarkReport, BenchmarkError> {
    if events.is_empty() {
        return Err(BenchmarkError::EmptyEvents);
    }
    let index = ShadowLedgerIndex::from_ledgers(ledgers)?;
    let mut counts = BucketCounts::default();
    let mut classified = Vec::with_capacity(events.len());
    for event in events {
        let row = classify_event(event, &index);
        counts.record(row.bucket);
        classified.push(row);
    }
    Ok(BenchmarkReport {
        schema_version: BENCHMARK_REPORT_SCHEMA_VERSION.to_string(),
        event_count: events.len(),
        bucket_counts: counts,
        no_send_enforced: index.no_send_enforced,
        ledger_services: index.services.into_iter().collect(),
        events: classified,
    })
}

/// Render a human-readable markdown summary (bucket counts + missed-detection detail).
pub fn render_markdown_report(report: &BenchmarkReport) -> String {
    let mut out = String::new();
    out.push_str("# Shadow known-bot benchmark report\n\n");
    out.push_str(&format!(
        "- Schema: `{}`\n",
        report.schema_version
    ));
    out.push_str(&format!("- Events: {}\n", report.event_count));
    out.push_str(&format!(
        "- no_send enforced: {}\n",
        report.no_send_enforced
    ));
    out.push_str(&format!(
        "- Ledger services: {:?}\n\n",
        report.ledger_services
    ));
    out.push_str("## Bucket counts\n\n");
    out.push_str(&format!(
        "| Bucket | Count |\n| --- | ---: |\n| missed_detection (1) | {} |\n| unprofitable_or_revert (2) | {} |\n| would_have_been_profitable (3) | {} |\n\n",
        report.bucket_counts.missed_detection,
        report.bucket_counts.unprofitable_or_revert,
        report.bucket_counts.would_have_been_profitable,
    ));

    let missed: Vec<&ClassifiedEvent> = report
        .events
        .iter()
        .filter(|e| e.bucket == Bucket::MissedDetection)
        .collect();
    out.push_str("## Bucket 1 — missed detection (investigation detail)\n\n");
    if missed.is_empty() {
        out.push_str("_None._\n\n");
    } else {
        for e in missed {
            out.push_str(&format!(
                "- block={} bot={} tx={} pools={:?} route={:?} label={:?}\n  {}\n",
                e.block_number,
                e.bot_address,
                e.tx_hash,
                e.ordered_pools,
                e.route,
                e.label,
                e.detail
            ));
        }
        out.push('\n');
    }

    out.push_str("## All events\n\n");
    for e in &report.events {
        out.push_str(&format!(
            "- [{}] block={} bot={} tx={} services={:?} outcome={:?} net={:?}\n  {}\n",
            e.bucket.as_str(),
            e.block_number,
            e.bot_address,
            e.tx_hash,
            e.matching_services,
            e.best_outcome_kind,
            e.best_net_profit,
            e.detail
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = "v2_monitor_executor_service";

    fn header_line(service: &str, capability: &str) -> String {
        format!(
            r#"{{"row_type":"run_header","sequence":0,"schema_version":"{LEDGER_SCHEMA_VERSION}","run_id":"0xrun","git_commit":"abc","chain_id":5000,"service":"{service}","executor_contract":"0x2","wmnt_address":"0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8","config_digest":"0x0","storage_layout_digest":"0x0","wmnt_descriptor_digest":"0x0","moe_allowlist_digest":"0x0","identity_digest":"0x0","approved_pools_digest":"0x0","threshold_config_digest":"0x0","profile_digest":"0x0","override_digest":"0x0","send_capability":"{capability}","start_identity":null,"started_at_unix":1}}"#
        )
    }

    fn context_line(
        digest: &str,
        block: u64,
        pools: &[&str],
        net_profit: &str,
    ) -> String {
        let pools_json: Vec<String> = pools.iter().map(|p| format!("\"{p}\"")).collect();
        format!(
            r#"{{"row_type":"context","sequence":1,"schema_version":"{LEDGER_SCHEMA_VERSION}","digest":"{digest}","identity":{{"snapshot_id":{{"chain_id":5000,"block_number":{block},"block_hash":"0xb"}},"header":{{"parent_hash":"0xp","block_timestamp":1}},"pool_universe_fingerprint":"0xfp","route":{{"protocols":["v2"],"hop_count":2,"v3_tick_crossings":0,"moe_bin_crossings":0}},"fee_context":{{"block_number":{block},"block_hash":"0xb","base_fee_per_gas":0,"block_gas_limit":1}},"gas_profile_identity":"gp"}},"opportunity_id":"0xopp","ordered_pools":[{pools}],"amount_in":"1000","gross_profit":"50","net_profit":"{net_profit}","profit_basis":"simulated"}}"#,
            pools = pools_json.join(",")
        )
    }

    fn candidate_line(digest: &str, outcome_json: &str) -> String {
        format!(
            r#"{{"row_type":"candidate","sequence":2,"schema_version":"{LEDGER_SCHEMA_VERSION}","digest":"{digest}","policy_key":"mandatory","outcome":{outcome_json},"block_tag":"latest","latency_ms":1,"detail":null,"recorded_at_unix":1}}"#
        )
    }

    fn ledger(lines: &[&str]) -> LedgerBytes {
        let mut body = lines.join("\n");
        body.push('\n');
        LedgerBytes {
            label: "test-ledger.jsonl".into(),
            bytes: body.into_bytes(),
        }
    }

    fn event(block: u64, pools: &[&str]) -> KnownBotEvent {
        KnownBotEvent {
            bot_address: "0xbot".into(),
            tx_hash: "0xtx".into(),
            block_number: block,
            ordered_pools: pools.iter().map(|s| (*s).to_string()).collect(),
            route: None,
            label: Some("fixture".into()),
        }
    }

    #[test]
    fn fixture_files_classify_all_three_buckets() {
        let ledger = LedgerBytes::load("tests/fixtures/shadow_bot_benchmark/ledger.jsonl")
            .expect("fixture ledger");
        let events = load_known_bot_events("tests/fixtures/shadow_bot_benchmark/known_bots.json")
            .expect("fixture events");
        let report = compare(&[ledger], &events).expect("compare fixtures");
        assert_eq!(report.bucket_counts.would_have_been_profitable, 1);
        assert_eq!(report.bucket_counts.unprofitable_or_revert, 1);
        assert_eq!(report.bucket_counts.missed_detection, 1);
        assert!(report.no_send_enforced);
    }

    #[test]
    fn bucket1_missed_when_no_candidate_at_block() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xpoolA", "0xpoolB"], "10"),
            &candidate_line("0xd1", r#"{"kind":"pass"}"#),
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        let row = classify_event(&event(999, &[]), &index);
        assert_eq!(row.bucket, Bucket::MissedDetection);
        assert!(row.matching_digests.is_empty());
    }

    #[test]
    fn bucket1_missed_when_pools_do_not_match() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xpoolA", "0xpoolB"], "10"),
            &candidate_line("0xd1", r#"{"kind":"pass"}"#),
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        let row = classify_event(&event(100, &["0xother", "0xpoolB"]), &index);
        assert_eq!(row.bucket, Bucket::MissedDetection);
    }

    #[test]
    fn bucket2_when_candidate_reverts() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xpoolA", "0xpoolB"], "10"),
            &candidate_line("0xd1", r#"{"kind":"revert","reason":"slippage"}"#),
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        let row = classify_event(&event(100, &["0xpoolA", "0xpoolB"]), &index);
        assert_eq!(row.bucket, Bucket::UnprofitableOrRevert);
        assert_eq!(row.best_outcome_kind.as_deref(), Some("revert"));
    }

    #[test]
    fn bucket2_when_pass_but_zero_net_profit() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xpoolA"], "0"),
            &candidate_line("0xd1", r#"{"kind":"pass"}"#),
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        let row = classify_event(&event(100, &[]), &index);
        assert_eq!(row.bucket, Bucket::UnprofitableOrRevert);
    }

    #[test]
    fn bucket3_when_pass_with_positive_net_profit() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xAa", "0xBb"], "42"),
            &candidate_line("0xd1", r#"{"kind":"pass"}"#),
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        // Case-insensitive pool match.
        let row = classify_event(&event(100, &["0xaa", "0xbb"]), &index);
        assert_eq!(row.bucket, Bucket::WouldHaveBeenProfitable);
        assert_eq!(row.best_net_profit.as_deref(), Some("42"));
    }

    #[test]
    fn compare_classifies_all_three_buckets_in_one_report() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            // block 10 profitable
            &context_line("0xd1", 10, &["0xp1"], "5"),
            &candidate_line("0xd1", r#"{"kind":"pass"}"#),
            // block 20 revert
            &context_line("0xd2", 20, &["0xp2"], "5"),
            &candidate_line("0xd2", r#"{"kind":"revert","reason":"x"}"#),
            // block 30 present but no event will use a different block for miss
        ]);
        let events = vec![
            event(10, &[]), // bucket 3
            event(20, &[]), // bucket 2
            event(99, &[]), // bucket 1
        ];
        let report = compare(&[led], &events).unwrap();
        assert_eq!(report.bucket_counts.would_have_been_profitable, 1);
        assert_eq!(report.bucket_counts.unprofitable_or_revert, 1);
        assert_eq!(report.bucket_counts.missed_detection, 1);
        assert!(report.no_send_enforced);
        let md = render_markdown_report(&report);
        assert!(md.contains("missed_detection"));
        assert!(md.contains("block=99"));
    }

    #[test]
    fn rejects_non_no_send_capability() {
        let led = ledger(&[&header_line(SERVICE, "full_send")]);
        let err = ShadowLedgerIndex::from_ledgers(&[led]).unwrap_err();
        assert!(matches!(
            err,
            BenchmarkError::SendCapabilityViolation { .. }
        ));
    }

    #[test]
    fn context_without_candidate_does_not_count_as_detection() {
        let led = ledger(&[
            &header_line(SERVICE, "no_send"),
            &context_line("0xd1", 100, &["0xp"], "10"),
            // no candidate row
        ]);
        let index = ShadowLedgerIndex::from_ledgers(&[led]).unwrap();
        let row = classify_event(&event(100, &[]), &index);
        assert_eq!(row.bucket, Bucket::MissedDetection);
    }

    #[test]
    fn parse_u256_positive_digits_only_like_shadow_report() {
        assert!(parse_u256_positive("1"));
        assert!(parse_u256_positive("42"));
        assert!(!parse_u256_positive("0"));
        assert!(!parse_u256_positive(""));
        // Hex / signed are rejected (fail-closed for bucket 3).
        assert!(!parse_u256_positive("0x1"));
        assert!(!parse_u256_positive("-1"));
    }
}
