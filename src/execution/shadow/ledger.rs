//! `ShadowLedgerWriter` — an append-only, schema-versioned JSONL ledger for shadow-mode
//! candidates.
//!
//! One JSON object per line, three row shapes tagged by `row_type`:
//! - `run_header`, written once at [`ShadowLedgerWriter::open`], pinning the manifest
//!   digests a run started with (so a mid-run config change is visible against the
//!   header rather than silently applied — see `manifest.rs`).
//! - `provenance`, one per candidate, recording how its pool address was established
//!   (CREATE2-verified, allowlisted, skipped, rejected — `manifest::PoolProvenanceOutcome`).
//!   Written by the caller (`ShadowExecutionContext`, task #11) via
//!   [`ShadowLedgerWriter::record_provenance`], since that check happens before the
//!   candidate's pool/token context is erased into a `FinalRequest` — see
//!   `call_executor.rs`'s doc comment for why `PreflightAttemptSink::record` alone can't
//!   see it.
//! - `candidate`, one per [`preflight::PreflightAttempt`], written through
//!   [`preflight::PreflightAttemptSink`] with zero glue code.
//!
//! Durability is OS-buffer `flush()` per row, not `sync_data()` — sufficient for
//! process-level durability across ordinary runs; none of WHI-549's acceptance criteria
//! call for crash-safety across a hard OS crash, so this module does not pay that
//! latency cost.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use alloy::primitives::U256;
use serde::{Deserialize, Serialize};

use super::manifest::{PoolProvenanceOutcome, ShadowOverrideManifest};
use crate::execution::fee_context::BlockFeeContext;
use crate::execution::final_request::FinalRequestDigest;
use crate::execution::gas_profile::RouteKey;
use crate::execution::identity::ExecutionIdentity;
use crate::execution::preflight::{
    self, BlockTag, PolicyKey, PreflightAttempt, PreflightOutcome, RpcErrorClass,
};
use crate::state_space::{BlockHeaderContext, SnapshotId};

/// Schema version for every row this module writes. Bump alongside any breaking change
/// to a row's shape.
pub(crate) const LEDGER_SCHEMA_VERSION: &str = "whisker-arb/shadow-ledger/v1";

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("shadow ledger io: {0}")]
    Io(String),
    #[error("shadow ledger json: {0}")]
    Json(String),
}

/// Serde mirror of [`preflight::PolicyKey`] -- that type has no `Serialize` (WHI-521
/// never needed one), so this module owns the wire representation rather than adding a
/// dependency preflight.rs doesn't otherwise need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LedgerPolicyKey {
    Mandatory,
    ApprovedStableDisabled,
    ApprovedStableSampled,
}

impl From<PolicyKey> for LedgerPolicyKey {
    fn from(key: PolicyKey) -> Self {
        match key {
            PolicyKey::Mandatory => Self::Mandatory,
            PolicyKey::ApprovedStableDisabled => Self::ApprovedStableDisabled,
            PolicyKey::ApprovedStableSampled => Self::ApprovedStableSampled,
        }
    }
}

/// Serde mirror of [`preflight::BlockTag`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LedgerBlockTag {
    Latest,
    Pending,
}

impl From<BlockTag> for LedgerBlockTag {
    fn from(tag: BlockTag) -> Self {
        match tag {
            BlockTag::Latest => Self::Latest,
            BlockTag::Pending => Self::Pending,
        }
    }
}

/// Serde mirror of [`preflight::RpcErrorClass`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LedgerRpcErrorClass {
    Transport,
    ErrorResponse,
    Other,
}

impl From<RpcErrorClass> for LedgerRpcErrorClass {
    fn from(class: RpcErrorClass) -> Self {
        match class {
            RpcErrorClass::Transport => Self::Transport,
            RpcErrorClass::ErrorResponse => Self::ErrorResponse,
            RpcErrorClass::Other => Self::Other,
        }
    }
}

/// Serde mirror of [`preflight::PreflightOutcome`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum LedgerOutcome {
    Pass,
    Revert { reason: String },
    RpcError { class: LedgerRpcErrorClass },
    EnvUnsupported,
    SkippedApproved,
    SampledOut,
}

impl From<PreflightOutcome> for LedgerOutcome {
    fn from(outcome: PreflightOutcome) -> Self {
        match outcome {
            PreflightOutcome::Pass => Self::Pass,
            PreflightOutcome::Revert(reason) => Self::Revert { reason },
            PreflightOutcome::RpcError(class) => Self::RpcError {
                class: class.into(),
            },
            PreflightOutcome::EnvUnsupported => Self::EnvUnsupported,
            PreflightOutcome::SkippedApproved => Self::SkippedApproved,
            PreflightOutcome::SampledOut => Self::SampledOut,
        }
    }
}

/// Written once at [`ShadowLedgerWriter::open`]; pins the manifest digests every
/// candidate row in this run is implicitly checked against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerRunHeader {
    pub schema_version: String,
    pub storage_layout_digest: String,
    pub wmnt_descriptor_digest: String,
    pub moe_allowlist_digest: String,
    pub identity_digest: String,
    pub approved_pools_digest: String,
    pub threshold_config_digest: String,
    pub started_at_unix: u64,
}

impl LedgerRunHeader {
    pub(crate) fn from_manifest(manifest: &ShadowOverrideManifest, started_at_unix: u64) -> Self {
        Self {
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            storage_layout_digest: manifest.storage_layout_digest.to_string(),
            wmnt_descriptor_digest: manifest.wmnt_descriptor_digest.to_string(),
            moe_allowlist_digest: manifest.moe_allowlist_digest.to_string(),
            identity_digest: manifest.identity_digest.to_string(),
            approved_pools_digest: manifest.approved_pools_digest.to_string(),
            threshold_config_digest: manifest.threshold_config_digest.to_string(),
            started_at_unix,
        }
    }
}

/// Serde mirror of [`state_space::SnapshotId`] -- that type has no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerSnapshotId {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: String,
}

impl From<SnapshotId> for LedgerSnapshotId {
    fn from(id: SnapshotId) -> Self {
        Self {
            chain_id: id.chain_id,
            block_number: id.block_number,
            block_hash: id.block_hash.to_string(),
        }
    }
}

/// Serde mirror of [`state_space::BlockHeaderContext`] -- that type has no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerBlockHeaderContext {
    pub parent_hash: String,
    pub block_timestamp: u64,
}

impl From<BlockHeaderContext> for LedgerBlockHeaderContext {
    fn from(header: BlockHeaderContext) -> Self {
        Self {
            parent_hash: header.parent_hash.to_string(),
            block_timestamp: header.block_timestamp,
        }
    }
}

/// Serde mirror of [`crate::execution::fee_context::BlockFeeContext`] -- that type has
/// no `Serialize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerBlockFeeContext {
    pub block_number: u64,
    pub block_hash: String,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

impl From<BlockFeeContext> for LedgerBlockFeeContext {
    fn from(fee_context: BlockFeeContext) -> Self {
        Self {
            block_number: fee_context.block_number,
            block_hash: fee_context.block_hash.to_string(),
            base_fee_per_gas: fee_context.base_fee_per_gas,
            block_gas_limit: fee_context.block_gas_limit,
        }
    }
}

/// Serde mirror of [`crate::execution::identity::ExecutionIdentity`] -- that type has
/// no `Serialize`. Carries the pinned block identity (`snapshot_id`/`header`) a
/// candidate's `eth_call` was evaluated against, so a shadow run's per-candidate rows
/// are reproducible without assuming an implicit "latest" per call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerExecutionIdentity {
    pub snapshot_id: LedgerSnapshotId,
    pub header: LedgerBlockHeaderContext,
    pub pool_universe_fingerprint: String,
    pub route: RouteKey,
    pub fee_context: LedgerBlockFeeContext,
    pub gas_profile_identity: String,
}

impl From<&ExecutionIdentity> for LedgerExecutionIdentity {
    fn from(identity: &ExecutionIdentity) -> Self {
        Self {
            snapshot_id: identity.snapshot_id.into(),
            header: identity.header.into(),
            pool_universe_fingerprint: identity.pool_universe_fingerprint.to_string(),
            route: identity.route.clone(),
            fee_context: identity.fee_context.clone().into(),
            gas_profile_identity: identity.gas_profile_identity.clone(),
        }
    }
}

/// Provenance of a candidate's recorded profit -- [`FinalRequest::min_profit`] is a bare
/// `U256` with no provenance today, so this is recorded alongside it rather than inferred
/// later. Shadow mode never broadcasts a transaction, so no row in this ledger can ever
/// carry a realized, on-chain-executed profit -- every value is inherently a simulated
/// one, whether it originates from the off-chain path-optimizer estimate baked into the
/// request or from the shadow `eth_call` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfitBasis {
    Simulated,
}

/// One candidate's execution identity and recorded profit, keyed by the same digest as
/// its [`LedgerProvenanceRow`]/[`LedgerCandidateRow`] -- written before the `eth_call`
/// alongside `record_provenance`, for the same reason (see this module's doc comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerContextRow {
    pub schema_version: String,
    pub digest: String,
    pub identity: LedgerExecutionIdentity,
    pub min_profit: String,
    pub profit_basis: ProfitBasis,
}

/// One candidate's pool-provenance check, recorded ahead of (and independently from) its
/// [`LedgerCandidateRow`] -- see this module's doc comment for why the two can't be
/// merged into a single row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerProvenanceRow {
    pub schema_version: String,
    pub digest: String,
    pub outcome: PoolProvenanceOutcome,
}

/// One [`preflight::PreflightAttempt`], as recorded through [`preflight::PreflightAttemptSink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerCandidateRow {
    pub schema_version: String,
    pub digest: String,
    pub policy_key: LedgerPolicyKey,
    pub outcome: LedgerOutcome,
    pub block_tag: Option<LedgerBlockTag>,
    pub latency_ms: Option<u128>,
    pub detail: Option<String>,
    pub recorded_at_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "row_type", rename_all = "snake_case")]
pub(crate) enum LedgerRow {
    RunHeader(LedgerRunHeader),
    Provenance(LedgerProvenanceRow),
    Candidate(LedgerCandidateRow),
    Context(LedgerContextRow),
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn write_row(file: &mut File, row: &LedgerRow) -> Result<(), LedgerError> {
    let mut line = serde_json::to_string(row).map_err(|e| LedgerError::Json(e.to_string()))?;
    line.push('\n');
    file.write_all(line.as_bytes())
        .map_err(|e| LedgerError::Io(e.to_string()))?;
    file.flush().map_err(|e| LedgerError::Io(e.to_string()))
}

/// Append-only shadow-mode ledger. Implements [`preflight::PreflightAttemptSink`]
/// directly so it plugs into `RiskTieredPreflight` as the attempt sink with zero glue
/// code; [`Self::record_provenance`] is a separate, non-trait method for the pool-
/// provenance row that the fixed `PreflightAttemptSink::record` signature has no way to
/// carry (see this module's doc comment).
pub struct ShadowLedgerWriter {
    file: Mutex<File>,
}

impl ShadowLedgerWriter {
    /// Opens `path` for append (creating it and its parent directories if needed) and
    /// writes `header` as the first row of this call. Reopening an existing ledger file
    /// appends a fresh header rather than truncating -- callers that care about one
    /// header per file should give each run its own path.
    pub(crate) fn open(path: &Path, header: LedgerRunHeader) -> Result<Self, LedgerError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| LedgerError::Io(e.to_string()))?;
            }
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        write_row(&mut file, &LedgerRow::RunHeader(header))?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    /// Records how `digest`'s candidate pool address was established. Must be called
    /// before the matching [`LedgerCandidateRow`] is written for the same digest, since
    /// the pool/token context this check needs is not available once a `FinalRequest`
    /// has been built (see `call_executor.rs`'s doc comment).
    pub fn record_provenance(
        &self,
        digest: FinalRequestDigest,
        outcome: PoolProvenanceOutcome,
    ) -> Result<(), LedgerError> {
        let row = LedgerRow::Provenance(LedgerProvenanceRow {
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            digest: digest.0.to_string(),
            outcome,
        });
        let mut file = self
            .file
            .lock()
            .map_err(|_| LedgerError::Io("ledger file mutex poisoned".to_string()))?;
        write_row(&mut file, &row)
    }

    /// Records `digest`'s candidate execution identity (pinned block identity, route,
    /// fee context) and recorded profit/basis. Must be called before the matching
    /// [`LedgerCandidateRow`] is written for the same digest, for the same ordering
    /// reason as [`Self::record_provenance`].
    pub fn record_context(
        &self,
        digest: FinalRequestDigest,
        identity: &ExecutionIdentity,
        min_profit: U256,
        profit_basis: ProfitBasis,
    ) -> Result<(), LedgerError> {
        let row = LedgerRow::Context(LedgerContextRow {
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            digest: digest.0.to_string(),
            identity: identity.into(),
            min_profit: min_profit.to_string(),
            profit_basis,
        });
        let mut file = self
            .file
            .lock()
            .map_err(|_| LedgerError::Io("ledger file mutex poisoned".to_string()))?;
        write_row(&mut file, &row)
    }
}

impl preflight::PreflightAttemptSink for ShadowLedgerWriter {
    fn record(&self, attempt: PreflightAttempt) {
        let row = LedgerRow::Candidate(LedgerCandidateRow {
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            digest: attempt.digest.0.to_string(),
            policy_key: attempt.policy_key.into(),
            outcome: attempt.outcome.into(),
            block_tag: attempt.block_tag.map(Into::into),
            latency_ms: attempt.latency.map(|d| d.as_millis()),
            detail: attempt.detail,
            recorded_at_unix: unix_now(),
        });

        let Ok(mut file) = self.file.lock() else {
            tracing::error!(
                target: "execution.shadow.ledger",
                "ledger file mutex poisoned; dropping candidate row"
            );
            return;
        };
        if let Err(err) = write_row(&mut file, &row) {
            tracing::error!(
                target: "execution.shadow.ledger",
                error = %err,
                "failed to write shadow ledger candidate row"
            );
        }
    }
}

/// Lets one `Arc<ShadowLedgerWriter>` be shared as the `PreflightAttemptSink` across many
/// per-candidate `RiskTieredPreflight` instances — each candidate needs its own
/// `ShadowSemanticCallExecutor` (its `StateOverride` is baked in at construction, see
/// `call_executor.rs`), and `RiskTieredPreflight` owns its sink by value, so sharing one
/// underlying ledger file across candidates requires cloning the `Arc`, not the writer.
impl preflight::PreflightAttemptSink for std::sync::Arc<ShadowLedgerWriter> {
    fn record(&self, attempt: PreflightAttempt) {
        (**self).record(attempt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::preflight::PreflightAttemptSink;
    use alloy::primitives::B256;
    use std::io::{BufRead, BufReader};
    use std::sync::Arc;
    use std::time::Duration;

    fn sample_manifest() -> ShadowOverrideManifest {
        ShadowOverrideManifest {
            storage_layout_digest: B256::repeat_byte(0x11),
            wmnt_descriptor_digest: B256::repeat_byte(0x22),
            moe_allowlist_digest: B256::repeat_byte(0x33),
            identity_digest: B256::repeat_byte(0x44),
            approved_pools_digest: B256::repeat_byte(0x55),
            threshold_config_digest: B256::repeat_byte(0x66),
        }
    }

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        let file = File::open(path).unwrap();
        BufReader::new(file)
            .lines()
            .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
            .collect()
    }

    #[test]
    fn open_writes_a_run_header_row_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);

        let _writer = ShadowLedgerWriter::open(&path, header.clone()).unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["row_type"], "run_header");
        assert_eq!(lines[0]["identity_digest"], header.identity_digest);
        assert_eq!(
            lines[0]["approved_pools_digest"],
            header.approved_pools_digest
        );
        assert_eq!(
            lines[0]["threshold_config_digest"],
            header.threshold_config_digest
        );
    }

    #[test]
    fn record_appends_a_candidate_row_after_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        writer.record(PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Revert("insufficient liquidity".to_string()),
            digest: FinalRequestDigest(B256::repeat_byte(0xAB)),
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(12)),
            detail: Some("insufficient liquidity".to_string()),
        });

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "candidate");
        assert_eq!(lines[1]["policy_key"], "mandatory");
        assert_eq!(lines[1]["outcome"]["kind"], "revert");
        assert_eq!(lines[1]["outcome"]["reason"], "insufficient liquidity");
        assert_eq!(lines[1]["block_tag"], "latest");
        assert_eq!(lines[1]["latency_ms"], 12);
    }

    #[test]
    fn record_skip_rows_have_no_block_tag_or_latency() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        writer.record(PreflightAttempt {
            policy_key: PolicyKey::ApprovedStableSampled,
            outcome: PreflightOutcome::SampledOut,
            digest: FinalRequestDigest(B256::repeat_byte(0xCD)),
            block_tag: None,
            latency: None,
            detail: None,
        });

        let lines = read_lines(&path);
        assert_eq!(lines[1]["outcome"]["kind"], "sampled_out");
        assert!(lines[1]["block_tag"].is_null());
        assert!(lines[1]["latency_ms"].is_null());
    }

    #[test]
    fn record_provenance_writes_a_distinct_row_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        writer
            .record_provenance(
                FinalRequestDigest(B256::repeat_byte(0xEF)),
                PoolProvenanceOutcome::MoeAllowlisted,
            )
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "provenance");
        assert_eq!(lines[1]["outcome"], "moe_allowlisted");
    }

    fn sample_identity() -> ExecutionIdentity {
        ExecutionIdentity {
            snapshot_id: SnapshotId::new(5000, 10, B256::repeat_byte(0x01)),
            header: BlockHeaderContext::new(B256::repeat_byte(0x02), 100),
            pool_universe_fingerprint: B256::repeat_byte(0x03),
            route: RouteKey::new(vec![crate::execution::gas_profile::ProtocolKind::V2]).unwrap(),
            fee_context: BlockFeeContext {
                block_number: 10,
                block_hash: B256::repeat_byte(0x01),
                base_fee_per_gas: 1_000_000_000,
                block_gas_limit: 30_000_000,
            },
            gas_profile_identity: "test-profile".to_string(),
        }
    }

    #[test]
    fn record_context_writes_a_distinct_row_type_with_the_pinned_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        let identity = sample_identity();
        writer
            .record_context(
                FinalRequestDigest(B256::repeat_byte(0x9A)),
                &identity,
                U256::from(42u64),
                ProfitBasis::Simulated,
            )
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "context");
        assert_eq!(lines[1]["min_profit"], "42");
        assert_eq!(lines[1]["profit_basis"], "simulated");
        assert_eq!(
            lines[1]["identity"]["snapshot_id"]["block_number"],
            10
        );
        assert_eq!(
            lines[1]["identity"]["header"]["block_timestamp"],
            100
        );
    }

    #[test]
    fn arc_wrapped_writer_records_into_the_same_underlying_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);
        let writer = Arc::new(ShadowLedgerWriter::open(&path, header).unwrap());

        // Two clones of the same `Arc` -- as `ShadowExecutionContext::build_preflight`
        // constructs per candidate -- must both append to the one file behind them.
        let sink_a: Arc<ShadowLedgerWriter> = Arc::clone(&writer);
        let sink_b: Arc<ShadowLedgerWriter> = Arc::clone(&writer);

        PreflightAttemptSink::record(
            &sink_a,
            PreflightAttempt {
                policy_key: PolicyKey::Mandatory,
                outcome: PreflightOutcome::Pass,
                digest: FinalRequestDigest(B256::repeat_byte(0x01)),
                block_tag: Some(BlockTag::Latest),
                latency: Some(Duration::from_millis(1)),
                detail: None,
            },
        );
        PreflightAttemptSink::record(
            &sink_b,
            PreflightAttempt {
                policy_key: PolicyKey::Mandatory,
                outcome: PreflightOutcome::Pass,
                digest: FinalRequestDigest(B256::repeat_byte(0x02)),
                block_tag: Some(BlockTag::Latest),
                latency: Some(Duration::from_millis(1)),
                detail: None,
            },
        );

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["row_type"], "run_header");
        assert_eq!(lines[1]["row_type"], "candidate");
        assert_eq!(lines[2]["row_type"], "candidate");
    }

    #[test]
    fn reopening_an_existing_ledger_appends_rather_than_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header = LedgerRunHeader::from_manifest(&sample_manifest(), 1_700_000_000);

        let writer = ShadowLedgerWriter::open(&path, header.clone()).unwrap();
        writer.record(PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Pass,
            digest: FinalRequestDigest(B256::repeat_byte(0x01)),
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(3)),
            detail: None,
        });
        drop(writer);

        let _writer2 = ShadowLedgerWriter::open(&path, header).unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["row_type"], "run_header");
        assert_eq!(lines[1]["row_type"], "candidate");
        assert_eq!(lines[2]["row_type"], "run_header");
    }
}
