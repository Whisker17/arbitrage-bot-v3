//! Discovery-side measured fee scoring (WHI-949 / G-2).
//!
//! Live discovery ranks candidates with the **same** pure function the send path
//! uses: [`crate::execution::fee_plan_cost`] / [`FeePolicy::build`]. Offline
//! fixtures keep the fixed hop table in [`crate::service::gas::GasConfig`].

use crate::execution::{
    fee_plan_cost, BlockFeeContext, FeePlanError, FeePolicy, FeeScoreKey, GasQuote, RouteKey,
    RuntimeGasProfile, RuntimeGasProfileError,
};
use alloy::primitives::U256;
use std::sync::Arc;

/// Measured fee inputs shared by discovery ranking and send admission.
///
/// Holds the O(1) profile index + EIP-1559 policy + live block fee context.
/// No per-candidate RPC.
#[derive(Clone, Debug)]
pub struct MeasuredFeeScoring {
    pub gas_profile: Arc<RuntimeGasProfile>,
    pub priority_fee_per_gas: u128,
    pub block_gas_reserve: u64,
    pub fee_context: BlockFeeContext,
}

/// Fail-closed discovery fee errors (profile lookup or FeePolicy rejections).
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryFeeError {
    #[error(transparent)]
    Profile(#[from] RuntimeGasProfileError),
    #[error(transparent)]
    FeePlan(#[from] FeePlanError),
}

impl MeasuredFeeScoring {
    pub fn new(
        gas_profile: Arc<RuntimeGasProfile>,
        priority_fee_per_gas: u128,
        block_gas_reserve: u64,
        fee_context: BlockFeeContext,
    ) -> Self {
        Self {
            gas_profile,
            priority_fee_per_gas,
            block_gas_reserve,
            fee_context,
        }
    }

    pub fn policy(&self) -> FeePolicy {
        FeePolicy::new(self.priority_fee_per_gas, self.block_gas_reserve)
    }

    pub fn fee_score_key(&self) -> FeeScoreKey {
        FeeScoreKey::from_policy_and_context(self.policy(), &self.fee_context)
    }

    /// G-2 surface for G-1: `fee_plan_cost(route_key, fee_context)`.
    ///
    /// O(1) profile lookup + shared [`fee_plan_cost`]. Unknown / unapproved
    /// route buckets fail closed (no hop-table fallback).
    pub fn fee_plan_cost(&self, route_key: &RouteKey) -> Result<U256, DiscoveryFeeError> {
        let quote = self.gas_profile.quote(route_key)?;
        Ok(fee_plan_cost(
            &quote,
            &self.fee_context,
            self.policy(),
        )?)
    }

    /// Same as [`Self::fee_plan_cost`] but returns the full quote + cost for tests.
    pub fn quote_and_cost(
        &self,
        route_key: &RouteKey,
    ) -> Result<(GasQuote, U256), DiscoveryFeeError> {
        let quote = self.gas_profile.quote(route_key)?;
        let cost = fee_plan_cost(&quote, &self.fee_context, self.policy())?;
        Ok((quote, cost))
    }
}

/// Classify a discovery fee failure for metrics.
pub fn discovery_fee_reject_reason(err: &DiscoveryFeeError) -> &'static str {
    use crate::metrics::reject_reason;
    match err {
        DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_))
        | DiscoveryFeeError::Profile(RuntimeGasProfileError::UnapprovedRoute(_)) => {
            reject_reason::GAS_PROFILE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::GasLimitExceedsBlockReserve { .. }) => {
            reject_reason::GAS_RESERVE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::InvalidGasQuote) => reject_reason::GAS_SCREEN,
        _ => reject_reason::GAS_SCREEN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        ExecutorIdentity, ProtocolKind, RuntimeProfileConfig, TickCrossingBucket,
    };
    use alloy::primitives::B256;
    use std::path::PathBuf;

    fn load_mainnet_profile() -> Arc<RuntimeGasProfile> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("config/gas_profiles/mantle_mainnet_v1.json");
        Arc::new(
            RuntimeGasProfile::load(&path, RuntimeProfileConfig::mantle_mainnet(Vec::new()))
                .expect("load mainnet profile"),
        )
    }

    fn scoring(priority: u128, reserve: u64, base_fee: u128, block_gas_limit: u64) -> MeasuredFeeScoring {
        MeasuredFeeScoring::new(
            load_mainnet_profile(),
            priority,
            reserve,
            BlockFeeContext {
                block_number: 1,
                block_hash: B256::ZERO,
                base_fee_per_gas: base_fee,
                block_gas_limit,
            },
        )
    }

    #[test]
    fn discovery_and_executor_share_wei_identical_cost() {
        let priority = 100_000u128;
        let reserve = 1u64;
        let base_fee = 50_000_000u128;
        let m = scoring(priority, reserve, base_fee, 30_000_000);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();

        // Discovery surface: MeasuredFeeScoring::fee_plan_cost (profile lookup + cost).
        let discovery_cost = m.fee_plan_cost(&route).expect("approved v2/v2");
        // Executor surface: quote from profile + FeePolicy::build (send_path path).
        let quote = m.gas_profile.quote(&route).expect("quote");
        let executor_plan = FeePolicy::new(priority, reserve)
            .build(&quote, &m.fee_context)
            .expect("executor FeePolicy::build");

        assert_eq!(discovery_cost, executor_plan.expected_gas_cost);
        let expected = U256::from(quote.expected_gas_used)
            * U256::from(base_fee.checked_add(priority).unwrap());
        assert_eq!(discovery_cost, expected);
        assert_eq!(executor_plan.max_priority_fee_per_gas, priority);
    }

    #[test]
    fn non_zero_priority_fee_is_included_in_cost() {
        let base_fee = 40u128;
        let with_priority = scoring(10, 1, base_fee, 30_000_000);
        let zero_priority = scoring(0, 1, base_fee, 30_000_000);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();

        let a = with_priority.fee_plan_cost(&route).unwrap();
        let b = zero_priority.fee_plan_cost(&route).unwrap();
        assert!(a > b, "priority fee must increase expected_gas_cost");
        assert_eq!(a - b, U256::from(with_priority.gas_profile.quote(&route).unwrap().expected_gas_used) * U256::from(10u64));
    }

    #[test]
    fn gas_limit_exceeds_block_reserve_rejected_on_discovery() {
        // Tiny block gas limit so approved profile gas_limit exceeds available.
        let m = scoring(10, 100, 50, 200);
        let route = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::FeePlan(FeePlanError::GasLimitExceedsBlockReserve { .. })
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::GAS_RESERVE
        );
    }

    #[test]
    fn unknown_route_bucket_fails_closed_no_hop_table_fallback() {
        let m = scoring(10, 1, 50, 30_000_000);
        // 3-hop pure v2 is unsupported in the pinned mainnet profile.
        let route = RouteKey::new(vec![
            ProtocolKind::V2,
            ProtocolKind::V2,
            ProtocolKind::V2,
        ])
        .unwrap();
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::Profile(RuntimeGasProfileError::UnapprovedRoute(_))
                | DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_))
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::GAS_PROFILE
        );
    }

    #[test]
    fn fee_score_key_tracks_priority_and_reserve() {
        let a = scoring(100_000, 1, 50, 30_000_000).fee_score_key();
        let b = scoring(200_000, 1, 50, 30_000_000).fee_score_key();
        let c = scoring(100_000, 2, 50, 30_000_000).fee_score_key();
        let d = scoring(100_000, 1, 50, 29_000_000).fee_score_key();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert_eq!(a.base_fee_per_gas, b.base_fee_per_gas);
    }

    #[test]
    fn identity_config_still_loads() {
        // Sanity: RuntimeGasProfile identity pins stay green under G-2 wiring.
        let _ = load_mainnet_profile();
        let _ = ExecutorIdentity::mantle_mainnet();
        let _ = TickCrossingBucket::Zero;
    }
}
