//! NoSend / signerless startup helpers (WHI-727 / WHI-860).
//!
//! Near-pure relocation of the already-generic logic from
//! `examples/protocols/intent_service_support.rs`. Callers must still invoke
//! [`crate::execution::guard_shadow_env`] at process start when running
//! signerless / shadow mode.
//!
//! **Invariant (WHI-860):** [`production_send_allowed`] is process-global and
//! defaults to `false`. It flips only after [`crate::service::send_path::arm_production_send_path`]
//! succeeds (explicit opt-in + on-chain role / pause / breaker checks).

use crate::execution::{
    BlockFeeContextCache, ExecutionContext, Executor, ExecutorConfig, IArbitrageExecutor,
    RuntimeGasProfile, RuntimeProfileConfig, ShadowConfigPaths, ShadowExecutionContext,
    ShadowLedgerSetup, ShadowOverrideTarget, ShadowPinnedConfig,
};
use crate::service::error::ProtocolError;
use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::{eyre, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Production broadcast gate (WHI-860).
///
/// Defaults to `false`. Armed only by
/// [`crate::service::send_path::arm_production_send_path`] after explicit opt-in
/// and on-chain preconditions. Env alone never flips this.
pub use crate::service::send_path::production_send_allowed;

/// Shadow ledger `service` identity for the merged multi-protocol `bot` binary
/// (WHI-739). Free-form at construction; registering it in
/// `REQUIRED_SHADOW_SERVICES` / the continuous runner is WHI-740.
pub const MERGED_BOT_SHADOW_SERVICE: &str = "bot";

/// Config-only settlement check (no executor RPC).
///
/// Used by offline fixture mode where there is no deployed executor to query.
/// Asserts `settlement_asset == gas_asset` and non-zero.
pub fn validate_settlement_asset_config(
    settlement_asset: Address,
    gas_asset: Address,
) -> Result<(), ProtocolError> {
    if settlement_asset == Address::ZERO {
        return Err(ProtocolError::SettlementAssetZero);
    }
    if settlement_asset != gas_asset {
        return Err(ProtocolError::SettlementAssetGasMismatch {
            configured: settlement_asset,
            gas_asset,
        });
    }
    Ok(())
}

/// Fail-closed settlement validation for live mode (WHI-529 / B9).
///
/// Requires **all three**:
/// * `settlement_asset != Address::ZERO`
/// * `settlement_asset == gas_asset` (wrapped native / WMNT on Mantle)
/// * `settlement_asset == executor.WMNT()`
///
/// Supporting any other settlement asset needs a generalized executor **and**
/// a native-gas → settlement conversion — neither exists.
pub async fn validate_settlement_asset<P: Provider>(
    settlement_asset: Address,
    gas_asset: Address,
    executor: Address,
    provider: &P,
) -> Result<(), ProtocolError> {
    validate_settlement_asset_config(settlement_asset, gas_asset)?;
    let executor_wmnt = IArbitrageExecutor::new(executor, provider)
        .WMNT()
        .call()
        .await
        .map_err(|e| ProtocolError::SettlementAssetRpc(e.to_string()))?;
    if settlement_asset != executor_wmnt {
        return Err(ProtocolError::SettlementAssetMismatch {
            configured: settlement_asset,
            executor_wmnt,
            gas_asset,
        });
    }
    Ok(())
}

/// Whether shadow mode is requested (`SHADOW_MODE=1`), via the library's single
/// definition of the env-var convention.
pub fn shadow_mode_enabled() -> bool {
    crate::execution::shadow_mode_requested(&crate::execution::e2e::ProcessEnvSource)
}

/// Resolve the shadow ledger path for this service process.
///
/// Prefer an explicit `--ledger` CLI argument (used by the WHI-526 runbook so
/// all services share one append-only ledger). Fall back to `default` when the
/// flag is absent. Unknown flags fail closed.
pub fn shadow_ledger_path(default: impl Into<PathBuf>) -> Result<PathBuf> {
    resolve_shadow_ledger_path(std::env::args_os(), default.into())
}

/// Testable core of [`shadow_ledger_path`].
pub fn resolve_shadow_ledger_path<I, T>(args: I, default: PathBuf) -> Result<PathBuf>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
{
    let mut args = args.into_iter().map(Into::into);
    // Skip argv0.
    let _ = args.next();
    let mut ledger: Option<PathBuf> = None;
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        let arg_str = arg.to_string_lossy();
        match arg_str.as_ref() {
            "--ledger" => {
                let value = iter
                    .next()
                    .ok_or_else(|| eyre!("invalid service arguments: --ledger requires a path"))?;
                ledger = Some(PathBuf::from(value));
            }
            s if s.starts_with("--ledger=") => {
                ledger = Some(PathBuf::from(&s["--ledger=".len()..]));
            }
            "--shadow" => {
                // Compatibility flag for the WHI-526 runbook. Shadow mode itself
                // remains controlled by SHADOW_MODE=1.
            }
            s if s.starts_with('-') => {
                return Err(eyre!("invalid service arguments: unknown flag {s}"));
            }
            _ => {
                // Positional args are ignored (examples don't take any).
            }
        }
    }
    Ok(ledger.unwrap_or(default))
}

/// Loads the canonical Mantle-mainnet gas-profile artifact and builds a real
/// [`Executor`] from it (WHI-553).
pub async fn build_execution_runtime<P: Provider + Clone + 'static>(
    provider: P,
    executor_contract: Address,
    wmnt_address: Address,
    executor_config: ExecutorConfig,
) -> Result<Executor> {
    let profile_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("config/gas_profiles/mantle_mainnet_v1.json");
    let gas_profile = RuntimeGasProfile::load(
        &profile_path,
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
    )
    .map_err(|e| eyre!("failed to load gas profile runtime artifact: {e}"))?;
    let block_fee_contexts = Arc::new(BlockFeeContextCache::default());
    let context = ExecutionContext::from_provider(
        provider,
        executor_contract,
        wmnt_address,
        gas_profile,
        block_fee_contexts,
    )
    .await?;
    Ok(Executor::new(context, executor_config))
}

/// Degrade-closed wrapper around [`build_execution_runtime`].
///
/// On executor-identity mismatch (e.g. Sepolia), returns `None` so the caller
/// can continue monitor-only. Never auto-selects a different profile artifact.
pub async fn build_execution_runtime_or_monitor_only<P: Provider + Clone + 'static>(
    provider: P,
    executor_contract: Address,
    wmnt_address: Address,
    executor_config: ExecutorConfig,
    service: &'static str,
) -> Option<Executor> {
    match build_execution_runtime(provider, executor_contract, wmnt_address, executor_config).await
    {
        Ok(executor) => Some(executor),
        Err(error) => {
            tracing::warn!(
                target: "execution.runtime",
                service,
                executor = %executor_contract,
                wmnt = %wmnt_address,
                error = %error,
                "Execution runtime unavailable (executor identity does not match \
                 config/gas_profiles/mantle_mainnet_v1.json, e.g. on a Sepolia \
                 deployment). Continuing MONITOR-ONLY: no pipeline head, no signing, no \
                 broadcast. Point the service at the pinned mainnet executor to re-enable \
                 the execution path."
            );
            None
        }
    }
}

/// Assembles a [`ShadowExecutionContext`] for WHI-549 signerless shadow mode.
pub fn build_shadow_execution_context<P: Provider + Clone + 'static>(
    provider: P,
    target: ShadowOverrideTarget,
    executor_config: ExecutorConfig,
    ledger_path: &Path,
    service: &'static str,
) -> Result<ShadowExecutionContext> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let gas_profiles = manifest_dir.join("config/gas_profiles");
    let threshold_config_path = std::env::var_os("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| {
            eyre!("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH is required for signerless shadow mode")
        })?;

    let pinned_config = ShadowPinnedConfig::load(
        ShadowConfigPaths {
            artifact_dir: manifest_dir.join("contracts/executor/artifacts"),
            wmnt_descriptor_path: gas_profiles.join("wmnt_descriptor.mantle_mainnet.json"),
            moe_allowlist_path: gas_profiles.join("moe_allowlist.mantle_mainnet.json"),
            approved_pools_path: gas_profiles.join("approved_pools.mantle_mainnet.json"),
            threshold_config_path,
            gas_profile_artifact_path: gas_profiles.join("mantle_mainnet_v1.json"),
        },
        target,
    )
    .map_err(|e| eyre!("failed to pin the shadow config generation: {e}"))?;

    let started_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    ShadowExecutionContext::new(
        provider,
        pinned_config,
        RuntimeProfileConfig::mantle_mainnet(Vec::new()),
        Arc::new(BlockFeeContextCache::default()),
        executor_config,
        ShadowLedgerSetup {
            path: ledger_path,
            started_at_unix,
            service,
        },
    )
    .map_err(|e| eyre!("failed to build shadow execution context: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::Address;
    use alloy::providers::ProviderBuilder;
    use alloy::sol_types::SolValue;
    use alloy::transports::mock::Asserter;

    #[test]
    fn production_send_allowed_defaults_false() {
        // WHI-860: gate starts closed; only arm_production_send_path may open it.
        crate::service::send_path::disarm_production_sends();
        assert!(!production_send_allowed());
    }

    #[test]
fn merged_bot_shadow_service_identity_is_bot() {
        assert_eq!(MERGED_BOT_SHADOW_SERVICE, "bot");
    }

    #[test]
    fn build_shadow_execution_context_requires_thresholds_path() {
        use alloy::providers::ProviderBuilder;
        use alloy::transports::mock::Asserter;
        use crate::execution::ShadowOverrideTarget;

        // Ensure the env var is absent for this check. Other tests may set it;
        // restore any previous value on the way out.
        let previous = std::env::var_os("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH");
        std::env::remove_var("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH");

        let provider = ProviderBuilder::new().connect_mocked_client(Asserter::new());
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("ledger.jsonl");
        let result = build_shadow_execution_context(
            provider,
            ShadowOverrideTarget {
                executor_contract: Address::repeat_byte(0xE0),
                wmnt_address: Address::repeat_byte(0xF0),
            },
            ExecutorConfig::default(),
            &ledger,
            MERGED_BOT_SHADOW_SERVICE,
        );
        let err = match result {
            Ok(_) => panic!("missing thresholds path must fail closed"),
            Err(e) => e,
        };
        let msg = format!("{err:#}");
        assert!(
            msg.contains("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH"),
            "error must name the missing env var: {msg}"
        );

        match previous {
            Some(value) => std::env::set_var("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH", value),
            None => std::env::remove_var("MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH"),
        }
    }

    #[test]
    fn validate_settlement_config_rejects_zero_and_mismatch() {
        let wmnt = Address::repeat_byte(0x78);
        let other = Address::repeat_byte(0x11);
        assert!(matches!(
            validate_settlement_asset_config(Address::ZERO, wmnt),
            Err(ProtocolError::SettlementAssetZero)
        ));
        assert!(matches!(
            validate_settlement_asset_config(other, wmnt),
            Err(ProtocolError::SettlementAssetGasMismatch { .. })
        ));
        assert!(validate_settlement_asset_config(wmnt, wmnt).is_ok());
    }

    #[tokio::test]
    async fn validate_settlement_asset_ok_when_executor_matches() {
        let wmnt = Address::repeat_byte(0x78);
        let executor = Address::repeat_byte(0xE0);
        let asserter = Asserter::new();
        // eth_call return for WMNT()
        asserter.push_success(&alloy::primitives::Bytes::from(wmnt.abi_encode()));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        validate_settlement_asset(wmnt, wmnt, executor, &provider)
            .await
            .expect("matching settlement must pass");
    }

    #[tokio::test]
    async fn validate_settlement_asset_err_when_executor_mismatches() {
        let configured = Address::repeat_byte(0x78);
        let executor_wmnt = Address::repeat_byte(0x99);
        let executor = Address::repeat_byte(0xE0);
        let asserter = Asserter::new();
        asserter.push_success(&alloy::primitives::Bytes::from(executor_wmnt.abi_encode()));
        let provider = ProviderBuilder::new().connect_mocked_client(asserter);
        let err = validate_settlement_asset(configured, configured, executor, &provider)
            .await
            .expect_err("mismatched executor WMNT must fail");
        match err {
            ProtocolError::SettlementAssetMismatch {
                configured: c,
                executor_wmnt: e,
                gas_asset: g,
            } => {
                assert_eq!(c, configured);
                assert_eq!(e, executor_wmnt);
                assert_eq!(g, configured);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn ledger_path_defaults_when_flag_absent() {
        let path = resolve_shadow_ledger_path(
            ["v2_monitor_executor_service"],
            PathBuf::from("logs/shadow_ledger_v2.jsonl"),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("logs/shadow_ledger_v2.jsonl"));
    }

    #[test]
    fn ledger_path_accepts_explicit_flag() {
        let path = resolve_shadow_ledger_path(
            [
                "v2_monitor_executor_service",
                "--ledger",
                "logs/shared_shadow_ledger.jsonl",
            ],
            PathBuf::from("logs/shadow_ledger_v2.jsonl"),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("logs/shared_shadow_ledger.jsonl"));
    }

    #[test]
    fn ledger_path_accepts_shadow_and_ledger_together() {
        let path = resolve_shadow_ledger_path(
            [
                "v2_monitor_executor_service",
                "--shadow",
                "--ledger",
                "evidence/shadow/ledger.jsonl",
            ],
            PathBuf::from("logs/shadow_ledger_v2.jsonl"),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("evidence/shadow/ledger.jsonl"));
    }

    #[test]
    fn ledger_path_rejects_unknown_flags() {
        let err = resolve_shadow_ledger_path(
            ["v2_monitor_executor_service", "--not-a-real-flag"],
            PathBuf::from("logs/shadow_ledger_v2.jsonl"),
        )
        .expect_err("unknown flags must fail closed");
        assert!(
            err.to_string().contains("invalid service arguments"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn no_signer_construction_in_startup_source() {
        // Guardrail: this module must not grow signer construction. Scan only
        // the non-test portion of the file so this assertion doesn't match
        // itself. Hot-signer loading lives in `send_path.rs` (WHI-860).
        let src = include_str!("startup.rs");
        let production = src
            .split("#[cfg(test)]")
            .next()
            .expect("startup.rs has a test module");
        for needle in ["PrivateKeySigner", "LocalSigner", "sign_transaction"] {
            assert!(
                !production.contains(needle),
                "startup.rs must not contain signer construction ({needle})"
            );
        }
        // WHI-860: gate is re-exported from send_path (not a hard-false body).
        assert!(
            production.contains("pub use crate::service::send_path::production_send_allowed"),
            "startup must re-export production_send_allowed from send_path"
        );
        assert!(
            !production.contains("pub fn production_send_allowed() -> bool {\n    false\n}"),
            "hard-false production_send_allowed body must not return"
        );
    }
}
