//! Static eligibility + armed candidate selection (WHI-951 / G-4).
//!
//! A block must not waste its only execution attempt on a candidate that cannot
//! be sent (cross-protocol, over per-tx cap, missing gas profile, …) while a
//! sendable candidate sits behind it. Static filters run **before** ranking is
//! consumed for the attempt; dynamic preflight failures may advance to the next
//! eligible candidate only inside a strict wall-clock budget.
//!
//! Capability boundary is explicit (`StaticIneligibility`), not an opaque `Err`
//! from the armed send path. Mixed-route **send** remains out of scope for the
//! first funded canary — see `docs/runbooks/WHI-951-pure-route-canary.md`.

use crate::execution::RouteKey;
use crate::service::discovery::DiscoveredOpportunity;
use alloy::primitives::U256;
use std::time::{Duration, Instant};

/// Default wall-clock budget for dynamic candidate attempts within one block.
///
/// Mantle block time is ~2s; stay well under that so processing never falls
/// behind tip under normal preflight cost.
pub const DEFAULT_ATTEMPT_BUDGET: Duration = Duration::from_millis(400);

/// Env override for [`DEFAULT_ATTEMPT_BUDGET`] (milliseconds).
pub const ENV_ATTEMPT_BUDGET_MS: &str = "BOT_ATTEMPT_BUDGET_MS";

/// Why a candidate is statically ineligible for the armed send attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticIneligibility {
    /// Cross-protocol routes are observed but not sent (canary = pure only).
    CrossProtocol,
    /// `amount_in` exceeds `MAX_INPUT_PER_TX_WMNT_WEI`.
    InputExceedsPerTxCap,
    /// `amount_in` exceeds the known executor WMNT balance.
    InputExceedsBalance,
    /// Executor balance exceeds `MAX_TOTAL_INVENTORY_WMNT_WEI` — nothing is
    /// sendable this block (smaller input does not help).
    InventoryPreconditionFailed,
    /// Route bucket has no approved gas profile (fail closed).
    MissingGasProfile,
}

/// Result of static eligibility evaluation for one candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticEligibility {
    Eligible,
    Ineligible(StaticIneligibility),
}

impl StaticEligibility {
    pub fn is_eligible(self) -> bool {
        matches!(self, Self::Eligible)
    }
}

/// Bounds used for static eligibility when the production send path is armed.
///
/// Mirrors the reject conditions in [`crate::service::send_path::enforce_inventory_caps`]
/// plus protocol-mix and gas-profile presence. Fields that are `None` skip that check.
/// Live canary/production pins `executor_balance` once per head (WHI-950 strategy A).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EligibilityBounds {
    pub max_input_per_tx_wmnt_wei: Option<U256>,
    pub max_total_inventory_wmnt_wei: Option<U256>,
    pub executor_balance: Option<U256>,
    /// When true, `has_gas_profile(route_key)` must succeed.
    pub require_gas_profile: bool,
}

impl EligibilityBounds {
    /// No static caps — used when the send gate is closed (historical top-1 path).
    pub fn unrestricted() -> Self {
        Self {
            max_input_per_tx_wmnt_wei: None,
            max_total_inventory_wmnt_wei: None,
            executor_balance: None,
            require_gas_profile: false,
        }
    }

    /// Armed production bounds from breaker caps (balance optional until read).
    pub fn from_breaker_caps(
        max_input_per_tx_wmnt_wei: u128,
        max_total_inventory_wmnt_wei: u128,
        executor_balance: Option<U256>,
    ) -> Self {
        Self {
            max_input_per_tx_wmnt_wei: Some(U256::from(max_input_per_tx_wmnt_wei)),
            max_total_inventory_wmnt_wei: Some(U256::from(max_total_inventory_wmnt_wei)),
            executor_balance,
            require_gas_profile: true,
        }
    }

    /// Global inventory precondition: balance already exceeds total inventory cap.
    pub fn inventory_precondition_failed(&self) -> bool {
        match (self.executor_balance, self.max_total_inventory_wmnt_wei) {
            (Some(bal), Some(cap)) => bal > cap,
            _ => false,
        }
    }
}

/// Resolve the attempt budget from env or the default.
pub fn resolve_attempt_budget() -> Duration {
    match std::env::var(ENV_ATTEMPT_BUDGET_MS) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => DEFAULT_ATTEMPT_BUDGET,
        },
        Err(_) => DEFAULT_ATTEMPT_BUDGET,
    }
}

/// Wall-clock deadline for dynamic attempts inside one block.
#[derive(Debug, Clone, Copy)]
pub struct AttemptBudget {
    deadline: Instant,
}

impl AttemptBudget {
    pub fn from_now(budget: Duration) -> Self {
        Self {
            deadline: Instant::now() + budget,
        }
    }

    pub fn exhausted(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// Evaluate one candidate against static **send** eligibility.
///
/// Applied for summary counters always (G-5 observability) and for the armed
/// attempt plan (G-4). Gate-closed attempt selection still uses historical
/// top-1 via [`candidates_for_attempt`] and does not consult this result.
pub fn evaluate_static_eligibility(
    opp: &DiscoveredOpportunity,
    bounds: &EligibilityBounds,
    has_gas_profile: impl Fn(&RouteKey) -> bool,
) -> StaticEligibility {
    if bounds.inventory_precondition_failed() {
        return StaticEligibility::Ineligible(StaticIneligibility::InventoryPreconditionFailed);
    }

    if opp.is_cross_protocol {
        return StaticEligibility::Ineligible(StaticIneligibility::CrossProtocol);
    }

    let amount_in = opp.candidate.input;
    if let Some(per_tx) = bounds.max_input_per_tx_wmnt_wei {
        if amount_in > per_tx {
            return StaticEligibility::Ineligible(StaticIneligibility::InputExceedsPerTxCap);
        }
    }
    if let Some(balance) = bounds.executor_balance {
        if amount_in > balance {
            return StaticEligibility::Ineligible(StaticIneligibility::InputExceedsBalance);
        }
    }
    if bounds.require_gas_profile && !has_gas_profile(&opp.route_key) {
        return StaticEligibility::Ineligible(StaticIneligibility::MissingGasProfile);
    }

    StaticEligibility::Eligible
}

/// Per-block classification over a net-PnL-ranked opportunity list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EligibilityView {
    /// Indices into the original `opportunities` slice, still in net-PnL order.
    pub eligible_indices: Vec<usize>,
    pub eligible_count: u64,
    pub mixed_skipped_count: u64,
    pub best_mixed_net: Option<U256>,
    pub best_net: Option<U256>,
    /// True when inventory precondition fails — nothing is sendable this block.
    pub inventory_block_unsendable: bool,
}

/// Classify ranked opportunities: static filter first, preserve net order among eligible.
///
/// Always records `mixed_skipped_count` / `best_mixed_net` for the per-block summary
/// (G-5), regardless of whether the send gate is armed. Does **not** emit a log line
/// per skipped candidate. `eligible_*` uses send-eligibility (pure + caps + profile),
/// not the gate-closed attempt plan.
pub fn classify_opportunities(
    opportunities: &[DiscoveredOpportunity],
    bounds: &EligibilityBounds,
    has_gas_profile: impl Fn(&RouteKey) -> bool,
) -> EligibilityView {
    let mut view = EligibilityView {
        inventory_block_unsendable: bounds.inventory_precondition_failed(),
        ..EligibilityView::default()
    };

    for (idx, opp) in opportunities.iter().enumerate() {
        let net = opp.candidate.net_profit;
        view.best_net = Some(view.best_net.map_or(net, |b| b.max(net)));

        // Mixed observability is independent of caps: always count cross-protocol
        // candidates for G-5 summary fields.
        if opp.is_cross_protocol {
            view.mixed_skipped_count += 1;
            view.best_mixed_net = Some(view.best_mixed_net.map_or(net, |b| b.max(net)));
        }

        let eligibility = evaluate_static_eligibility(opp, bounds, &has_gas_profile);
        if eligibility.is_eligible() {
            view.eligible_indices.push(idx);
            view.eligible_count += 1;
        }
    }

    view
}

/// Candidates to attempt, in order.
///
/// * Gate **closed**: historical top-1 only (behaviour unchanged).
/// * Gate **armed**: every statically eligible candidate in net-PnL order.
pub fn candidates_for_attempt<'a>(
    opportunities: &'a [DiscoveredOpportunity],
    view: &EligibilityView,
    production_send_armed: bool,
) -> Vec<&'a DiscoveredOpportunity> {
    if opportunities.is_empty() {
        return Vec::new();
    }
    if !production_send_armed {
        return opportunities.first().into_iter().collect();
    }
    view.eligible_indices
        .iter()
        .filter_map(|&i| opportunities.get(i))
        .collect()
}

/// Outcome of walking eligible candidates under a time budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptSelectionOutcome {
    /// No candidate to attempt (empty list or all statically filtered when armed).
    NoCandidate,
    /// Budget exhausted before a successful broadcast (or before any try finished).
    BudgetExhausted { tried: u64 },
    /// At least one dynamic preflight failed and no further eligible candidates remain.
    ExhaustedEligible { tried: u64 },
    /// A candidate was selected for attempt (caller performs the async call).
    Try { index_in_plan: usize },
}

/// Decide whether to try the next candidate given budget and prior dynamic failures.
pub fn next_attempt_decision(
    plan_len: usize,
    next_index: usize,
    tried: u64,
    budget: &AttemptBudget,
) -> AttemptSelectionOutcome {
    if plan_len == 0 {
        return AttemptSelectionOutcome::NoCandidate;
    }
    if next_index >= plan_len {
        return AttemptSelectionOutcome::ExhaustedEligible { tried };
    }
    if budget.exhausted() {
        return AttemptSelectionOutcome::BudgetExhausted { tried };
    }
    AttemptSelectionOutcome::Try {
        index_in_plan: next_index,
    }
}

/// Classify opportunities using the armed send runtime's caps + gas profile (or
/// unrestricted bounds when no runtime is present).
///
/// `executor_balance` is the strategy-A pin from WHI-950 / G-3 (one hash-pinned
/// read per block). When `None`, only mix / per-tx / profile filters apply at
/// static time; balance remains a send-time reject that can advance dynamically.
pub fn classify_with_send_runtime(
    opportunities: &[DiscoveredOpportunity],
    send: Option<&crate::service::send_path::SendRuntime>,
    executor_balance: Option<U256>,
) -> EligibilityView {
    match send {
        Some(rt) => {
            let bounds = rt.eligibility_bounds(executor_balance);
            classify_opportunities(opportunities, &bounds, |k| rt.route_has_gas_profile(k))
        }
        None => classify_opportunities(
            opportunities,
            &EligibilityBounds::unrestricted(),
            |_| true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amms::amm::AMM;
    use crate::amms::uniswap_v2::UniswapV2Pool;
    use crate::amms::amm::AutomatedMarketMaker;
    use crate::arbitrage::pathfinder::{ArbitragePath, PathHop};
    use crate::execution::{ProtocolKind, RouteKey};
    use crate::service::protocol::{Candidate, V2_FEE};
    use crate::state_space::SnapshotId;
    use alloy::primitives::{address, B256, I256};

    fn sample_pool() -> AMM {
        let mut p = UniswapV2Pool::new(address!("00000000000000000000000000000000000000a1"), V2_FEE);
        p.token_a = crate::amms::Token::new_with_decimals(
            address!("0000000000000000000000000000000000000001"),
            18,
        );
        p.token_b = crate::amms::Token::new_with_decimals(
            address!("0000000000000000000000000000000000000002"),
            18,
        );
        p.reserve_0 = 1_000_000_000_000_000_000_000;
        p.reserve_1 = 1_000_000_000_000_000_000_000;
        AMM::UniswapV2Pool(p)
    }

    fn opp(
        net: u64,
        input: u64,
        is_cross: bool,
        kinds: &[ProtocolKind],
    ) -> DiscoveredOpportunity {
        let pool = sample_pool();
        let pool_addr = pool.address();
        let t0 = address!("0000000000000000000000000000000000000001");
        let t1 = address!("0000000000000000000000000000000000000002");
        let path = ArbitragePath {
            hops: vec![PathHop {
                pool_address: pool_addr,
                token_in: t0,
                token_out: t1,
                fee_bps: 30,
            }],
        };
        let route_key = RouteKey::new(kinds.to_vec()).expect("route key");
        DiscoveredOpportunity {
            candidate: Candidate {
                snapshot_id: SnapshotId::new(5000, 1, B256::ZERO),
                signature: format!("sig-{net}"),
                hops: 1,
                input: U256::from(input),
                output: U256::from(input + net),
                profit: I256::try_from(net as i128).unwrap_or(I256::ZERO),
                net_profit: U256::from(net),
                pool_addresses: vec![pool_addr],
                token_path: vec![t0, t1],
                amounts_out: vec![U256::from(input + net)],
                expected_states: vec![U256::from(1u64), U256::from(1u64)],
                path,
                pools: vec![pool],
                log_hops: "h".into(),
                roi: "0".into(),
            },
            route_key,
            is_cross_protocol: is_cross,
            protocol_kinds: kinds.to_vec(),
        }
    }

    fn always_profile(_: &RouteKey) -> bool {
        true
    }

    fn never_profile(_: &RouteKey) -> bool {
        false
    }

    /// Acceptance: `[mixed(high net), pure(low net)]` → armed selects pure.
    #[test]
    fn armed_skips_mixed_high_net_selects_pure() {
        let mixed = opp(100, 1_000, true, &[ProtocolKind::V2, ProtocolKind::V3]);
        let pure = opp(10, 1_000, false, &[ProtocolKind::V2]);
        let opps = vec![mixed, pure];
        let bounds = EligibilityBounds::from_breaker_caps(10_000, 1_000_000, Some(U256::from(50_000u64)));
        let view = classify_opportunities(&opps, &bounds, always_profile);
        assert_eq!(view.mixed_skipped_count, 1);
        assert_eq!(view.best_mixed_net, Some(U256::from(100u64)));
        assert_eq!(view.eligible_count, 1);
        let plan = candidates_for_attempt(&opps, &view, true);
        assert_eq!(plan.len(), 1);
        assert!(!plan[0].is_cross_protocol);
        assert_eq!(plan[0].candidate.net_profit, U256::from(10u64));
    }

    /// Acceptance: `[pure-unqualified(high, over per-tx), pure-eligible(low)]`.
    #[test]
    fn armed_skips_over_per_tx_cap_selects_eligible() {
        let over = opp(100, 50_000, false, &[ProtocolKind::V2]);
        let ok = opp(10, 1_000, false, &[ProtocolKind::V2]);
        let opps = vec![over, ok];
        let bounds = EligibilityBounds::from_breaker_caps(10_000, 1_000_000, Some(U256::from(100_000u64)));
        let view = classify_opportunities(&opps, &bounds, always_profile);
        assert_eq!(view.eligible_count, 1);
        let plan = candidates_for_attempt(&opps, &view, true);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].candidate.input, U256::from(1_000u64));
        assert_eq!(plan[0].candidate.net_profit, U256::from(10u64));
    }

    /// Acceptance: gate closed → behaviour unchanged (top-1, including mixed).
    #[test]
    fn gate_closed_keeps_top1_including_mixed() {
        let mixed = opp(100, 1_000, true, &[ProtocolKind::V2, ProtocolKind::V3]);
        let pure = opp(10, 1_000, false, &[ProtocolKind::V2]);
        let opps = vec![mixed, pure];
        let bounds = EligibilityBounds::unrestricted();
        let view = classify_opportunities(&opps, &bounds, always_profile);
        // Summary still sees the mixed candidate and counts only pure as eligible.
        assert_eq!(view.mixed_skipped_count, 1);
        assert_eq!(view.best_mixed_net, Some(U256::from(100u64)));
        assert_eq!(view.eligible_count, 1);
        // Gate-closed attempt plan is historical top-1 (mixed), not the eligible list.
        let plan = candidates_for_attempt(&opps, &view, false);
        assert_eq!(plan.len(), 1);
        assert!(plan[0].is_cross_protocol);
        assert_eq!(plan[0].candidate.net_profit, U256::from(100u64));
    }

    #[test]
    fn inventory_precondition_makes_block_unsendable() {
        let pure = opp(50, 1_000, false, &[ProtocolKind::V2]);
        let opps = vec![pure];
        // balance 2_000_000 > max_total 1_000_000
        let bounds =
            EligibilityBounds::from_breaker_caps(10_000, 1_000_000, Some(U256::from(2_000_000u64)));
        let view = classify_opportunities(&opps, &bounds, always_profile);
        assert!(view.inventory_block_unsendable);
        assert_eq!(view.eligible_count, 0);
        assert!(candidates_for_attempt(&opps, &view, true).is_empty());
    }

    #[test]
    fn missing_gas_profile_filters_candidate() {
        let pure = opp(50, 1_000, false, &[ProtocolKind::V2]);
        let opps = vec![pure];
        let bounds = EligibilityBounds::from_breaker_caps(10_000, 1_000_000, Some(U256::from(50_000u64)));
        let view = classify_opportunities(&opps, &bounds, never_profile);
        assert_eq!(view.eligible_count, 0);
        assert_eq!(
            evaluate_static_eligibility(&opps[0], &bounds, never_profile),
            StaticEligibility::Ineligible(StaticIneligibility::MissingGasProfile)
        );
    }

    #[test]
    fn balance_cap_filters_oversize_input() {
        let big = opp(50, 9_000, false, &[ProtocolKind::V2]);
        let small = opp(10, 500, false, &[ProtocolKind::V2]);
        let opps = vec![big, small];
        let bounds =
            EligibilityBounds::from_breaker_caps(10_000, 1_000_000, Some(U256::from(1_000u64)));
        let view = classify_opportunities(&opps, &bounds, always_profile);
        let plan = candidates_for_attempt(&opps, &view, true);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].candidate.input, U256::from(500u64));
    }

    #[test]
    fn budget_exhaustion_stops_before_next_try() {
        let budget = AttemptBudget {
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert!(budget.exhausted());
        assert_eq!(
            next_attempt_decision(3, 0, 0, &budget),
            AttemptSelectionOutcome::BudgetExhausted { tried: 0 }
        );
        let live = AttemptBudget::from_now(Duration::from_secs(5));
        assert_eq!(
            next_attempt_decision(2, 0, 0, &live),
            AttemptSelectionOutcome::Try { index_in_plan: 0 }
        );
        assert_eq!(
            next_attempt_decision(2, 2, 2, &live),
            AttemptSelectionOutcome::ExhaustedEligible { tried: 2 }
        );
        assert_eq!(
            next_attempt_decision(0, 0, 0, &live),
            AttemptSelectionOutcome::NoCandidate
        );
    }

    #[test]
    fn summary_fields_do_not_sum_skipped_profit() {
        // Two mixed candidates: best_mixed_net is max, not sum.
        let m1 = opp(30, 100, true, &[ProtocolKind::V2, ProtocolKind::Moe]);
        let m2 = opp(70, 100, true, &[ProtocolKind::V2, ProtocolKind::V3]);
        let pure = opp(5, 100, false, &[ProtocolKind::V2]);
        let opps = vec![m2, m1, pure]; // already net-ranked-ish
        let bounds = EligibilityBounds::from_breaker_caps(10_000, 1_000_000, None);
        let view = classify_opportunities(&opps, &bounds, always_profile);
        assert_eq!(view.mixed_skipped_count, 2);
        assert_eq!(view.best_mixed_net, Some(U256::from(70u64)));
        assert_ne!(view.best_mixed_net, Some(U256::from(100u64))); // not 30+70
    }

    #[test]
    fn default_attempt_budget_is_under_one_second() {
        assert!(DEFAULT_ATTEMPT_BUDGET.as_millis() > 0);
        assert!(DEFAULT_ATTEMPT_BUDGET.as_millis() < 1000);
    }
}
