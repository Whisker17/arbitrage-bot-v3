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
//!
//! ## Size bound (WHI-952 / G-5)
//!
//! Segments rotate when the active file reaches `RotationPolicy::max_segment_bytes`
//! (env `SHADOW_LEDGER_MAX_SEGMENT_BYTES`, default 64 MiB). Rotated files are named
//! `path.1`, `path.2`, … and reclaimed until total size ≤
//! `SHADOW_LEDGER_MAX_TOTAL_BYTES` (default 512 MiB). Each new segment starts with a
//! fresh `run_header` and sequence `0` so every segment is independently auditable.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ops::{
    apply_retention, rotate_active_file, total_bytes_for_path, RotationPolicy, SegmentPaths,
};

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use super::manifest::{PoolProvenanceOutcome, ShadowOverrideManifest};
use super::overrides::ShadowRouteSummary;
use crate::execution::fee_context::BlockFeeContext;
use crate::execution::final_request::FinalRequestDigest;
use crate::execution::gas_profile::RouteKey;
use crate::execution::identity::ExecutionIdentity;
use crate::execution::preflight::{
    self, BlockTag, PolicyKey, PreflightAttempt, PreflightOutcome, RpcErrorClass,
};
use crate::execution::shadow_gate_plan::digest_bytes;
use crate::state_space::{BlockHeaderContext, SnapshotId};

/// Schema version for every row this module writes. Bump alongside any breaking change
/// to a row's shape.
pub(crate) const LEDGER_SCHEMA_VERSION: &str = "whisker-arb/shadow-ledger/v3";

pub(crate) const NO_SEND_CAPABILITY: &str = "no_send";

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("shadow ledger io: {0}")]
    Io(String),
    #[error("shadow ledger json: {0}")]
    Json(String),
    #[error("shadow ledger prefix changed while the writer was open")]
    PrefixChanged,
    #[error("shadow ledger sequence at line {line} is {found}, expected {expected}")]
    InvalidSequence {
        line: usize,
        found: u64,
        expected: u64,
    },
    #[error("shadow ledger row at line {line} has no sequence")]
    MissingSequence { line: usize },
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

/// Bundles the run-level identity fields [`LedgerRunHeader`] pins beyond the manifest's
/// own config digests, so [`LedgerRunHeader::from_manifest`]'s signature doesn't grow an
/// unbounded parameter list as the header gains fields.
pub(crate) struct RunMetadata {
    pub run_id: String,
    pub git_commit: String,
    pub chain_id: u64,
    pub service: String,
    pub executor_contract: Address,
    pub wmnt_address: Address,
    pub started_at_unix: u64,
}

/// Written once at [`ShadowLedgerWriter::open`]; pins the manifest digests every
/// candidate row in this run is implicitly checked against, plus the run's own identity
/// (which service/executor/WMNT deployment/build it came from).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerRunHeader {
    pub sequence: u64,
    pub schema_version: String,
    pub run_id: String,
    pub git_commit: String,
    pub chain_id: u64,
    pub service: String,
    pub executor_contract: String,
    pub wmnt_address: String,
    pub config_digest: String,
    pub storage_layout_digest: String,
    pub wmnt_descriptor_digest: String,
    pub moe_allowlist_digest: String,
    pub identity_digest: String,
    pub approved_pools_digest: String,
    pub threshold_config_digest: String,
    pub profile_digest: String,
    pub override_digest: String,
    pub send_capability: String,
    /// The pinned block/route identity this run started against, if known at
    /// header-write time. Always `None` today: `ShadowExecutionContext::new` makes
    /// zero RPC calls, so there is no live block to pin when the header is written.
    /// Recorded as an honest absence -- the same sentinel convention
    /// `wmnt_descriptor.rs`'s `runtime_codehash` uses for a not-yet-independently-
    /// verified value -- rather than fabricated or backfilled via an RPC call the
    /// shadow-runtime spec forbids at construction time.
    pub start_identity: Option<LedgerExecutionIdentity>,
    pub started_at_unix: u64,
}

impl LedgerRunHeader {
    pub(crate) fn from_manifest(manifest: &ShadowOverrideManifest, metadata: RunMetadata) -> Self {
        Self {
            sequence: 0,
            schema_version: LEDGER_SCHEMA_VERSION.to_string(),
            run_id: metadata.run_id,
            git_commit: metadata.git_commit,
            chain_id: metadata.chain_id,
            service: metadata.service,
            executor_contract: metadata.executor_contract.to_string(),
            wmnt_address: metadata.wmnt_address.to_string(),
            config_digest: manifest.config_digest().to_string(),
            storage_layout_digest: manifest.storage_layout_digest.to_string(),
            wmnt_descriptor_digest: manifest.wmnt_descriptor_digest.to_string(),
            moe_allowlist_digest: manifest.moe_allowlist_digest.to_string(),
            identity_digest: manifest.identity_digest.to_string(),
            approved_pools_digest: manifest.approved_pools_digest.to_string(),
            threshold_config_digest: manifest.threshold_config_digest.to_string(),
            profile_digest: manifest.profile_digest.to_string(),
            override_digest: manifest.override_digest.to_string(),
            send_capability: NO_SEND_CAPABILITY.to_string(),
            start_identity: None,
            started_at_unix: metadata.started_at_unix,
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

/// One candidate's execution identity, route, and recorded profit, keyed by the same
/// digest as its [`LedgerProvenanceRow`]/[`LedgerCandidateRow`] -- written before the
/// `eth_call` alongside `record_provenance`, for the same reason (see this module's doc
/// comment).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerContextRow {
    pub sequence: u64,
    pub schema_version: String,
    pub digest: String,
    pub identity: LedgerExecutionIdentity,
    /// Route-topology fingerprint (digest over the ordered, decoded pool list) --
    /// stable across nonce/gas variations of the same opportunity, distinct from
    /// `digest` (which is over the fully-built `FinalRequest`).
    pub opportunity_id: String,
    pub ordered_pools: Vec<String>,
    pub amount_in: String,
    pub gross_profit: String,
    pub net_profit: String,
    pub profit_basis: ProfitBasis,
}

/// One candidate's pool-provenance check, recorded ahead of (and independently from) its
/// [`LedgerCandidateRow`] -- see this module's doc comment for why the two can't be
/// merged into a single row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerProvenanceRow {
    pub sequence: u64,
    pub schema_version: String,
    pub digest: String,
    /// Combined worst-case outcome across every hop (see
    /// `overrides.rs::combine_provenance_outcomes`) -- what the short-circuit
    /// `EnvUnsupported` check in `call_executor.rs` acts on.
    pub outcome: PoolProvenanceOutcome,
    /// Per-hop provenance, in route order. `outcome` alone collapses a multi-hop
    /// route to its single weakest hop; this preserves every hop's own result so a
    /// route's full provenance coverage is independently auditable.
    pub hop_outcomes: Vec<PoolProvenanceOutcome>,
}

/// One [`preflight::PreflightAttempt`], as recorded through [`preflight::PreflightAttemptSink`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerCandidateRow {
    pub sequence: u64,
    pub schema_version: String,
    pub digest: String,
    pub policy_key: LedgerPolicyKey,
    pub outcome: LedgerOutcome,
    pub block_tag: Option<LedgerBlockTag>,
    pub latency_ms: Option<u128>,
    pub detail: Option<String>,
    pub recorded_at_unix: u64,
}

/// Optional discovery snapshot on an observation row (WHI-957 dirty-cycle input).
///
/// Backward-compatible: older ledgers omit this field entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct LedgerDiscoveryView {
    /// Watch loop did not process this head (pin skip / halt).
    #[serde(default)]
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    /// Dirty pool addresses for this head (lower-case hex preferred).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty_pools: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles_optimized: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cycles_total: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paths_quoted: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amm_quotes: Option<u64>,
    /// `"full"` | `"touched"` when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// One independently observed canonical block. These rows are written from each
/// service's block loop, including blocks with no candidate, so runtime and
/// continuity evidence cannot be inferred from opportunity activity alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LedgerObservationRow {
    pub sequence: u64,
    pub schema_version: String,
    pub snapshot_id: LedgerSnapshotId,
    pub header: LedgerBlockHeaderContext,
    pub recorded_at_unix: u64,
    /// WHI-957: optional dirty-set / skip snapshot for peer attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<LedgerDiscoveryView>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "row_type", rename_all = "snake_case")]
pub(crate) enum LedgerRow {
    RunHeader(LedgerRunHeader),
    Provenance(LedgerProvenanceRow),
    Candidate(LedgerCandidateRow),
    Context(LedgerContextRow),
    Observation(LedgerObservationRow),
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerAudit {
    pub row_count: u64,
    pub next_sequence: u64,
    pub prefix_digest: String,
}

struct LedgerState {
    path: PathBuf,
    byte_len: u64,
    prefix_digest: String,
    next_sequence: u64,
}

pub struct ShadowLedgerWriter {
    file: Mutex<File>,
    state: Mutex<LedgerState>,
    failure: Mutex<Option<String>>,
    /// Soft segment / hard total caps (WHI-952). Defaults from env at open.
    policy: RotationPolicy,
    /// Template re-emitted as `run_header` on every new segment (sequence rewritten).
    header_template: LedgerRunHeader,
    /// How many times this writer has rotated the active segment (tests + metrics).
    rotations: AtomicU64,
}

fn audit_bytes_internal(bytes: &[u8]) -> Result<LedgerAudit, LedgerError> {
    let mut expected = 0u64;
    for (index, line) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_slice(line).map_err(|error| LedgerError::Json(error.to_string()))?;
        let sequence = value
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .ok_or(LedgerError::MissingSequence { line: index + 1 })?;
        if sequence != expected {
            return Err(LedgerError::InvalidSequence {
                line: index + 1,
                found: sequence,
                expected,
            });
        }
        expected += 1;
    }
    Ok(LedgerAudit {
        row_count: expected,
        next_sequence: expected,
        prefix_digest: digest_bytes(bytes),
    })
}

pub fn audit_bytes(bytes: &[u8]) -> Result<LedgerAudit, LedgerError> {
    audit_bytes_internal(bytes)
}

impl ShadowLedgerWriter {
    /// Opens `path` for append (creating it and its parent directories if needed) and
    /// writes `header` as the first row of this call. Reopening an existing ledger file
    /// appends a fresh header rather than truncating -- callers that care about one
    /// header per file should give each run its own path.
    ///
    /// Rotation policy is loaded from `SHADOW_LEDGER_MAX_*` env vars (see
    /// [`RotationPolicy::shadow_ledger_from_env`]).
    pub(crate) fn open(path: &Path, header: LedgerRunHeader) -> Result<Self, LedgerError> {
        let policy = RotationPolicy::shadow_ledger_from_env()
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        Self::open_with_policy(path, header, policy)
    }

    /// Like [`Self::open`] but with an explicit rotation policy (tests force a small
    /// threshold to prove rotation; production uses env defaults).
    pub(crate) fn open_with_policy(
        path: &Path,
        header: LedgerRunHeader,
        policy: RotationPolicy,
    ) -> Result<Self, LedgerError> {
        policy
            .validate()
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| LedgerError::Io(e.to_string()))?;
            }
        }
        let existing = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(LedgerError::Io(error.to_string())),
        };
        // If the leftover active segment already exceeds the soft cap (e.g. policy
        // tightened between runs), rotate it before writing the new header.
        let paths = SegmentPaths::new(path);
        let mut existing = existing;
        if !existing.is_empty() && policy.should_rotate_segment(existing.len() as u64) {
            rotate_active_file(&paths).map_err(|e| LedgerError::Io(e.to_string()))?;
            apply_retention(&paths, policy).map_err(|e| LedgerError::Io(e.to_string()))?;
            existing = Vec::new();
        }
        let audit = audit_bytes_internal(&existing)?;
        let header_template = header.clone();
        let mut header = header;
        header.sequence = audit.next_sequence;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        write_row(&mut file, &LedgerRow::RunHeader(header))?;
        let bytes = fs::read(path).map_err(|e| LedgerError::Io(e.to_string()))?;
        // If the single header alone exceeds the soft cap (tiny test policies), rotate
        // immediately so subsequent rows start a fresh segment.
        let writer = Self {
            file: Mutex::new(file),
            state: Mutex::new(LedgerState {
                path: path.to_path_buf(),
                byte_len: bytes.len() as u64,
                prefix_digest: digest_bytes(&bytes),
                next_sequence: audit.next_sequence + 1,
            }),
            failure: Mutex::new(None),
            policy,
            header_template,
            rotations: AtomicU64::new(0),
        };
        if policy.should_rotate_segment(bytes.len() as u64) {
            let mut file = writer
                .file
                .lock()
                .map_err(|_| LedgerError::Io("ledger file mutex poisoned".to_string()))?;
            let mut state = writer
                .state
                .lock()
                .map_err(|_| LedgerError::Io("ledger state mutex poisoned".to_string()))?;
            writer.rotate_locked(&mut file, &mut state)?;
        }
        Ok(writer)
    }

    /// Number of segment rotations performed since open (WHI-952 acceptance).
    pub fn rotation_count(&self) -> u64 {
        self.rotations.load(Ordering::Relaxed)
    }

    /// Active + rotated total bytes (for tests / operator checks).
    pub fn total_bytes(&self) -> Result<u64, LedgerError> {
        let state = self
            .state
            .lock()
            .map_err(|_| LedgerError::Io("ledger state mutex poisoned".to_string()))?;
        let paths = SegmentPaths::new(&state.path);
        total_bytes_for_path(&paths).map_err(|e| LedgerError::Io(e.to_string()))
    }

    fn append_row<F>(&self, build: F) -> Result<(), LedgerError>
    where
        F: FnOnce(u64) -> LedgerRow,
    {
        let result = self.append_row_inner(build);
        if let Err(error) = &result {
            self.record_failure(error);
        }
        result
    }

    fn append_row_inner<F>(&self, build: F) -> Result<(), LedgerError>
    where
        F: FnOnce(u64) -> LedgerRow,
    {
        let mut file = self
            .file
            .lock()
            .map_err(|_| LedgerError::Io("ledger file mutex poisoned".to_string()))?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| LedgerError::Io("ledger state mutex poisoned".to_string()))?;
        let current = fs::read(&state.path).map_err(|e| LedgerError::Io(e.to_string()))?;
        if current.len() as u64 != state.byte_len || digest_bytes(&current) != state.prefix_digest {
            return Err(LedgerError::PrefixChanged);
        }
        let row = build(state.next_sequence);
        write_row(&mut file, &row)?;
        let bytes = fs::read(&state.path).map_err(|e| LedgerError::Io(e.to_string()))?;
        state.byte_len = bytes.len() as u64;
        state.prefix_digest = digest_bytes(&bytes);
        state.next_sequence += 1;
        if self.policy.should_rotate_segment(state.byte_len) {
            self.rotate_locked(&mut file, &mut state)?;
        }
        Ok(())
    }

    /// Close the active segment, shift it to `.1`, open a fresh active file, and
    /// write a new run header at sequence 0. Caller holds both mutexes.
    ///
    /// On Unix the open fd may still point at the inode after rename; replacing
    /// `*file` drops that handle so subsequent writes target the new active path.
    fn rotate_locked(
        &self,
        file: &mut File,
        state: &mut LedgerState,
    ) -> Result<(), LedgerError> {
        file.flush().map_err(|e| LedgerError::Io(e.to_string()))?;
        let paths = SegmentPaths::new(&state.path);
        rotate_active_file(&paths).map_err(|e| LedgerError::Io(e.to_string()))?;
        apply_retention(&paths, self.policy).map_err(|e| LedgerError::Io(e.to_string()))?;

        let mut new_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&state.path)
            .map_err(|e| LedgerError::Io(e.to_string()))?;
        let mut header = self.header_template.clone();
        header.sequence = 0;
        write_row(&mut new_file, &LedgerRow::RunHeader(header))?;
        let bytes = fs::read(&state.path).map_err(|e| LedgerError::Io(e.to_string()))?;
        *file = new_file;
        state.byte_len = bytes.len() as u64;
        state.prefix_digest = digest_bytes(&bytes);
        state.next_sequence = 1;
        self.rotations.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn record_failure(&self, error: &LedgerError) {
        if let Ok(mut failure) = self.failure.lock() {
            if failure.is_none() {
                *failure = Some(error.to_string());
            }
        }
    }

    pub(crate) fn failure(&self) -> Option<String> {
        match self.failure.lock() {
            Ok(failure) => failure.clone(),
            Err(_) => Some("shadow ledger failure state mutex poisoned".to_string()),
        }
    }

    /// Records how `digest`'s candidate pool address was established. Must be called
    /// before the matching [`LedgerCandidateRow`] is written for the same digest, since
    /// the pool/token context this check needs is not available once a `FinalRequest`
    /// has been built (see `call_executor.rs`'s doc comment).
    pub fn record_provenance(
        &self,
        digest: FinalRequestDigest,
        outcome: PoolProvenanceOutcome,
        hop_outcomes: Vec<PoolProvenanceOutcome>,
    ) -> Result<(), LedgerError> {
        self.append_row(|sequence| {
            LedgerRow::Provenance(LedgerProvenanceRow {
                sequence,
                schema_version: LEDGER_SCHEMA_VERSION.to_string(),
                digest: digest.0.to_string(),
                outcome,
                hop_outcomes,
            })
        })
    }

    /// Records `digest`'s candidate execution identity (pinned block identity, route,
    /// fee context), route topology (see [`ShadowRouteSummary`]), and recorded gross/net
    /// profit. Must be called before the matching [`LedgerCandidateRow`] is written for
    /// the same digest, for the same ordering reason as [`Self::record_provenance`].
    pub fn record_context(
        &self,
        digest: FinalRequestDigest,
        identity: &ExecutionIdentity,
        route: &ShadowRouteSummary,
        gross_profit: U256,
        net_profit: U256,
        profit_basis: ProfitBasis,
    ) -> Result<(), LedgerError> {
        self.append_row(|sequence| {
            LedgerRow::Context(LedgerContextRow {
                sequence,
                schema_version: LEDGER_SCHEMA_VERSION.to_string(),
                digest: digest.0.to_string(),
                identity: identity.into(),
                opportunity_id: route.opportunity_id.to_string(),
                ordered_pools: route
                    .ordered_pools
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                amount_in: route.amount_in.to_string(),
                gross_profit: gross_profit.to_string(),
                net_profit: net_profit.to_string(),
                profit_basis,
            })
        })
    }

    /// Records a canonical header accepted by a service before candidate
    /// discovery. The report uses these rows, rather than candidate rows, for
    /// canonical-block coverage, runtime, and continuity calculations.
    pub fn record_canonical_observation(
        &self,
        snapshot_id: SnapshotId,
        header: BlockHeaderContext,
    ) -> Result<(), LedgerError> {
        self.record_canonical_observation_with_discovery(snapshot_id, header, None)
    }

    /// Like [`Self::record_canonical_observation`], optionally attaching a
    /// discovery snapshot (dirty pools / skip) for WHI-957 peer attribution.
    pub fn record_canonical_observation_with_discovery(
        &self,
        snapshot_id: SnapshotId,
        header: BlockHeaderContext,
        discovery: Option<LedgerDiscoveryView>,
    ) -> Result<(), LedgerError> {
        self.append_row(|sequence| {
            LedgerRow::Observation(LedgerObservationRow {
                sequence,
                schema_version: LEDGER_SCHEMA_VERSION.to_string(),
                snapshot_id: snapshot_id.into(),
                header: header.into(),
                recorded_at_unix: unix_now(),
                discovery,
            })
        })
    }
}

impl preflight::PreflightAttemptSink for ShadowLedgerWriter {
    fn record(&self, attempt: PreflightAttempt) {
        if let Err(err) = self.append_row(|sequence| {
            LedgerRow::Candidate(LedgerCandidateRow {
                sequence,
                schema_version: LEDGER_SCHEMA_VERSION.to_string(),
                digest: attempt.digest.0.to_string(),
                policy_key: attempt.policy_key.into(),
                outcome: attempt.outcome.into(),
                block_tag: attempt.block_tag.map(Into::into),
                latency_ms: attempt.latency.map(|d| d.as_millis()),
                detail: attempt.detail,
                recorded_at_unix: unix_now(),
            })
        }) {
            tracing::error!(
                target: "execution.shadow.ledger",
                error = %err,
                "failed to write shadow ledger candidate row"
            );
        }
    }

    fn failure(&self) -> Option<String> {
        ShadowLedgerWriter::failure(self)
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

    fn failure(&self) -> Option<String> {
        (**self).failure()
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
            profile_digest: B256::repeat_byte(0x77),
            override_digest: B256::repeat_byte(0x88),
        }
    }

    fn sample_metadata(started_at_unix: u64) -> RunMetadata {
        RunMetadata {
            run_id: "test-run-id".to_string(),
            git_commit: "deadbeef".to_string(),
            chain_id: 5000,
            service: "test-service".to_string(),
            executor_contract: Address::repeat_byte(0xEE),
            wmnt_address: Address::repeat_byte(0xFF),
            started_at_unix,
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
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));

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
        assert_eq!(lines[0]["profile_digest"], header.profile_digest);
        assert_eq!(lines[0]["override_digest"], header.override_digest);
        assert_eq!(lines[0]["run_id"], "test-run-id");
        assert_eq!(lines[0]["git_commit"], "deadbeef");
        assert_eq!(lines[0]["chain_id"], 5000);
        assert_eq!(lines[0]["service"], "test-service");
        assert_eq!(lines[0]["executor_contract"], header.executor_contract);
        assert_eq!(lines[0]["wmnt_address"], header.wmnt_address);
        assert!(lines[0]["start_identity"].is_null());
    }

    #[test]
    fn record_appends_a_candidate_row_after_the_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
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
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
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
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        writer
            .record_provenance(
                FinalRequestDigest(B256::repeat_byte(0xEF)),
                PoolProvenanceOutcome::MoeAllowlisted,
                vec![PoolProvenanceOutcome::MoeAllowlisted],
            )
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "provenance");
        assert_eq!(lines[1]["outcome"], "moe_allowlisted");
        assert_eq!(lines[1]["hop_outcomes"][0], "moe_allowlisted");
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
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        let identity = sample_identity();
        let ordered_pools = vec![Address::repeat_byte(0x01), Address::repeat_byte(0x02)];
        writer
            .record_context(
                FinalRequestDigest(B256::repeat_byte(0x9A)),
                &identity,
                &ShadowRouteSummary {
                    opportunity_id: B256::repeat_byte(0x9B),
                    ordered_pools: ordered_pools.clone(),
                    amount_in: U256::from(1_000u64),
                },
                U256::from(50u64),
                U256::from(42u64),
                ProfitBasis::Simulated,
            )
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "context");
        assert_eq!(
            lines[1]["opportunity_id"],
            B256::repeat_byte(0x9B).to_string()
        );
        assert_eq!(lines[1]["ordered_pools"][0], ordered_pools[0].to_string());
        assert_eq!(lines[1]["ordered_pools"][1], ordered_pools[1].to_string());
        assert_eq!(lines[1]["amount_in"], "1000");
        assert_eq!(lines[1]["gross_profit"], "50");
        assert_eq!(lines[1]["net_profit"], "42");
        assert_eq!(lines[1]["profit_basis"], "simulated");
        assert_eq!(lines[1]["identity"]["snapshot_id"]["block_number"], 10);
        assert_eq!(lines[1]["identity"]["header"]["block_timestamp"], 100);
    }

    #[test]
    fn canonical_observation_is_independent_of_candidate_activity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();

        writer
            .record_canonical_observation(
                SnapshotId::new(5000, 42, B256::repeat_byte(0x42)),
                BlockHeaderContext::new(B256::repeat_byte(0x41), 1_700_000_042),
            )
            .unwrap();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1]["row_type"], "observation");
        assert_eq!(lines[1]["snapshot_id"]["block_number"], 42);
        assert_eq!(
            lines[1]["header"]["parent_hash"],
            B256::repeat_byte(0x41).to_string()
        );
    }

    #[test]
    fn arc_wrapped_writer_records_into_the_same_underlying_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
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
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));

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

    #[test]
    fn audit_reports_contiguous_sequences() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();
        writer.record(PreflightAttempt {
            policy_key: PolicyKey::Mandatory,
            outcome: PreflightOutcome::Pass,
            digest: FinalRequestDigest(B256::repeat_byte(0x01)),
            block_tag: Some(BlockTag::Latest),
            latency: Some(Duration::from_millis(1)),
            detail: None,
        });

        let audit = audit_bytes(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(audit.row_count, 2);
        assert_eq!(audit.next_sequence, 2);
        assert!(!audit.prefix_digest.is_empty());
    }

    #[test]
    fn audit_rejects_missing_or_non_contiguous_sequences() {
        let missing = br#"{"row_type":"run_header"}
"#;
        assert!(matches!(
            audit_bytes(missing),
            Err(LedgerError::MissingSequence { line: 1 })
        ));

        let skipped = br#"{"sequence":0}
{"sequence":2}
"#;
        assert!(matches!(
            audit_bytes(skipped),
            Err(LedgerError::InvalidSequence {
                line: 2,
                found: 2,
                expected: 1
            })
        ));
    }

    #[test]
    fn writer_rejects_same_length_prefix_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] = if bytes[0] == b'{' { b'[' } else { b'{' };
        fs::write(&path, bytes).unwrap();

        let err = writer
            .record_provenance(
                FinalRequestDigest(B256::repeat_byte(0x01)),
                PoolProvenanceOutcome::MoeAllowlisted,
                vec![PoolProvenanceOutcome::MoeAllowlisted],
            )
            .unwrap_err();
        assert!(matches!(err, LedgerError::PrefixChanged));
        assert!(writer.failure().is_some());
    }

    #[test]
    fn writer_rejects_prefix_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        let writer = ShadowLedgerWriter::open(&path, header).unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(0).unwrap();

        let err = writer
            .record_provenance(
                FinalRequestDigest(B256::repeat_byte(0x01)),
                PoolProvenanceOutcome::MoeAllowlisted,
                vec![PoolProvenanceOutcome::MoeAllowlisted],
            )
            .unwrap_err();
        assert!(matches!(err, LedgerError::PrefixChanged));
    }

    /// WHI-952 acceptance: force at least one ledger rotation with a small
    /// threshold and verify retention keeps total size under the hard cap.
    #[test]
    fn force_rotation_with_small_threshold_and_retention_bounds_total() {
        use crate::ops::{list_rotated_segments, SegmentPaths};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shadow.jsonl");
        let header =
            LedgerRunHeader::from_manifest(&sample_manifest(), sample_metadata(1_700_000_000));
        // Header alone is hundreds of bytes; 400-byte segment + 1200 total cap
        // forces multiple rotations and reclaims old segments.
        let policy = RotationPolicy::new(400, 1_200).unwrap();
        let writer = ShadowLedgerWriter::open_with_policy(&path, header, policy).unwrap();

        for i in 0..40u8 {
            writer
                .record_provenance(
                    FinalRequestDigest(B256::repeat_byte(i)),
                    PoolProvenanceOutcome::MoeAllowlisted,
                    vec![PoolProvenanceOutcome::MoeAllowlisted],
                )
                .unwrap();
            let total = writer.total_bytes().unwrap();
            assert!(
                total <= policy.max_total_bytes,
                "total {total} exceeded hard cap {} after row {i}",
                policy.max_total_bytes
            );
        }

        assert!(
            writer.rotation_count() >= 1,
            "small threshold must force ≥1 rotation; got {}",
            writer.rotation_count()
        );
        let paths = SegmentPaths::new(&path);
        let rotated = list_rotated_segments(&paths).unwrap();
        assert!(
            !rotated.is_empty() || writer.rotation_count() >= 1,
            "rotation must leave reclaimable segments or a fresh active file"
        );
        // Active segment must still be a valid ledger (header present).
        let active = fs::read(&path).unwrap();
        if !active.is_empty() {
            let audit = audit_bytes(&active).expect("active segment must audit cleanly");
            assert!(audit.row_count >= 1);
        }
    }
}
