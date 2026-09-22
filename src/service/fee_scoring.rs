//! Discovery-side measured fee scoring (WHI-949 / G-2).
//!
//! Live discovery ranks candidates with the **same** pure function the send path
//! uses: [`crate::execution::fee_plan_cost`] / [`FeePolicy::build`]. Offline
//! fixtures keep the fixed hop table in [`crate::service::gas::GasConfig`].
//!
//! Tip fee stamping for the initial live discovery pass (WHI-975) also lives
//! here: resolve the tip via the shared WHI-967 helper, then fail closed with a
//! message that distinguishes "never fetched" from "fetched zeros".

use crate::execution::{
    fee_plan_cost, BlockFeeContext, FeePlanError, FeePolicy, FeeScoreKey, GasQuote, RouteKey,
    RuntimeGasProfile, RuntimeGasProfileError,
};
use crate::state_space::resolve_canonical_tip;
use alloy::consensus::BlockHeader;
use alloy::network::primitives::{BlockResponse, HeaderResponse};
use alloy::network::Network;
use alloy::primitives::{B256, U256};
use alloy::providers::Provider;
use alloy::rpc::types::Block;
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
        DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_)) => {
            reject_reason::UNKNOWN_ROUTE
        }
        DiscoveryFeeError::Profile(RuntimeGasProfileError::UnapprovedRoute(_)) => {
            reject_reason::UNAPPROVED_ROUTE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::GasLimitExceedsBlockReserve { .. }) => {
            reject_reason::GAS_RESERVE
        }
        DiscoveryFeeError::FeePlan(FeePlanError::InvalidGasQuote) => reject_reason::GAS_SCREEN,
        _ => reject_reason::GAS_SCREEN,
    }
}

/// Tip header fields needed to stamp measured discovery scoring (WHI-949 / WHI-975).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryTipFeeFields {
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub base_fee_per_gas: u128,
    pub block_gas_limit: u64,
}

/// Fail-closed tip fee context for measured live discovery (WHI-975).
///
/// Distinguishes a tip that was **never resolved** from a tip header that was
/// fetched but carried genuine zero gas fields — so operators do not chase
/// "chain sent zeros" when the block body simply never arrived.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DiscoveryTipFeeError {
    #[error(
        "live discovery tip was never resolved for measured FeePolicy scoring \
         (WHI-975); tip resolution failed: {detail}; refusing hop-table fallback"
    )]
    NotResolved { detail: String },
    #[error(
        "live discovery tip header carried zero base_fee_per_gas or block_gas_limit \
         (base_fee_per_gas={base_fee_per_gas}, block_gas_limit={block_gas_limit}) \
         for measured FeePolicy scoring (WHI-949); refusing hop-table fallback"
    )]
    ZeroGasFields {
        base_fee_per_gas: u128,
        block_gas_limit: u64,
    },
}

/// Pure WHI-949 guard over fields taken from a **successfully fetched** tip.
pub(crate) fn require_nonzero_tip_fee_fields(
    base_fee_per_gas: u128,
    block_gas_limit: u64,
) -> Result<(), DiscoveryTipFeeError> {
    if base_fee_per_gas == 0 || block_gas_limit == 0 {
        Err(DiscoveryTipFeeError::ZeroGasFields {
            base_fee_per_gas,
            block_gas_limit,
        })
    } else {
        Ok(())
    }
}

/// Resolve the tip with the shared WHI-967 retry, then require non-zero fee fields.
///
/// Used by the live bot's one-shot discovery stamp (WHI-975). Retries `Ok(None)`
/// tip bodies; never falls back to default zeros.
pub async fn resolve_discovery_tip_fee_fields<N, P>(
    provider: &P,
) -> Result<DiscoveryTipFeeFields, DiscoveryTipFeeError>
where
    P: Provider<N>,
    N: Network<BlockResponse = Block>,
{
    let (tip, block) = resolve_canonical_tip(provider).await.map_err(|e| {
        DiscoveryTipFeeError::NotResolved {
            detail: e.to_string(),
        }
    })?;
    let header = block.header();
    let base_fee_per_gas = header.base_fee_per_gas().map(u128::from).unwrap_or(0);
    let block_gas_limit = header.gas_limit();
    require_nonzero_tip_fee_fields(base_fee_per_gas, block_gas_limit)?;
    Ok(DiscoveryTipFeeFields {
        block_number: tip,
        block_hash: header.hash(),
        parent_hash: header.parent_hash(),
        timestamp: header.timestamp(),
        base_fee_per_gas,
        block_gas_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        ExecutorIdentity, ProtocolKind, RuntimeProfileConfig, TickCrossingBucket,
    };
    use crate::state_space::TIP_RESOLUTION_MAX_ATTEMPTS;
    use alloy::network::Ethereum;
    use alloy::primitives::B256;
    use alloy::providers::{mock::Asserter, ProviderBuilder};
    use alloy::rpc::types::Header as RpcHeader;
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

    fn test_hash(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn mock_block_with_fees(
        number: u64,
        hash: B256,
        parent_hash: B256,
        base_fee: Option<u64>,
        gas_limit: u64,
    ) -> Block {
        let mut inner = alloy::consensus::Header::default();
        inner.number = number;
        inner.parent_hash = parent_hash;
        inner.timestamp = number;
        inner.base_fee_per_gas = base_fee;
        inner.gas_limit = gas_limit;
        let mut header = RpcHeader::new(inner);
        header.hash = hash;
        Block::empty(header)
    }

    #[test]
    fn zero_gas_fields_message_distinct_from_not_resolved() {
        let zero = require_nonzero_tip_fee_fields(0, 0).unwrap_err();
        let not_resolved = DiscoveryTipFeeError::NotResolved {
            detail: "Canonical tip block not found at number 42".into(),
        };
        let zero_msg = zero.to_string();
        let not_resolved_msg = not_resolved.to_string();
        assert!(
            zero_msg.contains("carried zero base_fee_per_gas or block_gas_limit"),
            "zero-fields message: {zero_msg}"
        );
        assert!(
            not_resolved_msg.contains("was never resolved"),
            "not-resolved message: {not_resolved_msg}"
        );
        assert_ne!(zero_msg, not_resolved_msg);
        assert!(matches!(
            zero,
            DiscoveryTipFeeError::ZeroGasFields {
                base_fee_per_gas: 0,
                block_gas_limit: 0
            }
        ));
    }

    /// WHI-975: first get_block_by_number returns null (race); second succeeds
    /// with non-zero fee fields — discovery tip stamp proceeds.
    #[tokio::test]
    async fn tip_fee_resolution_retries_null_then_succeeds() {
        let tip = 99u64;
        let tip_hash = test_hash(0x99);
        let parent = test_hash(0x98);
        let asserter = Asserter::new();
        // Attempt 1: number then null body (load-balanced race).
        asserter.push_success(&tip);
        asserter.push_success(&Option::<Block>::None);
        // Attempt 2: number then header with Mantle-like non-zero fees.
        asserter.push_success(&tip);
        asserter.push_success(&Some(mock_block_with_fees(
            tip,
            tip_hash,
            parent,
            Some(50_000_000_000),
            60_000_000,
        )));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let fields = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect("null tip body must be retried and then succeed");

        assert_eq!(fields.block_number, tip);
        assert_eq!(fields.block_hash, tip_hash);
        assert_eq!(fields.parent_hash, parent);
        assert_eq!(fields.base_fee_per_gas, 50_000_000_000);
        assert_eq!(fields.block_gas_limit, 60_000_000);
        assert!(asserter.read_q().is_empty());
    }

    /// WHI-975: a successfully fetched header with genuine zeros aborts with
    /// the zero-fields message, not the not-resolved message.
    #[tokio::test]
    async fn tip_fee_resolution_aborts_on_genuine_zero_fields() {
        let tip = 77u64;
        let tip_hash = test_hash(0x77);
        let parent = test_hash(0x76);
        let asserter = Asserter::new();
        asserter.push_success(&tip);
        // Default header has base_fee=None (→ 0) and gas_limit=0.
        asserter.push_success(&Some(mock_block_with_fees(
            tip, tip_hash, parent, None, 0,
        )));

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let err = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect_err("genuine zero gas fields must fail closed");

        let msg = err.to_string();
        assert!(
            msg.contains("carried zero base_fee_per_gas or block_gas_limit"),
            "expected zero-fields wording, got: {msg}"
        );
        assert!(
            !msg.contains("was never resolved"),
            "must not blame tip resolution when the header was fetched: {msg}"
        );
        assert!(matches!(
            err,
            DiscoveryTipFeeError::ZeroGasFields {
                base_fee_per_gas: 0,
                block_gas_limit: 0
            }
        ));
        assert!(asserter.read_q().is_empty());
    }

    /// WHI-975: exhausted null tip bodies map to NotResolved, not ZeroGasFields.
    #[tokio::test]
    async fn tip_fee_resolution_exhausted_is_not_resolved() {
        let tip = 55u64;
        let asserter = Asserter::new();
        for _ in 0..TIP_RESOLUTION_MAX_ATTEMPTS {
            asserter.push_success(&tip);
            asserter.push_success(&Option::<Block>::None);
        }

        let provider = ProviderBuilder::new().connect_mocked_client(asserter.clone());
        let err = resolve_discovery_tip_fee_fields::<Ethereum, _>(&provider)
            .await
            .expect_err("exhausted tip resolution must fail as not-resolved");

        let msg = err.to_string();
        assert!(
            msg.contains("was never resolved"),
            "expected not-resolved wording, got: {msg}"
        );
        assert!(
            !msg.contains("carried zero"),
            "must not claim zeros when the tip was never fetched: {msg}"
        );
        assert!(matches!(err, DiscoveryTipFeeError::NotResolved { .. }));
        assert!(asserter.read_q().is_empty());
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
    fn unapproved_route_bucket_fails_closed_with_unapproved_metric_label() {
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
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::UNAPPROVED_ROUTE
        );
    }

    #[test]
    fn unknown_route_bucket_fails_closed_with_unknown_metric_label() {
        use crate::execution::TickCrossingBucket;
        let m = scoring(10, 1, 50, 30_000_000);
        // ['v2', 'v3', 'v2'] with 0 tick crossings is absent from the profile -> UnknownRoute.
        let route = RouteKey::new(vec![
            ProtocolKind::V2,
            ProtocolKind::V3,
            ProtocolKind::V2,
        ])
        .unwrap()
        .with_v3_ticks(TickCrossingBucket::Zero);
        let err = m.fee_plan_cost(&route).unwrap_err();
        assert!(matches!(
            err,
            DiscoveryFeeError::Profile(RuntimeGasProfileError::UnknownRoute(_))
        ));
        assert_eq!(
            discovery_fee_reject_reason(&err),
            crate::metrics::reject_reason::UNKNOWN_ROUTE
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
