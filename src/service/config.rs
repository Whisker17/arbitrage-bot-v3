//! Unified service configuration loader (WHI-727).
//!
//! Protocol-specific literals (min-profit floors, executor env-key lists, HTTP
//! defaults) are supplied via [`ServiceConfigOpts`] rather than hard-coded into
//! the shared loader.

use crate::execution::ExecutorConfig;
use alloy::primitives::{address, Address, U256};
use eyre::{eyre, Result};
use std::str::FromStr;
use tracing::{info, warn};

/// Default Mantle mainnet WS endpoint (v3/moe shape, with https→wss normalize).
pub const DEFAULT_WS: &str = "wss://mantle.publicnode.com";
/// Default Mantle mainnet HTTP endpoint.
pub const DEFAULT_HTTP_MAINNET: &str = "https://rpc.mantle.xyz";
/// Default Mantle Sepolia HTTP endpoint (legacy v2 shape).
pub const DEFAULT_HTTP_SEPOLIA: &str = "https://rpc.sepolia.mantle.xyz";
/// Canonical Mantle WMNT.
pub const DEFAULT_WMNT: Address = address!("78c1b0C915c4FAA5FffA6CAbf0219DA63d7f4cb8");

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

/// Shared service configuration.
#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub ws_endpoint: String,
    pub http_endpoint: String,
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
    /// HTTP default when no env var is set.
    pub default_http: &'static str,
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
            default_http: DEFAULT_HTTP_SEPOLIA,
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
            default_http: DEFAULT_HTTP_MAINNET,
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
            default_http: DEFAULT_HTTP_MAINNET,
            log_target: "moe.config",
            hardcoded_executor_fallback: None,
            normalize_ws: true,
            allow_service_wmnt_env: false,
        }
    }
}

impl ServiceConfig {
    pub fn from_env(opts: ServiceConfigOpts) -> Result<Self> {
        let ws_endpoint = if opts.normalize_ws {
            resolve_ws_endpoint()
        } else {
            std::env::var("RPC_WS_URL")
                .or_else(|_| std::env::var("MANTLE_WS_URL"))
                .unwrap_or_else(|_| DEFAULT_WS.to_string())
        };
        info!(
            target: "service.config",
            protocol = opts.log_target,
            ws = %ws_endpoint,
            "Using WebSocket endpoint"
        );

        let http_endpoint = resolve_http_endpoint(opts.default_http);
        info!(
            target: "service.config",
            protocol = opts.log_target,
            http = %http_endpoint,
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
            ws_endpoint,
            http_endpoint,
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

/// Resolve WS endpoint with https→wss normalize (v3/moe).
pub fn resolve_ws_endpoint() -> String {
    let raw = std::env::var("RPC_WS_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_WS_URL").ok())
        .unwrap_or_else(|| DEFAULT_WS.to_string());
    let normalized = normalize_ws_endpoint(raw.trim());
    if normalized != raw {
        info!(
            target: "service.config",
            original = %raw,
            normalized = %normalized,
            "Normalized WS endpoint"
        );
    }
    normalized
}

/// Resolve HTTP endpoint, falling back to `default` when unset/empty.
///
/// Env order matches the three services: `RPC_HTTP_URL` → `MANTLE_HTTP_URL`,
/// then (for V2 legacy parity) `MANTLE_SEPOLIA_RPC_URL`, then `default`.
pub fn resolve_http_endpoint(default: &str) -> String {
    std::env::var("RPC_HTTP_URL")
        .ok()
        .or_else(|| std::env::var("MANTLE_HTTP_URL").ok())
        .or_else(|| std::env::var("MANTLE_SEPOLIA_RPC_URL").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
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
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
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
}
