use super::gas_profile::{
    load_artifact, validate_artifact, GasProfileArtifact, GasProfileError, GasQuote, MarginPolicy,
    ProfileStatus, RouteKey, GAS_PROFILE_TOOL_VERSION, MANTLE_MAINNET_CHAIN_ID,
    WHI501_EXECUTOR_CODEHASH,
};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub const WHI501_EXECUTOR_ABI_DIGEST: &str =
    "0x9f2f241bdb5795475410fc8db6f7089bc7300e3b47963b7aa56296a05e21a0d6";
pub const MANTLE_MAINNET_PROFILE_DIGEST: &str =
    "0x1c20fb23259e5fdde2e9f2dd9927bcfbcffcaabc6ea2c562121f67fcde076a4a";

/// Mantle mainnet `ArbitrageExecutor` runtime, WMNT-patched (WHI-551). This is the
/// **live** on-chain identity — `ExecutionContext::from_provider` (`types.rs`) checks
/// a deployed contract's bytecode against this, never against
/// [`WHI501_EXECUTOR_CODEHASH`] (which is the unfilled *template* hash and can never
/// equal a real deployment's bytecode). Derived by
/// `cargo run --example derive_runtime_identity`; must match
/// `config/executor_identity.json`'s `patched_runtime_hash` — see
/// `src/execution/runtime_identity.rs`.
pub const WHI501_EXECUTOR_PATCHED_RUNTIME_HASH: &str =
    "0xe2f8a1e096446aadf231ca7ea5d4771008d1c40fa4dded8b2d0681ad9afaeafd";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutorIdentity {
    pub chain_id: u64,
    /// Unfilled template hash — build-provenance linkage for the frozen gas-profile
    /// data this identity is checked against ([`RuntimeGasProfile`]). Never compare
    /// this against live on-chain bytecode; use `patched_runtime_hash` for that.
    pub template_hash: String,
    /// Live on-chain identity: the template with WMNT patched in. This is what
    /// `ExecutionContext::from_provider` checks a deployed contract's bytecode
    /// against.
    pub patched_runtime_hash: String,
    pub abi_digest: String,
}

impl ExecutorIdentity {
    pub fn mantle_mainnet() -> Self {
        Self {
            chain_id: MANTLE_MAINNET_CHAIN_ID,
            template_hash: WHI501_EXECUTOR_CODEHASH.into(),
            patched_runtime_hash: WHI501_EXECUTOR_PATCHED_RUNTIME_HASH.into(),
            abi_digest: WHI501_EXECUTOR_ABI_DIGEST.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeProfileConfig {
    pub executor_identity: ExecutorIdentity,
    pub expected_content_digest: String,
    pub expected_margin_policy: MarginPolicy,
    pub required_route_keys: Vec<RouteKey>,
}

impl RuntimeProfileConfig {
    pub fn mantle_mainnet(required_route_keys: Vec<RouteKey>) -> Self {
        Self {
            executor_identity: ExecutorIdentity::mantle_mainnet(),
            expected_content_digest: MANTLE_MAINNET_PROFILE_DIGEST.into(),
            expected_margin_policy: MarginPolicy::default(),
            required_route_keys,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeGasProfileError {
    #[error(transparent)]
    Artifact(#[from] GasProfileError),
    #[error("profile {field} mismatch: expected {expected}, observed {observed}")]
    Identity {
        field: &'static str,
        expected: String,
        observed: String,
    },
    #[error("profile margin policy does not match the approved policy")]
    MarginPolicyMismatch,
    #[error("profile has {actual} qualification samples, below required {required}")]
    InsufficientSamples { actual: usize, required: usize },
    #[error("route {route} has {actual} qualification samples, below required {required}")]
    InsufficientRouteSamples {
        route: String,
        actual: usize,
        required: usize,
    },
    #[error("profile route is not approved: {0}")]
    UnapprovedRoute(String),
    #[error("profile route is unknown: {0}")]
    UnknownRoute(String),
    #[error("approved profile is missing a required gas field: {0}")]
    IncompleteApprovedRoute(String),
    #[error("runtime profile state is poisoned")]
    ProfileStatePoisoned,
    #[error("unable to persist profile invalidation state at {path}: {message}")]
    InvalidationState { path: String, message: String },
}

#[derive(Clone, Debug)]
enum RuntimeRoute {
    Approved(GasQuote),
    Unsupported(String),
    ResearchOnly,
}

/// Validated, immutable gas-profile index loaded once during service startup.
#[derive(Clone, Debug)]
pub struct RuntimeGasProfile {
    executor_identity: ExecutorIdentity,
    routes: HashMap<RouteKey, RuntimeRoute>,
    invalidated_routes: Arc<RwLock<std::collections::HashSet<RouteKey>>>,
    artifact_digest: String,
    invalidation_path: Option<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InvalidationState {
    content_digest: String,
    routes: Vec<RouteKey>,
}

impl RuntimeGasProfile {
    /// Load and validate the profile. A persisted invalidation for a required route
    /// intentionally fails startup until the profile is regenerated.
    pub fn load(path: &Path, config: RuntimeProfileConfig) -> Result<Self, RuntimeGasProfileError> {
        Self::from_artifact_with_path(load_artifact(path)?, config, Some(invalidation_path(path)))
    }

    pub(crate) fn from_artifact(
        artifact: GasProfileArtifact,
        config: RuntimeProfileConfig,
    ) -> Result<Self, RuntimeGasProfileError> {
        Self::from_artifact_with_path(artifact, config, None)
    }

    fn from_artifact_with_path(
        artifact: GasProfileArtifact,
        config: RuntimeProfileConfig,
        invalidation_path: Option<PathBuf>,
    ) -> Result<Self, RuntimeGasProfileError> {
        validate_artifact(&artifact)?;
        verify_identity(
            "runtime chain_id",
            MANTLE_MAINNET_CHAIN_ID.to_string(),
            config.executor_identity.chain_id.to_string(),
        )?;
        verify_identity(
            "runtime executor_template_hash",
            WHI501_EXECUTOR_CODEHASH.into(),
            config.executor_identity.template_hash.clone(),
        )?;
        verify_identity(
            "runtime executor_patched_runtime_hash",
            WHI501_EXECUTOR_PATCHED_RUNTIME_HASH.into(),
            config.executor_identity.patched_runtime_hash.clone(),
        )?;
        verify_identity(
            "runtime executor_abi_digest",
            WHI501_EXECUTOR_ABI_DIGEST.into(),
            config.executor_identity.abi_digest.clone(),
        )?;
        verify_identity(
            "chain_id",
            MANTLE_MAINNET_CHAIN_ID.to_string(),
            artifact.chain_id.to_string(),
        )?;
        verify_identity(
            "executor_code_hash",
            WHI501_EXECUTOR_CODEHASH.into(),
            artifact.executor_code_hash.clone(),
        )?;
        verify_identity(
            "executor_abi_digest",
            WHI501_EXECUTOR_ABI_DIGEST.into(),
            artifact.executor_abi_digest.clone(),
        )?;
        verify_identity(
            "tool_version",
            GAS_PROFILE_TOOL_VERSION.into(),
            artifact.tool_version.clone(),
        )?;
        verify_identity(
            "content_digest",
            config.expected_content_digest,
            artifact.content_digest.clone(),
        )?;
        if artifact.margin_policy != config.expected_margin_policy {
            return Err(RuntimeGasProfileError::MarginPolicyMismatch);
        }
        if artifact.qualification_sample_count < artifact.margin_policy.min_samples {
            return Err(RuntimeGasProfileError::InsufficientSamples {
                actual: artifact.qualification_sample_count,
                required: artifact.margin_policy.min_samples,
            });
        }

        let mut routes = HashMap::with_capacity(artifact.profiles.len());
        for profile in artifact.profiles {
            let route_key = profile.route_key.clone();
            let route = match profile.status {
                ProfileStatus::Approved => {
                    let stats = profile.stats.as_ref().ok_or_else(|| {
                        RuntimeGasProfileError::IncompleteApprovedRoute(route_key.key_string())
                    })?;
                    if stats.sample_count < artifact.margin_policy.min_samples {
                        return Err(RuntimeGasProfileError::InsufficientRouteSamples {
                            route: route_key.key_string(),
                            actual: stats.sample_count,
                            required: artifact.margin_policy.min_samples,
                        });
                    }
                    RuntimeRoute::Approved(GasQuote {
                        gas_limit: profile.gas_limit.ok_or_else(|| {
                            RuntimeGasProfileError::IncompleteApprovedRoute(route_key.key_string())
                        })?,
                        expected_gas_used: profile.expected_gas_used.ok_or_else(|| {
                            RuntimeGasProfileError::IncompleteApprovedRoute(route_key.key_string())
                        })?,
                        profile_identity: format!(
                            "{}:{}:{}",
                            artifact.content_digest,
                            route_key.key_string(),
                            artifact.executor_code_hash
                        ),
                        route_key: route_key.clone(),
                    })
                }
                ProfileStatus::Unsupported => RuntimeRoute::Unsupported(
                    profile.reason.unwrap_or_else(|| route_key.key_string()),
                ),
                ProfileStatus::ResearchOnly => RuntimeRoute::ResearchOnly,
            };
            routes.insert(route_key, route);
        }

        let artifact_digest = artifact.content_digest.clone();
        let invalidated = load_invalidations(
            invalidation_path.as_deref(),
            &artifact_digest,
            &routes,
        )?;
        let runtime = Self {
            executor_identity: config.executor_identity,
            routes,
            invalidated_routes: Arc::new(RwLock::new(invalidated)),
            artifact_digest,
            invalidation_path,
        };
        for route_key in &config.required_route_keys {
            runtime.quote(route_key)?;
        }
        Ok(runtime)
    }

    /// O(1) lookup for the latency-critical candidate path.
    pub fn quote(&self, route_key: &RouteKey) -> Result<GasQuote, RuntimeGasProfileError> {
        let invalidated = self
            .invalidated_routes
            .read()
            .map_err(|_| RuntimeGasProfileError::ProfileStatePoisoned)?;
        if invalidated.contains(route_key) {
            return Err(RuntimeGasProfileError::UnapprovedRoute(format!(
                "route invalidated after receipt qualification breach: {}",
                route_key.key_string()
            )));
        }
        match self.routes.get(route_key) {
            Some(RuntimeRoute::Approved(quote)) => Ok(quote.clone()),
            Some(RuntimeRoute::Unsupported(reason)) => {
                Err(RuntimeGasProfileError::UnapprovedRoute(reason.clone()))
            }
            Some(RuntimeRoute::ResearchOnly) => Err(RuntimeGasProfileError::UnapprovedRoute(
                route_key.key_string(),
            )),
            None => Err(RuntimeGasProfileError::UnknownRoute(route_key.key_string())),
        }
    }

    pub fn executor_identity(&self) -> &ExecutorIdentity {
        &self.executor_identity
    }

    pub fn invalidate(&self, route_key: &RouteKey) -> Result<(), RuntimeGasProfileError> {
        let mut invalidated = self
            .invalidated_routes
            .write()
            .map_err(|_| RuntimeGasProfileError::ProfileStatePoisoned)?;
        invalidated.insert(route_key.clone());
        if let Some(path) = &self.invalidation_path {
            let state = InvalidationState {
                content_digest: self.artifact_digest.clone(),
                routes: invalidated.iter().cloned().collect(),
            };
            let encoded = serde_json::to_vec_pretty(&state).map_err(|error| {
                RuntimeGasProfileError::InvalidationState {
                    path: path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
            let temp_path = path.with_extension("tmp");
            fs::write(&temp_path, encoded).map_err(|error| {
                RuntimeGasProfileError::InvalidationState {
                    path: path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
            fs::rename(&temp_path, path).map_err(|error| {
                RuntimeGasProfileError::InvalidationState {
                    path: path.display().to_string(),
                    message: error.to_string(),
                }
            })?;
        }
        Ok(())
    }
}

fn invalidation_path(path: &Path) -> PathBuf {
    path.with_extension("invalidated.json")
}

fn load_invalidations(
    path: Option<&Path>,
    content_digest: &str,
    routes: &HashMap<RouteKey, RuntimeRoute>,
) -> Result<std::collections::HashSet<RouteKey>, RuntimeGasProfileError> {
    let Some(path) = path else {
        return Ok(std::collections::HashSet::new());
    };
    let temp_path = path.with_extension("tmp");
    let mut states = Vec::new();
    for candidate in [path, temp_path.as_path()] {
        let encoded = match fs::read_to_string(candidate) {
            Ok(encoded) => encoded,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(RuntimeGasProfileError::InvalidationState {
                    path: candidate.display().to_string(),
                    message: error.to_string(),
                })
            }
        };
        let state: InvalidationState = serde_json::from_str(&encoded).map_err(|error| {
            RuntimeGasProfileError::InvalidationState {
                path: candidate.display().to_string(),
                message: error.to_string(),
            }
        })?;
        states.push(state);
    }
    let mut invalidated = std::collections::HashSet::new();
    for state in states {
        if state.content_digest == content_digest {
            invalidated.extend(state.routes);
        }
    }
    Ok(invalidated
        .into_iter()
        .filter(|route| matches!(routes.get(route), Some(RuntimeRoute::Approved(_))))
        .collect())
}

fn verify_identity(
    field: &'static str,
    expected: String,
    observed: String,
) -> Result<(), RuntimeGasProfileError> {
    if expected == observed {
        Ok(())
    } else {
        Err(RuntimeGasProfileError::Identity {
            field,
            expected,
            observed,
        })
    }
}
