//! Principal protection for resized arbitrage executions (WHI-503 / M0-3).
//!
//! When executor WMNT balance is below the optimized input, the path is re-simulated
//! at the reduced size. On-chain safety uses an explicit positive `minProfit` on the
//! hardened executor ABI (`balanceAfter >= balanceBefore + minProfit`), not a
//! protocol-specific `amountsOut[last]` floor.

use alloy::primitives::U256;
use thiserror::Error;

/// Planned execution amounts after balance resize + profitability checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalPlan {
    /// Input actually used (may be smaller than the unconstrained optimum).
    pub amount_in: U256,
    /// Explicit on-chain `minProfit` (WMNT balance increase). Always strictly positive.
    pub min_profit: U256,
    /// Simulated final WMNT out at `amount_in`.
    pub simulated_output: U256,
    /// `simulated_output - amount_in` (gross inventory profit).
    pub gross_profit: U256,
    /// Gross profit after deducting estimated gas cost (in MNT wei).
    pub net_profit: U256,
    /// True when `amount_in` was reduced to fit executor balance.
    pub was_resized: bool,
}

/// Default gas safety margin used by Moe discovery (`is_profitable_after_gas(..., 1.2)`).
pub const DEFAULT_GAS_SAFETY_MARGIN: f64 = 1.2;

/// Failures that must abort send rather than emit unprotected calldata.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PrincipalProtectionError {
    #[error("executor has zero WMNT balance")]
    ZeroBalance,
    #[error("path simulation failed at adjusted input")]
    SimulationFailed,
    #[error("simulation at adjusted input is not profitable (output <= input)")]
    NonPositiveGrossProfit,
    #[error("estimated gas cost exceeds simulated gross profit")]
    GasExceedsProfit,
    #[error("net profit {net} is below configured minimum {min}")]
    BelowMinNetProfit { net: U256, min: U256 },
    #[error("gross profit fails gas safety margin (required {required}, have {have})")]
    FailsGasSafetyMargin { required: U256, have: U256 },
    #[error("slippage haircut left a non-positive minProfit")]
    NonPositiveMinProfit,
}

/// Cap desired input at executor balance. Returns `(amount_in, was_resized)`.
pub fn adjust_input_to_balance(
    desired_input: U256,
    executor_balance: U256,
) -> Result<(U256, bool), PrincipalProtectionError> {
    if executor_balance.is_zero() {
        return Err(PrincipalProtectionError::ZeroBalance);
    }
    if executor_balance < desired_input {
        Ok((executor_balance, true))
    } else {
        Ok((desired_input, false))
    }
}

/// Apply a basis-point haircut: `amount * (10_000 - bps) / 10_000`.
pub fn apply_slippage_bps(amount: U256, slippage_bps: u32) -> U256 {
    if amount.is_zero() || slippage_bps == 0 {
        return amount;
    }
    let bps = U256::from(slippage_bps.min(10_000));
    let keep = U256::from(10_000u64).saturating_sub(bps);
    amount.saturating_mul(keep) / U256::from(10_000u64)
}

/// Validate a (possibly resized) simulation and build an explicit positive `minProfit`.
///
/// Callers must pass `simulated_output` from a **complete path simulation at `amount_in`**
/// (re-run after any resize). This function does not encode principal safety via hop outs.
///
/// # Abort conditions
/// - gross profit non-positive
/// - gas cost exceeds gross profit
/// - net profit below `min_net_profit`
/// - gross profit fails `gas_safety_margin` (e.g. 1.2 → 20% above gas)
/// - slippage haircut zeroes `minProfit`
pub fn build_principal_plan(
    amount_in: U256,
    simulated_output: U256,
    gas_cost_wei: U256,
    min_net_profit: U256,
    slippage_bps: u32,
    gas_safety_margin: f64,
    was_resized: bool,
) -> Result<PrincipalPlan, PrincipalProtectionError> {
    let gross_profit = simulated_output
        .checked_sub(amount_in)
        .filter(|p| !p.is_zero())
        .ok_or(PrincipalProtectionError::NonPositiveGrossProfit)?;

    let net_profit = gross_profit
        .checked_sub(gas_cost_wei)
        .ok_or(PrincipalProtectionError::GasExceedsProfit)?;

    if net_profit < min_net_profit {
        return Err(PrincipalProtectionError::BelowMinNetProfit {
            net: net_profit,
            min: min_net_profit,
        });
    }

    if gas_safety_margin > 1.0 && !gas_cost_wei.is_zero() {
        // margin * 1000 as integer milles to avoid float→U256 round-trip surprises
        let margin_millis = (gas_safety_margin * 1000.0).round() as u128;
        let required = gas_cost_wei
            .saturating_mul(U256::from(margin_millis))
            / U256::from(1000u64);
        if gross_profit < required {
            return Err(PrincipalProtectionError::FailsGasSafetyMargin {
                required,
                have: gross_profit,
            });
        }
    }

    // On-chain floor: expected gross inventory increase after slippage haircut.
    // Must stay strictly positive — principal protection is independent of hop protocol.
    let min_profit = apply_slippage_bps(gross_profit, slippage_bps);
    if min_profit.is_zero() {
        return Err(PrincipalProtectionError::NonPositiveMinProfit);
    }

    Ok(PrincipalPlan {
        amount_in,
        min_profit,
        simulated_output,
        gross_profit,
        net_profit,
        was_resized,
    })
}

/// Convenience: resize to balance, then build a plan from a re-simulation at the adjusted size.
///
/// `simulate_at` receives the adjusted `amount_in` and must return the final path output.
pub fn plan_resized_execution<F, E>(
    desired_input: U256,
    executor_balance: U256,
    gas_cost_wei: U256,
    min_net_profit: U256,
    slippage_bps: u32,
    gas_safety_margin: f64,
    simulate_at: F,
) -> Result<PrincipalPlan, PrincipalProtectionError>
where
    F: FnOnce(U256) -> Result<U256, E>,
{
    let (amount_in, was_resized) = adjust_input_to_balance(desired_input, executor_balance)?;
    let simulated_output =
        simulate_at(amount_in).map_err(|_| PrincipalProtectionError::SimulationFailed)?;
    build_principal_plan(
        amount_in,
        simulated_output,
        gas_cost_wei,
        min_net_profit,
        slippage_bps,
        gas_safety_margin,
        was_resized,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(v: u128) -> U256 {
        U256::from(v)
    }

    #[test]
    fn adjust_keeps_input_when_balance_sufficient() {
        let (amount, resized) = adjust_input_to_balance(u(100), u(150)).unwrap();
        assert_eq!(amount, u(100));
        assert!(!resized);
    }

    #[test]
    fn adjust_shrinks_to_balance_when_insufficient() {
        let (amount, resized) = adjust_input_to_balance(u(100), u(40)).unwrap();
        assert_eq!(amount, u(40));
        assert!(resized);
    }

    #[test]
    fn adjust_rejects_zero_balance() {
        assert_eq!(
            adjust_input_to_balance(u(100), U256::ZERO),
            Err(PrincipalProtectionError::ZeroBalance)
        );
    }

    #[test]
    fn plan_builds_positive_min_profit_without_resize() {
        // input 100, out 150 → gross 50; gas 10 → net 40; min_net 5; slippage 0
        let plan = build_principal_plan(u(100), u(150), u(10), u(5), 0, 1.0, false).unwrap();
        assert_eq!(plan.amount_in, u(100));
        assert_eq!(plan.gross_profit, u(50));
        assert_eq!(plan.net_profit, u(40));
        assert_eq!(plan.min_profit, u(50));
        assert!(!plan.was_resized);
        assert!(plan.min_profit > U256::ZERO);
    }

    #[test]
    fn plan_applies_slippage_haircut_to_min_profit() {
        // gross 10_000, 30 bps → keep 9970
        let plan =
            build_principal_plan(u(100_000), u(110_000), u(100), u(1), 30, 1.0, false).unwrap();
        assert_eq!(plan.min_profit, u(9_970));
    }

    #[test]
    fn plan_aborts_when_resized_simulation_not_profitable() {
        // output == input
        let err = build_principal_plan(u(100), u(100), u(1), u(0), 0, 1.0, true).unwrap_err();
        assert_eq!(err, PrincipalProtectionError::NonPositiveGrossProfit);
    }

    #[test]
    fn plan_aborts_when_gas_exceeds_gross() {
        let err = build_principal_plan(u(100), u(120), u(30), u(0), 0, 1.0, true).unwrap_err();
        assert_eq!(err, PrincipalProtectionError::GasExceedsProfit);
    }

    #[test]
    fn plan_aborts_when_net_below_configured_minimum() {
        // gross 50, gas 10, net 40 < min 100
        let err = build_principal_plan(u(100), u(150), u(10), u(100), 0, 1.0, true).unwrap_err();
        assert_eq!(
            err,
            PrincipalProtectionError::BelowMinNetProfit {
                net: u(40),
                min: u(100)
            }
        );
    }

    #[test]
    fn plan_aborts_when_gas_safety_margin_fails() {
        // gas 100, margin 1.2 → need gross >= 120; have 110
        let err = build_principal_plan(u(1000), u(1110), u(100), u(0), 0, 1.2, true).unwrap_err();
        match err {
            PrincipalProtectionError::FailsGasSafetyMargin { required, have } => {
                assert_eq!(required, u(120));
                assert_eq!(have, u(110));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn plan_resized_execution_re_simulates_at_balance() {
        let desired = u(1_000);
        let balance = u(400);
        // Linear mock: out = in * 11 / 10 (10% gross)
        let plan = plan_resized_execution(
            desired,
            balance,
            u(5),  // gas
            u(1),  // min net
            0,     // no slippage
            1.0,   // no safety margin
            |amount_in| {
                assert_eq!(amount_in, balance, "must re-sim at resized input");
                Ok::<U256, ()>(amount_in * u(11) / u(10))
            },
        )
        .unwrap();

        assert!(plan.was_resized);
        assert_eq!(plan.amount_in, balance);
        assert_eq!(plan.gross_profit, u(40));
        assert_eq!(plan.min_profit, u(40));
        assert!(plan.min_profit > U256::ZERO);
    }

    #[test]
    fn plan_resized_execution_aborts_unprofitable_resize() {
        let err = plan_resized_execution(
            u(1_000),
            u(400),
            u(1),
            u(0),
            0,
            1.0,
            |amount_in| {
                // Lossy at smaller size
                Ok::<U256, ()>(amount_in.saturating_sub(u(1)))
            },
        )
        .unwrap_err();
        assert_eq!(err, PrincipalProtectionError::NonPositiveGrossProfit);
    }

    #[test]
    fn plan_resized_execution_aborts_when_simulation_errors() {
        let err = plan_resized_execution(
            u(1_000),
            u(400),
            u(1),
            u(0),
            0,
            1.0,
            |_amount_in| Err::<U256, ()>(()),
        )
        .unwrap_err();
        assert_eq!(err, PrincipalProtectionError::SimulationFailed);
    }

    #[test]
    fn min_profit_is_independent_of_amounts_out() {
        // Documents the M0-3 contract: principal floor lives in minProfit, not hop outs.
        let plan = build_principal_plan(u(50), u(80), u(5), u(1), 0, 1.0, false).unwrap();
        // amountsOut for Moe stay all-zero; minProfit carries the floor.
        let amounts_out_moe = [U256::ZERO, U256::ZERO];
        let legacy_style_min = amounts_out_moe
            .last()
            .copied()
            .unwrap_or_default()
            .saturating_sub(plan.amount_in);
        assert_eq!(legacy_style_min, U256::ZERO, "legacy all-zero amountsOut is unsafe");
        assert!(plan.min_profit > U256::ZERO, "explicit minProfit must protect principal");
        assert_eq!(plan.min_profit, u(30));
    }
}
