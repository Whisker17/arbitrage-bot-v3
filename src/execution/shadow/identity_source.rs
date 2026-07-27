//! `ShadowIdentitySource` — a signerless [`ExecutionIdentitySource`] for shadow mode.
//!
//! Validates identity exactly the way [`LiveExecutionIdentitySource`] does — by
//! delegating to one directly, so there is exactly one implementation of the
//! state-space/fee-context/route checks, not a duplicated copy that could drift — but
//! `acquire_send_lease` always fails closed with [`IdentityError::SendLeaseUnavailable`].
//! This is the same pattern `NoopPreflight`/`NoopDurableHook` already use to encode a
//! capability restriction in the type rather than a runtime flag: shadow mode
//! structurally cannot reach a send lease, and therefore can never
//! `sign_final_request`/broadcast, regardless of what a caller passes in.

use super::super::identity::{
    ExecutionIdentity, ExecutionIdentityLease, ExecutionIdentitySource, IdentityError,
    LiveExecutionIdentitySource,
};

#[derive(Clone)]
pub struct ShadowIdentitySource {
    inner: LiveExecutionIdentitySource,
}

impl ShadowIdentitySource {
    pub fn new(inner: LiveExecutionIdentitySource) -> Self {
        Self { inner }
    }
}

impl ExecutionIdentitySource for ShadowIdentitySource {
    async fn validate(&self, identity: &ExecutionIdentity) -> Result<(), IdentityError> {
        self.inner.validate(identity).await
    }

    async fn acquire_send_lease(
        &self,
        _identity: &ExecutionIdentity,
    ) -> Result<ExecutionIdentityLease, IdentityError> {
        Err(IdentityError::SendLeaseUnavailable(
            "shadow mode never sends".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::fee_context::{BlockFeeContext, BlockFeeContextCache};
    use crate::execution::gas_profile::{load_artifact, ProtocolKind, RouteKey};
    use crate::execution::gas_runtime::{RuntimeGasProfile, RuntimeProfileConfig};
    use crate::state_space::{BlockHeaderContext, SnapshotId, SnapshotPublisher};
    use alloy::primitives::B256;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn sample_identity() -> ExecutionIdentity {
        ExecutionIdentity {
            snapshot_id: SnapshotId::new(5000, 1, B256::repeat_byte(1)),
            header: BlockHeaderContext::new(B256::ZERO, 1_700_000_000),
            pool_universe_fingerprint: B256::repeat_byte(2),
            route: RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap(),
            fee_context: BlockFeeContext {
                block_number: 1,
                block_hash: B256::repeat_byte(1),
                base_fee_per_gas: 1,
                block_gas_limit: 1_000_000,
            },
            gas_profile_identity: "fixture".into(),
        }
    }

    fn artifact_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("config/gas_profiles/mantle_mainnet_v1.json")
    }

    fn shadow_source() -> ShadowIdentitySource {
        let route_key = RouteKey::new(vec![ProtocolKind::V2, ProtocolKind::V2]).unwrap();
        let gas_profile = RuntimeGasProfile::from_artifact(
            load_artifact(&artifact_path()).unwrap(),
            RuntimeProfileConfig::mantle_mainnet(vec![route_key]),
        )
        .unwrap();
        let publisher = SnapshotPublisher::new();
        let fee_contexts = Arc::new(BlockFeeContextCache::default());
        let live = LiveExecutionIdentitySource::new(publisher, fee_contexts, gas_profile);
        ShadowIdentitySource::new(live)
    }

    #[tokio::test]
    async fn acquire_send_lease_always_fails_closed_regardless_of_validity() {
        let source = shadow_source();
        let error = source
            .acquire_send_lease(&sample_identity())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            IdentityError::SendLeaseUnavailable("shadow mode never sends".to_string())
        );
    }

    #[tokio::test]
    async fn validate_fails_the_same_way_the_wrapped_live_source_does() {
        // No snapshot has ever been published, so the publisher is never `Ready` --
        // the same failure a `LiveExecutionIdentitySource` used directly would report.
        let source = shadow_source();
        let error = source.validate(&sample_identity()).await.unwrap_err();
        assert_eq!(error, IdentityError::PublisherNotReady);
    }
}
