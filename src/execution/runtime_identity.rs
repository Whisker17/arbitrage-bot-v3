//! Source-bound immutable-patch derivation and verification for `ArbitrageExecutor`
//! (WHI-551).
//!
//! `ArbitrageExecutor` has exactly one runtime immutable: `WMNT` (an `address`).
//! Solidity leaves immutable slots zero-filled in the exported
//! `deployedBytecode.object` (the *template*); a live deployment has that slot
//! patched with the real address. This module resolves the `immutableReferences`
//! (keyed by Solidity AST id) against the committed AST, requires the resolved set to
//! be exactly `{ WMNT: address }`, patches it, and derives a deterministic,
//! source-bound [`ValidatedImmutablePlan`]. [`verify_deployed_runtime`] then checks a
//! live contract's bytecode against that plan and mints an opaque
//! [`VerifiedRuntimeIdentity`].
//!
//! Regenerate the template evidence and derive a plan end-to-end:
//! ```bash
//! (cd contracts/executor && scripts/export_artifacts.sh)
//! cargo run --example derive_runtime_identity -- \
//!   --artifact contracts/executor/artifacts/ \
//!   --wmnt 0x78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8 --chain-id 5000 \
//!   --out config/executor_identity.json
//! ```
//!
//! ## Digest scheme
//!
//! All digests are `keccak256`, rendered as `0x`-prefixed lowercase hex.
//!
//! - `storage_layout_digest` = digest of the artifact's `storageLayout` value.
//! - `compiler_config_digest` = digest of the artifact's `metadata.settings` value,
//!   after normalizing any string that embeds this checkout's absolute filesystem
//!   path (e.g. `metadata.settings.remappings`) down to a repo-relative
//!   `contracts/...` form, so the digest doesn't depend on where the repo was cloned.
//! - `build_info_digest` = digest of `{ solc_long_version, language, ast }` — compiler
//!   identity plus parsed source structure, i.e. "what was compiled", distinct from
//!   `compiler_config_digest` ("what flags compiled it"). Same normalization applied
//!   defensively (currently a no-op: this Foundry root's AST paths are already
//!   relative).
//! - `immutable_values_digest` = digest over the sorted `{name -> typed value}`
//!   immutable set (today always the single entry `{"WMNT": address}`).
//! - `plan_digest` and `identity_digest` follow the WHI-551 spec exactly; see
//!   [`resolve_immutable_plan`] and [`verify_deployed_runtime`].
//!
//! `ValidatedImmutablePlan` and `VerifiedRuntimeIdentity` are opaque: every field is
//! private, and neither type has a public constructor other than the two functions
//! above, so no JSON/manifest/config/environment value can forge one.

use alloy::primitives::{keccak256, Address, B256};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use thiserror::Error;

const PLAN_DOMAIN: &[u8] = b"whisker-arb/immutable-plan/v1";
const IDENTITY_DOMAIN: &[u8] = b"whisker-arb/executor-runtime-identity/v1";

/// A single resolved-and-typed immutable value. `Address` is the only variant needed
/// by `ArbitrageExecutor` today; kept as an enum so the digest scheme generalizes if a
/// future contract has more immutable kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypedImmutableValue {
    Address(Address),
}

impl TypedImmutableValue {
    /// The Solidity AST `typeDescriptions.typeIdentifier` this variant must resolve to.
    fn solidity_type_identifier(&self) -> &'static str {
        match self {
            TypedImmutableValue::Address(_) => "t_address",
        }
    }

    /// Byte width of this value's ABI-word encoding.
    fn byte_width(&self) -> usize {
        match self {
            TypedImmutableValue::Address(_) => 32,
        }
    }

    fn type_tag(&self) -> u8 {
        match self {
            TypedImmutableValue::Address(_) => 0x01,
        }
    }

    /// ABI-word encoding: left-padded to 32 bytes.
    fn abi_word(&self) -> [u8; 32] {
        match self {
            TypedImmutableValue::Address(addr) => {
                let mut word = [0u8; 32];
                word[12..].copy_from_slice(addr.as_slice());
                word
            }
        }
    }
}

/// Caller-supplied immutable inputs for `ArbitrageExecutor`. Concrete (not a generic
/// map) because this contract has exactly one immutable; [`resolve_immutable_plan`]
/// still digests it as "the sorted typed immutable set" internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImmutableInputs {
    pub wmnt: Address,
}

impl ImmutableInputs {
    fn as_sorted_map(&self) -> BTreeMap<String, TypedImmutableValue> {
        let mut map = BTreeMap::new();
        map.insert("WMNT".to_string(), TypedImmutableValue::Address(self.wmnt));
        map
    }
}

#[derive(Debug, Error)]
pub enum RuntimeIdentityError {
    #[error("missing build evidence file: {0}")]
    MissingEvidence(String),
    #[error("build evidence json error: {0}")]
    Json(String),
    #[error("build evidence missing field: {0}")]
    MissingField(String),
    #[error("unknown immutableReferences AST id {ast_id}")]
    UnknownAstId { ast_id: u64 },
    #[error("resolved immutable set does not match the required {{WMNT: address}}: found {found:?}")]
    UnexpectedImmutableSet { found: Vec<String> },
    #[error("immutable {name} has Solidity type {actual}, expected {expected}")]
    UnsupportedImmutableType {
        name: String,
        expected: &'static str,
        actual: String,
    },
    #[error("immutable {name} range length {actual_len} does not match its type's expected {expected_len} bytes")]
    ImmutableRangeLengthMismatch {
        name: String,
        expected_len: usize,
        actual_len: usize,
    },
    #[error("immutable {name} has no byte ranges to patch")]
    NoRangesForImmutable { name: String },
    #[error("immutable range out of bounds: start={start} length={length} template_len={template_len}")]
    RangeOutOfBounds {
        start: usize,
        length: usize,
        template_len: usize,
    },
    #[error("immutable ranges overlap: [{a_start}, {a_end}) vs [{b_start}, {b_end})")]
    OverlappingRanges {
        a_start: usize,
        a_end: usize,
        b_start: usize,
        b_end: usize,
    },
    #[error("template immutable range at byte offset {offset} is not zero-filled")]
    NonZeroTemplateRange { offset: usize },
    #[error("deployed runtime length mismatch: expected {expected}, observed {observed}")]
    LengthMismatch { expected: usize, observed: usize },
    #[error("deployed runtime bytes mismatch at {} offset(s)", .0.len())]
    RuntimeMismatch(Vec<ByteMismatch>),
}

/// A single byte divergence reported by [`verify_deployed_runtime`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteMismatch {
    pub offset: usize,
    pub expected: u8,
    pub observed: u8,
}

/// One `immutableReferences` byte range, exactly as solc declares it: a start offset
/// and a length. Kept as its own type (rather than a bare `(usize, usize)` tuple) so
/// this never gets confused with the `(start, end)` span [`validate_ranges`] derives
/// from it internally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: usize,
    length: usize,
}

/// Parsed forge build evidence for `ArbitrageExecutor` (the full per-contract
/// artifact: `abi`, `bytecode`, `deployedBytecode` incl. `immutableReferences`, `ast`,
/// `storageLayout`, `metadata`).
#[derive(Debug, Clone)]
pub struct BuildEvidence {
    template: Vec<u8>,
    immutable_references: BTreeMap<u64, Vec<ByteRange>>,
    ast: Value,
    storage_layout: Value,
    compiler_settings: Value,
    solc_long_version: String,
    language: String,
}

impl BuildEvidence {
    /// Load the full per-contract forge artifact (as written by
    /// `contracts/executor/scripts/export_artifacts.sh` to
    /// `ArbitrageExecutor.full.json`) from a directory.
    pub fn load(artifact_dir: &Path) -> Result<Self, RuntimeIdentityError> {
        let path = artifact_dir.join("ArbitrageExecutor.full.json");
        let raw = fs::read_to_string(&path)
            .map_err(|_| RuntimeIdentityError::MissingEvidence(path.display().to_string()))?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|e| RuntimeIdentityError::Json(e.to_string()))?;
        Self::from_json(value)
    }

    /// Parse build evidence from an in-memory JSON value shaped like forge's
    /// per-contract artifact. Exposed so tests/tools can construct fixtures without
    /// touching the filesystem.
    pub fn from_json(value: Value) -> Result<Self, RuntimeIdentityError> {
        let deployed_bytecode = field(&value, "deployedBytecode")?;
        let object = deployed_bytecode
            .get("object")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeIdentityError::MissingField("deployedBytecode.object".into()))?;
        let template = hex_decode(object)
            .map_err(|e| RuntimeIdentityError::Json(format!("deployedBytecode.object: {e}")))?;

        let mut immutable_references = BTreeMap::new();
        if let Some(refs) = deployed_bytecode.get("immutableReferences").and_then(Value::as_object)
        {
            for (ast_id_str, ranges) in refs {
                let ast_id: u64 = ast_id_str.parse().map_err(|_| {
                    RuntimeIdentityError::Json(format!("non-numeric AST id key: {ast_id_str}"))
                })?;
                let ranges_array = ranges.as_array().ok_or_else(|| {
                    RuntimeIdentityError::Json(format!(
                        "immutableReferences[{ast_id_str}] is not an array"
                    ))
                })?;
                let mut parsed_ranges = Vec::with_capacity(ranges_array.len());
                for range in ranges_array {
                    let start = range
                        .get("start")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            RuntimeIdentityError::MissingField("immutableReferences[].start".into())
                        })? as usize;
                    let length = range
                        .get("length")
                        .and_then(Value::as_u64)
                        .ok_or_else(|| {
                            RuntimeIdentityError::MissingField("immutableReferences[].length".into())
                        })? as usize;
                    parsed_ranges.push(ByteRange { start, length });
                }
                immutable_references.insert(ast_id, parsed_ranges);
            }
        }

        let ast = field(&value, "ast")?.clone();
        let storage_layout = field(&value, "storageLayout")?.clone();
        let metadata = parse_metadata(field(&value, "metadata")?)?;
        let compiler_settings = metadata
            .get("settings")
            .cloned()
            .ok_or_else(|| RuntimeIdentityError::MissingField("metadata.settings".into()))?;
        let solc_long_version = metadata
            .get("compiler")
            .and_then(|c| c.get("version"))
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeIdentityError::MissingField("metadata.compiler.version".into()))?
            .to_string();
        let language = metadata
            .get("language")
            .and_then(Value::as_str)
            .ok_or_else(|| RuntimeIdentityError::MissingField("metadata.language".into()))?
            .to_string();

        Ok(Self {
            template,
            immutable_references,
            ast,
            storage_layout,
            compiler_settings,
            solc_long_version,
            language,
        })
    }
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, RuntimeIdentityError> {
    value
        .get(key)
        .ok_or_else(|| RuntimeIdentityError::MissingField(key.into()))
}

fn parse_metadata(value: &Value) -> Result<Value, RuntimeIdentityError> {
    match value {
        Value::String(raw) => {
            serde_json::from_str(raw).map_err(|e| RuntimeIdentityError::Json(e.to_string()))
        }
        Value::Object(_) => Ok(value.clone()),
        _ => Err(RuntimeIdentityError::MissingField("metadata".into())),
    }
}

fn hex_decode(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        return Err("odd-length hex string".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    for chunk in bytes.chunks(2) {
        let hi = hex_nibble(chunk[0])?;
        let lo = hex_nibble(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", c as char)),
    }
}

// ---------------------------------------------------------------------------
// Canonicalization / digests
// ---------------------------------------------------------------------------

/// Rewrite any string embedding this checkout's absolute filesystem path down to a
/// repo-relative `contracts/...` form, so digests don't depend on clone location.
/// Generic over any `.../contracts/...` absolute path, not just today's one leak
/// (`metadata.settings.remappings`).
///
/// A forge remapping is `alias=path` (e.g. `forge-std/=/abs/.../contracts/lib/...`).
/// Only the path side is checkout-dependent, so this normalizes at most the substring
/// after the first `=`, leaving any alias prefix intact — otherwise two *different*
/// remapping aliases pointing at the same relative suffix would collapse to the same
/// normalized string and silently produce identical digests for different configs.
fn normalize_path_like(s: &str) -> String {
    match s.split_once('=') {
        Some((alias, path)) => format!("{alias}={}", strip_absolute_prefix(path)),
        None => strip_absolute_prefix(s),
    }
}

fn strip_absolute_prefix(s: &str) -> String {
    match s.rfind("/contracts/") {
        Some(idx) => s[idx + 1..].to_string(),
        None => s.to_string(),
    }
}

fn normalize_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(normalize_path_like(s)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_value).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), normalize_value(v));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

fn sort_json(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                if let Some(v) = map.get(&k) {
                    out.insert(k, sort_json(v.clone()));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sort_json).collect()),
        other => other,
    }
}

/// Normalize path-like strings, sort object keys, and `keccak256` the resulting
/// canonical JSON bytes.
fn digest_of(value: &Value) -> B256 {
    let normalized = normalize_value(value);
    let sorted = sort_json(normalized);
    let bytes = serde_json::to_vec(&sorted).expect("serde_json::Value always serializes");
    keccak256(bytes)
}

fn immutable_values_digest(immutables: &BTreeMap<String, TypedImmutableValue>) -> B256 {
    let mut buf = Vec::new();
    for (name, value) in immutables {
        buf.push(name.len() as u8);
        buf.extend_from_slice(name.as_bytes());
        buf.push(value.type_tag());
        buf.extend_from_slice(&value.abi_word());
    }
    keccak256(buf)
}

// ---------------------------------------------------------------------------
// AST resolution
// ---------------------------------------------------------------------------

struct ResolvedImmutable {
    name: String,
    type_identifier: String,
    ranges: Vec<ByteRange>,
}

fn find_ast_node(node: &Value, target_id: u64) -> Option<&Value> {
    if node.get("id").and_then(Value::as_u64) == Some(target_id) {
        return Some(node);
    }
    match node {
        Value::Object(map) => map.values().find_map(|v| find_ast_node(v, target_id)),
        Value::Array(items) => items.iter().find_map(|item| find_ast_node(item, target_id)),
        _ => None,
    }
}

/// Resolve one `immutableReferences` AST id against the committed AST. Only looks up
/// the node's name/type; does not validate them against any expectation (that happens
/// once the full resolved set is known, in [`resolve_immutable_plan`]).
fn resolve_ast_immutable(
    ast: &Value,
    ast_id: u64,
    ranges: Vec<ByteRange>,
) -> Result<ResolvedImmutable, RuntimeIdentityError> {
    let node = find_ast_node(ast, ast_id).ok_or(RuntimeIdentityError::UnknownAstId { ast_id })?;
    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or(RuntimeIdentityError::UnknownAstId { ast_id })?
        .to_string();
    if ranges.is_empty() {
        return Err(RuntimeIdentityError::NoRangesForImmutable { name });
    }
    let type_identifier = node
        .get("typeDescriptions")
        .and_then(|t| t.get("typeIdentifier"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(ResolvedImmutable {
        name,
        type_identifier,
        ranges,
    })
}

/// A validated `[start, end)` span, distinct from [`ByteRange`] (`start` + `length`) so
/// the two representations — "as solc declared it" vs. "as checked against the
/// template" — are never confused with each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    start: usize,
    end: usize,
}

fn validate_ranges(
    all_ranges_by_immutable: &[&[ByteRange]],
    template_len: usize,
) -> Result<(), RuntimeIdentityError> {
    let mut spans: Vec<Span> = Vec::new();
    for ranges in all_ranges_by_immutable {
        for &ByteRange { start, length } in *ranges {
            let end = start
                .checked_add(length)
                .filter(|&end| end <= template_len)
                .ok_or(RuntimeIdentityError::RangeOutOfBounds {
                    start,
                    length,
                    template_len,
                })?;
            spans.push(Span { start, end });
        }
    }
    spans.sort_unstable();
    for pair in spans.windows(2) {
        if pair[1].start < pair[0].end {
            return Err(RuntimeIdentityError::OverlappingRanges {
                a_start: pair[0].start,
                a_end: pair[0].end,
                b_start: pair[1].start,
                b_end: pair[1].end,
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// ValidatedImmutablePlan
// ---------------------------------------------------------------------------

/// Opaque, source-bound plan: a validated resolution of `ArbitrageExecutor`'s
/// immutable(s) against one specific build's AST/template, for one specific chain.
/// Every field is private; a plan derived from a different chain, template, or build
/// cannot validate another runtime (its `plan_digest`/`patched_runtime_hash` simply
/// won't match).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ValidatedImmutablePlan {
    chain_id: u64,
    template_hash: B256,
    template_length: u64,
    build_info_digest: B256,
    compiler_config_digest: B256,
    immutable_values_digest: B256,
    patched_runtime_hash: B256,
    plan_digest: B256,
    patched_bytes: Vec<u8>,
    // Not part of `plan_digest`'s binding (already covered cryptographically via
    // `immutable_values_digest`) but retained privately so `build_export` can report
    // them without taking `wmnt`/`evidence` as separate, independently-forgeable
    // parameters that could disagree with what this plan actually resolved.
    wmnt: Address,
    storage_layout_digest: B256,
}

impl ValidatedImmutablePlan {
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }
    pub fn template_hash(&self) -> B256 {
        self.template_hash
    }
    pub fn template_length(&self) -> u64 {
        self.template_length
    }
    pub fn build_info_digest(&self) -> B256 {
        self.build_info_digest
    }
    pub fn compiler_config_digest(&self) -> B256 {
        self.compiler_config_digest
    }
    pub fn immutable_values_digest(&self) -> B256 {
        self.immutable_values_digest
    }
    pub fn patched_runtime_hash(&self) -> B256 {
        self.patched_runtime_hash
    }
    pub fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
}

/// Resolve, validate, and patch `evidence`'s immutable(s) for `chain_id`, returning an
/// opaque, source-bound [`ValidatedImmutablePlan`].
///
/// Fails closed on: missing build evidence (surfaced by [`BuildEvidence::load`]
/// itself), an unknown AST id, a resolved immutable set other than exactly
/// `{WMNT: address}`, a type/length mismatch, an out-of-bounds or overlapping range,
/// or a template range that isn't zero-filled.
pub fn resolve_immutable_plan(
    evidence: &BuildEvidence,
    immutables: ImmutableInputs,
    chain_id: u64,
) -> Result<ValidatedImmutablePlan, RuntimeIdentityError> {
    let inputs = immutables.as_sorted_map();

    let mut resolved = Vec::new();
    for (&ast_id, ranges) in &evidence.immutable_references {
        resolved.push(resolve_ast_immutable(&evidence.ast, ast_id, ranges.clone())?);
    }

    let mut resolved_names: Vec<String> = resolved.iter().map(|r| r.name.clone()).collect();
    resolved_names.sort();
    let expected_names: Vec<String> = inputs.keys().cloned().collect();
    if resolved_names != expected_names {
        return Err(RuntimeIdentityError::UnexpectedImmutableSet {
            found: resolved_names,
        });
    }

    // Name-set matches exactly; pair each resolved immutable with its expected typed
    // value and validate Solidity type + range width before touching any bytes.
    let mut paired: Vec<(ResolvedImmutable, TypedImmutableValue)> = Vec::with_capacity(resolved.len());
    for immutable in resolved {
        let expected_value = *inputs.get(&immutable.name).expect("name checked above");
        if immutable.type_identifier != expected_value.solidity_type_identifier() {
            return Err(RuntimeIdentityError::UnsupportedImmutableType {
                name: immutable.name,
                expected: expected_value.solidity_type_identifier(),
                actual: immutable.type_identifier,
            });
        }
        for &ByteRange { length, .. } in &immutable.ranges {
            if length != expected_value.byte_width() {
                return Err(RuntimeIdentityError::ImmutableRangeLengthMismatch {
                    name: immutable.name,
                    expected_len: expected_value.byte_width(),
                    actual_len: length,
                });
            }
        }
        paired.push((immutable, expected_value));
    }

    let range_lists: Vec<&[ByteRange]> = paired.iter().map(|(r, _)| r.ranges.as_slice()).collect();
    validate_ranges(&range_lists, evidence.template.len())?;

    for (immutable, _) in &paired {
        for &ByteRange { start, length } in &immutable.ranges {
            if evidence.template[start..start + length].iter().any(|&b| b != 0) {
                return Err(RuntimeIdentityError::NonZeroTemplateRange { offset: start });
            }
        }
    }

    let mut patched_bytes = evidence.template.clone();
    for (immutable, value) in &paired {
        let word = value.abi_word();
        for &ByteRange { start, length } in &immutable.ranges {
            patched_bytes[start..start + length].copy_from_slice(&word[32 - length..]);
        }
    }

    let template_hash = keccak256(&evidence.template);
    let template_length = evidence.template.len() as u64;
    let build_info_evidence = serde_json::json!({
        "solc_long_version": evidence.solc_long_version,
        "language": evidence.language,
        "ast": evidence.ast,
    });
    let build_info_digest = digest_of(&build_info_evidence);
    let compiler_config_digest = digest_of(&evidence.compiler_settings);
    let storage_layout_digest = digest_of(&evidence.storage_layout);
    let values_digest = immutable_values_digest(&inputs);
    let patched_runtime_hash = keccak256(&patched_bytes);

    let plan_digest = {
        let mut buf = Vec::with_capacity(PLAN_DOMAIN.len() + 1 + 8 + 32 + 8 + 32 + 32 + 32 + 32);
        buf.extend_from_slice(PLAN_DOMAIN);
        buf.push(0x00);
        buf.extend_from_slice(&chain_id.to_be_bytes());
        buf.extend_from_slice(template_hash.as_slice());
        buf.extend_from_slice(&template_length.to_be_bytes());
        buf.extend_from_slice(build_info_digest.as_slice());
        buf.extend_from_slice(compiler_config_digest.as_slice());
        buf.extend_from_slice(values_digest.as_slice());
        buf.extend_from_slice(patched_runtime_hash.as_slice());
        keccak256(buf)
    };

    Ok(ValidatedImmutablePlan {
        chain_id,
        template_hash,
        template_length,
        build_info_digest,
        compiler_config_digest,
        immutable_values_digest: values_digest,
        patched_runtime_hash,
        plan_digest,
        patched_bytes,
        wmnt: immutables.wmnt,
        storage_layout_digest,
    })
}

// ---------------------------------------------------------------------------
// VerifiedRuntimeIdentity
// ---------------------------------------------------------------------------

/// Opaque proof that a specific on-chain bytecode blob matches a
/// [`ValidatedImmutablePlan`]'s patched runtime, exactly. Constructible only by
/// [`verify_deployed_runtime`] — every field is private and there is no other public
/// constructor, so no JSON/manifest/config/environment value can build one.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VerifiedRuntimeIdentity {
    chain_id: u64,
    patched_runtime_hash: B256,
    identity_digest: B256,
    plan_digest: B256,
}

impl VerifiedRuntimeIdentity {
    pub fn chain_id(&self) -> u64 {
        self.chain_id
    }
    pub fn patched_runtime_hash(&self) -> B256 {
        self.patched_runtime_hash
    }
    pub fn identity_digest(&self) -> B256 {
        self.identity_digest
    }
    pub fn plan_digest(&self) -> B256 {
        self.plan_digest
    }
}

fn compute_identity_digest(chain_id: u64, patched_runtime_hash: B256, plan_digest: B256) -> B256 {
    let mut buf = Vec::with_capacity(IDENTITY_DOMAIN.len() + 1 + 8 + 32 + 32);
    buf.extend_from_slice(IDENTITY_DOMAIN);
    buf.push(0x00);
    buf.extend_from_slice(&chain_id.to_be_bytes());
    buf.extend_from_slice(patched_runtime_hash.as_slice());
    buf.extend_from_slice(plan_digest.as_slice());
    keccak256(buf)
}

/// Verify `on_chain_code` against `plan`'s patched runtime, byte for byte.
///
/// Accepts no caller-supplied template or ranges — everything comes from `plan`. On a
/// length mismatch or any byte divergence, returns a structured error describing
/// exactly where the deployed code diverges from the source-bound expectation.
pub fn verify_deployed_runtime(
    on_chain_code: &[u8],
    plan: &ValidatedImmutablePlan,
) -> Result<VerifiedRuntimeIdentity, RuntimeIdentityError> {
    if on_chain_code.len() != plan.patched_bytes.len() {
        return Err(RuntimeIdentityError::LengthMismatch {
            expected: plan.patched_bytes.len(),
            observed: on_chain_code.len(),
        });
    }

    let mismatches: Vec<ByteMismatch> = plan
        .patched_bytes
        .iter()
        .zip(on_chain_code.iter())
        .enumerate()
        .filter_map(|(offset, (&expected, &observed))| {
            (expected != observed).then_some(ByteMismatch {
                offset,
                expected,
                observed,
            })
        })
        .collect();
    if !mismatches.is_empty() {
        return Err(RuntimeIdentityError::RuntimeMismatch(mismatches));
    }

    Ok(VerifiedRuntimeIdentity {
        chain_id: plan.chain_id,
        patched_runtime_hash: plan.patched_runtime_hash,
        identity_digest: compute_identity_digest(
            plan.chain_id,
            plan.patched_runtime_hash,
            plan.plan_digest,
        ),
        plan_digest: plan.plan_digest,
    })
}

/// Committed export schema for `config/executor_identity.json`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutorIdentityExport {
    pub schema_version: u32,
    pub chain_id: u64,
    pub template_hash: String,
    pub patched_runtime_hash: String,
    pub wmnt: String,
    pub immutable_values_digest: String,
    pub compiler_config_digest: String,
    pub build_info_digest: String,
    pub storage_layout_digest: String,
    pub plan_digest: String,
    pub identity_digest: String,
    pub tool_version: String,
}

/// Tool version embedded in `config/executor_identity.json` (bump when derivation
/// logic changes). 0.2.0: fixed remapping-alias normalization (see git history) —
/// changes `compiler_config_digest`/`plan_digest`/`identity_digest`.
pub const RUNTIME_IDENTITY_TOOL_VERSION: &str = "0.2.0";

/// Build the committed export record for `plan`. Takes only the plan — never a
/// separately-passed `evidence`/`wmnt` — so the exported WMNT address and
/// storage-layout digest can't disagree with what `plan` actually resolved; both are
/// read from `plan`'s own (private) fields.
pub fn build_export(plan: &ValidatedImmutablePlan) -> ExecutorIdentityExport {
    let identity_digest =
        compute_identity_digest(plan.chain_id(), plan.patched_runtime_hash(), plan.plan_digest());
    ExecutorIdentityExport {
        schema_version: 1,
        chain_id: plan.chain_id(),
        template_hash: plan.template_hash().to_string(),
        patched_runtime_hash: plan.patched_runtime_hash().to_string(),
        wmnt: plan.wmnt.to_string(),
        immutable_values_digest: plan.immutable_values_digest().to_string(),
        compiler_config_digest: plan.compiler_config_digest().to_string(),
        build_info_digest: plan.build_info_digest().to_string(),
        storage_layout_digest: plan.storage_layout_digest.to_string(),
        plan_digest: plan.plan_digest().to_string(),
        identity_digest: identity_digest.to_string(),
        tool_version: RUNTIME_IDENTITY_TOOL_VERSION.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_like_strips_absolute_prefix_up_to_contracts() {
        assert_eq!(
            normalize_path_like("/Users/whisker/repo/contracts/lib/forge-std/src/"),
            "contracts/lib/forge-std/src/"
        );
        assert_eq!(normalize_path_like("ArbitrageExecutor.sol"), "ArbitrageExecutor.sol");
        assert_eq!(normalize_path_like("contracts/lib/x"), "contracts/lib/x");
    }

    #[test]
    fn digest_of_ignores_checkout_path_differences() {
        let a = serde_json::json!({"remappings": ["forge-std/=/home/alice/repo/contracts/lib/forge-std/src/"]});
        let b = serde_json::json!({"remappings": ["forge-std/=/Users/bob/other/contracts/lib/forge-std/src/"]});
        assert_eq!(digest_of(&a), digest_of(&b));
    }

    #[test]
    fn normalize_path_like_preserves_the_remapping_alias() {
        assert_eq!(
            normalize_path_like("forge-std/=/Users/x/repo/contracts/lib/forge-std/src/"),
            "forge-std/=contracts/lib/forge-std/src/"
        );
    }

    #[test]
    fn digest_of_distinguishes_different_remapping_aliases() {
        // Same checkout-relative suffix, different alias -> must NOT collapse to the
        // same digest (a different alias is a different compiler config).
        let a = serde_json::json!({"remappings": ["forge-std/=/Users/x/repo/contracts/lib/forge-std/src/"]});
        let b = serde_json::json!({"remappings": ["evil-alias/=/Users/x/repo/contracts/lib/forge-std/src/"]});
        assert_ne!(digest_of(&a), digest_of(&b));
    }

    #[test]
    fn digest_of_is_order_independent_over_object_keys() {
        let a = serde_json::json!({"b": 1, "a": 2});
        let b = serde_json::json!({"a": 2, "b": 1});
        assert_eq!(digest_of(&a), digest_of(&b));
    }

    #[test]
    fn immutable_values_digest_changes_with_the_address() {
        let mut a = BTreeMap::new();
        a.insert(
            "WMNT".to_string(),
            TypedImmutableValue::Address(Address::ZERO),
        );
        let mut b = BTreeMap::new();
        b.insert(
            "WMNT".to_string(),
            TypedImmutableValue::Address(Address::repeat_byte(1)),
        );
        assert_ne!(immutable_values_digest(&a), immutable_values_digest(&b));
    }

    #[test]
    fn hex_decode_roundtrips() {
        assert_eq!(hex_decode("0x00ff").unwrap(), vec![0x00, 0xff]);
        assert_eq!(hex_decode("00ff").unwrap(), vec![0x00, 0xff]);
        assert!(hex_decode("0xfff").is_err());
        assert!(hex_decode("0xzz").is_err());
    }
}
