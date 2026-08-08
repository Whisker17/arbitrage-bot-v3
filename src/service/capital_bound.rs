//! Optimal-input capital bounds (WHI-950 / G-3).
//!
//! Separates two different kinds of constraint that `send_path::enforce_inventory_caps`
//! encodes:
//!
//! 1. **Inventory precondition** — `executor_balance <= MAX_TOTAL_INVENTORY`. If
//!    violated, *nothing* is sendable this block; a smaller `amount_in` does not help.
//! 2. **Per-attempt input cap** — mode-dependent `min(...)` of balance / per-tx /
//!    approved strategy (or canary notional). Shadow never reads chain balance.
//!
//! # Balance-read strategy (chosen and fixed)
//!
//! **Strategy A** — one hash-pinned `balanceOf` per block, reused by discovery and
//! send via [`SnapshotBoundBalance`]. Adds one fixed RPC per head (timed as
//! [`crate::metrics::stage::BALANCE_READ`]). Strategy B (coarse-screen then re-opt)
//! was rejected: the inventory precondition and "no bail!-triggering optimal_input"
//! acceptances require the real balance *before* sizing, not after a positive hit.
//!
//! Reuses [`SnapshotBoundBalance`] / [`max_input_bound_for_snapshot`] — no second
//! snapshot-identity mechanism.

use alloy::primitives::U256;
use thiserror::Error;

use crate::state_space::{
    max_input_bound_for_snapshot, SnapshotBalanceError, SnapshotBoundBalance, SnapshotId,
};

/// Documented balance-read strategy for WHI-537 benchmark attribution.
pub const BALANCE_READ_STRATEGY: &str = "A";

/// Human-readable description of the chosen strategy (run_plan / evidence).
pub const BALANCE_READ_STRATEGY_DESCRIPTION: &str =
    "one hash-pinned balanceOf per block, reused by discovery and send (SnapshotBoundBalance)";

/// Env: approved strategy input ceiling for production mode (wei).
pub const ENV_APPROVED_STRATEGY_CAP_WMNT_WEI: &str = "APPROVED_STRATEGY_CAP_WMNT_WEI";
/// Env: approved canary notional for first-funded canary (wei).
pub const ENV_APPROVED_CANARY_NOTIONAL_WMNT_WEI: &str = "APPROVED_CANARY_NOTIONAL_WMNT_WEI";
/// Env: pre-declared assumed capital for shadow (wei). Never reads chain balance.
pub const ENV_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI: &str = "SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI";

/// Default shadow assumed capital when env is unset: 10 WMNT (18 decimals).
///
/// Sized large enough to exercise multi-peak optimal-input (G-1) while the
/// executor remains unfunded (WHI-547). Operators must write the actual value
/// into the evidence `run_plan` when it differs.
pub const DEFAULT_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI: u128 = 10_000_000_000_000_000_000;

/// Operating mode for capital bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapitalMode {
    /// Live production: `min(balance, per_tx, approved_strategy_cap)`.
    Production,
    /// Funded canary: `min(balance, per_tx, approved_canary_notional)`.
    Canary,
    /// Signerless shadow: pre-declared assumed cap; **never** reads chain balance.
    Shadow,
}

impl CapitalMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Production => "production",
            Self::Canary => "canary",
            Self::Shadow => "shadow",
        }
    }
}

/// Policy knobs for one run (loaded at startup; fixed for the process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapitalPolicy {
    pub mode: CapitalMode,
    /// `MAX_INPUT_PER_TX_WMNT_WEI` — per-attempt ceiling.
    pub max_input_per_tx_wmnt_wei: U256,
    /// `MAX_TOTAL_INVENTORY_WMNT_WEI` — inventory *precondition*, not an input cap.
    pub max_total_inventory_wmnt_wei: U256,
    /// Production: approved strategy cap. Canary: approved notional. Shadow: assumed cap.
    pub mode_cap_wmnt_wei: U256,
}

impl CapitalPolicy {
    /// Production policy from breaker caps + approved strategy ceiling.
    pub fn production(
        max_input_per_tx_wmnt_wei: u128,
        max_total_inventory_wmnt_wei: u128,
        approved_strategy_cap_wmnt_wei: u128,
    ) -> Self {
        Self {
            mode: CapitalMode::Production,
            max_input_per_tx_wmnt_wei: U256::from(max_input_per_tx_wmnt_wei),
            max_total_inventory_wmnt_wei: U256::from(max_total_inventory_wmnt_wei),
            mode_cap_wmnt_wei: U256::from(approved_strategy_cap_wmnt_wei),
        }
    }

    /// Canary policy from breaker caps + approved canary notional.
    pub fn canary(
        max_input_per_tx_wmnt_wei: u128,
        max_total_inventory_wmnt_wei: u128,
        approved_canary_notional_wmnt_wei: u128,
    ) -> Self {
        Self {
            mode: CapitalMode::Canary,
            max_input_per_tx_wmnt_wei: U256::from(max_input_per_tx_wmnt_wei),
            max_total_inventory_wmnt_wei: U256::from(max_total_inventory_wmnt_wei),
            mode_cap_wmnt_wei: U256::from(approved_canary_notional_wmnt_wei),
        }
    }

    /// Shadow policy: assumed capital only (inventory precondition N/A — no real balance).
    pub fn shadow(assumed_capital_cap_wmnt_wei: u128) -> Self {
        Self {
            mode: CapitalMode::Shadow,
            // Unused for shadow domain resolution, kept for evidence completeness.
            max_input_per_tx_wmnt_wei: U256::from(assumed_capital_cap_wmnt_wei),
            max_total_inventory_wmnt_wei: U256::MAX,
            mode_cap_wmnt_wei: U256::from(assumed_capital_cap_wmnt_wei),
        }
    }

    /// Assumed capital written into shadow evidence / run_plan (shadow mode only).
    pub fn assumed_capital_cap_for_evidence(&self) -> Option<U256> {
        match self.mode {
            CapitalMode::Shadow => Some(self.mode_cap_wmnt_wei),
            _ => None,
        }
    }

    /// True when this mode requires a hash-pinned chain balance (strategy A).
    pub fn requires_balance_read(&self) -> bool {
        matches!(self.mode, CapitalMode::Production | CapitalMode::Canary)
    }
}

/// Feasible domain for optimal-input search, or a block-level unsendable verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapitalDomain {
    /// Inventory precondition failed — emit **no** candidates this block.
    BlockUnsendable {
        executor_balance: U256,
        max_total_inventory_wmnt_wei: U256,
    },
    /// Search / size candidates with this `max_input` ceiling.
    Feasible {
        max_input: U256,
        /// Present for production/canary after strategy-A read; `None` in shadow.
        executor_balance: Option<U256>,
    },
}

impl CapitalDomain {
    pub fn is_unsendable(&self) -> bool {
        matches!(self, Self::BlockUnsendable { .. })
    }

    pub fn max_input(&self) -> Option<U256> {
        match self {
            Self::Feasible { max_input, .. } => Some(*max_input),
            Self::BlockUnsendable { .. } => None,
        }
    }

    pub fn executor_balance(&self) -> Option<U256> {
        match self {
            Self::Feasible {
                executor_balance, ..
            } => *executor_balance,
            Self::BlockUnsendable {
                executor_balance, ..
            } => Some(*executor_balance),
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CapitalBoundError {
    #[error("production/canary capital bound requires a SnapshotBoundBalance (strategy A)")]
    MissingBalance,
    #[error(transparent)]
    Snapshot(#[from] SnapshotBalanceError),
    #[error("mode_cap_wmnt_wei must be > 0")]
    ZeroModeCap,
    #[error("max_input_per_tx_wmnt_wei must be > 0 for {0} mode")]
    ZeroPerTxCap(&'static str),
}

/// Resolve the optimal-input feasible domain for one block.
///
/// * **Shadow** — ignores `balance`; returns `Feasible { max_input: assumed }`.
/// * **Production / Canary** — requires `balance`; checks inventory precondition
///   first; then `max_input = min(balance, per_tx, mode_cap)` with snapshot identity
///   validation via [`max_input_bound_for_snapshot`].
pub fn resolve_capital_domain(
    policy: &CapitalPolicy,
    pool_snapshot: SnapshotId,
    balance: Option<SnapshotBoundBalance>,
) -> Result<CapitalDomain, CapitalBoundError> {
    if policy.mode_cap_wmnt_wei.is_zero() {
        return Err(CapitalBoundError::ZeroModeCap);
    }

    match policy.mode {
        CapitalMode::Shadow => Ok(CapitalDomain::Feasible {
            max_input: policy.mode_cap_wmnt_wei,
            executor_balance: None,
        }),
        CapitalMode::Production | CapitalMode::Canary => {
            if policy.max_input_per_tx_wmnt_wei.is_zero() {
                return Err(CapitalBoundError::ZeroPerTxCap(policy.mode.as_str()));
            }
            let bound = balance.ok_or(CapitalBoundError::MissingBalance)?;
            // Inventory precondition — not an input cap.
            if bound.amount > policy.max_total_inventory_wmnt_wei {
                return Ok(CapitalDomain::BlockUnsendable {
                    executor_balance: bound.amount,
                    max_total_inventory_wmnt_wei: policy.max_total_inventory_wmnt_wei,
                });
            }
            // Cap: min(balance, per_tx, mode_cap) with snapshot-id check.
            let configured = policy
                .max_input_per_tx_wmnt_wei
                .min(policy.mode_cap_wmnt_wei);
            let max_input = max_input_bound_for_snapshot(pool_snapshot, bound, configured)?;
            Ok(CapitalDomain::Feasible {
                max_input,
                executor_balance: Some(bound.amount),
            })
        }
    }
}

/// True when `amount_in` is admissible under a **Feasible** domain via the real
/// [`crate::service::send_path::enforce_inventory_caps`] (not a parallel copy).
///
/// Shadow domains have no chain balance — send is not armed; any
/// `amount_in <= max_input` is treated as admissible for algorithm exercise.
pub fn amount_survives_send_caps(
    domain: &CapitalDomain,
    amount_in: U256,
    max_input_per_tx: U256,
    max_total_inventory: U256,
) -> bool {
    match domain {
        CapitalDomain::BlockUnsendable { .. } => false,
        CapitalDomain::Feasible {
            max_input,
            executor_balance,
        } => {
            if amount_in > *max_input {
                return false;
            }
            let Some(bal) = executor_balance else {
                return true;
            };
            crate::service::send_path::enforce_inventory_caps(
                amount_in,
                max_input_per_tx,
                *bal,
                max_total_inventory,
            )
            .is_ok()
        }
    }
}

/// Apply a resolved domain onto discovery knobs.
///
/// Returns `true` when discovery should run (`Feasible`); `false` when the block
/// is inventory-unsendable (caller must emit **no** candidates).
pub fn apply_capital_domain_to_discovery(
    domain: &CapitalDomain,
    discovery_max_input: &mut U256,
) -> bool {
    match domain {
        CapitalDomain::BlockUnsendable { .. } => false,
        CapitalDomain::Feasible { max_input, .. } => {
            *discovery_max_input = *max_input;
            true
        }
    }
}

/// Strategy-A balance pin with WHI-537 timing (`metrics::stage::BALANCE_READ`).
///
/// One hash-pinned `balanceOf` per head — callers must not re-read for send when
/// this pin is available.
pub async fn pin_executor_balance_strategy_a(
    runtime: &crate::service::send_path::SendRuntime,
    snapshot_id: SnapshotId,
) -> Result<SnapshotBoundBalance, eyre::Report> {
    use crate::metrics::{self, stage as metric_stage};
    use std::time::Instant;
    let started = Instant::now();
    let bound = runtime
        .executor_wmnt_balance_bound(snapshot_id)
        .await
        .map_err(|e| eyre::eyre!("strategy-A executor WMNT balanceOf: {e}"))?;
    metrics::record_pipeline_stage(metric_stage::BALANCE_READ, "merged", started.elapsed());
    Ok(bound)
}

/// Load shadow assumed capital from env or the documented default.
///
/// Fails closed when the env var is set but not a valid `u128`.
pub fn shadow_assumed_capital_from_env() -> Result<u128, String> {
    match std::env::var(ENV_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI) {
        Ok(raw) => raw.parse::<u128>().map_err(|e| {
            format!("{ENV_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI}={raw:?} is not a valid u128: {e}")
        }),
        Err(_) => Ok(DEFAULT_SHADOW_ASSUMED_CAPITAL_CAP_WMNT_WEI),
    }
}

/// Load approved strategy cap; `Ok(None)` if unset; Err if set but invalid.
pub fn approved_strategy_cap_from_env() -> Result<Option<u128>, String> {
    parse_optional_u128_env(ENV_APPROVED_STRATEGY_CAP_WMNT_WEI)
}

/// Load approved canary notional; `Ok(None)` if unset; Err if set but invalid.
pub fn approved_canary_notional_from_env() -> Result<Option<u128>, String> {
    parse_optional_u128_env(ENV_APPROVED_CANARY_NOTIONAL_WMNT_WEI)
}

fn parse_optional_u128_env(key: &str) -> Result<Option<u128>, String> {
    match std::env::var(key) {
        Ok(raw) => {
            let v = raw
                .parse::<u128>()
                .map_err(|e| format!("{key}={raw:?} is not a valid u128: {e}"))?;
            Ok(Some(v))
        }
        Err(_) => Ok(None),
    }
}

/// Evidence fields for shadow run_plan / STATUS (serialisable shape).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CapitalEvidence {
    pub capital_mode: &'static str,
    pub balance_read_strategy: &'static str,
    pub balance_read_strategy_description: &'static str,
    /// Shadow only: assumed capital cap used for optimal-input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assumed_capital_cap_wmnt_wei: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_input_per_tx_wmnt_wei: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_total_inventory_wmnt_wei: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode_cap_wmnt_wei: Option<String>,
}

impl CapitalEvidence {
    pub fn from_policy(policy: &CapitalPolicy) -> Self {
        let u = |v: U256| v.to_string();
        match policy.mode {
            CapitalMode::Shadow => Self {
                capital_mode: policy.mode.as_str(),
                // Strategy A is the process-wide choice; shadow skips the RPC and
                // uses the pre-declared assumed cap instead of a chain balance.
                balance_read_strategy: BALANCE_READ_STRATEGY,
                balance_read_strategy_description:
                    "strategy A process-wide; shadow mode does not read chain balance \
                     (uses assumed_capital_cap_wmnt_wei)",
                assumed_capital_cap_wmnt_wei: Some(u(policy.mode_cap_wmnt_wei)),
                max_input_per_tx_wmnt_wei: None,
                max_total_inventory_wmnt_wei: None,
                mode_cap_wmnt_wei: Some(u(policy.mode_cap_wmnt_wei)),
            },
            CapitalMode::Production | CapitalMode::Canary => Self {
                capital_mode: policy.mode.as_str(),
                balance_read_strategy: BALANCE_READ_STRATEGY,
                balance_read_strategy_description: BALANCE_READ_STRATEGY_DESCRIPTION,
                assumed_capital_cap_wmnt_wei: None,
                max_input_per_tx_wmnt_wei: Some(u(policy.max_input_per_tx_wmnt_wei)),
                max_total_inventory_wmnt_wei: Some(u(policy.max_total_inventory_wmnt_wei)),
                mode_cap_wmnt_wei: Some(u(policy.mode_cap_wmnt_wei)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    fn snap(n: u64) -> SnapshotId {
        SnapshotId::new(5000, n, B256::ZERO)
    }

    fn bal(id: SnapshotId, amount: u64) -> SnapshotBoundBalance {
        SnapshotBoundBalance::new(id, U256::from(amount))
    }

    // --- precondition ---

    #[test]
    fn precondition_balance_over_total_inventory_is_unsendable() {
        let policy = CapitalPolicy::production(1_000, 5_000, 10_000);
        let id = snap(1);
        // balance 6_000 > max_total 5_000
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 6_000))).unwrap();
        assert!(domain.is_unsendable());
        assert_eq!(domain.max_input(), None);
        match domain {
            CapitalDomain::BlockUnsendable {
                executor_balance,
                max_total_inventory_wmnt_wei,
            } => {
                assert_eq!(executor_balance, U256::from(6_000u64));
                assert_eq!(max_total_inventory_wmnt_wei, U256::from(5_000u64));
            }
            other => panic!("expected BlockUnsendable, got {other:?}"),
        }
        // No "just use a smaller input" — even tiny amounts fail survive check.
        assert!(!amount_survives_send_caps(
            &domain,
            U256::from(1u64),
            U256::from(1_000u64),
            U256::from(5_000u64),
        ));
    }

    #[test]
    fn precondition_balance_equal_total_inventory_is_still_feasible() {
        // `>` is the reject in send_path; equal is allowed.
        let policy = CapitalPolicy::production(1_000, 5_000, 10_000);
        let id = snap(2);
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 5_000))).unwrap();
        assert!(!domain.is_unsendable());
        assert_eq!(domain.max_input(), Some(U256::from(1_000u64))); // min(5000, 1000, 10000)
    }

    // --- production ---

    #[test]
    fn production_cap_is_min_of_balance_per_tx_and_strategy() {
        // strategy binds: min(80, 100, 50) = 50
        let strategy_tight = CapitalPolicy::production(100, 10_000, 50);
        let id = snap(3);
        let domain =
            resolve_capital_domain(&strategy_tight, id, Some(bal(id, 80))).unwrap();
        assert_eq!(domain.max_input(), Some(U256::from(50u64)));
        // balance binds: min(30, 100, 50) = 30
        let domain =
            resolve_capital_domain(&strategy_tight, id, Some(bal(id, 30))).unwrap();
        assert_eq!(domain.max_input(), Some(U256::from(30u64)));
        // per_tx binds uniquely: min(500, 40, 200) = 40
        let per_tx_tight = CapitalPolicy::production(40, 10_000, 200);
        let domain =
            resolve_capital_domain(&per_tx_tight, id, Some(bal(id, 500))).unwrap();
        assert_eq!(domain.max_input(), Some(U256::from(40u64)));
    }

    #[test]
    fn production_optimal_input_at_cap_never_triggers_send_bail() {
        let per_tx = U256::from(100u64);
        let total = U256::from(10_000u64);
        let policy = CapitalPolicy::production(100, 10_000, 80);
        let id = snap(4);
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 500))).unwrap();
        let max = domain.max_input().expect("feasible");
        // Cap itself and any smaller amount must survive send caps.
        assert!(amount_survives_send_caps(&domain, max, per_tx, total));
        assert!(amount_survives_send_caps(
            &domain,
            max.saturating_sub(U256::from(1u64)),
            per_tx,
            total
        ));
        // Over cap would not be emitted by discovery (max_input binds search).
        assert!(!amount_survives_send_caps(
            &domain,
            max + U256::from(1u64),
            per_tx,
            total
        ));
    }

    #[test]
    fn production_requires_balance() {
        let policy = CapitalPolicy::production(100, 10_000, 50);
        let err = resolve_capital_domain(&policy, snap(5), None).unwrap_err();
        assert_eq!(err, CapitalBoundError::MissingBalance);
    }

    // --- canary ---

    #[test]
    fn canary_cap_uses_approved_notional_not_strategy() {
        let policy = CapitalPolicy::canary(
            1_000, // per_tx
            50_000,
            25, // canary notional (tight)
        );
        let id = snap(6);
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 10_000))).unwrap();
        assert_eq!(domain.max_input(), Some(U256::from(25u64)));
    }

    #[test]
    fn canary_precondition_still_applies() {
        let policy = CapitalPolicy::canary(1_000, 5_000, 25);
        let id = snap(7);
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 9_000))).unwrap();
        assert!(domain.is_unsendable());
    }

    // --- shadow ---

    #[test]
    fn shadow_ignores_zero_chain_balance_and_uses_assumed_cap() {
        let policy = CapitalPolicy::shadow(10_000);
        let id = snap(8);
        // Even if someone passed a zero balance, shadow must not consult it.
        let domain = resolve_capital_domain(&policy, id, Some(bal(id, 0))).unwrap();
        assert!(!domain.is_unsendable());
        assert_eq!(domain.max_input(), Some(U256::from(10_000u64)));
        assert_eq!(domain.executor_balance(), None);
        // No balance at all is fine.
        let domain = resolve_capital_domain(&policy, id, None).unwrap();
        assert_eq!(domain.max_input(), Some(U256::from(10_000u64)));
    }

    #[test]
    fn shadow_evidence_records_assumed_cap() {
        let policy = CapitalPolicy::shadow(42);
        assert_eq!(
            policy.assumed_capital_cap_for_evidence(),
            Some(U256::from(42u64))
        );
        let ev = CapitalEvidence::from_policy(&policy);
        assert_eq!(ev.capital_mode, "shadow");
        assert_eq!(ev.balance_read_strategy, BALANCE_READ_STRATEGY);
        assert_eq!(
            ev.assumed_capital_cap_wmnt_wei.as_deref(),
            Some("42")
        );
        // JSON shape for run_plan merge.
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["assumed_capital_cap_wmnt_wei"], "42");
        assert_eq!(v["balance_read_strategy"], "A");
    }

    #[test]
    fn production_evidence_has_no_assumed_cap() {
        let policy = CapitalPolicy::production(1, 2, 3);
        assert!(policy.assumed_capital_cap_for_evidence().is_none());
        let ev = CapitalEvidence::from_policy(&policy);
        assert!(ev.assumed_capital_cap_wmnt_wei.is_none());
        assert_eq!(ev.capital_mode, "production");
    }

    #[test]
    fn mismatched_snapshot_id_fails_closed() {
        let policy = CapitalPolicy::production(100, 10_000, 50);
        let pool = snap(10);
        let bal_id = snap(11);
        let err = resolve_capital_domain(&policy, pool, Some(bal(bal_id, 80))).unwrap_err();
        assert!(matches!(err, CapitalBoundError::Snapshot(_)));
    }

    #[test]
    fn strategy_a_is_documented() {
        assert_eq!(BALANCE_READ_STRATEGY, "A");
        assert!(CapitalPolicy::production(1, 2, 3).requires_balance_read());
        assert!(CapitalPolicy::canary(1, 2, 3).requires_balance_read());
        assert!(!CapitalPolicy::shadow(1).requires_balance_read());
        // WHI-537 report taxonomy must list the balance_read stage independently.
        assert!(crate::metrics::stage::WHI537_REPORT_STAGES
            .contains(&crate::metrics::stage::BALANCE_READ));
    }

    #[test]
    fn apply_domain_skips_discovery_when_unsendable() {
        let mut max = U256::from(999u64);
        let unsendable = CapitalDomain::BlockUnsendable {
            executor_balance: U256::from(10u64),
            max_total_inventory_wmnt_wei: U256::from(1u64),
        };
        assert!(!apply_capital_domain_to_discovery(&unsendable, &mut max));
        assert_eq!(max, U256::from(999u64));
        let feasible = CapitalDomain::Feasible {
            max_input: U256::from(42u64),
            executor_balance: Some(U256::from(100u64)),
        };
        assert!(apply_capital_domain_to_discovery(&feasible, &mut max));
        assert_eq!(max, U256::from(42u64));
    }

    #[test]
    fn zero_mode_cap_rejected() {
        let mut policy = CapitalPolicy::shadow(1);
        policy.mode_cap_wmnt_wei = U256::ZERO;
        assert_eq!(
            resolve_capital_domain(&policy, snap(1), None).unwrap_err(),
            CapitalBoundError::ZeroModeCap
        );
    }
}
