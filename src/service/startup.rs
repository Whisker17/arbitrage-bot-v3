//! NoSend / signerless startup helpers (WHI-727).
//!
//! Near-pure relocation of the already-generic logic from
//! `examples/protocols/intent_service_support.rs`. Callers must still invoke
//! [`crate::execution::guard_shadow_env`] at process start.
//!
//! **Invariant:** [`production_send_allowed`] is hard-coded `false`. Do not add
//! any code path that could flip this without an explicit WHI issue.

use crate::execution::{
    BlockFeeContextCache, ExecutionContext, Executor, ExecutorConfig, RuntimeGasProfile,
    RuntimeProfileConfig, ShadowConfigPaths, ShadowExecutionContext, ShadowLedgerSetup,
    ShadowOverrideTarget, ShadowPinnedConfig,
};
use alloy::primitives::Address;
use alloy::providers::Provider;
use eyre::{eyre, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Production broadcast remains fail-closed (WHI-519 / WHI-526).
///
/// Hard-coded `false` — do not add any env var or flag that flips this.
pub fn production_send_allowed() -> bool {
    false
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

    #[test]
    fn production_send_allowed_is_hard_false() {
        assert!(!production_send_allowed());
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
        // itself.
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
        // production_send_allowed must stay a literal false.
        assert!(production.contains("pub fn production_send_allowed() -> bool {\n    false\n}"));
    }
}
