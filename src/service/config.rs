//! Unified service configuration loader (WHI-727).
//!
//! Protocol-specific literals (min-profit floors, executor env-key lists) are
//! supplied via [`ServiceConfigOpts`] rather than hard-coded into the shared
//! loader. HTTP/WS endpoint defaults are selected by the expected chain id
//! (WHI-776), not by protocol opts.
//!
//! # RPC endpoint resolution (WHI-776)
//!
//! Endpoints are selected for a **declared** expected chain id (`--chain-id` /
//! `BOT_CHAIN_ID`), never by walking a fixed list that can silently pick Sepolia
//! when the operator meant mainnet.
//!
//! Precedence (HTTP and WS share the shape; variable **names** only are logged):
//!
//! 1. Explicit override: `RPC_HTTP_URL` / `RPC_WS_URL`
//! 2. Chain-specific canonical: `MANTLE_MAINNET_RPC_URL` (+ `_WS`) or
//!    `MANTLE_SEPOLIA_RPC_URL` (+ `_WS`), chosen by the expected chain
//! 3. Legacy mainnet aliases (mainnet only): `MANTLE_RPC_URL` / `MANTLE_RPC_WS_URL`
//! 4. Legacy generic aliases: `MANTLE_HTTP_URL` / `MANTLE_WS_URL`
//! 5. Built-in default for that chain
//!
//! Unknown chain ids still accept the explicit override + generic legacy aliases
//! and fall back to the mainnet defaults (operators must set overrides).

use crate::execution::ExecutorConfig;
use alloy::primitives::{address, Address, U256};
use alloy::providers::Provider;
use eyre::{bail, eyre, Result};
use std::str::FromStr;
use tracing::{info, warn};

/// Mantle mainnet chain id (single source: gas-profile constant).
pub const MANTLE_MAINNET_CHAIN_ID: u64 = crate::execution::MANTLE_MAINNET_CHAIN_ID;
/// Mantle Sepolia chain id.
pub const MANTLE_SEPOLIA_CHAIN_ID: u64 = 5003;
/// Default expected chain for the multi-protocol bot (mainnet).
pub const DEFAULT_EXPECTED_CHAIN_ID: u64 = MANTLE_MAINNET_CHAIN_ID;

/// Default Mantle mainnet WS endpoint (v3/moe shape, with https→wss normalize).
pub const DEFAULT_WS: &str = "wss://mantle.publicnode.com";
/// Default Mantle Sepolia WS endpoint.
pub const DEFAULT_WS_SEPOLIA: &str = "wss://ws.sepolia.mantle.xyz";
/// Default Mantle mainnet HTTP endpoint.
pub const DEFAULT_HTTP_MAINNET: &str = "https://rpc.mantle.xyz";
/// Default Mantle Sepolia HTTP endpoint (legacy v2 shape).
pub const DEFAULT_HTTP_SEPOLIA: &str = "https://rpc.sepolia.mantle.xyz";
/// Canonical Mantle WMNT.
pub const DEFAULT_WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");
/// FusionX V2 factory — the **interim** venue behind `SelectedProtocol::AgniV2`
/// rows in the frozen universe (WHI-910). Single source of truth for the
/// generator and for offline venue-support analysis; **do not** add further V2
/// venues against the hard-coded `V2_FEE = 300`.
pub const INTERIM_V2_FACTORY: Address = address!("E5020961fA51ffd3662CDf307dEf18F9a87Cce7c");

/// Sentinel source name when the built-in default for a chain was used.
pub const ENDPOINT_SOURCE_DEFAULT: &str = "default";

/// Agni-V3 min-profit floor: 0.01 MNT (18 decimals).
pub const V3_MIN_PROFIT_FLOOR_WEI: &str = "10000000000000000";
/// Moe min-profit floor: 0.25 MNT wei string used by the example
/// (`MIN_PROFIT_FLOOR_WEI = "250000000000000000"`; comment says ~0.3 MNT).
pub const MOE_MIN_PROFIT_FLOOR_WEI: &str = "250000000000000000";
/// Agni-V2 min-profit floor for the unified loader.
///
/// The legacy v2 example defaults both thresholds to zero when the env vars are
/// unset. The unified loader still accepts an explicit zero floor via
/// [`ServiceConfigOpts::min_profit_floor`] = `U256::ZERO`.
pub const V2_MIN_PROFIT_FLOOR_WEI: &str = "0";

/// Env-key fallback list used by Agni-V3 (includes `PRIVATE_EXECUTOR_ADDRESS`).
pub const V3_EXECUTOR_ENV_KEYS: &[&str] = &[
    "ARBITRAGE_EXECUTOR_ADDRESS",
    "EXECUTOR_ADDRESS",
    "EXECUTION_EXECUTOR_ADDRESS",
    "PRIVATE_EXECUTOR_ADDRESS",
];

/// Env-key fallback list for Moe after unification (adds `PRIVATE_EXECUTOR_ADDRESS`).
///
/// Adding the extra fallback is low-risk/additive: it only looks up one more
/// env var and does not remove any existing key.
pub const MOE_EXECUTOR_ENV_KEYS: &[&str] = &[
    "ARBITRAGE_EXECUTOR_ADDRESS",
    "EXECUTOR_ADDRESS",
    "EXECUTION_EXECUTOR_ADDRESS",
    "PRIVATE_EXECUTOR_ADDRESS",
];

/// Env-key list matching legacy v2 (single key + hardcoded fallback).
pub const V2_EXECUTOR_ENV_KEYS: &[&str] = &["ARBITRAGE_EXECUTOR_ADDRESS"];

/// Hardcoded executor used by the legacy v2 example when env is unset.
pub const V2_DEFAULT_EXECUTOR: Address =
    address!("59E5019B0d0e40762Df46fE472c0ae5a5c80b80f");

/// Resolved RPC endpoint: URL plus the **name** of the source that supplied it.
///
/// Callers may log [`Self::source`] freely; they must never log [`Self::url`]
/// (WHI-776 — no URL leakage in startup logs or tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedEndpoint {
    pub url: String,
    /// Env-var name, or [`ENDPOINT_SOURCE_DEFAULT`].
    pub source: &'static str,
}

/// Shared service configuration.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub ws_endpoint: String,
    /// Env-var name (or [`ENDPOINT_SOURCE_DEFAULT`]) that supplied `ws_endpoint`.
    pub ws_endpoint_source: &'static str,
    pub http_endpoint: String,
    /// Env-var name (or [`ENDPOINT_SOURCE_DEFAULT`]) that supplied `http_endpoint`.
    pub http_endpoint_source: &'static str,
    /// Operator-declared expected chain id (`--chain-id` / `BOT_CHAIN_ID`).
    pub expected_chain_id: u64,
    pub executor_address: Address,
    /// Wrapped native gas asset (Mantle WMNT). Used for gas-unit commensurability.
    pub wmnt_address: Address,
    /// Strategy settlement asset for closed arb cycles.
    ///
    /// On this deployment must equal [`Self::wmnt_address`] (validated at startup by
    /// [`super::startup::validate_settlement_asset`]). Distinct field so the strategy
    /// concept is named explicitly (WHI-529).
    pub settlement_asset: Address,
    pub min_gross_profit: U256,
    pub min_net_profit: U256,
    pub execution_slippage_bps: u32,
    pub block_cooldown: u64,
    pub executor_config: ExecutorConfig,
}

/// Protocol-supplied knobs for [`ServiceConfig::from_env`].
#[derive(Clone, Debug)]
pub struct ServiceConfigOpts {
    /// Min-profit floor applied to both gross and net thresholds.
    ///
    /// - `Some(floor)` — env values below the floor are clamped up; when the
    ///   env var is unset the floor is used (v3/moe shape). `Some(ZERO)` is
    ///   the unified v2 shape (env can raise thresholds; unset → zero).
    /// - `None` — both thresholds are forced to `U256::ZERO` and env vars are
    ///   ignored (only useful for pure monitor-only fixtures; prefer
    ///   `Some(ZERO)` for v2 parity).
    pub min_profit_floor: Option<U256>,
    /// Ordered env-var names tried for the executor address.
    pub executor_env_keys: &'static [&'static str],
    /// Log target for config messages (e.g. `"v3.config"`).
    pub log_target: &'static str,
    /// When set, used if no executor env var is present (legacy v2).
    pub hardcoded_executor_fallback: Option<Address>,
    /// When true, apply https→wss normalize on the WS endpoint (v3/moe).
    pub normalize_ws: bool,
    /// When true, prefer `SERVICE_WMNT_ADDRESS` over [`DEFAULT_WMNT`].
    pub allow_service_wmnt_env: bool,
}

impl ServiceConfigOpts {
    pub fn agni_v2() -> Self {
        Self {
            min_profit_floor: Some(U256::ZERO),
            executor_env_keys: V2_EXECUTOR_ENV_KEYS,
            log_target: "v2.config",
            hardcoded_executor_fallback: Some(V2_DEFAULT_EXECUTOR),
            normalize_ws: false,
            allow_service_wmnt_env: true,
        }
    }

    pub fn agni_v3() -> Self {
        Self {
            min_profit_floor: Some(
                U256::from_str(V3_MIN_PROFIT_FLOOR_WEI).expect("const floor parses"),
            ),
            executor_env_keys: V3_EXECUTOR_ENV_KEYS,
            log_target: "v3.config",
            hardcoded_executor_fallback: None,
            normalize_ws: true,
            allow_service_wmnt_env: false,
        }
    }

    pub fn moe() -> Self {
        Self {
            min_profit_floor: Some(
                U256::from_str(MOE_MIN_PROFIT_FLOOR_WEI).expect("const floor parses"),
            ),
            executor_env_keys: MOE_EXECUTOR_ENV_KEYS,
            log_target: "moe.config",
            hardcoded_executor_fallback: None,
            normalize_ws: true,
            allow_service_wmnt_env: false,
        }
    }
}

impl ServiceConfig {
    /// Load service config from the environment for `expected_chain_id`.
    ///
    /// Endpoint selection follows the declared chain (WHI-776). Logs only the
    /// **variable name** that supplied each endpoint — never the URL.
    pub fn from_env(opts: ServiceConfigOpts, expected_chain_id: u64) -> Result<Self> {
        if expected_chain_id == 0 {
            bail!("expected chain_id must be non-zero (set --chain-id / BOT_CHAIN_ID)");
        }

        // Legacy v2 skipped https→wss normalize; keep that when `normalize_ws`
        // is false. Defaults already use `wss://`.
        let ws = if opts.normalize_ws {
            resolve_ws_endpoint(expected_chain_id)
        } else {
            resolve_ws_endpoint_raw(expected_chain_id)
        };
        info!(
            target: "service.config",
            protocol = opts.log_target,
            expected_chain_id,
            source = ws.source,
            "Using WebSocket endpoint"
        );

        let http = resolve_http_endpoint(expected_chain_id);
        info!(
            target: "service.config",
            protocol = opts.log_target,
            expected_chain_id,
            source = http.source,
            "Using HTTP endpoint"
        );

        let executor_address = match read_address_from_env(opts.executor_env_keys) {
            Ok((address, source)) => {
                info!(
                    target: "service.config",
                    protocol = opts.log_target,
                    executor = %address,
                    source,
                    "Using executor address"
                );
                address
            }
            Err(_) => match opts.hardcoded_executor_fallback {
                Some(fallback) => {
                    info!(
                        target: "service.config",
                        protocol = opts.log_target,
                        executor = %fallback,
                        source = "hardcoded_fallback",
                        "Using hardcoded executor address"
                    );
                    fallback
                }
                None => return Err(eyre!("Missing executor address env variable")),
            },
        };

        let wmnt_address = if opts.allow_service_wmnt_env {
            std::env::var("SERVICE_WMNT_ADDRESS")
                .ok()
                .and_then(|s| Address::from_str(s.trim()).ok())
                .unwrap_or(DEFAULT_WMNT)
        } else {
            DEFAULT_WMNT
        };

        let (min_gross_profit, min_net_profit) = match opts.min_profit_floor {
            None => (U256::ZERO, U256::ZERO),
            Some(floor) => {
                let min_gross = read_min_profit_threshold("MIN_GROSS_PROFIT_WEI", &floor)?;
                let min_net_raw = read_min_profit_threshold("MIN_NET_PROFIT_WEI", &floor)?;
                let min_net = if min_net_raw < min_gross {
                    warn!(
                        target: "service.config",
                        protocol = opts.log_target,
                        provided = %min_net_raw,
                        adjusted = %min_gross,
                        "Net profit threshold below gross profit threshold; using gross threshold"
                    );
                    min_gross
                } else {
                    min_net_raw
                };
                (min_gross, min_net)
            }
        };

        let execution_slippage_bps = std::env::var("EXECUTION_SLIPPAGE_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(30);
        let block_cooldown = std::env::var("EXECUTION_BLOCK_COOLDOWN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        let mut executor_config = ExecutorConfig::default();
        executor_config.chain_id = expected_chain_id;
        executor_config.min_net_profit_mnt_wei = min_net_profit;
        if let Ok(raw) = std::env::var("EXECUTOR_PRIORITY_FEE_WEI") {
            if let Ok(value) = raw.trim().parse::<u128>() {
                executor_config.default_priority_fee_wei = value;
            }
        }

        // Strategy settlement asset tracks the gas asset on this deployment.
        // Startup validation (`validate_settlement_asset`) enforces equality with
        // executor.WMNT() before any discovery/execution (WHI-529).
        let settlement_asset = wmnt_address;

        Ok(Self {
            ws_endpoint: ws.url,
            ws_endpoint_source: ws.source,
            http_endpoint: http.url,
            http_endpoint_source: http.source,
            expected_chain_id,
            executor_address,
            wmnt_address,
            settlement_asset,
            min_gross_profit,
            min_net_profit,
            execution_slippage_bps,
            block_cooldown,
            executor_config,
        })
    }
}

/// Fail closed when the connected provider's chain id differs from the expected
/// value. Message names both numbers; never warn-and-continue (WHI-776).
pub fn assert_expected_chain_id(expected: u64, observed: u64) -> Result<()> {
    if expected != observed {
        bail!(
            "chain_id mismatch: expected={expected}, observed={observed}. \
             Check --chain-id / BOT_CHAIN_ID against the RPC endpoint selected \
             for that chain (see MANTLE_MAINNET_RPC_URL / MANTLE_SEPOLIA_RPC_URL)."
        );
    }
    Ok(())
}

/// Fail closed when HTTP and WS providers report different chain ids (WHI-776).
pub fn assert_http_ws_chain_ids_agree(http_chain_id: u64, ws_chain_id: u64) -> Result<()> {
    if http_chain_id != ws_chain_id {
        bail!(
            "HTTP/WS chain_id mismatch: http={http_chain_id}, ws={ws_chain_id}. \
             Both transports must resolve to the same declared chain."
        );
    }
    Ok(())
}

/// Read `eth_chainId` from `provider` and assert it equals `expected`.
///
/// Returns the observed id on success so callers can thread it into
/// `SnapshotId` / shadow ledger headers.
pub async fn observe_and_assert_chain_id<P: Provider>(
    provider: &P,
    expected: u64,
) -> Result<u64> {
    let observed = provider
        .get_chain_id()
        .await
        .map_err(|e| eyre!("eth_chainId: {e}"))?;
    assert_expected_chain_id(expected, observed)?;
    Ok(observed)
}

/// Resolve the HTTP endpoint for `expected_chain_id` (WHI-776).
///
/// See module docs for precedence. Returns the URL plus the source **name**.
pub fn resolve_http_endpoint(expected_chain_id: u64) -> ResolvedEndpoint {
    resolve_from_candidates(
        http_env_candidates(expected_chain_id),
        default_http_for_chain(expected_chain_id),
    )
}

/// Resolve the WS endpoint for `expected_chain_id`, with https→wss normalize.
pub fn resolve_ws_endpoint(expected_chain_id: u64) -> ResolvedEndpoint {
    let resolved = resolve_ws_endpoint_raw(expected_chain_id);
    let normalized = normalize_ws_endpoint(resolved.url.trim());
    if normalized != resolved.url {
        info!(
            target: "service.config",
            source = resolved.source,
            "Normalized WS endpoint scheme (https→wss or bare host→wss)"
        );
    }
    ResolvedEndpoint {
        url: normalized,
        source: resolved.source,
    }
}

/// Resolve WS without scheme normalize (legacy v2 shape).
fn resolve_ws_endpoint_raw(expected_chain_id: u64) -> ResolvedEndpoint {
    resolve_from_candidates(
        ws_env_candidates(expected_chain_id),
        default_ws_for_chain(expected_chain_id),
    )
}

fn default_http_for_chain(chain_id: u64) -> &'static str {
    if chain_id == MANTLE_SEPOLIA_CHAIN_ID {
        DEFAULT_HTTP_SEPOLIA
    } else {
        DEFAULT_HTTP_MAINNET
    }
}

fn default_ws_for_chain(chain_id: u64) -> &'static str {
    if chain_id == MANTLE_SEPOLIA_CHAIN_ID {
        DEFAULT_WS_SEPOLIA
    } else {
        DEFAULT_WS
    }
}

/// Ordered `(env_var_name,)` candidates for HTTP, selected by expected chain.
///
/// Explicit override first; then chain-specific; then legacy aliases that are
/// safe for that chain; never the other network's chain-specific vars.
fn http_env_candidates(expected_chain_id: u64) -> &'static [&'static str] {
    if expected_chain_id == MANTLE_SEPOLIA_CHAIN_ID {
        &[
            "RPC_HTTP_URL",
            "MANTLE_SEPOLIA_RPC_URL",
            "MANTLE_HTTP_URL",
        ]
    } else if expected_chain_id == MANTLE_MAINNET_CHAIN_ID {
        &[
            "RPC_HTTP_URL",
            "MANTLE_MAINNET_RPC_URL",
            "MANTLE_RPC_URL",
            "MANTLE_HTTP_URL",
        ]
    } else {
        // Unknown chain: only explicit / generic aliases.
        &["RPC_HTTP_URL", "MANTLE_HTTP_URL"]
    }
}

fn ws_env_candidates(expected_chain_id: u64) -> &'static [&'static str] {
    if expected_chain_id == MANTLE_SEPOLIA_CHAIN_ID {
        &[
            "RPC_WS_URL",
            "MANTLE_SEPOLIA_RPC_WS_URL",
            "MANTLE_WS_URL",
        ]
    } else if expected_chain_id == MANTLE_MAINNET_CHAIN_ID {
        &[
            "RPC_WS_URL",
            "MANTLE_MAINNET_RPC_WS_URL",
            "MANTLE_RPC_WS_URL",
            "MANTLE_WS_URL",
        ]
    } else {
        &["RPC_WS_URL", "MANTLE_WS_URL"]
    }
}

fn resolve_from_candidates(
    candidates: &[&'static str],
    default_url: &'static str,
) -> ResolvedEndpoint {
    for &var in candidates {
        if let Ok(raw) = std::env::var(var) {
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                return ResolvedEndpoint {
                    url: trimmed,
                    source: var,
                };
            }
        }
    }
    ResolvedEndpoint {
        url: default_url.to_string(),
        source: ENDPOINT_SOURCE_DEFAULT,
    }
}

/// Normalize a WS URL: empty → default; `https://` → `wss://`; bare host → `wss://host`.
pub fn normalize_ws_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_WS.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        format!("wss://{trimmed}")
    }
}

/// Read the first present address env var from `vars`.
pub fn read_address_from_env<'a>(vars: &'a [&'a str]) -> Result<(Address, &'a str)> {
    for &var in vars {
        if let Ok(raw) = std::env::var(var) {
            let parsed = Address::from_str(raw.trim())
                .map_err(|e| eyre!("invalid address in {var}: {e}"))?;
            return Ok((parsed, var));
        }
    }
    Err(eyre!("Missing executor address env variable"))
}

/// Read a min-profit threshold, clamping values below `floor` up to the floor.
pub fn read_min_profit_threshold(var: &str, floor: &U256) -> Result<U256> {
    let value = match std::env::var(var) {
        Ok(raw) => {
            let parsed = U256::from_str(raw.trim())?;
            if parsed < *floor {
                warn!(
                    target: "service.config",
                    variable = var,
                    provided = %parsed,
                    floor = %floor,
                    "Configured profit threshold below floor; using floor"
                );
                *floor
            } else {
                parsed
            }
        }
        Err(_) => *floor,
    };
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::providers::ProviderBuilder;
    use alloy::transports::mock::Asserter;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Clear every RPC env var the resolvers consult so tests are hermetic.
    fn clear_rpc_env() {
        for var in [
            "RPC_HTTP_URL",
            "RPC_WS_URL",
            "MANTLE_MAINNET_RPC_URL",
            "MANTLE_MAINNET_RPC_WS_URL",
            "MANTLE_SEPOLIA_RPC_URL",
            "MANTLE_SEPOLIA_RPC_WS_URL",
            "MANTLE_RPC_URL",
            "MANTLE_RPC_WS_URL",
            "MANTLE_HTTP_URL",
            "MANTLE_WS_URL",
        ] {
            std::env::remove_var(var);
        }
    }

    #[test]
    fn normalize_ws_converts_https() {
        assert_eq!(
            normalize_ws_endpoint("https://rpc.example.com"),
            "wss://rpc.example.com"
        );
        assert_eq!(
            normalize_ws_endpoint("http://rpc.example.com"),
            "ws://rpc.example.com"
        );
        assert_eq!(
            normalize_ws_endpoint("wss://rpc.example.com"),
            "wss://rpc.example.com"
        );
        assert_eq!(
            normalize_ws_endpoint("rpc.example.com"),
            "wss://rpc.example.com"
        );
    }

    #[test]
    fn moe_executor_keys_include_private_executor() {
        assert!(MOE_EXECUTOR_ENV_KEYS.contains(&"PRIVATE_EXECUTOR_ADDRESS"));
        assert_eq!(MOE_EXECUTOR_ENV_KEYS, V3_EXECUTOR_ENV_KEYS);
    }

    #[test]
    fn profit_floors_parse() {
        assert_eq!(
            U256::from_str(V3_MIN_PROFIT_FLOOR_WEI).unwrap(),
            U256::from(10_000_000_000_000_000u64)
        );
        assert_eq!(
            U256::from_str(MOE_MIN_PROFIT_FLOOR_WEI).unwrap(),
            U256::from(250_000_000_000_000_000u64)
        );
    }

    #[test]
    fn read_min_profit_clamps_below_floor() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var("MIN_GROSS_PROFIT_WEI", "1");
        let floor = U256::from(100u64);
        let value = read_min_profit_threshold("MIN_GROSS_PROFIT_WEI", &floor).unwrap();
        assert_eq!(value, floor);
        std::env::remove_var("MIN_GROSS_PROFIT_WEI");
    }

    #[test]
    fn assert_expected_chain_id_accepts_match() {
        assert_expected_chain_id(5000, 5000).unwrap();
    }

    #[test]
    fn assert_expected_chain_id_fails_closed_naming_both_ids() {
        let err = assert_expected_chain_id(5000, 5003).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected=5000"), "{msg}");
        assert!(msg.contains("observed=5003"), "{msg}");
        assert!(
            !msg.contains("http://") && !msg.contains("wss://") && !msg.contains("https://"),
            "error must not embed a URL: {msg}"
        );
    }

    #[test]
    fn assert_http_ws_chain_ids_agree_fails_closed() {
        let err = assert_http_ws_chain_ids_agree(5000, 5003).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("http=5000"), "{msg}");
        assert!(msg.contains("ws=5003"), "{msg}");
    }

    #[tokio::test]
    async fn observe_and_assert_chain_id_rejects_mismatch_via_mock_provider() {
        let asserter = Asserter::new();
        // eth_chainId response (hex quantity is also accepted; alloy mock takes u64).
        asserter.push_success(&5003u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);

        let err = observe_and_assert_chain_id(&provider, 5000)
            .await
            .expect_err("mainnet expected against sepolia provider must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("expected=5000"), "{msg}");
        assert!(msg.contains("observed=5003"), "{msg}");
    }

    #[tokio::test]
    async fn observe_and_assert_chain_id_returns_observed_on_match() {
        let asserter = Asserter::new();
        asserter.push_success(&5000u64);
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let observed = observe_and_assert_chain_id(&provider, 5000)
            .await
            .expect("match");
        assert_eq!(observed, 5000);
    }

    #[test]
    fn mainnet_prefers_mainnet_var_over_sepolia_var() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        std::env::set_var("MANTLE_SEPOLIA_RPC_URL", "https://sepolia.example/ignored");
        std::env::set_var("MANTLE_MAINNET_RPC_URL", "https://mainnet.example/http");
        let resolved = resolve_http_endpoint(MANTLE_MAINNET_CHAIN_ID);
        assert_eq!(resolved.source, "MANTLE_MAINNET_RPC_URL");
        assert_eq!(resolved.url, "https://mainnet.example/http");
        clear_rpc_env();
    }

    #[test]
    fn mainnet_uses_legacy_mantle_rpc_url_without_override() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        // Sepolia present must NOT win for expected mainnet.
        std::env::set_var("MANTLE_SEPOLIA_RPC_URL", "https://sepolia.example/ignored");
        std::env::set_var("MANTLE_RPC_URL", "https://mainnet.example/legacy");
        let resolved = resolve_http_endpoint(MANTLE_MAINNET_CHAIN_ID);
        assert_eq!(resolved.source, "MANTLE_RPC_URL");
        assert_eq!(resolved.url, "https://mainnet.example/legacy");
        clear_rpc_env();
    }

    #[test]
    fn sepolia_does_not_consult_mainnet_vars() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        std::env::set_var("MANTLE_MAINNET_RPC_URL", "https://mainnet.example/ignored");
        std::env::set_var("MANTLE_RPC_URL", "https://mainnet.example/legacy-ignored");
        std::env::set_var("MANTLE_SEPOLIA_RPC_URL", "https://sepolia.example/http");
        let resolved = resolve_http_endpoint(MANTLE_SEPOLIA_CHAIN_ID);
        assert_eq!(resolved.source, "MANTLE_SEPOLIA_RPC_URL");
        assert_eq!(resolved.url, "https://sepolia.example/http");
        clear_rpc_env();
    }

    #[test]
    fn explicit_rpc_http_url_overrides_chain_specific() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        std::env::set_var("RPC_HTTP_URL", "https://override.example/http");
        std::env::set_var("MANTLE_MAINNET_RPC_URL", "https://mainnet.example/ignored");
        let resolved = resolve_http_endpoint(MANTLE_MAINNET_CHAIN_ID);
        assert_eq!(resolved.source, "RPC_HTTP_URL");
        assert_eq!(resolved.url, "https://override.example/http");
        clear_rpc_env();
    }

    #[test]
    fn mainnet_ws_resolves_legacy_mantle_rpc_ws_url() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        std::env::set_var("MANTLE_RPC_WS_URL", "wss://mainnet.example/ws");
        let resolved = resolve_ws_endpoint(MANTLE_MAINNET_CHAIN_ID);
        assert_eq!(resolved.source, "MANTLE_RPC_WS_URL");
        assert_eq!(resolved.url, "wss://mainnet.example/ws");
        clear_rpc_env();
    }

    #[test]
    fn sepolia_ws_defaults_when_unset() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        let resolved = resolve_ws_endpoint(MANTLE_SEPOLIA_CHAIN_ID);
        assert_eq!(resolved.source, ENDPOINT_SOURCE_DEFAULT);
        assert_eq!(resolved.url, DEFAULT_WS_SEPOLIA);
        clear_rpc_env();
    }

    #[test]
    fn mainnet_http_default_when_unset() {
        let _guard = env_lock().lock().unwrap();
        clear_rpc_env();
        let resolved = resolve_http_endpoint(MANTLE_MAINNET_CHAIN_ID);
        assert_eq!(resolved.source, ENDPOINT_SOURCE_DEFAULT);
        assert_eq!(resolved.url, DEFAULT_HTTP_MAINNET);
        clear_rpc_env();
    }

    #[test]
    fn from_env_rejects_zero_chain_id() {
        let err = ServiceConfig::from_env(ServiceConfigOpts::agni_v3(), 0).unwrap_err();
        assert!(format!("{err:#}").contains("non-zero"));
    }
}
