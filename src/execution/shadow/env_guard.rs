//! Shadow-mode signer env-var guard (WHI-549).
//!
//! Shadow mode structurally never sends (see
//! [`super::identity_source::ShadowIdentitySource`]'s fail-closed
//! `acquire_send_lease`), so it must never even *observe* signer material.
//! This checks, by variable name only, before any value is read, that no real
//! signing-key env var is present while shadow mode is requested — reusing
//! the exact `EnvSource`/`FORBIDDEN_ENV_VAR_NAMES` pattern already proven in
//! [`crate::execution::e2e::env_guard`] rather than inventing a second one.

use crate::execution::e2e::{EnvSource, FORBIDDEN_ENV_VAR_NAMES};

/// The `SHADOW_MODE` env-var convention already used by every
/// `*_monitor_executor_service` example (see
/// `examples/protocols/intent_service_support.rs::shadow_mode_enabled`).
pub const ENV_SHADOW_MODE: &str = "SHADOW_MODE";

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum ShadowEnvGuardError {
    #[error(
        "forbidden signer env var {0} is present while SHADOW_MODE is requested; unset it \
         before running in shadow mode"
    )]
    ForbiddenEnvVarPresent(&'static str),
}

/// Whether `env` requests shadow mode: [`ENV_SHADOW_MODE`] set to `"1"` or a
/// case-insensitive `"true"`.
///
/// The single definition of that convention. Every service's own
/// "am I in shadow mode?" check (`intent_service_support::shadow_mode_enabled`) routes
/// through here rather than re-reading the variable, so a service can never take the
/// signerless branch on a spelling [`guard_shadow_env`] did not recognize as shadow mode
/// — which would let it run with signer material still present in its environment.
pub fn shadow_mode_requested(env: &dyn EnvSource) -> bool {
    env.get(ENV_SHADOW_MODE)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Fails closed, by variable name only, if shadow mode is requested and any
/// forbidden signer env var is present in `env`. A no-op (never inspects the
/// forbidden names at all) when shadow mode is not requested, since the
/// production path is allowed to carry a real signer.
pub fn guard_shadow_env(env: &dyn EnvSource) -> Result<(), ShadowEnvGuardError> {
    if !shadow_mode_requested(env) {
        return Ok(());
    }
    for forbidden in FORBIDDEN_ENV_VAR_NAMES {
        if env.contains(forbidden) {
            return Err(ShadowEnvGuardError::ForbiddenEnvVarPresent(forbidden));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::e2e::MapEnvSource;
    use std::collections::BTreeMap;

    fn env_with(pairs: &[(&str, &str)]) -> MapEnvSource {
        let mut map = BTreeMap::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), (*value).to_string());
        }
        MapEnvSource(map)
    }

    #[test]
    fn passes_when_shadow_mode_is_not_requested_even_with_a_real_signer_present() {
        let env = env_with(&[("PRIVATE_KEY", "deadbeef")]);
        assert!(guard_shadow_env(&env).is_ok());
    }

    #[test]
    fn passes_when_shadow_mode_is_requested_and_no_forbidden_var_is_present() {
        let env = env_with(&[(ENV_SHADOW_MODE, "1")]);
        assert!(guard_shadow_env(&env).is_ok());
    }

    #[test]
    fn rejects_each_forbidden_signer_var_by_presence_only_when_shadow_mode_is_on() {
        for forbidden in FORBIDDEN_ENV_VAR_NAMES {
            // Presence alone must be rejected, even with a garbage/empty value.
            let env = env_with(&[(ENV_SHADOW_MODE, "1"), (forbidden, "")]);
            let error =
                guard_shadow_env(&env).expect_err("forbidden var presence must fail closed");
            assert_eq!(
                error,
                ShadowEnvGuardError::ForbiddenEnvVarPresent(forbidden)
            );
        }
    }

    #[test]
    fn recognizes_the_case_insensitive_true_spelling() {
        let env = env_with(&[(ENV_SHADOW_MODE, "true"), ("PRIVATE_KEY", "deadbeef")]);
        let error = guard_shadow_env(&env).expect_err("shadow mode must be recognized");
        assert_eq!(
            error,
            ShadowEnvGuardError::ForbiddenEnvVarPresent("PRIVATE_KEY")
        );
    }

    #[test]
    fn ignores_an_unrelated_shadow_mode_value() {
        let env = env_with(&[(ENV_SHADOW_MODE, "0"), ("PRIVATE_KEY", "deadbeef")]);
        assert!(guard_shadow_env(&env).is_ok());
    }
}
