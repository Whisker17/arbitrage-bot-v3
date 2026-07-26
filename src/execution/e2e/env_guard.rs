//! Startup env validation for the E2E capability layer (WHI-555).
//!
//! Reads only the documented `MANTLE_SEPOLIA_E2E_*` namespace. Refuses to
//! start if any forbidden production/legacy credential variable name is
//! *present* in the environment (checked by name only — values are never
//! read for forbidden names), so an operator's shell carrying leftover
//! mainnet/legacy credentials can't silently leak into the E2E signer.
//!
//! The RPC URL is validated for presence and a plausible scheme, then
//! discarded immediately: this module never retains it, so it can never
//! leak into a later log line, error, or piece of evidence. Establishing the
//! live provider connection is the caller's responsibility (mirroring how
//! [`super::super::Executor`] takes an already-connected provider rather than
//! building one itself); only the caller ever sees the URL.

use std::collections::BTreeMap;
use std::str::FromStr;

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;

use super::error::E2eCapabilityError;

pub const ENV_E2E_RPC_URL: &str = "MANTLE_SEPOLIA_E2E_RPC_URL";
pub const ENV_E2E_PRIVATE_KEY: &str = "MANTLE_SEPOLIA_E2E_PRIVATE_KEY";
pub const ENV_E2E_EXECUTOR_ADDRESS: &str = "MANTLE_SEPOLIA_E2E_EXECUTOR_ADDRESS";

/// Production/legacy credential variable names that must be absent from the
/// process environment before the E2E signer is ever constructed. Checked by
/// name presence only (`EnvSource::contains`) — values are never read.
pub const FORBIDDEN_ENV_VAR_NAMES: &[&str] = &[
    "MANTLE_SEPOLIA_PRIVATE_KEY",
    "MANTLE_MAINNET_PRIVATE_KEY",
    "MANTLE_PRIVATE_KEY",
    "PRIVATE_KEY",
];

/// Committed denylist of production signer addresses that must never be used
/// as the E2E signer. Empty until a production executor signer exists
/// (WHI-547/548); the check and its tests are exercised with an injected
/// denylist so the mechanism is proven correct independent of that.
pub const PRODUCTION_SIGNER_DENYLIST: &[Address] = &[];

/// Abstraction over "the process environment" so startup validation is
/// pure/testable without mutating real env vars from parallel tests.
pub trait EnvSource {
    fn contains(&self, key: &str) -> bool;
    fn get(&self, key: &str) -> Option<String>;
}

/// Real process environment, used by all non-test callers.
pub struct ProcessEnvSource;

impl EnvSource for ProcessEnvSource {
    fn contains(&self, key: &str) -> bool {
        std::env::var_os(key).is_some()
    }

    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// In-memory environment snapshot for tests.
#[derive(Debug, Default, Clone)]
pub struct MapEnvSource(pub BTreeMap<String, String>);

impl EnvSource for MapEnvSource {
    fn contains(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// Validated E2E startup material: an E2E signer whose address has cleared
/// the production denylist, and the configured executor address. Fields are
/// visible only within `crate::execution::e2e` (not merely crate-private) —
/// only [`super::capability`] can consume this to build the module-private
/// wallet; no code outside this module tree, anywhere in the crate, can
/// reach the signer through this type.
#[derive(Debug)]
pub struct ValidatedE2eStartup {
    pub(in crate::execution::e2e) signer: PrivateKeySigner,
    pub(in crate::execution::e2e) executor_address: Address,
}

impl ValidatedE2eStartup {
    pub fn signer_address(&self) -> Address {
        self.signer.address()
    }

    pub fn executor_address(&self) -> Address {
        self.executor_address
    }
}

fn plausible_rpc_url(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && (value.starts_with("https://")
            || value.starts_with("http://")
            || value.starts_with("wss://")
            || value.starts_with("ws://"))
}

/// Validate startup against the committed [`PRODUCTION_SIGNER_DENYLIST`]. This
/// is the entry point real callers should use — the denylist is never
/// optional here, so a caller cannot accidentally validate startup without
/// it (unlike [`validate_e2e_startup_with_denylist`], which exists so tests
/// can inject a synthetic denylist instead of depending on the committed
/// one).
pub fn validate_e2e_startup(
    env: &dyn EnvSource,
) -> Result<ValidatedE2eStartup, E2eCapabilityError> {
    validate_e2e_startup_with_denylist(env, PRODUCTION_SIGNER_DENYLIST)
}

/// Validate startup: namespace-only env reads, forbidden-name presence check,
/// E2E var parsing, and signer-vs-denylist comparison. All of this happens
/// before any signer, permit, or network connection is constructed.
pub fn validate_e2e_startup_with_denylist(
    env: &dyn EnvSource,
    denylist: &[Address],
) -> Result<ValidatedE2eStartup, E2eCapabilityError> {
    for forbidden in FORBIDDEN_ENV_VAR_NAMES {
        if env.contains(forbidden) {
            return Err(E2eCapabilityError::ForbiddenEnvVarPresent(forbidden));
        }
    }

    let rpc_url = env
        .get(ENV_E2E_RPC_URL)
        .ok_or(E2eCapabilityError::MissingEnvVar(ENV_E2E_RPC_URL))?;
    if !plausible_rpc_url(&rpc_url) {
        return Err(E2eCapabilityError::InvalidEnvVar(
            ENV_E2E_RPC_URL,
            "must start with http(s):// or ws(s)://".to_string(),
        ));
    }
    // `rpc_url` is validated for shape only and never retained past this
    // point — it goes out of scope here, so this module can never leak it
    // into a later log line, error, or piece of evidence.

    let private_key = env
        .get(ENV_E2E_PRIVATE_KEY)
        .ok_or(E2eCapabilityError::MissingEnvVar(ENV_E2E_PRIVATE_KEY))?;
    let signer = PrivateKeySigner::from_str(private_key.trim()).map_err(|_| {
        E2eCapabilityError::InvalidEnvVar(
            ENV_E2E_PRIVATE_KEY,
            "not a valid secp256k1 private key".to_string(),
        )
    })?;
    // `private_key` is consumed into `signer` above and never retained as a
    // string past this point.

    let executor_address_raw = env
        .get(ENV_E2E_EXECUTOR_ADDRESS)
        .ok_or(E2eCapabilityError::MissingEnvVar(ENV_E2E_EXECUTOR_ADDRESS))?;
    let executor_address = Address::from_str(executor_address_raw.trim()).map_err(|_| {
        E2eCapabilityError::InvalidEnvVar(
            ENV_E2E_EXECUTOR_ADDRESS,
            "not a valid address".to_string(),
        )
    })?;

    let signer_address = signer.address();
    if denylist.contains(&signer_address) {
        return Err(E2eCapabilityError::DenylistedSigner);
    }

    Ok(ValidatedE2eStartup {
        signer,
        executor_address,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor_address() -> Address {
        Address::repeat_byte(0xE2)
    }

    fn base_env() -> MapEnvSource {
        let mut map = BTreeMap::new();
        map.insert(
            ENV_E2E_RPC_URL.to_string(),
            "https://rpc.sepolia.mantle.xyz".to_string(),
        );
        map.insert(
            ENV_E2E_PRIVATE_KEY.to_string(),
            "0000000000000000000000000000000000000000000000000000000000000001".to_string(),
        );
        // `Address::to_string()` always produces valid EIP-55 checksummed hex,
        // unlike a hand-typed literal (which can trip `Address::from_str`'s
        // checksum validation if its casing happens not to match the checksum).
        map.insert(
            ENV_E2E_EXECUTOR_ADDRESS.to_string(),
            test_executor_address().to_string(),
        );
        MapEnvSource(map)
    }

    #[test]
    fn validates_a_well_formed_e2e_only_environment() {
        let env = base_env();
        let startup =
            validate_e2e_startup_with_denylist(&env, &[]).expect("well-formed env must validate");
        assert_eq!(startup.executor_address(), test_executor_address());
    }

    #[test]
    fn rejects_each_forbidden_legacy_var_by_presence_only() {
        for forbidden in FORBIDDEN_ENV_VAR_NAMES {
            let mut env = base_env();
            // Presence alone must be rejected, even with a garbage/empty value.
            env.0.insert((*forbidden).to_string(), String::new());
            let err = validate_e2e_startup_with_denylist(&env, &[])
                .expect_err("forbidden var presence must fail closed");
            assert_eq!(err, E2eCapabilityError::ForbiddenEnvVarPresent(forbidden));
        }
    }

    #[test]
    fn missing_required_var_is_reported_by_name() {
        let mut env = base_env();
        env.0.remove(ENV_E2E_PRIVATE_KEY);
        let err = validate_e2e_startup_with_denylist(&env, &[]).expect_err("missing var must fail");
        assert_eq!(err, E2eCapabilityError::MissingEnvVar(ENV_E2E_PRIVATE_KEY));
    }

    #[test]
    fn rejects_implausible_rpc_url_scheme() {
        let mut env = base_env();
        env.0
            .insert(ENV_E2E_RPC_URL.to_string(), "not-a-url".to_string());
        let err = validate_e2e_startup_with_denylist(&env, &[]).expect_err("bad scheme must fail");
        assert!(matches!(
            err,
            E2eCapabilityError::InvalidEnvVar(ENV_E2E_RPC_URL, _)
        ));
    }

    #[test]
    fn rejects_denylisted_signer_before_any_permit_could_be_minted() {
        let env = base_env();
        let signer = PrivateKeySigner::from_str(
            "0000000000000000000000000000000000000000000000000000000000000001",
        )
        .unwrap();
        let denylist = [signer.address()];
        let err = validate_e2e_startup_with_denylist(&env, &denylist)
            .expect_err("denylisted signer must fail");
        assert_eq!(err, E2eCapabilityError::DenylistedSigner);
    }

    #[test]
    fn error_display_never_contains_the_private_key_value() {
        let mut env = base_env();
        env.0.insert(
            ENV_E2E_PRIVATE_KEY.to_string(),
            "not-a-valid-hex-key".to_string(),
        );
        let err = validate_e2e_startup_with_denylist(&env, &[]).expect_err("garbage key must fail");
        let rendered = err.to_string();
        assert!(!rendered.contains("not-a-valid-hex-key"));
    }
}
