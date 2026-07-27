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
//!   after normalizing any string *or object key* that embeds this checkout's absolute
//!   filesystem path (e.g. `metadata.settings.remappings`, and
//!   `metadata.settings.compilationTarget`, which is keyed by source path) down to a
//!   repo-relative `contracts/...` form, so the digest doesn't depend on where the repo
//!   was cloned.
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

use alloy::hex;
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

    /// A zero value patches the template with zeroes, i.e. leaves it unpatched. Never
    /// a legitimate runtime immutable — see [`resolve_immutable_plan`]'s zero guard.
    fn is_zero(&self) -> bool {
        match self {
            TypedImmutableValue::Address(addr) => addr.is_zero(),
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
    #[error("build evidence file {path} could not be read: {message}")]
    EvidenceRead { path: String, message: String },
    #[error("build evidence json error: {0}")]
    Json(String),
    #[error("build evidence missing field: {0}")]
    MissingField(String),
    #[error("unknown immutableReferences AST id {ast_id}")]
    UnknownAstId { ast_id: u64 },
    #[error(
        "immutableReferences AST id {ast_id} does not resolve to an immutable state variable \
         (nodeType={node_type:?}, mutability={mutability:?}, stateVariable={state_variable})"
    )]
    NotAnImmutableStateVariable {
        ast_id: u64,
        node_type: String,
        mutability: String,
        state_variable: bool,
    },
    #[error("AST node {ast_id} is missing the required field `{field}`")]
    AstNodeMissingField { ast_id: u64, field: &'static str },
    #[error(
        "immutable {name} was given a zero value; a zero-valued immutable leaves the runtime \
         byte-identical to the unpatched template"
    )]
    ZeroImmutableValue { name: String },
    #[error(
        "patched runtime is byte-identical to the unpatched template ({template_hash}); the \
         template hash is never a valid live runtime identity"
    )]
    UnpatchedRuntime { template_hash: B256 },
    #[error(
        "resolved immutable set does not match the required {{WMNT: address}}: found {found:?}"
    )]
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
    #[error(
        "immutable range out of bounds: start={start} length={length} template_len={template_len}"
    )]
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
    #[error(
        "deployed runtime bytes mismatch at {total} offset(s); showing the first {}: {offsets:?}",
        .offsets.len()
    )]
    RuntimeMismatch {
        /// Total number of diverging bytes.
        total: usize,
        /// The first [`MAX_REPORTED_MISMATCHES`] divergences, so an unrelated contract
        /// (which diverges in ~every byte) can't blow up an error log.
        offsets: Vec<ByteMismatch>,
    },
}

/// Upper bound on the byte divergences [`verify_deployed_runtime`] reports; the total
/// count is always reported exactly.
pub const MAX_REPORTED_MISMATCHES: usize = 16;

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
        let raw = fs::read_to_string(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                RuntimeIdentityError::MissingEvidence(path.display().to_string())
            }
            // A permission/IO failure is not "this file isn't part of the build
            // evidence" — surface it distinctly so it can't be mistaken for the
            // benign missing-artifact case.
            _ => RuntimeIdentityError::EvidenceRead {
                path: path.display().to_string(),
                message: e.to_string(),
            },
        })?;
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
        let template = hex::decode(object)
            .map_err(|e| RuntimeIdentityError::Json(format!("deployedBytecode.object: {e}")))?;

        let mut immutable_references = BTreeMap::new();
        if let Some(refs) = deployed_bytecode
            .get("immutableReferences")
            .and_then(Value::as_object)
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
                    let start = range.get("start").and_then(Value::as_u64).ok_or_else(|| {
                        RuntimeIdentityError::MissingField("immutableReferences[].start".into())
                    })? as usize;
                    let length = range.get("length").and_then(Value::as_u64).ok_or_else(|| {
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

    /// The raw `storageLayout` artifact value (`storage` array with `slot`/`offset`
    /// per declared state variable). Exposed so callers (e.g. the WHI-557 mainnet
    /// fork harness) can derive storage slots from the real compiler-emitted layout
    /// instead of re-deriving offsets by hand.
    pub fn storage_layout(&self) -> &Value {
        &self.storage_layout
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

/// Cut an absolute checkout prefix off a path by anchoring on the **first**
/// `/contracts/` segment — i.e. the repo root's own `contracts/` directory. Anchoring
/// on the last match instead would let a nested dependency path
/// (`/repo/contracts/lib/openzeppelin-contracts/contracts/`) collapse to plain
/// `contracts/`, colliding with a genuinely different compiler config.
fn strip_absolute_prefix(s: &str) -> String {
    match s.find("/contracts/") {
        Some(idx) => s[idx + 1..].to_string(),
        None => s.to_string(),
    }
}

fn normalize_value(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(normalize_path_like(s)),
        Value::Array(items) => Value::Array(items.iter().map(normalize_value).collect()),
        Value::Object(map) => {
            // Keys can be checkout-dependent too: `metadata.settings.compilationTarget`
            // is keyed by source path, so keys go through the same rule as values.
            // Normalizing keys is only safe while it stays injective over this object;
            // if two distinct keys would collapse into one, an entry would silently
            // vanish from the digest, so that object keeps its keys verbatim instead.
            let normalized_keys: Vec<String> = map.keys().map(|k| normalize_path_like(k)).collect();
            let injective = {
                let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
                normalized_keys.iter().all(|k| seen.insert(k.as_str()))
            };
            let mut out = serde_json::Map::with_capacity(map.len());
            for ((k, v), normalized_key) in map.iter().zip(normalized_keys) {
                let key = if injective { normalized_key } else { k.clone() };
                out.insert(key, normalize_value(v));
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
///
/// `pub(crate)` so `execution::shadow` can digest its own committed config files
/// (Moe allowlist, WMNT descriptor) with the exact same canonicalization scheme
/// used for build-evidence digests, instead of a second implementation.
pub(crate) fn digest_of(value: &Value) -> B256 {
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

/// Resolve one `immutableReferences` AST id against the committed AST. Requires the
/// node to actually *be* an immutable state variable declaration (an id search alone
/// can land on any AST node that happens to carry a name and a type), then looks up its
/// name/type. The name/type are not validated against any expectation here — that
/// happens once the full resolved set is known, in [`resolve_immutable_plan`].
fn resolve_ast_immutable(
    ast: &Value,
    ast_id: u64,
    ranges: Vec<ByteRange>,
) -> Result<ResolvedImmutable, RuntimeIdentityError> {
    let node = find_ast_node(ast, ast_id).ok_or(RuntimeIdentityError::UnknownAstId { ast_id })?;

    let node_type = node
        .get("nodeType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mutability = node
        .get("mutability")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state_variable = node
        .get("stateVariable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if node_type != "VariableDeclaration" || mutability != "immutable" || !state_variable {
        return Err(RuntimeIdentityError::NotAnImmutableStateVariable {
            ast_id,
            node_type: node_type.to_string(),
            mutability: mutability.to_string(),
            state_variable,
        });
    }

    let name = node
        .get("name")
        .and_then(Value::as_str)
        .ok_or(RuntimeIdentityError::AstNodeMissingField {
            ast_id,
            field: "name",
        })?
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
    /// The WMNT-patched runtime bytecode this plan validated. Only obtainable from a
    /// fully validated plan, so deploy/test tooling cannot assemble a runtime that
    /// bypasses the fail-closed derivation.
    pub fn patched_bytes(&self) -> &[u8] {
        &self.patched_bytes
    }
}

/// Final backstop before a plan is minted: the patched runtime must differ from the
/// unfilled template. The template hash is build provenance only and can never be a
/// live runtime identity, so a derivation that produced it (however it got there — a
/// zero value, an empty patch, a future zero-width immutable type) must fail closed
/// rather than export the template as if it were a deployed runtime.
fn ensure_patched_differs_from_template(
    template_hash: B256,
    patched_runtime_hash: B256,
) -> Result<(), RuntimeIdentityError> {
    if patched_runtime_hash == template_hash {
        return Err(RuntimeIdentityError::UnpatchedRuntime { template_hash });
    }
    Ok(())
}

/// Resolve, validate, and patch `evidence`'s immutable(s) for `chain_id`, returning an
/// opaque, source-bound [`ValidatedImmutablePlan`].
///
/// Fails closed on: missing build evidence (surfaced by [`BuildEvidence::load`]
/// itself), an unknown AST id, an AST id that isn't an immutable state variable, a
/// resolved immutable set other than exactly `{WMNT: address}`, a type/length mismatch,
/// an out-of-bounds or overlapping range, a template range that isn't zero-filled, a
/// zero-valued immutable, or a patched runtime that came out byte-identical to the
/// template.
pub fn resolve_immutable_plan(
    evidence: &BuildEvidence,
    immutables: ImmutableInputs,
    chain_id: u64,
) -> Result<ValidatedImmutablePlan, RuntimeIdentityError> {
    let inputs = immutables.as_sorted_map();

    // A zero-valued immutable patches zeroes over an already-zero-filled template
    // range, so the "patched" runtime would be the bare template — and exporting that
    // would declare the unpatched template a valid live identity. Reject up front.
    for (name, value) in &inputs {
        if value.is_zero() {
            return Err(RuntimeIdentityError::ZeroImmutableValue { name: name.clone() });
        }
    }

    let mut resolved = Vec::new();
    for (&ast_id, ranges) in &evidence.immutable_references {
        resolved.push(resolve_ast_immutable(
            &evidence.ast,
            ast_id,
            ranges.clone(),
        )?);
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
    let mut paired: Vec<(ResolvedImmutable, TypedImmutableValue)> =
        Vec::with_capacity(resolved.len());
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
            if evidence.template[start..start + length]
                .iter()
                .any(|&b| b != 0)
            {
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
    ensure_patched_differs_from_template(template_hash, patched_runtime_hash)?;

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

    // Count every divergence but retain only the first `MAX_REPORTED_MISMATCHES`: an
    // unrelated contract of the same length diverges in nearly every byte, and the
    // error is `{:?}`-logged by callers.
    let mut total = 0usize;
    let mut offsets: Vec<ByteMismatch> = Vec::new();
    for (offset, (&expected, &observed)) in plan
        .patched_bytes
        .iter()
        .zip(on_chain_code.iter())
        .enumerate()
    {
        if expected != observed {
            total += 1;
            if offsets.len() < MAX_REPORTED_MISMATCHES {
                offsets.push(ByteMismatch {
                    offset,
                    expected,
                    observed,
                });
            }
        }
    }
    if total != 0 {
        return Err(RuntimeIdentityError::RuntimeMismatch { total, offsets });
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
/// changes `compiler_config_digest`/`plan_digest`/`identity_digest`. 0.3.0: path
/// normalization anchors on the *first* `/contracts/` segment and also applies to
/// object keys — a no-op for the currently committed evidence (every digest is
/// unchanged), but it changes derivation for artifacts with nested `contracts/`
/// remappings or absolute `compilationTarget` keys.
pub const RUNTIME_IDENTITY_TOOL_VERSION: &str = "0.3.0";

/// Build the committed export record for `plan`. Takes only the plan — never a
/// separately-passed `evidence`/`wmnt` — so the exported WMNT address and
/// storage-layout digest can't disagree with what `plan` actually resolved; both are
/// read from `plan`'s own (private) fields.
pub fn build_export(plan: &ValidatedImmutablePlan) -> ExecutorIdentityExport {
    let identity_digest = compute_identity_digest(
        plan.chain_id(),
        plan.patched_runtime_hash(),
        plan.plan_digest(),
    );
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
        assert_eq!(
            normalize_path_like("ArbitrageExecutor.sol"),
            "ArbitrageExecutor.sol"
        );
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
    fn strip_absolute_prefix_anchors_on_the_first_contracts_segment() {
        // A nested dependency path must keep its inner `contracts/` segment, otherwise
        // `@oz/=/repo/contracts/lib/openzeppelin-contracts/contracts/` would normalize
        // to `@oz/=contracts/` and collide with a different config.
        assert_eq!(
            normalize_path_like("@oz/=/repo/contracts/lib/openzeppelin-contracts/contracts/"),
            "@oz/=contracts/lib/openzeppelin-contracts/contracts/"
        );
        assert_ne!(
            digest_of(&serde_json::json!({
                "remappings": ["@oz/=/repo/contracts/lib/openzeppelin-contracts/contracts/"]
            })),
            digest_of(&serde_json::json!({ "remappings": ["@oz/=/repo/contracts/"] })),
        );
    }

    #[test]
    fn digest_of_normalizes_object_keys_too() {
        // `metadata.settings.compilationTarget` is keyed by source path.
        let a = serde_json::json!({
            "compilationTarget": {"/home/alice/repo/contracts/executor/E.sol": "E"}
        });
        let b = serde_json::json!({
            "compilationTarget": {"/Users/bob/other/contracts/executor/E.sol": "E"}
        });
        assert_eq!(digest_of(&a), digest_of(&b));

        // ...but a different source path is still a different config.
        let c = serde_json::json!({
            "compilationTarget": {"/home/alice/repo/contracts/executor/Other.sol": "E"}
        });
        assert_ne!(digest_of(&a), digest_of(&c));
    }

    #[test]
    fn key_normalization_never_drops_a_colliding_key() {
        // Two keys that would normalize to the same string must not collapse into one
        // (that would hide a difference from the digest).
        let two_keys = serde_json::json!({
            "/a/contracts/E.sol": "E",
            "/b/contracts/E.sol": "F",
        });
        let one_key = serde_json::json!({ "/a/contracts/E.sol": "E" });
        assert_eq!(normalize_value(&two_keys).as_object().unwrap().len(), 2);
        assert_ne!(digest_of(&two_keys), digest_of(&one_key));
    }

    #[test]
    fn ensure_patched_differs_from_template_rejects_an_unpatched_runtime() {
        let hash = keccak256(b"template");
        assert!(matches!(
            ensure_patched_differs_from_template(hash, hash),
            Err(RuntimeIdentityError::UnpatchedRuntime { template_hash }) if template_hash == hash
        ));
        assert!(ensure_patched_differs_from_template(hash, keccak256(b"patched")).is_ok());
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
    fn deployed_bytecode_hex_is_parsed_with_and_without_the_0x_prefix() {
        let with_prefix = hex::decode("0x00ff").expect("0x-prefixed hex decodes");
        let without_prefix = hex::decode("00ff").expect("bare hex decodes");
        assert_eq!(with_prefix, vec![0x00, 0xff]);
        assert_eq!(without_prefix, with_prefix);
        assert!(hex::decode("0xfff").is_err());
        assert!(hex::decode("0xzz").is_err());
    }

    #[test]
    fn from_json_rejects_malformed_deployed_bytecode_hex() {
        let err = BuildEvidence::from_json(serde_json::json!({
            "deployedBytecode": {"object": "0xnothex"},
        }))
        .unwrap_err();
        assert!(matches!(err, RuntimeIdentityError::Json(_)));
    }
}
